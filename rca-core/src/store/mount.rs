//! Declares typed child ownership and acquires immutable installed authority.
//!
//! The actor owns declarations and installed Features separately. A declaration
//! keeps its selection callbacks and identity, while weak sites name that
//! declaration without extending either payload lifetime. Acquisition joins the
//! actor mailbox and captures the installation current at that ordered turn.
//!
//! Callback payloads leave their lock or borrow guard before user code runs.
//! A callback panic consumes that payload and ends the actor. It does not leave
//! a declaration that can resume reconciliation.

#[cfg(all(test, feature = "allocation-measurement"))]
pub(super) mod allocation_tests;
#[cfg(all(test, feature = "allocation-measurement"))]
pub(super) mod children_costs;
#[cfg(all(test, feature = "allocation-measurement"))]
pub(in crate::store) mod collection_costs;

#[cfg(test)]
ordinary_test_items! {
    #[cfg(feature = "local")]
    mod tests;
    #[cfg(all(feature = "local", feature = "test-util"))]
    mod local_output_tests;
    mod lease_tests;
    mod root_reaction_tests;
    mod nested_tests;
    #[cfg(feature = "test-util")]
    mod output_tests;
}
pub(in crate::store) mod children;
pub(super) mod collection;
pub(in crate::store) mod commands;
#[cfg(feature = "local")]
pub(super) mod local;
pub(super) mod nested;
use super::mounted::path::BorrowPath;

use super::carriers::{Carriers, Shared};
use super::command::Command;
use super::command_port::{
    CommandAcquisition, CommandAcquisitionFuture, CommandMapper, CommandPort, CommandSite,
};
use super::ingress::Ingress;
use super::lifecycle::Lifecycle;
use super::mounted::{OptionalPath, SharedNode, SharedWeakNode};
use super::reaction::Hook as _;
use super::reaction::{self, SharedHook};
use super::scope::{
    ScopeIdentity, ScopeNodeId, ScopeRegistry, ScopeSlot, ScopeToken, ScopeTransitionBatch,
};
use super::task_context::{ReplyGuard, TaskAccessError, TaskAccessReason, TaskProgress};
use super::transaction::Batch;
use crate::feature::{InitialPolicy, ReactionChange};
use crate::{Feature, ReactionContext, ReduceContext, Task, TaskContext};
use parking_lot::Mutex;
use std::fmt;
use std::hash::Hash;
use std::pin::Pin;
use std::sync::{Arc, OnceLock, Weak};

/// Moves actor-owned callbacks outside their short metadata guard.
trait PayloadCell<P> {
    /// Removes the payload before the actor enters user code.
    fn take_payload(&self) -> Option<P>;

    /// Swaps the payload back without destroying either value under the guard.
    fn restore_payload(&self, payload: &mut Option<P>);
}

impl<P> PayloadCell<P> for Mutex<Option<P>> {
    fn take_payload(&self) -> Option<P> {
        self.lock().take()
    }

    fn restore_payload(&self, payload: &mut Option<P>) {
        std::mem::swap(&mut *self.lock(), payload);
    }
}

impl<P> PayloadCell<P> for std::cell::RefCell<Option<P>> {
    fn take_payload(&self) -> Option<P> {
        self.borrow_mut().take()
    }

    fn restore_payload(&self, payload: &mut Option<P>) {
        std::mem::swap(&mut *self.borrow_mut(), payload);
    }
}

/// Leases callbacks for one serial actor operation, outside their guard.
///
/// Actor serialisation supplies an occupied cell and leaves an empty restore
/// slot. A normal return restores the payload before returning its result.
/// A panic drops the leased payload outside the guard. The actor then stops,
/// so there is no restoration or post-panic retry contract.
fn with_payload<P, T>(cell: &impl PayloadCell<P>, apply: impl FnOnce(&mut P) -> T) -> T {
    let mut payload = cell
        .take_payload()
        .expect("serial actor leases declaration once");
    let result = apply(&mut payload);
    let mut restoring = Some(payload);
    cell.restore_payload(&mut restoring);
    drop(restoring);
    result
}

/// Selects an owned initial value and produces deferred work while admission remains live.
type InitialHook<S, A> = Box<dyn for<'a> FnMut(&S, ReduceContext<'a>) -> Option<Task<S, A>> + Send>;

/// Checks root admission between initial selection and the task factory.
fn initial_hook<S: 'static, A: 'static, V: 'static>(
    mut select: impl FnMut(&S) -> V + Send + 'static,
    mut factory: impl for<'a> FnMut(ReactionChange<V>, ReactionContext<'a, S, A>) -> Task<S, A>
    + Send
    + 'static,
) -> InitialHook<S, A> {
    Box::new(move |state, context| {
        let current = select(state);
        if !context.installation_is_live() {
            return None;
        }
        Some(factory(
            ReactionChange::Initial { current },
            ReactionContext::new(context),
        ))
    })
}

/// Declares explicit root installation without changing the Feature reducer contract.
struct MountConstruction<F: Feature> {
    /// Feature instance transferred to its installed node or root reducer.
    feature: F,
    /// Singleton hook, replaced by a later `on_initial` call.
    initial: Option<InitialHook<F::State, F::Action>>,
    /// Ordered registrations with independent initial policies.
    reactions: Vec<SharedHook<F::State, F::Action>>,
    /// Optional marker that expires authority without replacing the Feature.
    #[expect(
        clippy::type_complexity,
        reason = "Retains the typed declaration callback without adding an alias."
    )]
    configuration: Option<Box<dyn FnMut(&F::State) -> u64 + Send>>,
}

impl<F: Feature> MountConstruction<F> {
    /// Owns the root Feature; construction neither installs nor starts work.
    pub fn new(feature: F) -> Self {
        Self {
            feature,
            initial: None,
            reactions: Vec::new(),
            configuration: None,
        }
    }

    /// Declares configuration changes that retire captured root and descendant authority.
    ///
    /// Equal markers retain authority. A changed marker preserves all Feature
    /// instances and private caches, but requires explicit fresh acquisition.
    #[must_use]
    pub fn configuration(mut self, select: impl FnMut(&F::State) -> u64 + Send + 'static) -> Self {
        self.configuration = Some(Box::new(select));
        self
    }

    /// Declares one initial task from an owned selected root value.
    #[must_use]
    pub fn on_initial<V: 'static>(
        mut self,
        select: impl FnMut(&F::State) -> V + Send + 'static,
        factory: impl for<'a> FnMut(
            ReactionChange<V>,
            ReactionContext<'a, F::State, F::Action>,
        ) -> Task<F::State, F::Action>
        + Send
        + 'static,
    ) -> Self
    where
        F::State: 'static,
        F::Action: 'static,
    {
        self.initial = Some(initial_hook(select, factory));
        self
    }
}

/// Declares explicit root installation without changing the Feature reducer contract.
pub struct RootMount<F: Feature>(
    /// Root Feature and callbacks, not yet installed in an actor.
    MountConstruction<F>,
);

impl<F: Feature> RootMount<F> {
    /// Owns the root Feature without installing it.
    pub fn new(feature: F) -> Self {
        Self(MountConstruction::new(feature))
    }

    /// Appends an owned transaction reaction with its own initial policy.
    ///
    /// Registrations run in declaration order after the singleton Initial hook.
    /// Mounted children run before their parents. Each admitted mutation captures
    /// the live registrations before mutation and compares surviving,
    /// configuration-matched registrations after reconciliation. Only unequal
    /// endpoints call the factory. Returned tasks run later, without extending
    /// this finite reaction pass.
    ///
    /// Selected values must independently represent their observed State: cloning
    /// a shared mutable handle is not a historical snapshot. `V` needs neither
    /// `Clone` nor `Send` nor `Sync`; capturing it in a returned Shared task still
    /// requires that task's normal `Send` bounds. All live registrations are
    /// observed, including hooks outside the input's subtree.
    ///
    /// `initial` is explicit for each registration. `Skip` does not select on
    /// installation; `Run` selects and calls the factory once for a fresh
    /// incarnation. Configuration refresh invalidates old history without
    /// replaying Initial. Repeated calls append hooks, while `on_initial` retains
    /// its single replacement slot and runs before these initial factories.
    ///
    /// # Examples
    ///
    /// ```
    /// use rca_core::{Feature, FeatureContext, InitialPolicy, RootMount, Task};
    /// # struct Counter;
    /// # impl Feature for Counter {
    /// #     type State = i32;
    /// #     type Action = i32;
    /// #     fn update(&mut self, state: &mut i32, action: &i32, _: &mut FeatureContext<'_, i32, i32>) { *state += action; }
    /// # }
    /// let root = RootMount::new(Counter)
    ///     .on_change(|state| *state, InitialPolicy::Skip, |change, _| {
    ///         Task::run(move |_| async move { let _ = change; })
    ///     })
    ///     .on_change(|state| state.is_positive(), InitialPolicy::Run, |change, _| {
    ///         Task::run(move |_| async move { let _ = change; })
    ///     });
    /// # drop(root);
    /// ```
    #[must_use]
    pub fn on_change<V: PartialEq + 'static>(
        mut self,
        select: impl FnMut(&F::State) -> V + Send + 'static,
        initial: InitialPolicy,
        factory: impl for<'a> FnMut(
            ReactionChange<V>,
            ReactionContext<'a, F::State, F::Action>,
        ) -> Task<F::State, F::Action>
        + Send
        + 'static,
    ) -> Self
    where
        F::State: 'static,
        F::Action: 'static,
    {
        self.0
            .reactions
            .push(reaction::shared(select, initial, factory));
        self
    }

    /// Declares configuration changes that retire captured root and descendant authority.
    #[must_use]
    pub fn configuration(self, select: impl FnMut(&F::State) -> u64 + Send + 'static) -> Self {
        Self(self.0.configuration(select))
    }

    /// Declares one initial task from an owned selected root value.
    #[must_use]
    pub fn on_initial<V: 'static>(
        self,
        select: impl FnMut(&F::State) -> V + Send + 'static,
        factory: impl for<'a> FnMut(
            ReactionChange<V>,
            ReactionContext<'a, F::State, F::Action>,
        ) -> Task<F::State, F::Action>
        + Send
        + 'static,
    ) -> Self
    where
        F::State: 'static,
        F::Action: 'static,
    {
        Self(self.0.on_initial(select, factory))
    }

    /// Transfers the descriptor into the ordinary typed Feature builder.
    pub fn builder(
        self,
        state: F::State,
        runtime: crate::Runtime,
    ) -> super::builder::StoreBuilder<F::State, F::Action, F>
    where
        F: Send + 'static,
        F::State: Send + 'static,
        F::Action: Send + 'static,
    {
        let MountConstruction {
            feature,
            initial,
            reactions,
            configuration,
        } = self.0;
        super::builder::StoreBuilder::new(state, feature, runtime).with_root_mount(Box::new(
            RootDeclaration::<F::State, F::Action> {
                payload: Mutex::new(Some(RootInitial {
                    hook: initial,
                    reactions,
                    scope: None,
                    pending: true,
                    configuration,
                    current_configuration: None,
                })),
            },
        ))
    }
}

/// Owns construction data for one keyed entry incarnation, without granting authority.
pub struct EntryMount<F: Feature>(
    /// One collection entry's Feature and incarnation-owned callbacks.
    MountConstruction<F>,
);

impl<F: Feature> EntryMount<F> {
    /// Owns one freshly constructed entry Feature.
    pub fn new(feature: F) -> Self {
        Self(MountConstruction::new(feature))
    }

    /// Appends a transaction reaction with an explicit per-registration initial policy.
    ///
    /// See [`RootMount::on_change`] for snapshot, ordering and lifetime rules.
    #[must_use]
    pub fn on_change<V: PartialEq + 'static>(
        mut self,
        select: impl FnMut(&F::State) -> V + Send + 'static,
        initial: InitialPolicy,
        factory: impl for<'a> FnMut(
            ReactionChange<V>,
            ReactionContext<'a, F::State, F::Action>,
        ) -> Task<F::State, F::Action>
        + Send
        + 'static,
    ) -> Self
    where
        F::State: 'static,
        F::Action: 'static,
    {
        self.0
            .reactions
            .push(reaction::shared(select, initial, factory));
        self
    }

    /// Refreshes authority when the selected configuration changes, preserving the Feature.
    #[must_use]
    pub fn configuration(self, select: impl FnMut(&F::State) -> u64 + Send + 'static) -> Self {
        Self(self.0.configuration(select))
    }

    /// Declares one initial task for this entry incarnation.
    #[must_use]
    pub fn on_initial<V: 'static>(
        self,
        select: impl FnMut(&F::State) -> V + Send + 'static,
        factory: impl for<'a> FnMut(
            ReactionChange<V>,
            ReactionContext<'a, F::State, F::Action>,
        ) -> Task<F::State, F::Action>
        + Send
        + 'static,
    ) -> Self
    where
        F::State: 'static,
        F::Action: 'static,
    {
        Self(self.0.on_initial(select, factory))
    }
}

impl<F: Feature> fmt::Debug for EntryMount<F> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EntryMount").finish_non_exhaustive()
    }
}

impl<F: Feature> fmt::Debug for RootMount<F> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RootMount").finish_non_exhaustive()
    }
}

/// Retains root hooks and the configuration observed at the last reconciliation.
struct RootInitial<S, A, C: Carriers<S, A> = Shared> {
    /// Singleton initial callback, if configured.
    hook: Option<C::RootInitialCallback>,
    /// Ordered reaction registrations retained across configuration changes.
    reactions: Vec<C::RootReaction>,
    /// Root token, assigned during the first structural transaction.
    scope: Option<ScopeToken>,
    /// Whether the finite initial pass has not yet visited this declaration.
    pending: bool,
    /// User selector for the root configuration marker.
    configuration: Option<C::RootConfiguration>,
    /// Last selected marker; absence distinguishes first installation.
    current_configuration: Option<u64>,
}

/// Adds root hooks to the same finite roster used by child declarations.
struct RootDeclaration<S, A, C: Carriers<S, A> = Shared> {
    /// Leased during callbacks; the root declaration has no external acquisition site.
    payload: Mutex<Option<RootInitial<S, A, C>>>,
}

impl<S: 'static, A: 'static, C: Carriers<S, A> + 'static> ReactionRunner<S, A, C>
    for RootDeclaration<S, A, C>
where
    C::Task: 'static,
{
    fn run(
        &self,
        state: &S,
        _: ScopeToken,
        mut context: ReduceContext<'_>,
        frame: &mut reaction::Frame<S, Batch<S, A, C>>,
        work: &mut Vec<Batch<S, A, C>>,
    ) {
        with_payload(&self.payload, |value| {
            let fresh = std::mem::replace(&mut value.pending, false);
            if fresh
                && let Some(hook) = &mut value.hook
                && let Some(task) = hook(state, context.reborrow())
            {
                work.push(reaction::root_batch::<S, A, C>(task));
            }
            for (ordinal, hook) in value.reactions.iter().enumerate() {
                if !context.installation_is_live() {
                    break;
                }
                if fresh {
                    if hook.runs_initial()
                        && let Some(task) = hook.initial(state, context.reborrow())
                    {
                        work.push(reaction::root_batch::<S, A, C>(task));
                    }
                } else if let Some(captured) = frame.take(
                    self.registration_identity().expect("root registration"),
                    ordinal,
                    context.scopes(),
                ) && let Some(batch) = captured(state, context.reborrow())
                {
                    work.push(batch);
                }
            }
        });
    }
}

impl<S: 'static, A: 'static, C: Carriers<S, A> + 'static> MountDeclaration<S, A, C>
    for RootDeclaration<S, A, C>
where
    C::Task: 'static,
{
    fn installed_scope(&self) -> Option<ScopeToken> {
        self.payload.lock().as_ref().and_then(|value| value.scope)
    }

    fn registration_identity(&self) -> Option<usize> {
        Some(std::ptr::from_ref(self).cast::<()>() as usize)
    }

    fn has_reactions(&self) -> bool {
        self.payload
            .lock()
            .as_ref()
            .is_some_and(|value| !value.reactions.is_empty())
    }

    fn capture_reactions(
        &self,
        state: &S,
        scopes: &ScopeRegistry,
        frame: &mut reaction::Frame<S, Batch<S, A, C>>,
    ) {
        if !self.has_reactions() {
            return;
        }
        with_payload(&self.payload, |value| {
            let scope = scopes.root();
            let authority = scopes.authority(scope);
            for (ordinal, hook) in value.reactions.iter().enumerate() {
                if !scopes.installation_is_live() {
                    break;
                }
                if let Some(captured) = hook.capture(state, &|| scopes.installation_is_live()) {
                    frame.insert(
                        self.registration_identity().expect("root registration"),
                        ordinal,
                        authority.clone(),
                        Box::new(move |state: &S, context: ReduceContext<'_>| {
                            // The saved continuation owns admission at callback entry.
                            if !context.installation_is_live() {
                                return None;
                            }
                            captured(state, context).map(reaction::root_batch::<S, A, C>)
                        }),
                    );
                }
            }
        });
    }

    fn reaction_entry(&self) -> Option<(ScopeToken, &dyn ReactionRunner<S, A, C>)> {
        let payload = self.payload.lock();
        let value = payload.as_ref().expect("reaction roster owns payload");
        let scope = ((!value.reactions.is_empty()) || (value.pending && value.hook.is_some()))
            .then_some(value.scope)
            .flatten()?;
        Some((scope, self))
    }

    fn bind(&self, _: &Arc<super::ingress::CommandIngress<Command<S, A, C>>>, _: &Arc<Lifecycle>) {
        // Root work uses the existing actor route; there is no acquisition site.
    }

    fn reconcile(
        &self,
        state: &S,
        scopes: &mut ScopeRegistry,
        _: &mut ScopeTransitionBatch,
        _: &mut Vec<Box<C::Node>>,
        _: ScopeToken,
        _: &mut Vec<Box<C::Mount>>,
        _: &mut nested::Descendants<'_, S, A, C>,
        #[cfg(feature = "diagnostics")] mut lifecycle: Option<
            crate::diagnostics::LifecycleCall<'_>,
        >,
    ) {
        with_payload(&self.payload, |initial| {
            initial.scope = Some(scopes.root());
            if let Some(select) = &mut initial.configuration {
                let marker = select(state);
                if !scopes.installation_is_live() {
                    return;
                }
                if initial
                    .current_configuration
                    .is_some_and(|old| old != marker)
                {
                    #[cfg(feature = "diagnostics")]
                    let previous = scopes.configuration(scopes.root());
                    let current = scopes.refresh_configuration(scopes.root());
                    #[cfg(feature = "diagnostics")]
                    if let Some(call) = &mut lifecycle {
                        call.emit(crate::diagnostics::LifecycleKind::ConfigurationChanged {
                            lifetime: scopes.root().diagnostic_id(),
                            previous,
                            current,
                        });
                    }
                    #[cfg(not(feature = "diagnostics"))]
                    let _ = current;
                }
                initial.current_configuration = Some(marker);
            }
        });
    }
}

/// Separates child lifetime identity from configuration refresh.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MountIdentity<K> {
    /// Stable identity within this declaration and parent incarnation.
    pub key: K,
    /// Explicit replacement marker; changing it creates a new incarnation.
    pub replacement: u64,
    /// Configuration marker; changing it preserves the Feature but retires authority.
    pub configuration: u64,
}

/// Declares one optional child with its own independent Feature action domain.
///
/// Attachment does not run selectors or construct the child. The actor installs
/// it in a structural transaction; wait for [`crate::Store::ready`] before
/// acquiring an external context. An Initial task receives its typed context
/// from the runtime after that installation commits.
///
/// A configuration change preserves the child Feature and its private cache,
/// but retires previously acquired contexts. Explicitly acquire again to use
/// the current configuration. A replacement marker instead creates a new
/// incarnation. Equal final `Option` values cannot reveal a remove/reinsert
/// performed within one reducer call: express that replacement in the marker.
/// If identity is present but the child path disappears before construction,
/// installation is deferred until a later structural transaction can select it.
///
/// ```no_run
/// use rca_core::{Feature, FeatureContext, MountIdentity, OptionalMount,
///     RootMount, Runtime, Task, TaskAccessReason};
///
/// struct State { child: Option<usize>, configuration: u64 }
/// #[derive(Debug)]
/// enum RootAction { Refresh }
/// #[derive(Debug)]
/// enum ChildAction { Increment }
/// struct Root;
/// struct Child;
/// impl Feature for Root {
///     type State = State;
///     type Action = RootAction;
///     fn update(&mut self, state: &mut State, _: &RootAction,
///         _: &mut FeatureContext<'_, State, RootAction>) {
///         state.configuration += 1;
///     }
/// }
/// impl Feature for Child {
///     type State = usize;
///     type Action = ChildAction;
///     fn update(&mut self, state: &mut usize, _: &ChildAction,
///         _: &mut FeatureContext<'_, usize, ChildAction>) {
///         *state += 1;
///     }
/// }
/// async fn example(runtime: Runtime) {
///     let child = OptionalMount::new(
///         |state: &State| state.child.as_ref(),
///         |state: &mut State| state.child.as_mut(),
///         |state: &State| state.child.map(|_| MountIdentity {
///             key: (), replacement: 0, configuration: state.configuration,
///         }),
///         |_| Child,
///     ).on_initial(|child| *child, |change, _capture| {
///         // The factory owns this selected value; it does not borrow live State.
///         Task::run(move |context| async move {
///             // A later configuration refresh can retire this context too.
///             let live = context.read(|child| *child).await;
///             let _ = (change, live);
///         })
///     });
///     let (builder, site) = RootMount::new(Root)
///         .builder(State { child: Some(0), configuration: 0 }, runtime)
///         .mount_with_site(child);
///     let store = builder.build().unwrap();
///     store.ready().await.unwrap();
///     let old = store.acquire(&site).await.unwrap().unwrap();
///     old.send(ChildAction::Increment).await.unwrap();
///     store.send(RootAction::Refresh).await.unwrap();
///     assert_eq!(old.read(|child| *child).await.unwrap_err().reason(),
///         TaskAccessReason::ScopeEnded);
///     let fresh = store.acquire(&site).await.unwrap().unwrap();
///     assert_eq!(fresh.read(|child| *child).await.unwrap(), 1);
///     store.shutdown().await.unwrap();
/// }
/// ```
pub struct OptionalMount<R, F: Feature, K> {
    /// Borrows child State for reads and mutations without owning that State.
    path: OptionalPath<R, F::State>,
    /// Selects lifetime and configuration independently of the stored Feature.
    #[expect(
        clippy::type_complexity,
        reason = "Retains the typed declaration callback without adding an alias."
    )]
    identity: Box<dyn FnMut(&R, &dyn Fn() -> bool) -> Option<MountIdentity<K>> + Send>,
    /// Constructs one Feature after a new lifetime has been installed.
    #[expect(
        clippy::type_complexity,
        reason = "Retains the typed declaration callback without adding an alias."
    )]
    make: Box<dyn FnMut(&F::State) -> F + Send>,
    /// Singleton initial callback for each installed incarnation.
    initial: Option<InitialHook<F::State, F::Action>>,
    /// Ordered callbacks shared by the before/after reaction pass.
    reactions: Vec<SharedHook<F::State, F::Action>>,
}

impl<R, F: Feature, K> OptionalMount<R, F, K> {
    /// Declares selection, explicit identity and an incarnation-local constructor.
    ///
    /// `read` and `write` must select the same declared child and agree with
    /// `identity`. Selectors borrow State; they must not mutate it or replace a
    /// child while selecting. External side effects and panics are permitted;
    /// this law does not require pure callbacks or stable memory addresses.
    pub fn new(
        read: for<'a> fn(&'a R) -> Option<&'a F::State>,
        write: for<'a> fn(&'a mut R) -> Option<&'a mut F::State>,
        mut identity: impl FnMut(&R) -> Option<MountIdentity<K>> + Send + 'static,
        make: impl FnMut(&F::State) -> F + Send + 'static,
    ) -> Self {
        Self {
            path: OptionalPath::new(read, write),
            identity: Box::new(move |state, _| identity(state)),
            make: Box::new(make),
            initial: None,
            reactions: Vec::new(),
        }
    }

    /// Appends an owned transaction reaction with its own initial policy.
    ///
    /// Factories run child-before-parent and return deferred work, not mutations.
    /// Selected values must independently represent their observed State.
    /// If the child path is absent at callback entry, that registration is skipped
    /// for the transaction and any captured previous value is released.
    /// See [`RootMount::on_change`] for policy and registration rules.
    #[must_use]
    pub fn on_change<V: PartialEq + 'static>(
        mut self,
        select: impl FnMut(&F::State) -> V + Send + 'static,
        initial: InitialPolicy,
        factory: impl for<'a> FnMut(
            ReactionChange<V>,
            ReactionContext<'a, F::State, F::Action>,
        ) -> Task<F::State, F::Action>
        + Send
        + 'static,
    ) -> Self
    where
        F::State: 'static,
        F::Action: 'static,
    {
        self.reactions
            .push(reaction::shared(select, initial, factory));
        self
    }

    /// Selects an owned value and declares one task per newly installed incarnation.
    ///
    /// The initial pass visits this registration once. If an earlier callback
    /// makes the child path absent, neither callback runs and the pass does not retry.
    #[must_use]
    pub fn on_initial<V: 'static>(
        mut self,
        select: impl FnMut(&F::State) -> V + Send + 'static,
        factory: impl for<'a> FnMut(
            ReactionChange<V>,
            ReactionContext<'a, F::State, F::Action>,
        ) -> Task<F::State, F::Action>
        + Send
        + 'static,
    ) -> Self
    where
        F::State: 'static,
        F::Action: 'static,
    {
        self.initial = Some(initial_hook(select, factory));
        self
    }
}

impl<R, F: Feature, K> fmt::Debug for OptionalMount<R, F, K> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OptionalMount").finish_non_exhaustive()
    }
}

/// Keeps routed paths generic without exposing them in the public direct descriptor.
struct Descriptor<R, F: Feature, K, P> {
    /// Direct or composed child path, cloned into weak contexts and saved reactions.
    path: P,
    /// Selects the desired identity, with checkpoints for composed parent paths.
    #[expect(
        clippy::type_complexity,
        reason = "Retains the typed declaration callback without adding an alias."
    )]
    identity: Box<dyn FnMut(&R, &dyn Fn() -> bool) -> Option<MountIdentity<K>> + Send>,
    /// Constructs the incarnation-local Feature from the selected child State.
    #[expect(
        clippy::type_complexity,
        reason = "Retains the typed declaration callback without adding an alias."
    )]
    make: Box<dyn FnMut(&F::State) -> F + Send>,
    /// Optional singleton initial callback.
    initial: Option<InitialHook<F::State, F::Action>>,
    /// Ordered reaction registrations owned by this declaration.
    reactions: Vec<SharedHook<F::State, F::Action>>,
}

impl<R, F: Feature, K> From<OptionalMount<R, F, K>>
    for Descriptor<R, F, K, OptionalPath<R, F::State>>
{
    fn from(value: OptionalMount<R, F, K>) -> Self {
        Self {
            path: value.path,
            identity: value.identity,
            make: value.make,
            initial: value.initial,
            reactions: value.reactions,
        }
    }
}

/// Erases the declaration-specific future at the weak site's dynamic boundary.
type AcquisitionFuture<S, A> =
    Pin<Box<dyn Future<Output = Result<Option<TaskContext<S, A>>, TaskAccessError>> + Send>>;

/// Provides weak registration views and ordered acquisition for one declaration.
pub(in crate::store) trait Acquisition<R, S, A, RootAction>:
    super::output::HandlerRegistration<R, S, A, RootAction> + commands::Registration<R, RootAction, A>
{
    /// Returns the bound root identity while its lifecycle still exists.
    fn root_identity(&self) -> Option<Arc<()>>;

    /// Borrows State while the fold walk owns the committed declaration roster.
    ///
    /// This method checks admission before path selection. The caller checks
    /// admission after selection and holds the installed scope's authority.
    fn read_committed<'a>(&self, root: &'a R, live: &dyn Fn() -> bool) -> Option<&'a S>;

    /// Checks root ownership and admission before test-gate interaction.
    #[cfg(feature = "test-util")]
    fn validate(&self, root: &Arc<()>) -> Result<(), TaskAccessError>;

    /// Joins the ordered actor mailbox without retaining callback payloads while waiting.
    fn acquire(
        self: Arc<Self>,
        root: Arc<()>,
        #[cfg(feature = "test-util")] gate: Option<
            crate::testing::TransactionGateCore<R, RootAction>,
        >,
    ) -> AcquisitionFuture<S, A>;
}

/// Names a declaration without retaining its Store, callbacks or installed Feature.
pub struct MountSite<R, S, A, RootAction> {
    /// Header-only weak reference; the actor-owned declaration controls payload retirement.
    declaration: Weak<dyn Acquisition<R, S, A, RootAction>>,
}

impl<R, S, A, RootAction> MountSite<R, S, A, RootAction> {
    /// Shares the same weak header with command recipe registration.
    fn command_registration(&self) -> Weak<dyn commands::Registration<R, RootAction, A>> {
        Weak::<dyn Acquisition<R, S, A, RootAction>>::clone(&self.declaration)
    }

    /// Shares the same weak header with committed fold selection.
    pub(super) fn fold_registration(&self) -> Weak<dyn Acquisition<R, S, A, RootAction>> {
        Weak::clone(&self.declaration)
    }

    /// Checks ownership before exposing the declaration to an assertion gate.
    #[cfg(feature = "test-util")]
    pub(super) fn validate(&self, root: &Arc<()>) -> Result<(), TaskAccessError> {
        self.declaration
            .upgrade()
            .ok_or_else(closed)?
            .validate(root)
    }

    /// Shares the same weak header with output-source registration.
    pub(super) fn output_registration(&self) -> Weak<dyn super::output::OutputRegistration> {
        Weak::<dyn Acquisition<R, S, A, RootAction>>::clone(&self.declaration)
    }

    /// Shares the same weak header with typed output-handler registration.
    pub(super) fn handler_registration(
        &self,
    ) -> Weak<dyn super::output::HandlerRegistration<R, S, A, RootAction>> {
        Weak::<dyn Acquisition<R, S, A, RootAction>>::clone(&self.declaration)
    }
}

impl<R, S, A, RootAction> Clone for MountSite<R, S, A, RootAction> {
    fn clone(&self) -> Self {
        Self {
            declaration: Weak::<dyn Acquisition<R, S, A, RootAction>>::clone(&self.declaration),
        }
    }
}

impl<R, S, A, RootAction> fmt::Debug for MountSite<R, S, A, RootAction> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MountSite").finish_non_exhaustive()
    }
}

impl<R: 'static, S: Send + 'static, A: Send + 'static, RootAction: 'static>
    MountSite<R, S, A, RootAction>
{
    /// Captures the actor's current installation through an ordered mailbox request.
    pub(super) fn acquire(
        &self,
        root: Arc<()>,
    ) -> impl Future<Output = Result<Option<TaskContext<S, A>>, TaskAccessError>>
    + Send
    + 'static
    + use<R, S, A, RootAction> {
        let declaration = Weak::<dyn Acquisition<R, S, A, RootAction>>::clone(&self.declaration);
        async move {
            let declaration = declaration.upgrade().ok_or_else(closed)?;
            declaration
                .acquire(
                    root,
                    #[cfg(feature = "test-util")]
                    None,
                )
                .await
        }
    }
}

#[cfg(feature = "test-util")]
impl<R: 'static, S: Send + 'static, A: Send + 'static, RootAction: Send + 'static>
    MountSite<R, S, A, RootAction>
{
    /// Uses the assertion gate while preserving ordinary acquisition ownership checks.
    pub(super) fn acquire_for_test(
        &self,
        root: Arc<()>,
        gate: crate::testing::TransactionGateCore<R, RootAction>,
    ) -> impl Future<Output = Result<Option<TaskContext<S, A>>, TaskAccessError>>
    + Send
    + 'static
    + use<R, S, A, RootAction> {
        let declaration = Weak::<dyn Acquisition<R, S, A, RootAction>>::clone(&self.declaration);
        async move {
            declaration
                .upgrade()
                .ok_or_else(closed)?
                .acquire(root, Some(gate))
                .await
        }
    }
}

/// Connects a declaration to one actor without retaining the actor or its Store.
struct Binding<R, A, C: Carriers<R, A> = Shared> {
    /// Weak mailbox used to admit later acquisition and mounted work.
    ingress: Weak<super::ingress::CommandIngress<Command<R, A, C>>>,
    /// Weak source of root identity and admission state.
    lifecycle: Weak<Lifecycle>,
}

impl<R, A, C: Carriers<R, A>> Clone for Binding<R, A, C> {
    fn clone(&self) -> Self {
        Self {
            ingress: Weak::clone(&self.ingress),
            lifecycle: Weak::clone(&self.lifecycle),
        }
    }
}

/// Keeps command conversion actor-owned and outside declaration locks.
struct CommandMapping<C, A> {
    /// Converts one accepted command to the child action domain.
    map: Box<dyn FnMut(C) -> A + Send>,
    /// Preserves whether the command uses ordinary or control admission.
    class: super::mounted::InputClass,
    /// Weak inspection slot for the single suspended test request.
    #[cfg(feature = "test-util")]
    pending: Option<(usize, Weak<super::mounted::SendSlot<C>>)>,
}

/// Owns callbacks and the last reconciled identity for one declaration.
struct Declaration<R, F: Feature, K, M = (), P = OptionalPath<R, <F as Feature>::State>> {
    /// Selection and construction callbacks, retained across own replacements.
    descriptor: Descriptor<R, F, K, P>,
    /// Weak installed Feature; the actor's node roster owns its lifetime.
    current: Option<SharedWeakNode<F>>,
    /// Selected identity and token, separate from configuration-specific authority.
    installed: Option<(MountIdentity<K>, ScopeToken)>,
    /// Whether this new incarnation still needs its finite initial pass.
    initial_pending: bool,
    /// Command conversion state, or `()` for a direct mount.
    mapping: M,
}

/// Keeps header metadata readable independently of callback payload lifetime.
struct DeclarationCell<R, A, F: Feature, K, M = (), P = OptionalPath<R, <F as Feature>::State>> {
    /// Stable declaration identity used to register successive incarnations.
    node: ScopeNodeId,
    /// Written once when the consuming builder attaches this declaration.
    binding: OnceLock<Binding<R, A>>,
    /// Empty while actor callbacks run or after the owner retires.
    #[expect(
        clippy::type_complexity,
        reason = "The payload preserves Feature, identity, mapper and path types."
    )]
    payload: Mutex<Option<Declaration<R, F, K, M, P>>>,
}

/// Retires callbacks even when another thread temporarily retains the declaration header.
struct DeclarationOwner<R, A, F: Feature, K, M = (), P = OptionalPath<R, <F as Feature>::State>>(
    /// Shared header whose payload lifetime remains owned by this roster entry.
    Arc<DeclarationCell<R, A, F, K, M, P>>,
);

impl<R, A, F: Feature, K, M, P> Drop for DeclarationOwner<R, A, F, K, M, P> {
    fn drop(&mut self) {
        self.drain_payload();
    }
}

impl<R, A, F: Feature, K, M, P> DeclarationOwner<R, A, F, K, M, P> {
    /// Detaches callbacks, retires the Feature, then drops callbacks outside the lock.
    fn drain_payload(&self) {
        let payload = self.0.payload.lock().take();
        if let Some(current) = payload.as_ref().and_then(|value| value.current.as_ref()) {
            current.retire_feature();
        }
        drop(payload);
    }
}

/// Describes one entry's placement in the flat depth-first declaration roster.
pub(crate) struct MountPosition<'a> {
    /// Nesting depth used to delimit the following contiguous descendant prefix.
    pub(crate) depth: usize,
    /// Enclosing installed scope; absent for root-level declarations.
    pub(crate) parent: Option<ScopeToken>,
    /// Borrowed insertion identity used by keyed collection reconciliation.
    pub(crate) entry_stamp: Option<&'a crate::composition::EntryStamp>,
}

/// Runs an installed declaration's finite post-reconciliation reaction pass.
pub(crate) trait ReactionRunner<R, A, C: Carriers<R, A>> {
    /// Consumes matching captures and appends deferred work without polling it.
    fn run(
        &self,
        state: &R,
        scope: ScopeToken,
        context: ReduceContext<'_>,
        frame: &mut reaction::Frame<R, Batch<R, A, C>>,
        work: &mut Vec<Batch<R, A, C>>,
    );
}

/// Participates in actor-owned installation, reaction capture and finite descendant discovery.
pub(crate) trait MountDeclaration<R, A, C: Carriers<R, A> = Shared> {
    /// Whether construction requires the depth-first discovery path rather than the flat fast path.
    fn can_discover(&self) -> bool {
        false
    }

    /// Whether this declaration can reorder its retained descendants.
    fn can_reorder(&self) -> bool {
        false
    }

    /// Returns the declaration's nesting depth, not its scope lifetime identity.
    fn depth(&self) -> usize {
        0
    }

    /// Returns roster placement and optional collection insertion identity.
    fn position(&self) -> MountPosition<'_> {
        MountPosition {
            depth: 0,
            parent: None,
            entry_stamp: None,
        }
    }

    /// Returns the installed token, if this declaration currently has one.
    fn installed_scope(&self) -> Option<ScopeToken> {
        None
    }

    /// Completes reconciliation after discovered descendants have been visited.
    fn finish_reconcile(&self) {}

    /// Identifies a reaction registration within the owned transaction roster.
    fn registration_identity(&self) -> Option<usize> {
        None
    }

    /// Allows the capture pass to skip declarations without reaction callbacks.
    fn has_reactions(&self) -> bool {
        false
    }

    /// Captures previous values while the pre-mutation registration remains live.
    fn capture_reactions(
        &self,
        _: &R,
        _: &ScopeRegistry,
        _: &mut reaction::Frame<R, Batch<R, A, C>>,
    ) {
    }

    /// Borrows the installed runner when initial or paired callbacks can be relevant.
    fn reaction_entry(&self) -> Option<(ScopeToken, &dyn ReactionRunner<R, A, C>)> {
        None
    }

    /// Associates a consuming attachment with exactly one actor.
    fn bind(
        &self,
        ingress: &Arc<super::ingress::CommandIngress<Command<R, A, C>>>,
        lifecycle: &Arc<Lifecycle>,
    );

    /// Reconciles identity and appends newly discovered owners in declaration order.
    ///
    /// Retirement invalidates authority before dropping descendants and the old
    /// Feature. User callbacks run outside declaration guards and can close
    /// admission. Callers do not resume the actor after a callback panic.
    #[expect(
        clippy::too_many_arguments,
        reason = "explicit actor-owned reconciliation borrows expose roster retirement and discovery ordering"
    )]
    fn reconcile(
        &self,
        state: &R,
        scopes: &mut ScopeRegistry,
        transitions: &mut ScopeTransitionBatch,
        mounted: &mut Vec<Box<C::Node>>,
        parent: ScopeToken,
        discovered: &mut Vec<Box<C::Mount>>,
        retire_descendants: &mut nested::Descendants<'_, R, A, C>,
        #[cfg(feature = "diagnostics")] lifecycle: Option<crate::diagnostics::LifecycleCall<'_>>,
    );
}

impl<
    R: Send + 'static,
    A: Send + 'static,
    F: Feature + Send + 'static,
    K: Eq + Hash + Clone + Send + Sync + 'static,
    M: Send + 'static,
    P: BorrowPath<R, State = F::State> + Send + Sync + 'static,
> ReactionRunner<R, A, Shared> for DeclarationOwner<R, A, F, K, M, P>
where
    F::State: Send + 'static,
    F::Action: Send + 'static,
{
    fn run(
        &self,
        state: &R,
        scope: ScopeToken,
        mut context: ReduceContext<'_>,
        frame: &mut reaction::Frame<R, Batch<R, A, Shared>>,
        work: &mut Vec<Batch<R, A, Shared>>,
    ) {
        with_payload(&self.0.payload, |value| {
            let fresh = std::mem::replace(&mut value.initial_pending, false);
            let mut initial_authority = None;
            let route = |task, authority| Batch::Mounted {
                work: Some(
                    value
                        .current
                        .as_ref()
                        .expect("installed reaction node")
                        .initial_work(
                            value.descriptor.path.clone(),
                            Weak::clone(&self.0.binding.get().expect("bound reaction").ingress),
                            authority,
                            task,
                        ),
                ),
                scope,
            };
            if fresh && let Some(hook) = &mut value.descriptor.initial {
                let child = value
                    .descriptor
                    .path
                    .read_structural(state, &|| context.installation_is_live());
                if !context.installation_is_live() {
                    return;
                }
                if let Some(child) = child
                    && let Some(task) = hook(child, context.reborrow())
                {
                    if !context.installation_is_live() {
                        return;
                    }
                    let authority =
                        initial_authority.get_or_insert_with(|| context.scopes().authority(scope));
                    work.push(route(task, authority.clone()));
                }
            }
            for (ordinal, hook) in value.descriptor.reactions.iter().enumerate() {
                if !context.installation_is_live() {
                    break;
                }
                if fresh {
                    if !hook.runs_initial() {
                        continue;
                    }
                    let child = value
                        .descriptor
                        .path
                        .read_structural(state, &|| context.installation_is_live());
                    if !context.installation_is_live() {
                        break;
                    }
                    if let Some(child) = child
                        && let Some(task) = hook.initial(child, context.reborrow())
                    {
                        if !context.installation_is_live() {
                            break;
                        }
                        let authority = initial_authority
                            .get_or_insert_with(|| context.scopes().authority(scope));
                        work.push(route(task, authority.clone()));
                    }
                } else if let Some(captured) = frame.take(
                    self.registration_identity().expect("child registration"),
                    ordinal,
                    context.scopes(),
                ) {
                    if !context.installation_is_live() {
                        break;
                    }
                    if let Some(batch) = captured(state, context.reborrow()) {
                        work.push(batch);
                    }
                }
            }
        });
    }
}

impl<
    R: Send + 'static,
    A: Send + 'static,
    F: Feature + Send + 'static,
    K: Eq + Hash + Clone + Send + Sync + 'static,
    M: Send + 'static,
    P: BorrowPath<R, State = F::State> + Send + Sync + 'static,
> MountDeclaration<R, A> for DeclarationOwner<R, A, F, K, M, P>
where
    F::State: Send + 'static,
    F::Action: Send + 'static,
{
    fn installed_scope(&self) -> Option<ScopeToken> {
        self.0
            .payload
            .lock()
            .as_ref()
            .and_then(|value| value.installed.as_ref().map(|(_, token)| *token))
    }

    fn registration_identity(&self) -> Option<usize> {
        Some(Arc::as_ptr(&self.0).cast::<()>() as usize)
    }

    fn has_reactions(&self) -> bool {
        self.0
            .payload
            .lock()
            .as_ref()
            .is_some_and(|value| !value.descriptor.reactions.is_empty())
    }

    fn capture_reactions(
        &self,
        state: &R,
        scopes: &ScopeRegistry,
        frame: &mut reaction::Frame<R, Batch<R, A, Shared>>,
    ) {
        if !self.has_reactions() {
            return;
        }
        with_payload(&self.0.payload, |value| {
            let Some((_, scope)) = &value.installed else {
                return;
            };
            let scope = *scope;
            let authority = scopes.authority(scope);
            for (ordinal, hook) in value.descriptor.reactions.iter().enumerate() {
                if !scopes.installation_is_live() {
                    break;
                }
                let child = value
                    .descriptor
                    .path
                    .read_structural(state, &|| scopes.installation_is_live());
                if !scopes.installation_is_live() {
                    break;
                }
                let Some(child) = child else {
                    continue;
                };
                if let Some(captured) = hook.capture(child, &|| scopes.installation_is_live()) {
                    let path = value.descriptor.path.clone();
                    let node = value
                        .current
                        .as_ref()
                        .expect("installed reaction node")
                        .clone();
                    let ingress =
                        Weak::clone(&self.0.binding.get().expect("bound reaction").ingress);
                    let captured_authority = authority.clone();
                    frame.insert(
                        self.registration_identity().expect("child registration"),
                        ordinal,
                        authority.clone(),
                        Box::new(move |state: &R, context: ReduceContext<'_>| {
                            let child =
                                path.read_structural(state, &|| context.installation_is_live());
                            if !context.installation_is_live() {
                                return None;
                            }
                            let task = captured(child?, context)?;
                            Some(Batch::Mounted {
                                work: Some(node.initial_work(
                                    path,
                                    ingress,
                                    captured_authority,
                                    task,
                                )),
                                scope,
                            })
                        }),
                    );
                }
            }
        });
    }

    fn reaction_entry(&self) -> Option<(ScopeToken, &dyn ReactionRunner<R, A, Shared>)> {
        let payload = self.0.payload.lock();
        let value = payload.as_ref().expect("reaction roster owns payload");
        if value.initial_pending || !value.descriptor.reactions.is_empty() {
            value
                .installed
                .as_ref()
                .map(|(_, scope)| (*scope, self as &dyn ReactionRunner<R, A, Shared>))
        } else {
            None
        }
    }

    fn bind(&self, ingress: &Arc<Ingress<R, A>>, lifecycle: &Arc<Lifecycle>) {
        let result = self.0.binding.set(Binding {
            ingress: Arc::downgrade(ingress),
            lifecycle: Arc::downgrade(lifecycle),
        });
        assert!(result.is_ok(), "consuming attachment binds one actor");
    }

    fn reconcile(
        &self,
        state: &R,
        scopes: &mut ScopeRegistry,
        transitions: &mut ScopeTransitionBatch,
        mounted: &mut Vec<Box<dyn super::mounted::InstalledNode + Send>>,
        parent: ScopeToken,
        _: &mut Vec<Box<<Shared as Carriers<R, A>>::Mount>>,
        retire_descendants: &mut nested::Descendants<'_, R, A, Shared>,
        #[cfg(feature = "diagnostics")] mut lifecycle: Option<
            crate::diagnostics::LifecycleCall<'_>,
        >,
    ) {
        with_payload(&self.0.payload, |declaration| {
            let desired =
                (declaration.descriptor.identity)(state, &|| scopes.installation_is_live());
            if !scopes.installation_is_live() {
                return;
            }
            match reconcile_identity(
                &mut declaration.installed,
                desired,
                scopes,
                transitions,
                &self.0.node,
                parent,
                || {
                    retire_descendants.retire();
                    declaration
                        .current
                        .take()
                        .expect("installed declaration owns weak node")
                        .retire_feature();
                },
                #[cfg(feature = "diagnostics")]
                lifecycle
                    .as_mut()
                    .map(crate::diagnostics::LifecycleCall::reborrow),
            ) {
                Reconciled::Unchanged => {}
                Reconciled::Refreshed {
                    revision,
                    #[cfg(feature = "diagnostics")]
                    previous,
                } => {
                    declaration
                        .current
                        .as_ref()
                        .expect("installed declaration owns weak node")
                        .refresh_configuration(revision);
                    #[cfg(feature = "diagnostics")]
                    if let Some(call) = &mut lifecycle {
                        call.emit(crate::diagnostics::LifecycleKind::ConfigurationChanged {
                            lifetime: declaration
                                .installed
                                .as_ref()
                                .expect("refreshed installation")
                                .1
                                .diagnostic_id(),
                            previous,
                            current: revision,
                        });
                    }
                }
                Reconciled::Removed => {
                    declaration.current = None;
                    declaration.initial_pending = false;
                }
                Reconciled::Installed(token) => {
                    // K's Clone/Hash/Eq and retirement destruction are user code;
                    // reconcile_identity may therefore close the root as well.
                    if !scopes.installation_is_live() {
                        return;
                    }
                    let child = declaration
                        .descriptor
                        .path
                        .read_structural(state, &|| scopes.installation_is_live());
                    if !scopes.installation_is_live() {
                        return;
                    }
                    let Some(child) = child else {
                        // Selection can disappear during identity callbacks. Revoke
                        // the provisional lifetime before releasing its identity.
                        let abandoned = declaration.installed.take();
                        declaration.initial_pending = false;
                        transitions.extend(scopes.leave(token));
                        drop(abandoned);
                        return;
                    };
                    let feature = (declaration.descriptor.make)(child);
                    let node = SharedNode::new(feature, token, 0);
                    declaration.current = Some(node.downgrade());
                    declaration.initial_pending = declaration.descriptor.initial.is_some()
                        || !declaration.descriptor.reactions.is_empty();
                    mounted.push(Box::new(node));
                    #[cfg(feature = "diagnostics")]
                    if let Some(call) = &mut lifecycle {
                        call.emit(crate::diagnostics::LifecycleKind::Installed {
                            lifetime: token.diagnostic_id(),
                            parent: Some(parent.diagnostic_id()),
                            configuration: Some(scopes.configuration(token)),
                        });
                    }
                }
            }
        });
    }
}

/// Creates an unbound owner and a weak site without running descriptor callbacks.
#[expect(
    clippy::type_complexity,
    reason = "Returns the declaration owner and its typed weak acquisition sites."
)]
pub(super) fn attach<R, A, F, K>(
    descriptor: OptionalMount<R, F, K>,
) -> (
    Box<dyn MountDeclaration<R, A> + Send>,
    MountSite<R, F::State, F::Action, A>,
)
where
    R: Send + 'static,
    A: Send + 'static,
    F: Feature + Send + 'static,
    F::State: Send + 'static,
    F::Action: Send + 'static,
    K: Eq + Hash + Clone + Send + Sync + 'static,
{
    let (owner, site) = attach_descriptor(descriptor.into());
    (Box::new(owner), site)
}

/// Creates a removable optional route while retaining access to its owning slot.
#[expect(
    clippy::type_complexity,
    reason = "Returns the declaration owner and its typed weak acquisition sites."
)]
pub(in crate::store) fn attach_owned_optional<R, A, F, K>(
    read: for<'s> fn(&'s R) -> &'s Option<F::State>,
    write: for<'s> fn(&'s mut R) -> &'s mut Option<F::State>,
    mut identity: impl FnMut(&R) -> Option<MountIdentity<K>> + Send + 'static,
    make: impl FnMut(&F::State) -> F + Send + 'static,
) -> (
    Box<dyn MountDeclaration<R, A> + Send>,
    MountSite<R, F::State, F::Action, A>,
)
where
    R: Send + 'static,
    A: Send + 'static,
    F: Feature + Send + 'static,
    F::State: Send + 'static,
    F::Action: Send + 'static,
    K: Eq + Hash + Clone + Send + Sync + 'static,
{
    let (owner, site) = attach_descriptor(Descriptor {
        path: super::mounted::path::OwnedOptionalPath { read, write },
        identity: Box::new(move |root, _| identity(root)),
        make: Box::new(make),
        initial: None,
        reactions: Vec::new(),
    });
    (Box::new(owner), site)
}

/// Stores one routed descriptor and erases only the weak acquisition boundary.
#[expect(
    clippy::type_complexity,
    reason = "Returns the declaration owner and its typed weak acquisition sites."
)]
fn attach_descriptor<R, A, F, K, P>(
    descriptor: Descriptor<R, F, K, P>,
) -> (
    DeclarationOwner<R, A, F, K, (), P>,
    MountSite<R, F::State, F::Action, A>,
)
where
    R: Send + 'static,
    A: Send + 'static,
    F: Feature + Send + 'static,
    F::State: Send + 'static,
    F::Action: Send + 'static,
    K: Send + 'static,
    P: BorrowPath<R, State = F::State> + Send + Sync + 'static,
{
    let cell = Arc::new(DeclarationCell {
        node: ScopeRegistry::node(&mut ScopeSlot::new()),
        binding: OnceLock::new(),
        payload: Mutex::new(Some(Declaration {
            descriptor,
            current: None,
            installed: None,
            initial_pending: false,
            mapping: (),
        })),
    });
    let erased: Arc<dyn Acquisition<R, F::State, F::Action, A>> =
        Arc::<DeclarationCell<_, _, _, _, _, _>>::clone(&cell);
    let site = MountSite {
        declaration: Arc::downgrade(&erased),
    };
    drop(erased);
    (DeclarationOwner(cell), site)
}

/// Adds command conversion to the same declaration header and payload lifetime.
#[expect(
    clippy::type_complexity,
    reason = "Returns the declaration owner and its typed weak acquisition sites."
)]
pub(super) fn attach_command<R, A, F, K, C>(
    descriptor: OptionalMount<R, F, K>,
    mapper: impl FnMut(C) -> F::Action + Send + 'static,
    class: super::mounted::InputClass,
) -> (
    Box<dyn MountDeclaration<R, A> + Send>,
    MountSite<R, F::State, F::Action, A>,
    CommandSite<R, C>,
)
where
    R: Send + 'static,
    A: Send + 'static,
    F: Feature + Send + 'static,
    F::State: Send + 'static,
    F::Action: Send + 'static,
    K: Eq + Hash + Clone + Send + Sync + 'static,
    C: Send + 'static,
{
    let mapping = CommandMapping {
        map: Box::new(mapper),
        class,
        #[cfg(feature = "test-util")]
        pending: None,
    };
    let cell = Arc::new(DeclarationCell {
        node: ScopeRegistry::node(&mut ScopeSlot::new()),
        binding: OnceLock::new(),
        payload: Mutex::new(Some(Declaration {
            descriptor: descriptor.into(),
            current: None,
            installed: None,
            initial_pending: false,
            mapping,
        })),
    });
    let ordinary: Arc<dyn Acquisition<R, F::State, F::Action, A>> =
        Arc::<DeclarationCell<_, _, _, _, _, _>>::clone(&cell);
    let site = MountSite {
        declaration: Arc::downgrade(&ordinary),
    };
    let commanded: Arc<dyn CommandAcquisition<R, C>> =
        Arc::<DeclarationCell<_, _, _, _, _, _>>::clone(&cell);
    let command_site = CommandSite::from_declaration(Arc::downgrade(&commanded));
    drop((ordinary, commanded));
    (Box::new(DeclarationOwner(cell)), site, command_site)
}

/// Reports closure before an acquisition enters child code.
fn closed() -> TaskAccessError {
    TaskAccessError::new(TaskAccessReason::StoreClosed, TaskProgress::NotStarted)
}

/// Checks root identity before admitting a request to this declaration's actor.
fn validate<R, A, C: Carriers<R, A>>(
    binding: &Binding<R, A, C>,
    root: &Arc<()>,
) -> Result<(), TaskAccessError> {
    let lifecycle = binding.lifecycle.upgrade().ok_or_else(closed)?;
    if !Arc::ptr_eq(root, &lifecycle.root_identity()) {
        return Err(TaskAccessError::new(
            TaskAccessReason::ScopeEnded,
            TaskProgress::NotStarted,
        ));
    }
    lifecycle.access()
}

/// Describes the structural work required after comparing mount identities.
enum Reconciled {
    /// Keeps the existing incarnation, or leaves an absent declaration absent.
    Unchanged,
    /// Removes the current node, including closure during retirement.
    Removed,
    /// Constructs a Feature for the newly installed token.
    Installed(
        /// Newly allocated scope token; the caller still has to construct its node.
        ScopeToken,
    ),
    /// Retains the Feature while expiring its previous configuration authority.
    Refreshed {
        /// New registry revision to copy into the installed node.
        revision: u64,
        /// Previous revision used only by lifecycle diagnostics.
        #[cfg(feature = "diagnostics")]
        previous: u64,
    },
}

/// Reconciles one identity and retires its old payload before replacement.
///
/// Shared and Local declarations share this algorithm. Their callers construct
/// concrete nodes and restore their differently guarded payloads. Retirement
/// first invalidates the scope subtree, then drops descendants and the Feature.
/// The old selected identity drops before successor construction.
///
/// Key comparison, cloning, hashing and destruction can run user code. Callers
/// check root admission before constructing a Feature from an installed result.
#[cfg_attr(
    feature = "diagnostics",
    expect(
        clippy::too_many_arguments,
        reason = "explicit owned and borrowed placement and retirement inputs preserve reconciliation ordering"
    )
)]
fn reconcile_identity<K: Eq + Hash + Clone + Send + Sync + 'static>(
    installed: &mut Option<(MountIdentity<K>, ScopeToken)>,
    desired: Option<MountIdentity<K>>,
    scopes: &mut ScopeRegistry,
    transitions: &mut ScopeTransitionBatch,
    node: &ScopeNodeId,
    parent: ScopeToken,
    retire: impl FnOnce(),
    #[cfg(feature = "diagnostics")] mut lifecycle: Option<crate::diagnostics::LifecycleCall<'_>>,
) -> Reconciled {
    if let (Some((old, token)), Some(new)) = (installed.as_mut(), desired.as_ref())
        && scopes.is_current(*token)
        && old.key == new.key
        && old.replacement == new.replacement
    {
        if old.configuration != new.configuration {
            old.configuration = new.configuration;
            #[cfg(feature = "diagnostics")]
            let previous = scopes.configuration(*token);
            return Reconciled::Refreshed {
                revision: scopes.refresh_configuration(*token),
                #[cfg(feature = "diagnostics")]
                previous,
            };
        }
        return Reconciled::Unchanged;
    }
    let removed = if let Some((old, token)) = installed.take() {
        let retired = scopes.leave(token);
        #[cfg(feature = "diagnostics")]
        {
            transitions.extend(retired.iter().copied());
            if let Some(call) = &mut lifecycle {
                for token in retired {
                    call.emit(crate::diagnostics::LifecycleKind::Retired {
                        lifetime: token.diagnostic_id(),
                    });
                }
            } else {
                drop(retired);
            }
        }
        #[cfg(not(feature = "diagnostics"))]
        transitions.extend(retired);
        // Even an already-retired token still owns payloads until this barrier.
        retire();
        drop(old);
        true
    } else {
        false
    };
    // Feature, callback and key destructors may synchronously close the root.
    if !scopes.installation_is_live() {
        return Reconciled::Removed;
    }
    if let Some(identity) = desired {
        let token = scopes.mount(
            parent,
            node.clone(),
            ScopeIdentity::new((identity.key.clone(), identity.replacement)),
        );
        *installed = Some((identity, token));
        Reconciled::Installed(token)
    } else if removed {
        Reconciled::Removed
    } else {
        Reconciled::Unchanged
    }
}

impl<
    R: Send + 'static,
    A: Send + 'static,
    F: Feature + Send + 'static,
    K: Send + 'static,
    M: Send + 'static,
    P: BorrowPath<R, State = F::State> + Send + Sync + 'static,
> super::output::OutputRegistration for DeclarationCell<R, A, F, K, M, P>
where
    F::State: Send + 'static,
    F::Action: Send + 'static,
{
    fn installed(&self) -> Option<(Arc<()>, ScopeToken)> {
        let lifecycle = self.binding.get()?.lifecycle.upgrade()?;
        let token = self.payload.lock().as_ref()?.installed.as_ref()?.1;
        Some((lifecycle.root_identity(), token))
    }
}

impl<
    R: Send + 'static,
    A: Send + 'static,
    F: Feature + Send + 'static,
    K: Send + 'static,
    M: Send + 'static,
    P: BorrowPath<R, State = F::State> + Send + Sync + 'static,
> commands::Registration<R, A, F::Action> for DeclarationCell<R, A, F, K, M, P>
where
    F::State: Send + 'static,
    F::Action: Send + 'static,
{
    fn freeze_dispatch(
        &self,
        scopes: &ScopeRegistry,
    ) -> Arc<dyn commands::Dispatch<R, A, F::Action>> {
        let binding = self
            .binding
            .get()
            .expect("installed recipe parent is bound to its actor");
        let ingress = Weak::clone(&binding.ingress);
        let (path, current) = {
            let payload = self.payload.lock();
            let declaration = payload
                .as_ref()
                .expect("live recipe command retains its parent payload");
            (
                declaration.descriptor.path.clone(),
                declaration
                    .current
                    .clone()
                    .expect("live recipe command has an installed parent"),
            )
        };
        current.command_dispatch(path, ingress, scopes)
    }
}

impl<
    R: Send + 'static,
    A: Send + 'static,
    F: Feature + Send + 'static,
    K: Send + 'static,
    M: Send + 'static,
    P: BorrowPath<R, State = F::State> + Send + Sync + 'static,
> super::output::HandlerRegistration<R, F::State, F::Action, A>
    for DeclarationCell<R, A, F, K, M, P>
where
    F::State: Send + 'static,
    F::Action: Send + 'static,
{
    fn freeze_handler_route(
        &self,
        scopes: &ScopeRegistry,
    ) -> Option<Box<dyn super::output::HandlerRoute<R, A, F::State, F::Action> + Send>> {
        let ingress = Weak::clone(&self.binding.get()?.ingress);
        let (path, current) = {
            let payload = self.payload.lock();
            let declaration = payload.as_ref()?;
            (
                declaration.descriptor.path.clone(),
                declaration.current.clone()?,
            )
        };
        current.handler_route(path, ingress, scopes)
    }
}

impl<
    R: Send + 'static,
    A: Send + 'static,
    F: Feature + Send + 'static,
    K: Send + 'static,
    M: Send + 'static,
    P: BorrowPath<R, State = F::State> + Send + Sync + 'static,
> Acquisition<R, F::State, F::Action, A> for DeclarationCell<R, A, F, K, M, P>
where
    F::State: Send + 'static,
    F::Action: Send + 'static,
{
    fn root_identity(&self) -> Option<Arc<()>> {
        Some(self.binding.get()?.lifecycle.upgrade()?.root_identity())
    }

    fn read_committed<'a>(&self, root: &'a R, live: &dyn Fn() -> bool) -> Option<&'a F::State> {
        let path = {
            let payload = self.payload.lock();
            payload
                .as_ref()
                .expect("fold roster owns the committed declaration")
                .descriptor
                .path
                .clone()
        };
        if !live() {
            return None;
        }
        path.read_structural(root, live)
    }

    #[cfg(feature = "test-util")]
    fn validate(&self, root: &Arc<()>) -> Result<(), TaskAccessError> {
        validate(self.binding.get().ok_or_else(closed)?, root)
    }

    fn acquire(
        self: Arc<Self>,
        root: Arc<()>,
        #[cfg(feature = "test-util")] gate: Option<crate::testing::TransactionGateCore<R, A>>,
    ) -> AcquisitionFuture<F::State, F::Action> {
        let weak = Arc::downgrade(&self);
        let binding = self.binding.get().cloned();
        drop(self);
        Box::pin(async move {
            let binding = binding.ok_or_else(closed)?;
            validate(&binding, &root)?;
            #[cfg(all(test, feature = "test-util"))]
            ordinary_test_items! {
                super::mounted::mounted_admission_test_hook::before_ingress();
            }
            let ingress = binding.ingress.upgrade().ok_or_else(closed)?;
            let (reply, receive) = ReplyGuard::channel();
            let call = TypedAcquire { cell: weak, reply };
            #[cfg(feature = "test-util")]
            if let Some(gate) = gate
                && let Some(boundary) = gate.boundary()
            {
                gate.acquire(boundary, Box::new(call)).await;
                return receive
                    .await
                    .expect("owned acquisition reply publishes terminal result");
            }
            if let Err(error) = ingress
                .send_map(call, |call| Command::<R, A>::Acquire(Box::new(call)))
                .await
            {
                error.0.reply.complete(Err(closed()));
            }
            receive
                .await
                .expect("owned acquisition reply publishes terminal result")
        })
    }
}

/// Owns one ordered metadata request and its terminal reply.
pub(crate) trait AcquireCall {
    /// Captures current authority at the actor's ordered execution point.
    fn execute(self: Box<Self>, scopes: &ScopeRegistry, lifecycle: &Lifecycle);

    /// Completes a request that did not enter acquisition.
    fn reject(self: Box<Self>, error: TaskAccessError);

    /// Completes actor teardown without requiring a live declaration.
    fn close(self: Box<Self>) {
        self.reject(closed());
    }
}

/// Acquires a direct or nested mount without extending declaration payload lifetime.
struct TypedAcquire<R, A, F: Feature, K, M = (), P = OptionalPath<R, <F as Feature>::State>> {
    /// Weak header, rechecked after mailbox admission because nested owners can retire.
    cell: Weak<DeclarationCell<R, A, F, K, M, P>>,
    /// Publishes closure on drop if execution cannot publish a normal result.
    reply: ReplyGuard<Option<TaskContext<F::State, F::Action>>>,
}

impl<
    R: Send + 'static,
    A: Send + 'static,
    F: Feature + Send + 'static,
    K: Send + 'static,
    M: Send + 'static,
    P: BorrowPath<R, State = F::State> + Send + Sync + 'static,
> AcquireCall for TypedAcquire<R, A, F, K, M, P>
where
    F::State: Send + 'static,
    F::Action: Send + 'static,
{
    fn execute(self: Box<Self>, scopes: &ScopeRegistry, lifecycle: &Lifecycle) {
        let Self { cell, reply } = *self;
        let result = (|| {
            lifecycle.access()?;
            // A nested declaration can retire after admission but before this
            // ordered metadata read. Never retarget it by stable key.
            let cell = cell
                .upgrade()
                .ok_or_else(|| super::mounted::scope_ended(TaskProgress::NotStarted))?;
            let binding = cell
                .binding
                .get()
                .expect("admitted acquisition has bound ingress");
            validate(binding, &lifecycle.root_identity())?;
            let (path, current) = {
                let payload = cell.payload.lock();
                let declaration = payload
                    .as_ref()
                    .ok_or_else(|| super::mounted::scope_ended(TaskProgress::NotStarted))?;
                (
                    declaration.descriptor.path.clone(),
                    declaration.current.clone(),
                )
            };
            let ingress = Weak::clone(&binding.ingress);
            drop(cell);
            Ok(current.map(|node| node.context(path, ingress, scopes)))
        })();
        reply.complete(result);
    }

    fn reject(self: Box<Self>, error: TaskAccessError) {
        self.reply.complete(Err(error));
    }
}

impl<
    R: Send + 'static,
    A: Send + 'static,
    F: Feature + Send + 'static,
    K: Send + 'static,
    C: Send + 'static,
    P: BorrowPath<R, State = F::State> + Send + Sync + 'static,
> CommandMapper<C, F::Action> for DeclarationCell<R, A, F, K, CommandMapping<C, F::Action>, P>
where
    F::State: Send + 'static,
    F::Action: Send + 'static,
{
    fn map(&self, command: C) -> F::Action {
        with_payload(&self.payload, |declaration| {
            (declaration.mapping.map)(command)
        })
    }

    #[cfg(feature = "test-util")]
    fn publish(&self, slot: &Arc<super::mounted::SendSlot<C>>) {
        // Publication runs on the serial actor before it can retire the payload.
        self.payload
            .lock()
            .as_mut()
            .expect("actor owns command declaration")
            .mapping
            .pending = Some((Arc::as_ptr(slot) as usize, Arc::downgrade(slot)));
    }

    #[cfg(feature = "test-util")]
    fn inspect(&self, request: usize) -> Option<super::mounted::SendInspection<C>> {
        let slot = self
            .payload
            .lock()
            .as_ref()?
            .mapping
            .pending
            .as_ref()
            .filter(|(id, _)| *id == request)
            .and_then(|(_, slot)| slot.upgrade());
        slot.map(super::command_port::input::Request::inspect)
    }

    #[cfg(feature = "test-util")]
    fn clear(&self, request: usize) {
        if let Some(declaration) = self.payload.lock().as_mut()
            && declaration
                .mapping
                .pending
                .as_ref()
                .is_some_and(|(id, _)| *id == request)
        {
            declaration.mapping.pending.take();
        }
    }
}

impl<
    R: Send + 'static,
    A: Send + 'static,
    F: Feature + Send + 'static,
    K: Send + 'static,
    C: Send + 'static,
    P: BorrowPath<R, State = F::State> + Send + Sync + 'static,
> CommandAcquisition<R, C> for DeclarationCell<R, A, F, K, CommandMapping<C, F::Action>, P>
where
    F::State: Send + 'static,
    F::Action: Send + 'static,
{
    #[cfg(feature = "test-util")]
    fn validate(&self, root: &Arc<()>) -> Result<(), TaskAccessError> {
        validate(self.binding.get().ok_or_else(closed)?, root)
    }

    fn acquire(
        self: Arc<Self>,
        root: Arc<()>,
        #[cfg(feature = "test-util")] query: Option<super::command_port::AcquireSubmit>,
    ) -> CommandAcquisitionFuture<C> {
        let weak = Arc::downgrade(&self);
        let binding = self.binding.get().cloned();
        drop(self);
        let prepare = move || {
            let binding = binding.ok_or_else(closed)?;
            validate(&binding, &root)?;
            #[cfg(all(test, feature = "test-util"))]
            ordinary_test_items! {
                crate::store::mounted::mounted_admission_test_hook::before_ingress();
            }
            let ingress = binding.ingress.upgrade().ok_or_else(closed)?;
            let (reply, receive) = ReplyGuard::channel();
            let call = TypedCommandAcquire { cell: weak, reply };
            Ok::<_, TaskAccessError>((ingress, call, receive))
        };
        #[cfg(feature = "test-util")]
        if let Some(query) = query {
            return Box::pin(async move {
                let (ingress, call, receive) = prepare()?;
                // With no frontier, submit through the ingress already held
                // by this acquisition; the query need not upgrade it again.
                if let Some(call) = query(Box::new(call)).await
                    && let Err(error) = ingress.send_map(call, Command::Acquire).await
                {
                    error.0.close();
                }
                receive
                    .await
                    .expect("owned command acquisition publishes terminal result")
            });
        }
        Box::pin(async move {
            let (ingress, call, receive) = prepare()?;
            if let Err(error) = ingress
                .send_map(call, |call| Command::<R, A>::Acquire(Box::new(call)))
                .await
            {
                error.0.reply.complete(Err(closed()));
            }
            receive
                .await
                .expect("owned command acquisition publishes terminal result")
        })
    }
}

/// Acquires a root command declaration that stays in the roster until teardown.
///
/// Nested recipe commands instead use `commands::Acquire`, because their
/// declaration can retire independently while the actor remains live.
struct TypedCommandAcquire<R, A, F: Feature, K, C, P> {
    /// Weak reference to the original root declaration, never a key-based replacement.
    #[expect(
        clippy::type_complexity,
        reason = "The weak acquisition retains the concrete mapper and path types."
    )]
    cell: Weak<DeclarationCell<R, A, F, K, CommandMapping<C, F::Action>, P>>,
    /// Returns the port or publishes closure if the request is dropped.
    reply: ReplyGuard<Option<CommandPort<C>>>,
}

impl<
    R: Send + 'static,
    A: Send + 'static,
    F: Feature + Send + 'static,
    K: Send + 'static,
    C: Send + 'static,
    P: BorrowPath<R, State = F::State> + Send + Sync + 'static,
> AcquireCall for TypedCommandAcquire<R, A, F, K, C, P>
where
    F::State: Send + 'static,
    F::Action: Send + 'static,
{
    fn execute(self: Box<Self>, scopes: &ScopeRegistry, lifecycle: &Lifecycle) {
        let Self { cell, reply } = *self;
        let result = (|| {
            lifecycle.access()?;
            let cell = cell
                .upgrade()
                .expect("running actor owns root command declaration");
            let ingress = Weak::clone(
                &cell
                    .binding
                    .get()
                    .expect("admitted acquisition is bound")
                    .ingress,
            );
            let (path, current, class) = {
                let payload = cell.payload.lock();
                let declaration = payload
                    .as_ref()
                    .expect("ordered root command acquisition has a restored payload");
                (
                    declaration.descriptor.path.clone(),
                    declaration.current.clone(),
                    declaration.mapping.class,
                )
            };
            // Coerce the existing declaration allocation; no adapter owns the mapper.
            let mapper: Arc<dyn CommandMapper<C, F::Action>> = cell;
            let mapper = Arc::downgrade(&mapper);
            Ok(current.map(|node| node.command_port(path, ingress, scopes, mapper, class)))
        })();
        reply.complete(result);
    }

    fn reject(self: Box<Self>, error: TaskAccessError) {
        self.reply.complete(Err(error));
    }
}
