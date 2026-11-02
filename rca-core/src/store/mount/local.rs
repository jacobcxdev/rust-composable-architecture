//! Owner-thread declaration and acquisition leaves for the shared mount algorithm.
#[cfg(all(test, feature = "allocation-measurement"))]
pub(super) mod allocation_tests;
pub(in crate::store) mod children;
pub(in crate::store) mod commands;
#[cfg(all(test, feature = "test-util"))]
pub(super) mod fold_gate;
use super::*;
use crate::store::local::{self, Local};
use crate::store::mounted::{LocalNode, LocalWeakNode};
use std::cell::RefCell;
use std::rc::{Rc, Weak as LocalWeak};

#[cfg(test)]
ordinary_test_items! { mod lease_tests; }

/// Declares an owner-local optional child with its own action domain.
///
/// Attachment does not select State or construct the Feature. Installation uses
/// the common structural transaction. Configuration changes preserve the Feature
/// but require fresh acquisition; replacement changes create a new incarnation.
/// Missing child selection defers construction as in [`crate::OptionalMount`].
pub struct LocalOptionalMount<R, F: local::Feature, K> {
    /// Selects child state only while the actor holds the root borrow.
    path: OptionalPath<R, F::State>,
    /// Supplies lifetime and configuration markers during reconciliation.
    #[expect(
        clippy::type_complexity,
        reason = "Retains the typed declaration callback without adding an alias."
    )]
    identity: Box<dyn FnMut(&R, &dyn Fn() -> bool) -> Option<MountIdentity<K>>>,
    /// Constructs one Feature for each installed incarnation.
    #[expect(
        clippy::type_complexity,
        reason = "Retains the typed declaration callback without adding an alias."
    )]
    make: Box<dyn FnMut(&F::State) -> F>,
    /// Replaced by later initial declarations; runs after fresh installation.
    initial: Option<InitialHook<F::State, F::Action>>,
    /// Keeps reaction declaration order and independent initial policies.
    reactions: Vec<reaction::LocalHook<F::State, F::Action>>,
}

/// Keeps the same declaration payload for direct and composed child paths.
struct Descriptor<R, F: local::Feature, K, P> {
    /// Owns route metadata, never a borrowed child reference.
    path: P,
    /// Checks structural identity with liveness between composed selections.
    #[expect(
        clippy::type_complexity,
        reason = "Retains the typed declaration callback without adding an alias."
    )]
    identity: Box<dyn FnMut(&R, &dyn Fn() -> bool) -> Option<MountIdentity<K>>>,
    /// Runs only when reconciliation installs a new child incarnation.
    #[expect(
        clippy::type_complexity,
        reason = "Retains the typed declaration callback without adding an alias."
    )]
    make: Box<dyn FnMut(&F::State) -> F>,
    /// Produces deferred initial work without polling it during installation.
    initial: Option<InitialHook<F::State, F::Action>>,
    /// Owns transaction hooks in declaration order.
    reactions: Vec<reaction::LocalHook<F::State, F::Action>>,
}

/// Selects an owned initial value and produces owner-local deferred work.
type InitialHook<S, A> = Box<dyn for<'a> FnMut(&S, ReduceContext<'a>) -> Option<local::Task<S, A>>>;

/// Checks admission between user selection and entry to the task factory.
fn initial_hook<S: 'static, A: 'static, V: 'static>(
    mut select: impl FnMut(&S) -> V + 'static,
    mut factory: impl for<'a> FnMut(ReactionChange<V>, ReactionContext<'a, S, A>) -> local::Task<S, A>
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

/// Declares owner-local root lifetime and transaction reactions.
///
/// Construction uses the same root installation and transaction algorithm as
/// Shared, while callbacks, State and returned tasks may own non-Send values.
pub struct RootMount<F: local::Feature>(
    /// Transfers root construction and hook ownership together into the builder.
    EntryMount<F>,
);

impl<F: local::Feature> RootMount<F> {
    /// Owns the root Feature without installing it.
    pub fn new(feature: F) -> Self {
        Self(EntryMount::new(feature))
    }

    /// Refreshes captured root and descendant authority on configuration changes.
    #[must_use]
    pub fn configuration(self, select: impl FnMut(&F::State) -> u64 + 'static) -> Self {
        Self(self.0.configuration(select))
    }

    /// Replaces the singleton initial task factory.
    #[must_use]
    pub fn on_initial<V: 'static>(
        self,
        select: impl FnMut(&F::State) -> V + 'static,
        factory: impl for<'a> FnMut(
            ReactionChange<V>,
            ReactionContext<'a, F::State, F::Action>,
        ) -> local::Task<F::State, F::Action>
        + 'static,
    ) -> Self
    where
        F::State: 'static,
        F::Action: 'static,
    {
        Self(self.0.on_initial(select, factory))
    }

    /// Appends a transaction reaction with its own explicit initial policy.
    ///
    /// Uses the same snapshot, ordering and lifetime rules as
    /// [`crate::RootMount::on_change`], without Shared's callback or task `Send`
    /// requirements. Select independent values even when State owns mutable `Rc`s.
    ///
    /// # Examples
    ///
    /// ```
    /// use rca_core::InitialPolicy;
    /// use rca_core::local::{Feature, FeatureContext, RootMount, Task};
    /// use std::{cell::Cell, rc::Rc};
    /// # struct Counter;
    /// # impl Feature for Counter {
    /// #     type State = Rc<Cell<i32>>;
    /// #     type Action = i32;
    /// #     fn update(&mut self, state: &mut Self::State, action: &i32, _: &mut FeatureContext<'_, Self::State, i32>) { state.set(state.get() + action); }
    /// # }
    /// let root = RootMount::new(Counter).on_change(
    ///     |state| Rc::new(state.get()),
    ///     InitialPolicy::Skip,
    ///     |change, _| Task::run(move |_| async move { drop(change); }),
    /// );
    /// # drop(root);
    /// ```
    #[must_use]
    pub fn on_change<V: PartialEq + 'static>(
        self,
        select: impl FnMut(&F::State) -> V + 'static,
        initial: InitialPolicy,
        factory: impl for<'a> FnMut(
            ReactionChange<V>,
            ReactionContext<'a, F::State, F::Action>,
        ) -> local::Task<F::State, F::Action>
        + 'static,
    ) -> Self
    where
        F::State: 'static,
        F::Action: 'static,
    {
        Self(self.0.on_change(select, initial, factory))
    }

    /// Transfers ownership to the ordinary owner-local Feature builder.
    pub fn builder(
        self,
        state: F::State,
        runtime: crate::runtime::local::Runtime,
    ) -> local::StoreBuilder<F::State, F::Action, F>
    where
        F: 'static,
        F::State: 'static,
        F::Action: 'static,
    {
        let EntryMount {
            feature,
            initial,
            reactions,
            configuration,
        } = self.0;
        local::Store::builder(state, feature, runtime).with_mount(Box::new(
            super::RootDeclaration::<F::State, F::Action, Local> {
                payload: Mutex::new(Some(super::RootInitial {
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

impl<F: local::Feature> fmt::Debug for RootMount<F> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RootMount").finish_non_exhaustive()
    }
}

/// Owns one local keyed entry's construction data, without granting authority.
pub struct EntryMount<F: local::Feature> {
    /// Feature transferred to an installed node or the root reducer.
    feature: F,
    /// Singleton initial hook, replaced by a later declaration.
    initial: Option<InitialHook<F::State, F::Action>>,
    /// Ordered registrations with independent initial policies.
    reactions: Vec<reaction::LocalHook<F::State, F::Action>>,
    /// Changes captured authority without replacing the installed Feature.
    #[expect(
        clippy::type_complexity,
        reason = "Retains the typed declaration callback without adding an alias."
    )]
    configuration: Option<Box<dyn FnMut(&F::State) -> u64>>,
}

impl<F: local::Feature> EntryMount<F> {
    /// Owns one freshly constructed owner-local entry Feature.
    pub fn new(feature: F) -> Self {
        Self {
            feature,
            initial: None,
            reactions: Vec::new(),
            configuration: None,
        }
    }

    /// Appends a transaction reaction with an explicit per-registration initial policy.
    ///
    /// See [`RootMount::on_change`] for snapshot, ordering and lifetime rules.
    #[must_use]
    pub fn on_change<V: PartialEq + 'static>(
        mut self,
        select: impl FnMut(&F::State) -> V + 'static,
        initial: InitialPolicy,
        factory: impl for<'a> FnMut(
            ReactionChange<V>,
            ReactionContext<'a, F::State, F::Action>,
        ) -> local::Task<F::State, F::Action>
        + 'static,
    ) -> Self
    where
        F::State: 'static,
        F::Action: 'static,
    {
        self.reactions
            .push(reaction::local(select, initial, factory));
        self
    }

    /// Refreshes authority on configuration changes while preserving this Feature.
    #[must_use]
    pub fn configuration(mut self, select: impl FnMut(&F::State) -> u64 + 'static) -> Self {
        self.configuration = Some(Box::new(select));
        self
    }

    /// Declares one initial task for this entry incarnation.
    #[must_use]
    pub fn on_initial<V: 'static>(
        mut self,
        select: impl FnMut(&F::State) -> V + 'static,
        factory: impl for<'a> FnMut(
            ReactionChange<V>,
            ReactionContext<'a, F::State, F::Action>,
        ) -> local::Task<F::State, F::Action>
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

impl<F: local::Feature> fmt::Debug for EntryMount<F> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EntryMount").finish_non_exhaustive()
    }
}

impl<R, F: local::Feature, K> LocalOptionalMount<R, F, K> {
    /// Owns selectors and construction without executing them.
    ///
    /// `read` and `write` must select the same declared child and agree with
    /// `identity`. Selectors borrow State; they must not mutate it or replace a
    /// child while selecting. External side effects and panics are permitted;
    /// this law does not require pure callbacks or stable memory addresses.
    pub fn new(
        read: for<'a> fn(&'a R) -> Option<&'a F::State>,
        write: for<'a> fn(&'a mut R) -> Option<&'a mut F::State>,
        mut identity: impl FnMut(&R) -> Option<MountIdentity<K>> + 'static,
        make: impl FnMut(&F::State) -> F + 'static,
    ) -> Self {
        Self {
            path: OptionalPath::new(read, write),
            identity: Box::new(move |state, _| identity(state)),
            make: Box::new(make),
            initial: None,
            reactions: Vec::new(),
        }
    }

    /// Appends an owned owner-local reaction with its own initial policy.
    ///
    /// Selected values are independent history; factories return deferred work.
    /// See [`RootMount::on_change`] for policy and registration rules.
    #[must_use]
    pub fn on_change<V: PartialEq + 'static>(
        mut self,
        select: impl FnMut(&F::State) -> V + 'static,
        initial: InitialPolicy,
        factory: impl for<'a> FnMut(
            ReactionChange<V>,
            ReactionContext<'a, F::State, F::Action>,
        ) -> local::Task<F::State, F::Action>
        + 'static,
    ) -> Self
    where
        F::State: 'static,
        F::Action: 'static,
    {
        self.reactions
            .push(reaction::local(select, initial, factory));
        self
    }

    /// Declares one task from an owned value after each fresh installation.
    #[must_use]
    pub fn on_initial<V: 'static>(
        mut self,
        select: impl FnMut(&F::State) -> V + 'static,
        factory: impl for<'a> FnMut(
            ReactionChange<V>,
            ReactionContext<'a, F::State, F::Action>,
        ) -> local::Task<F::State, F::Action>
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

impl<R, F: local::Feature, K> fmt::Debug for LocalOptionalMount<R, F, K> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OptionalMount")
            .field("has_initial", &self.initial.is_some())
            .finish_non_exhaustive()
    }
}

/// Owns an ordered acquisition reply without requiring thread transfer.
type AcquisitionFuture<S, A> =
    Pin<Box<dyn Future<Output = Result<Option<local::TaskContext<S, A>>, TaskAccessError>>>>;

/// Erases the Feature type while retaining the typed child route and weak authority.
pub(in crate::store) trait Acquisition<R, S, X, A>:
    super::super::output::local::HandlerRegistration<R, S, X, A> + commands::Registration<R, A, X>
{
    /// Identifies the bound actor without retaining its lifecycle.
    fn root_identity(&self) -> Option<Arc<()>>;

    /// Selects state while the fold walk owns this committed declaration.
    fn read_committed<'a>(&self, root: &'a R, live: &dyn Fn() -> bool) -> Option<&'a S>;

    /// Rejects a foreign root or closed actor before test-gated admission.
    #[cfg(feature = "test-util")]
    fn validate(&self, root: &Arc<()>) -> Result<(), TaskAccessError>;

    /// Releases the strong header before awaiting admission and the typed reply.
    fn acquire(
        self: Rc<Self>,
        root: Arc<()>,
        #[cfg(feature = "test-util")] gate: Option<
            crate::testing::TransactionGateCore<R, A, Local>,
        >,
    ) -> AcquisitionFuture<S, X>;
}

/// A weak owner-local declaration site for explicit fresh acquisition.
///
/// The site does not own the Feature or State. Acquired contexts freeze one
/// incarnation and configuration; retaining them never retargets a replacement.
pub struct LocalMountSite<R, S, X, A> {
    /// Names a declaration without keeping its callbacks or Feature alive.
    declaration: LocalWeak<dyn Acquisition<R, S, X, A>>,
}

impl<R, S, X, A> LocalMountSite<R, S, X, A> {
    /// Narrows the same weak header to recipe-command dispatch.
    fn command_registration(&self) -> LocalWeak<dyn commands::Registration<R, A, X>> {
        LocalWeak::<dyn Acquisition<R, S, X, A>>::clone(&self.declaration)
    }

    /// Lets a committed fold resolve the declaration without retaining its payload.
    pub(in crate::store) fn fold_registration(&self) -> LocalWeak<dyn Acquisition<R, S, X, A>> {
        LocalWeak::clone(&self.declaration)
    }

    /// Checks root identity and lifecycle before the test gate captures a boundary.
    #[cfg(feature = "test-util")]
    pub(in crate::store) fn validate(&self, root: &Arc<()>) -> Result<(), TaskAccessError> {
        self.declaration
            .upgrade()
            .ok_or_else(closed)?
            .validate(root)
    }

    /// Names output-source authority without exposing the selected state type.
    pub(in crate::store) fn output_registration(
        &self,
    ) -> LocalWeak<dyn super::super::output::local::OutputRegistration> {
        LocalWeak::<dyn Acquisition<R, S, X, A>>::clone(&self.declaration)
    }

    /// Keeps the typed handler route available through the same weak declaration.
    pub(in crate::store) fn handler_registration(
        &self,
    ) -> LocalWeak<dyn super::super::output::local::HandlerRegistration<R, S, X, A>> {
        LocalWeak::<dyn Acquisition<R, S, X, A>>::clone(&self.declaration)
    }
}

impl<R, S, X, A> Clone for LocalMountSite<R, S, X, A> {
    fn clone(&self) -> Self {
        Self {
            declaration: LocalWeak::<dyn Acquisition<R, S, X, A>>::clone(&self.declaration),
        }
    }
}

impl<R, S, X, A> fmt::Debug for LocalMountSite<R, S, X, A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MountSite").finish_non_exhaustive()
    }
}

impl<R: 'static, S: 'static, X: 'static, A: 'static> LocalMountSite<R, S, X, A> {
    /// Captures a weak site now and defers test-gated admission until first poll.
    #[cfg(feature = "test-util")]
    pub(in crate::store) fn acquire_for_test(
        &self,
        root: Arc<()>,
        gate: crate::testing::TransactionGateCore<R, A, Local>,
    ) -> impl Future<Output = Result<Option<local::TaskContext<S, X>>, TaskAccessError>>
    + 'static
    + use<R, S, X, A> {
        let declaration = LocalWeak::<dyn Acquisition<R, S, X, A>>::clone(&self.declaration);
        async move {
            declaration
                .upgrade()
                .ok_or_else(closed)?
                .acquire(root, Some(gate))
                .await
        }
    }

    /// Captures a weak site now and acquires the installation at its mailbox turn.
    pub(in crate::store) fn acquire(
        &self,
        root: Arc<()>,
    ) -> impl Future<Output = Result<Option<local::TaskContext<S, X>>, TaskAccessError>>
    + 'static
    + use<R, S, X, A> {
        let declaration = LocalWeak::<dyn Acquisition<R, S, X, A>>::clone(&self.declaration);
        async move {
            declaration
                .upgrade()
                .ok_or_else(closed)?
                .acquire(
                    root,
                    #[cfg(feature = "test-util")]
                    None,
                )
                .await
        }
    }
}

/// Actor-owned callbacks and the current incarnation, leased outside the cell borrow.
struct Declaration<R, F: local::Feature, K, P = OptionalPath<R, <F as local::Feature>::State>> {
    /// Retains selection and factories across child removal and reinsertion.
    descriptor: Descriptor<R, F, K, P>,
    /// Accesses the installed Feature without extending its payload lifetime.
    current: Option<LocalWeakNode<F>>,
    /// Pairs declared identity with the registry's actual installed lifetime.
    installed: Option<(MountIdentity<K>, ScopeToken)>,
    /// Consumed once after each successful installation, before running hooks.
    initial_pending: bool,
}

/// Stable declaration header; weak sites may survive its drained callback payload.
struct Cell<R, A, F: local::Feature, K, P = OptionalPath<R, <F as local::Feature>::State>> {
    /// Identifies this structural declaration across installed incarnations.
    node: ScopeNodeId,
    /// Binds once to actor admission without owning the actor lifecycle.
    binding: OnceLock<Binding<R, A, Local>>,
    /// Empty during callback leases and permanently empty after owner teardown.
    payload: RefCell<Option<Declaration<R, F, K, P>>>,
}

/// Owns payload retirement independently of temporary header upgrades.
struct Owner<R, A, F: local::Feature, K, P = OptionalPath<R, <F as local::Feature>::State>>(
    /// Keeps the header alive until its payload has been retired outside the borrow.
    Rc<Cell<R, A, F, K, P>>,
);

impl<R, A, F: local::Feature, K, P> Drop for Owner<R, A, F, K, P> {
    fn drop(&mut self) {
        self.drain_payload();
    }
}

impl<R, A, F: local::Feature, K, P> Owner<R, A, F, K, P> {
    /// Makes retirement visible before dropping the Feature or callback captures.
    fn drain_payload(&self) {
        let payload = self.0.payload.borrow_mut().take();
        if let Some(current) = payload.as_ref().and_then(|value| value.current.as_ref()) {
            current.retire_feature();
        }
        drop(payload);
    }
}

impl<
    R: 'static,
    A: 'static,
    F: local::Feature + 'static,
    K: Eq + Hash + Clone + Send + Sync + 'static,
    P: BorrowPath<R, State = F::State> + 'static,
> ReactionRunner<R, A, Local> for Owner<R, A, F, K, P>
where
    F::State: 'static,
    F::Action: 'static,
{
    fn run(
        &self,
        state: &R,
        scope: ScopeToken,
        mut context: ReduceContext<'_>,
        frame: &mut reaction::Frame<R, Batch<R, A, Local>>,
        work: &mut Vec<Batch<R, A, Local>>,
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
                ) && let Some(batch) = captured(state, context.reborrow())
                {
                    work.push(batch);
                }
            }
        });
    }
}

impl<
    R: 'static,
    A: 'static,
    F: local::Feature + 'static,
    K: Eq + Hash + Clone + Send + Sync + 'static,
    P: BorrowPath<R, State = F::State> + 'static,
> MountDeclaration<R, A, Local> for Owner<R, A, F, K, P>
where
    F::State: 'static,
    F::Action: 'static,
{
    fn installed_scope(&self) -> Option<ScopeToken> {
        self.0
            .payload
            .borrow()
            .as_ref()
            .and_then(|value| value.installed.as_ref().map(|(_, token)| *token))
    }

    fn registration_identity(&self) -> Option<usize> {
        Some(Rc::as_ptr(&self.0).cast::<()>() as usize)
    }

    fn has_reactions(&self) -> bool {
        self.0
            .payload
            .borrow()
            .as_ref()
            .is_some_and(|value| !value.descriptor.reactions.is_empty())
    }

    fn capture_reactions(
        &self,
        state: &R,
        scopes: &ScopeRegistry,
        frame: &mut reaction::Frame<R, Batch<R, A, Local>>,
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

    fn reaction_entry(&self) -> Option<(ScopeToken, &dyn ReactionRunner<R, A, Local>)> {
        let payload = self.0.payload.borrow();
        let value = payload.as_ref().expect("reaction roster owns payload");
        if value.initial_pending || !value.descriptor.reactions.is_empty() {
            value
                .installed
                .as_ref()
                .map(|(_, scope)| (*scope, self as &dyn ReactionRunner<R, A, Local>))
        } else {
            None
        }
    }

    fn bind(
        &self,
        ingress: &Arc<super::super::ingress::CommandIngress<Command<R, A, Local>>>,
        lifecycle: &Arc<Lifecycle>,
    ) {
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
        mounted: &mut Vec<Box<dyn super::super::mounted::InstalledNode>>,
        parent: ScopeToken,
        _: &mut Vec<Box<<Local as Carriers<R, A>>::Mount>>,
        retire_descendants: &mut nested::Descendants<'_, R, A, Local>,
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
                        let abandoned = declaration.installed.take();
                        declaration.initial_pending = false;
                        transitions.extend(scopes.leave(token));
                        drop(abandoned);
                        return;
                    };
                    let feature = (declaration.descriptor.make)(child);
                    let node = LocalNode::new(feature, token, 0);
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

/// Attaches a local optional descriptor without selecting state or running factories.
#[expect(
    clippy::type_complexity,
    reason = "Returns the declaration owner and its typed weak acquisition sites."
)]
pub(in crate::store) fn attach_local<R, A, F, K>(
    descriptor: LocalOptionalMount<R, F, K>,
) -> (
    Box<dyn MountDeclaration<R, A, Local>>,
    LocalMountSite<R, F::State, F::Action, A>,
)
where
    R: 'static,
    A: 'static,
    F: local::Feature + 'static,
    F::State: 'static,
    F::Action: 'static,
    K: Eq + Hash + Clone + Send + Sync + 'static,
{
    let LocalOptionalMount {
        path,
        identity,
        make,
        initial,
        reactions,
    } = descriptor;
    let (owner, site) = attach_descriptor(Descriptor {
        path,
        identity,
        make,
        initial,
        reactions,
    });
    (Box::new(owner), site)
}

/// Retains an owning optional field path for later acquisition and removal.
#[expect(
    clippy::type_complexity,
    reason = "Returns the declaration owner and its typed weak acquisition sites."
)]
pub(in crate::store) fn attach_owned_optional<R, A, F, K>(
    read: for<'s> fn(&'s R) -> &'s Option<F::State>,
    write: for<'s> fn(&'s mut R) -> &'s mut Option<F::State>,
    mut identity: impl FnMut(&R) -> Option<MountIdentity<K>> + 'static,
    make: impl FnMut(&F::State) -> F + 'static,
) -> (
    Box<dyn MountDeclaration<R, A, Local>>,
    LocalMountSite<R, F::State, F::Action, A>,
)
where
    R: 'static,
    A: 'static,
    F: local::Feature + 'static,
    F::State: 'static,
    F::Action: 'static,
    K: Eq + Hash + Clone + Send + Sync + 'static,
{
    let (owner, site) = attach_descriptor(Descriptor {
        path: super::super::mounted::path::OwnedOptionalPath { read, write },
        identity: Box::new(move |root, _| identity(root)),
        make: Box::new(make),
        initial: None,
        reactions: Vec::new(),
    });
    (Box::new(owner), site)
}

/// Allocates the stable header and separates its payload owner from weak sites.
#[expect(
    clippy::type_complexity,
    reason = "Returns the declaration owner and its typed weak acquisition sites."
)]
fn attach_descriptor<R, A, F, K, P>(
    descriptor: Descriptor<R, F, K, P>,
) -> (
    Owner<R, A, F, K, P>,
    LocalMountSite<R, F::State, F::Action, A>,
)
where
    R: 'static,
    A: 'static,
    F: local::Feature + 'static,
    F::State: 'static,
    F::Action: 'static,
    K: 'static,
    P: BorrowPath<R, State = F::State> + 'static,
{
    let cell = Rc::new(Cell {
        node: ScopeRegistry::node(&mut ScopeSlot::new()),
        binding: OnceLock::new(),
        payload: RefCell::new(Some(Declaration {
            descriptor,
            current: None,
            installed: None,
            initial_pending: false,
        })),
    });
    let declaration = Rc::downgrade(&cell);
    let site = LocalMountSite { declaration };
    (Owner(cell), site)
}

impl<
    R: 'static,
    A: 'static,
    F: local::Feature + 'static,
    K: 'static,
    P: BorrowPath<R, State = F::State> + 'static,
> super::super::output::local::OutputRegistration for Cell<R, A, F, K, P>
where
    F::State: 'static,
    F::Action: 'static,
{
    fn installed(&self) -> Option<(Arc<()>, ScopeToken)> {
        // Local declaration owners drop before their actor's lifecycle guard.
        let lifecycle = self
            .binding
            .get()?
            .lifecycle
            .upgrade()
            .expect("bound local mount owner retains actor lifecycle");
        let token = {
            let payload = self.payload.borrow();
            payload.as_ref()?.installed.as_ref()?.1
        };
        Some((lifecycle.root_identity(), token))
    }
}

impl<
    R: 'static,
    A: 'static,
    F: local::Feature + 'static,
    K: 'static,
    P: BorrowPath<R, State = F::State> + 'static,
> super::super::output::local::HandlerRegistration<R, F::State, F::Action, A>
    for Cell<R, A, F, K, P>
where
    F::State: 'static,
    F::Action: 'static,
{
    fn freeze_handler_route(
        &self,
        scopes: &ScopeRegistry,
    ) -> Option<Box<dyn super::super::output::local::HandlerRoute<R, A, F::State, F::Action>>> {
        let ingress = Weak::clone(&self.binding.get()?.ingress);
        let (path, current) = {
            let payload = self.payload.borrow();
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
    R: 'static,
    A: 'static,
    F: local::Feature + 'static,
    K: 'static,
    P: BorrowPath<R, State = F::State> + 'static,
> commands::Registration<R, A, F::Action> for Cell<R, A, F, K, P>
where
    F::State: 'static,
    F::Action: 'static,
{
    fn freeze_dispatch(
        &self,
        scopes: &ScopeRegistry,
    ) -> Rc<dyn commands::Dispatch<R, A, F::Action>> {
        let binding = self
            .binding
            .get()
            .expect("installed recipe parent is bound to its actor");
        let ingress = Weak::clone(&binding.ingress);
        let (path, current) = {
            let payload = self.payload.borrow();
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
    R: 'static,
    A: 'static,
    F: local::Feature + 'static,
    K: 'static,
    P: BorrowPath<R, State = F::State> + 'static,
> Acquisition<R, F::State, F::Action, A> for Cell<R, A, F, K, P>
where
    F::State: 'static,
    F::Action: 'static,
{
    fn root_identity(&self) -> Option<Arc<()>> {
        let binding = self.binding.get()?;
        // No callback or yield separates the owner-local registration upgrade.
        Some(
            binding
                .lifecycle
                .upgrade()
                .expect("bound local mount owner retains actor lifecycle")
                .root_identity(),
        )
    }

    fn read_committed<'a>(&self, root: &'a R, live: &dyn Fn() -> bool) -> Option<&'a F::State> {
        let path = {
            let payload = self.payload.borrow();
            payload
                .as_ref()
                .expect("fold roster owns the committed declaration")
                .descriptor
                .path
                .clone()
        };
        // No callback or yield separates the walk's liveness check from this
        // first selector. Structural traversal still checks between selectors.
        path.read_structural(root, live)
    }

    #[cfg(feature = "test-util")]
    fn validate(&self, root: &Arc<()>) -> Result<(), TaskAccessError> {
        validate(self.binding.get().ok_or_else(closed)?, root)
    }

    fn acquire(
        self: Rc<Self>,
        root: Arc<()>,
        #[cfg(feature = "test-util")] gate: Option<
            crate::testing::TransactionGateCore<R, A, Local>,
        >,
    ) -> AcquisitionFuture<F::State, F::Action> {
        let weak = Rc::downgrade(&self);
        let binding = self.binding.get().cloned();
        drop(self);
        Box::pin(async move {
            let binding = binding.ok_or_else(closed)?;
            validate(&binding, &root)?;
            // No callback or yield separates validation from this owner-local
            // upgrade; the live Store still owns its ingress.
            let ingress = binding
                .ingress
                .upgrade()
                .expect("validated owner-local acquisition retains ingress");
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
                .send_map(call, |call| Command::<R, A, Local>::Acquire(Box::new(call)))
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

/// Captures one ordered acquisition without keeping a removed declaration alive.
struct TypedAcquire<R, A, F: local::Feature, K, P> {
    /// Revalidated after admission, since nested owners can retire while queued.
    cell: LocalWeak<Cell<R, A, F, K, P>>,
    /// Publishes one typed result, including rejection when the command is dropped.
    reply: ReplyGuard<Option<local::TaskContext<F::State, F::Action>>>,
}

impl<
    R: 'static,
    A: 'static,
    F: local::Feature + 'static,
    K: 'static,
    P: BorrowPath<R, State = F::State> + 'static,
> AcquireCall for TypedAcquire<R, A, F, K, P>
where
    F::State: 'static,
    F::Action: 'static,
{
    fn execute(self: Box<Self>, scopes: &ScopeRegistry, lifecycle: &Lifecycle) {
        let Self { cell, reply } = *self;
        let result = (|| {
            lifecycle.access()?;
            // A nested declaration can retire between admission and execution.
            let cell = cell
                .upgrade()
                .ok_or_else(|| super::super::mounted::scope_ended(TaskProgress::NotStarted))?;
            let binding = cell
                .binding
                .get()
                .expect("admitted acquisition has bound ingress");
            validate(binding, &lifecycle.root_identity())?;
            let (path, current) = {
                let payload = cell.payload.borrow();
                // Local callers retain no strong header across an await, and
                // ordered execution cannot overlap a declaration payload lease.
                let declaration = payload
                    .as_ref()
                    .expect("ordered local acquisition has a restored payload");
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

/// Builds nested fixture declarations through the production binding and retirement path.
#[cfg(test)]
pub(super) struct ChildMounts<'a, R, A, P> {
    /// Selects the fixture parent without retaining its state borrow.
    path: P,
    /// Installed lifetime inherited by declarations discovered in this recipe.
    parent: ScopeToken,
    /// Current preorder depth; each discovered child adds one level.
    depth: usize,
    /// Borrows the reconciling actor's weak admission endpoints.
    binding: &'a Binding<R, A, Local>,
    /// Collects bound child owners in recipe order.
    discovered: &'a mut Vec<Box<<Local as Carriers<R, A>>::Mount>>,
}

#[cfg(test)]
impl<R: 'static, A: 'static, P: BorrowPath<R> + 'static> ChildMounts<'_, R, A, P>
where
    P::State: 'static,
{
    ordinary_test_items! {
    /// Attaches a fixture output owner to the same parent lifetime and actor.
    #[cfg(feature = "test-util")]
    pub(super) fn output<O: 'static>(
        &mut self,
        source: &LocalMountSite<R, impl Sized + 'static, impl Sized + 'static, A>,
        configure: impl FnOnce(&mut super::super::output::local::OutputHandlers<R, A, O>),
    ) -> super::super::output::local::Output<O> {
        let (inner, output) =
            super::super::output::local::attach_owner(source.output_registration(), configure);
        let owner = super::nested::NestedOwner {
            inner,
            parent: self.parent,
            depth: self
                .depth
                .checked_add(1)
                .expect("mount depth space exhausted"),
        };
        let ingress = self
            .binding
            .ingress
            .upgrade()
            .expect("reconciling actor has ingress");
        let lifecycle = self
            .binding
            .lifecycle
            .upgrade()
            .expect("reconciling actor has lifecycle");
        owner.bind(&ingress, &lifecycle);
        self.discovered.push(Box::new(owner));
        output
    }
    }

    /// Reuses composed optional paths to exercise nested fixture retirement.
    pub(super) fn optional<F, K>(
        &mut self,
        descriptor: LocalOptionalMount<P::State, F, K>,
    ) -> LocalMountSite<R, F::State, F::Action, A>
    where
        F: local::Feature + 'static,
        F::State: 'static,
        F::Action: 'static,
        K: Eq + Hash + Clone + Send + Sync + 'static,
    {
        let LocalOptionalMount {
            path,
            mut identity,
            make,
            initial,
            reactions,
        } = descriptor;
        let parent_path = self.path.clone();
        let descriptor = Descriptor {
            path: crate::store::mounted::path::ComposedPath::new(self.path.clone(), path),
            identity: Box::new(move |root, live| {
                let parent = parent_path.read_structural(root, live)?;
                if !live() {
                    return None;
                }
                identity(parent, live)
            }),
            make,
            initial,
            reactions,
        };
        let (inner, site) = attach_descriptor(descriptor);
        let owner = super::nested::NestedOwner {
            inner,
            parent: self.parent,
            depth: self
                .depth
                .checked_add(1)
                .expect("mount depth space exhausted"),
        };
        let ingress = self
            .binding
            .ingress
            .upgrade()
            .expect("reconciling actor has ingress");
        let lifecycle = self
            .binding
            .lifecycle
            .upgrade()
            .expect("reconciling actor has lifecycle");
        owner.bind(&ingress, &lifecycle);
        self.discovered.push(Box::new(owner));
        site
    }
}

/// Declares one fixture key with insertion-stamp authority and a nested recipe.
#[cfg(test)]
#[expect(
    clippy::type_complexity,
    reason = "keep the paired local declaration owner and typed mount site explicit in the test helper"
)]
pub(super) fn attach_selected_key<R, A, F, K, G>(
    read: for<'a> fn(&'a R) -> &'a crate::KeyedState<K, F::State>,
    write: for<'a> fn(&'a mut R) -> &'a mut crate::KeyedState<K, F::State>,
    key: K,
    make: impl FnMut(&F::State) -> F + 'static,
    mut recipe: G,
) -> (
    Box<dyn MountDeclaration<R, A, Local>>,
    LocalMountSite<R, F::State, F::Action, A>,
)
where
    R: 'static,
    A: 'static,
    F: local::Feature + 'static,
    F::State: 'static,
    F::Action: 'static,
    K: Eq + Hash + Send + Sync + 'static,
    G: FnMut(
            &F::State,
            &mut ChildMounts<
                '_,
                R,
                A,
                crate::store::mounted::path::KeyedPath<R, K, F::State, Rc<K>>,
            >,
        ) + 'static,
{
    let key = Rc::new(key);
    let path = crate::store::mounted::path::KeyedPath::new(read, write, std::rc::Rc::clone(&key));
    let descriptor = Descriptor {
        path: path.clone(),
        identity: Box::new(move |root, live| {
            let collection = read(root);
            if !live() {
                return None;
            }
            collection.entry_stamp(&*key).map(|stamp| MountIdentity {
                key: stamp.to_owned(),
                replacement: 0,
                configuration: 0,
            })
        }),
        make: Box::new(make),
        initial: None,
        reactions: Vec::new(),
    };
    let (inner, site) = attach_descriptor(descriptor);
    let owner = super::nested::RecipeOwner::new(
        inner,
        move |root: &R,
              parent,
              depth,
              binding: &Binding<R, A, Local>,
              discovered: &mut Vec<Box<<Local as Carriers<R, A>>::Mount>>| {
            let live = || {
                binding
                    .lifecycle
                    .upgrade()
                    .is_some_and(|lifecycle| !lifecycle.is_closed())
            };
            let state = path.read_structural(root, &live);
            if !live() {
                return false;
            }
            let Some(state) = state else {
                return false;
            };
            recipe(
                state,
                &mut ChildMounts {
                    path: path.clone(),
                    parent,
                    depth,
                    binding,
                    discovered,
                },
            );
            true
        },
    );
    (Box::new(owner), site)
}
