//! Builds validated actor-backed stores.
//!
//! Builders own state, behaviour, dependencies and mount declarations until construction.
//! Validation precedes actor construction and spawning. Child factories and root
//! lifecycle hooks run on the actor, not while declaring mounts. Output methods
//! invoke their configuration closure immediately, then retain the registered handlers.

use std::fmt;
use std::marker::PhantomData;
use std::num::NonZeroUsize;
use std::sync::Arc;

use super::carriers::{Execution, Shared};
use super::command::Command;
use super::ingress::CommandIngress;
use super::{Actor, Lifecycle, Store};
use crate::effect::lineage::ControlWake;
use crate::{DependencyKey, DependencyValues, Reducer, Runtime, StoreBuildError};

/// Default number of waiting Store commands.
pub const DEFAULT_MAILBOX_CAPACITY: usize = 256;

/// Checks the fixed default at compile time for infallible convenience constructors.
pub(super) const DEFAULT_NONZERO_MAILBOX_CAPACITY: NonZeroUsize =
    NonZeroUsize::new(DEFAULT_MAILBOX_CAPACITY).expect("default mailbox capacity must be non-zero");

/// Configures actor-backed Store construction.
pub struct StoreBuilder<State, Action, R> {
    /// Initial state, transferred to the actor only after validation.
    state: State,
    /// Owned reducer or typed feature, wrapped after validation.
    reducer: R,
    /// Host that receives the actor future and supplies its runtime services.
    runtime: Runtime,
    /// Owned overrides and defaults shared by reducer and task dependency lookups.
    dependencies: DependencyValues,
    /// Reporting endpoint, including the deterministic recorder when installed.
    issues: crate::IssueReporter,
    /// Optional user callback combined with the reporting endpoint during construction.
    issue_handler: Option<crate::issue::IssueHandler>,
    /// Actor-owned observer, installed before the first actor poll.
    #[cfg(feature = "diagnostics")]
    lifecycle_sink:
        Option<Box<<Shared as super::carriers::Carriers<State, Action>>::LifecycleSink>>,
    /// Requested waiting-command bound, checked when construction starts.
    mailbox_capacity: usize,
    /// Owned declarations, bound in order before the actor is spawned.
    mounts: Vec<Box<dyn super::mount::MountDeclaration<State, Action> + Send>>,
    /// Tracks the action input type without owning an action value.
    action: PhantomData<fn(Action)>,
    /// Legacy assertion controller, mutually exclusive with the typed frontier.
    #[cfg(feature = "test-util")]
    assertion_gate: Option<crate::testing::AssertionGate<Action>>,
    /// Typed frontier controller, claimed only after mailbox validation succeeds.
    #[cfg(feature = "test-util")]
    transaction_gate: Option<crate::testing::TransactionGate<State, Action>>,
}

impl<State, Action, R> StoreBuilder<State, Action, R>
where
    State: Send + 'static,
    Action: Send + 'static,
{
    /// Retains construction inputs without wrapping behaviour or spawning an actor.
    pub(crate) fn new(state: State, reducer: R, runtime: Runtime) -> Self {
        Self {
            state,
            reducer,
            runtime,
            dependencies: DependencyValues::new(),
            issues: crate::IssueReporter::default(),
            issue_handler: None,
            #[cfg(feature = "diagnostics")]
            lifecycle_sink: None,
            mailbox_capacity: DEFAULT_MAILBOX_CAPACITY,
            mounts: Vec::new(),
            action: PhantomData,
            #[cfg(feature = "test-util")]
            assertion_gate: None,
            #[cfg(feature = "test-util")]
            transaction_gate: None,
        }
    }

    /// Uses the compile-time-checked capacity and no deterministic test gates.
    pub(super) fn build_default(state: State, reducer: R, runtime: Runtime) -> Store<State, Action>
    where
        R: Reducer<State = State, Action = Action> + Send + 'static,
    {
        Self::new(state, reducer, runtime).build_with_capacity(
            DEFAULT_NONZERO_MAILBOX_CAPACITY,
            #[cfg(feature = "test-util")]
            None,
            #[cfg(feature = "test-util")]
            None,
            |reducer| Box::new(crate::ReducerFeature::new(reducer)),
        )
    }

    /// Sets bounded command capacity.
    ///
    /// Defaults to [`DEFAULT_MAILBOX_CAPACITY`]. Zero is accepted by this setter
    /// and rejected by [`Self::build`] before an actor is spawned.
    #[must_use]
    pub const fn mailbox_capacity(mut self, capacity: usize) -> Self {
        self.mailbox_capacity = capacity;
        self
    }

    /// Observes lifecycle facts synchronously on this Store's actor.
    ///
    /// Replaces and drops the previous callback without invoking it. Delivery
    /// runs in debug and release builds, outside framework locks; blocking the
    /// callback blocks the actor. Panics propagate without rollback. See
    /// [`LifecycleEvent`](crate::diagnostics::LifecycleEvent) for ordering and
    /// cancellation semantics. This callback does not require `Sync`.
    #[cfg(feature = "diagnostics")]
    #[must_use]
    pub fn with_lifecycle_diagnostics(
        mut self,
        sink: impl FnMut(&crate::diagnostics::LifecycleEvent) + Send + 'static,
    ) -> Self {
        self.lifecycle_sink = Some(Box::new(sink));
        self
    }

    /// Attaches a typed declaration for the deterministic testing adapter.
    #[cfg(feature = "test-util")]
    #[doc(hidden)]
    #[must_use]
    pub fn mount<Child, K>(self, descriptor: super::mount::OptionalMount<State, Child, K>) -> Self
    where
        Child: crate::Feature + Send + 'static,
        Child::State: Send + 'static,
        Child::Action: Send + 'static,
        K: Eq + std::hash::Hash + Clone + Send + Sync + 'static,
    {
        let (attach, _) = Self::defer_mount(descriptor);
        attach(self)
    }

    /// Prepares one production declaration for deferred deterministic builder attachment.
    ///
    /// The closure owns the real declaration; its site is weak. Applying the
    /// closure transfers that same declaration without constructing the child.
    #[cfg(feature = "test-util")]
    #[doc(hidden)]
    #[expect(
        clippy::type_complexity,
        reason = "The opaque attachment closure and typed weak site are returned together without an allocation"
    )]
    pub fn defer_mount<Child, K>(
        descriptor: super::mount::OptionalMount<State, Child, K>,
    ) -> (
        impl FnOnce(Self) -> Self + Send + 'static,
        super::mount::MountSite<State, Child::State, Child::Action, Action>,
    )
    where
        Child: crate::Feature + Send + 'static,
        Child::State: Send + 'static,
        Child::Action: Send + 'static,
        K: Eq + std::hash::Hash + Clone + Send + Sync + 'static,
    {
        let (declaration, site) = super::mount::attach(descriptor);
        (
            move |mut builder: Self| {
                builder.mounts.push(declaration);
                builder
            },
            site,
        )
    }

    /// Prepares the existing mapped declaration for deferred test attachment.
    #[cfg(feature = "test-util")]
    #[doc(hidden)]
    #[expect(
        clippy::type_complexity,
        reason = "One opaque attachment closure returns the two independently typed weak sites without additional storage"
    )]
    pub fn defer_mount_with_command<Child, K, C>(
        descriptor: super::mount::OptionalMount<State, Child, K>,
        mapper: impl FnMut(C) -> Child::Action + Send + 'static,
    ) -> (
        impl FnOnce(Self) -> Self + Send + 'static,
        super::mount::MountSite<State, Child::State, Child::Action, Action>,
        super::command_port::CommandSite<State, C>,
    )
    where
        Child: crate::Feature + Send + 'static,
        Child::State: Send + 'static,
        Child::Action: Send + 'static,
        K: Eq + std::hash::Hash + Clone + Send + Sync + 'static,
        C: Send + 'static,
    {
        let (declaration, site, command) =
            super::mount::attach_command(descriptor, mapper, super::mounted::InputClass::Ordinary);
        (
            move |mut builder: Self| {
                builder.mounts.push(declaration);
                builder
            },
            site,
            command,
        )
    }

    /// Prepares an incarnation-owned child recipe for deferred test attachment.
    #[cfg(feature = "test-util")]
    #[doc(hidden)]
    #[expect(
        clippy::type_complexity,
        reason = "The opaque attachment closure and typed weak site are returned together without an allocation"
    )]
    pub fn defer_mount_with_children<Child, K>(
        descriptor: super::mount::OptionalMount<State, Child, K>,
        recipe: impl FnMut(
            &Child::State,
            &mut super::mount::children::MountChildren<
                '_,
                State,
                Action,
                Child::State,
                Child::Action,
            >,
        ) + Send
        + 'static,
    ) -> (
        impl FnOnce(Self) -> Self + Send + 'static,
        super::mount::MountSite<State, Child::State, Child::Action, Action>,
    )
    where
        Child: crate::Feature + Send + 'static,
        Child::State: Send + 'static,
        Child::Action: Send + 'static,
        K: Eq + std::hash::Hash + Clone + Send + Sync + 'static,
    {
        let (owner, site) = super::mount::children::attach(descriptor, recipe);
        (
            move |mut builder: Self| {
                builder.mounts.push(owner);
                builder
            },
            site,
        )
    }

    /// Prepares the same production output owner for deferred test attachment.
    ///
    /// Calls `configure` immediately. The returned closure owns the registered
    /// handlers until it transfers them into a builder. The output handle is weak.
    ///
    /// # Panics
    ///
    /// Panics if `configure` panics.
    #[cfg(feature = "test-util")]
    #[doc(hidden)]
    pub fn defer_output<O: Send + 'static>(
        source: &super::mount::MountSite<State, impl Send + 'static, impl Send + 'static, Action>,
        configure: impl FnOnce(&mut super::output::OutputHandlers<State, Action, O>),
    ) -> (
        impl FnOnce(Self) -> Self + Send + 'static,
        super::output::Output<O>,
    ) {
        let (owner, output) = super::output::attach(source.output_registration(), configure);
        (
            move |mut builder: Self| {
                builder.mounts.push(owner);
                builder
            },
            output,
        )
    }

    /// Overrides one Store-owned dependency.
    #[must_use]
    pub fn with_dependency<K: DependencyKey>(mut self, value: K::Value) -> Self {
        self.dependencies.set::<K>(value);
        self
    }

    /// Replaces the owned dependency environment, including overrides and registered defaults.
    ///
    /// Builder calls apply in order. Values set before this call are replaced;
    /// subsequent dependency overrides apply to the supplied environment.
    #[must_use]
    pub fn with_dependency_values(mut self, values: DependencyValues) -> Self {
        self.dependencies = values;
        self
    }

    /// Enables Store-local reporting with an outside-lock issue handler.
    ///
    /// Replaces the previous handler without invoking it. An installed test
    /// recorder remains active and records each issue before the handler runs.
    #[must_use]
    pub fn with_issue_handler(
        mut self,
        handler: impl Fn(&crate::Issue) + Send + Sync + 'static,
    ) -> Self {
        self.issue_handler = Some(Box::new(handler));
        self
    }

    /// Installs the mandatory deterministic TestStore issue recorder.
    #[cfg(feature = "test-util")]
    #[doc(hidden)]
    #[must_use]
    pub fn issue_recorder(mut self, recorder: crate::testing::IssueRecorder) -> Self {
        self.issues = recorder.into_reporter();
        self
    }

    /// Sets one highest-priority dependency for deterministic tests.
    #[cfg(feature = "test-util")]
    #[doc(hidden)]
    #[must_use]
    pub fn with_test_dependency<K: DependencyKey>(mut self, value: K::Value) -> Self {
        self.dependencies.set_test::<K>(value);
        self
    }

    /// Selects test defaults without changing explicit dependency precedence.
    #[cfg(feature = "test-util")]
    #[doc(hidden)]
    #[must_use]
    pub fn with_test_dependency_defaults(mut self) -> Self {
        self.dependencies.use_test_defaults();
        self
    }

    /// Installs deterministic effect-action assertion gate.
    ///
    /// Replaces any transaction gate. Construction fails while the gate remains
    /// attached to another actor. This setter does not claim the gate.
    #[cfg(feature = "test-util")]
    #[doc(hidden)]
    #[must_use]
    pub fn assertion_gate(mut self, gate: crate::testing::AssertionGate<Action>) -> Self {
        self.transaction_gate = None;
        self.assertion_gate = Some(gate);
        self
    }

    /// Installs the typed deterministic transaction frontier.
    ///
    /// Replaces any assertion gate. Construction fails if the gate is attached
    /// or reserved by unfinished work. This setter does not claim the gate.
    #[cfg(feature = "test-util")]
    #[doc(hidden)]
    #[must_use]
    pub fn transaction_gate(
        mut self,
        gate: crate::testing::TransactionGate<State, Action>,
    ) -> Self {
        self.assertion_gate = None;
        self.transaction_gate = Some(gate);
        self
    }

    /// Validates configuration and spawns one Store actor.
    ///
    /// Validation finishes before the reducer is wrapped or passed to the runtime.
    ///
    /// # Errors
    ///
    /// Returns [`StoreBuildError`] for zero capacity or a test gate that is
    /// attached or reserved by unfinished work. A failed build drops its owned inputs.
    ///
    /// # Panics
    ///
    /// Propagates panics from the runtime driver or captures discarded during gate attachment.
    pub fn build(self) -> Result<Store<State, Action>, StoreBuildError>
    where
        R: Reducer<State = State, Action = Action> + Send + 'static,
    {
        self.build_using(|reducer| Box::new(crate::ReducerFeature::new(reducer)))
    }

    /// Validates capacity and claims the selected gate before wrapping owned behaviour.
    fn build_using(
        self,
        driver: impl FnOnce(
            R,
        ) -> Box<
            dyn crate::feature::ReduceDriver<
                    State,
                    Action,
                    Effect = crate::Effect<Action>,
                    Task = crate::Task<State, Action>,
                > + Send,
        >,
    ) -> Result<Store<State, Action>, StoreBuildError> {
        let Some(mailbox_capacity) = NonZeroUsize::new(self.mailbox_capacity) else {
            return Err(StoreBuildError::zero_capacity());
        };
        #[cfg(feature = "test-util")]
        let assertion_gate = self
            .assertion_gate
            .as_ref()
            .map(crate::testing::AssertionGate::attach)
            .transpose()?;
        #[cfg(feature = "test-util")]
        let transaction_gate = self
            .transaction_gate
            .as_ref()
            .map(crate::testing::TransactionGate::attach)
            .transpose()?;
        Ok(self.build_with_capacity(
            mailbox_capacity,
            #[cfg(feature = "test-util")]
            assertion_gate,
            #[cfg(feature = "test-util")]
            transaction_gate,
            driver,
        ))
    }

    /// Constructs a typed feature builder for deterministic test integration.
    #[doc(hidden)]
    pub fn feature(state: State, feature: R, runtime: Runtime) -> Self
    where
        R: crate::Feature<State = State, Action = Action> + Send + 'static,
    {
        Self::new(state, feature, runtime)
    }

    /// Builds a typed feature through the same validated actor construction.
    ///
    /// # Errors
    ///
    /// Returns [`StoreBuildError`] for zero capacity or a test gate that is
    /// attached or reserved by unfinished work.
    ///
    /// # Panics
    ///
    /// Propagates panics from the runtime driver or captures discarded during gate attachment.
    #[doc(hidden)]
    pub fn build_feature(self) -> Result<Store<State, Action>, StoreBuildError>
    where
        R: crate::Feature<State = State, Action = Action> + Send + 'static,
    {
        self.build_using(|feature| Box::new(feature))
    }

    /// Transfers validated inputs and claimed gates into one spawned actor.
    fn build_with_capacity(
        self,
        mailbox_capacity: NonZeroUsize,
        #[cfg(feature = "test-util")] assertion_gate: Option<
            crate::testing::AssertionGateBinding<Action>,
        >,
        #[cfg(feature = "test-util")] transaction_gate: Option<
            crate::testing::TransactionGateBinding<State, Action>,
        >,
        driver: impl FnOnce(
            R,
        ) -> Box<
            dyn crate::feature::ReduceDriver<
                    State,
                    Action,
                    Effect = crate::Effect<Action>,
                    Task = crate::Task<State, Action>,
                > + Send,
        >,
    ) -> Store<State, Action> {
        let (actor, ingress, lifecycle) = prepare_actor::<State, Action, Shared>(
            self.state,
            driver(self.reducer),
            self.runtime.clone(),
            self.dependencies,
            self.issues,
            self.issue_handler,
            mailbox_capacity,
            self.mounts,
            #[cfg(feature = "diagnostics")]
            self.lifecycle_sink,
            #[cfg(feature = "test-util")]
            assertion_gate,
            #[cfg(feature = "test-util")]
            transaction_gate,
        );
        self.runtime.spawn(Box::pin(actor.run()));
        Store::from_parts(ingress, lifecycle)
    }
}

/// Unspawned actor and the shared ownership retained by its public Store handle.
type PreparedActor<S, A, C> = (
    Actor<S, A, C>,
    Arc<CommandIngress<Command<S, A, C>>>,
    Arc<Lifecycle>,
);

/// Assembles and binds an actor without handing its future to the runtime.
///
/// Shared and local builders use the same ownership transfer. An existing test
/// recorder survives installation of a user handler. Mounts, diagnostics and
/// the typed frontier are bound before the actor can process commands.
#[expect(
    clippy::too_many_arguments,
    reason = "Shared and local builders transfer their owned configuration into one actor constructor"
)]
pub(super) fn prepare_actor<S: 'static, A: 'static, C: Execution<S, A> + 'static>(
    state: S,
    driver: Box<C::Driver>,
    runtime: C::Runtime,
    dependencies: DependencyValues,
    mut issues: crate::IssueReporter,
    issue_handler: Option<crate::issue::IssueHandler>,
    mailbox_capacity: NonZeroUsize,
    mounts: Vec<Box<C::Mount>>,
    #[cfg(feature = "diagnostics")] lifecycle_sink: Option<Box<C::LifecycleSink>>,
    #[cfg(feature = "test-util")] assertion_gate: Option<crate::testing::AssertionGateBinding<A>>,
    #[cfg(feature = "test-util")] transaction_gate: Option<
        crate::testing::TransactionGateBinding<S, A, C>,
    >,
) -> PreparedActor<S, A, C> {
    if let Some(handler) = issue_handler {
        #[cfg(feature = "test-util")]
        if issues.endpoint_enabled() {
            issues.set_handler(handler);
        } else {
            issues = crate::IssueReporter::enabled(handler);
        }
        #[cfg(not(feature = "test-util"))]
        {
            issues = crate::IssueReporter::enabled(handler);
        }
    }

    let (sender, receiver) = super::mailbox::bounded(mailbox_capacity);
    let control = ControlWake::new();
    let ingress = Arc::new(CommandIngress::new(sender, Arc::clone(&control)));
    let lifecycle = Arc::new(Lifecycle::new());
    let mut actor = Actor::<S, A, C>::with_driver(
        state,
        driver,
        dependencies,
        issues,
        receiver,
        &ingress,
        Arc::clone(&lifecycle),
        runtime,
        #[cfg(feature = "test-util")]
        assertion_gate,
    );
    actor.attach_mounts(mounts);
    #[cfg(feature = "diagnostics")]
    actor.set_lifecycle_sink(lifecycle_sink);
    #[cfg(feature = "test-util")]
    actor.set_transaction_gate(transaction_gate);
    (actor, ingress, lifecycle)
}

impl<State, Action, R> fmt::Debug for StoreBuilder<State, Action, R> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StoreBuilder")
            .field("runtime", &self.runtime)
            .field("mailbox_capacity", &self.mailbox_capacity)
            .finish_non_exhaustive()
    }
}

/// Configures a typed feature using the ordinary Store construction path.
pub struct FeatureStoreBuilder<S, A, F> {
    /// Common configuration and owned feature, using the reducer builder's validation path.
    inner: StoreBuilder<S, A, F>,
    /// Root lifecycle declaration appended after explicit child and output declarations.
    root_mount: Option<Box<dyn super::mount::MountDeclaration<S, A> + Send>>,
    /// Lazily shared root registration for removal and root-sourced output declarations.
    removal_root: Option<Arc<super::removal::root::RootRegistration<S, A>>>,
}

impl<
    S: Send + 'static,
    A: Send + 'static,
    F: crate::Feature<State = S, Action = A> + Send + 'static,
> FeatureStoreBuilder<S, A, F>
{
    /// Attaches an optional child without retaining an acquisition site.
    #[cfg(test)]
    pub(super) fn with_mount(
        mut self,
        owner: Box<dyn super::mount::MountDeclaration<S, A> + Send>,
    ) -> Self {
        self.inner.mounts.push(owner);
        self
    }

    /// Registers owned output handlers for one mounted source.
    ///
    /// Handlers run nearest ancestor first, with declaration order breaking ties.
    /// Calls `configure` immediately, but stores its handlers for later actor
    /// delivery. The builder owns their captures; the returned output handle does not.
    ///
    /// # Panics
    ///
    /// Panics if `configure` panics.
    pub fn output<O: Send + 'static>(
        mut self,
        source: &super::mount::MountSite<S, impl Send + 'static, impl Send + 'static, A>,
        configure: impl FnOnce(&mut super::output::OutputHandlers<S, A, O>),
    ) -> (Self, super::output::Output<O>) {
        let (owner, output) = super::output::attach(source.output_registration(), configure);
        self.inner.mounts.push(owner);
        (self, output)
    }

    /// Creates one root owner on demand and reuses its registration for later declarations.
    fn removal_root(&mut self) -> Arc<super::removal::root::RootRegistration<S, A>> {
        Arc::clone(self.removal_root.get_or_insert_with(|| {
            let registration = super::removal::root::RootRegistration::new();
            self.inner
                .mounts
                .push(Box::new(super::removal::root::RootOwner(Arc::clone(
                    &registration,
                ))));
            registration
        }))
    }

    /// Declares root-sourced output with the ordinary strict-ancestor routing law.
    ///
    /// The root has no ancestor. Delivery returns the owned value as unhandled;
    /// registering the root itself does not make it an eligible recipient.
    /// Calls `configure` immediately and keeps its handler captures in the builder.
    ///
    /// # Panics
    ///
    /// Panics if `configure` panics.
    pub fn root_output<O: Send + 'static>(
        mut self,
        configure: impl FnOnce(&mut super::output::OutputHandlers<S, A, O>),
    ) -> (Self, super::output::Output<O>) {
        let source: Arc<dyn super::output::OutputRegistration> = self.removal_root();
        let (owner, output) = super::output::attach(Arc::downgrade(&source), configure);
        self.inner.mounts.push(owner);
        (self, output)
    }

    /// Declares an optional child and checked ownership extraction from this root.
    ///
    /// `read` and `write` must select the same owning field and agree with
    /// `identity`. Selectors borrow State without mutating it; in particular,
    /// they must not replace the child during selection. They may have external
    /// side effects or panic. Removal mutates the field only after selection.
    #[expect(
        clippy::type_complexity,
        reason = "Construction returns the builder plus distinct acquisition and owning-removal capabilities"
    )]
    pub fn mount_removable<Child, K>(
        mut self,
        read: for<'s> fn(&'s S) -> &'s Option<Child::State>,
        write: for<'s> fn(&'s mut S) -> &'s mut Option<Child::State>,
        identity: impl FnMut(&S) -> Option<super::mount::MountIdentity<K>> + Send + 'static,
        make: impl FnMut(&Child::State) -> Child + Send + 'static,
    ) -> (
        Self,
        super::mount::MountSite<S, Child::State, Child::Action, A>,
        super::removal::RemovalSite<S, Child::State, A>,
    )
    where
        Child: crate::Feature + Send + 'static,
        Child::State: Send + 'static,
        Child::Action: Send + 'static,
        K: Eq + std::hash::Hash + Clone + Send + Sync + 'static,
    {
        let root = self.removal_root();
        let (owner, site) = super::mount::attach_owned_optional(read, write, identity, make);
        let removal = super::removal::root_optional(&root, site.output_registration(), write);
        self.inner.mounts.push(owner);
        (self, site, removal)
    }

    /// Attaches an optional child without retaining an acquisition site.
    #[must_use]
    pub fn mount<Child, K>(self, descriptor: super::mount::OptionalMount<S, Child, K>) -> Self
    where
        Child: crate::Feature + Send + 'static,
        Child::State: Send + 'static,
        Child::Action: Send + 'static,
        K: Eq + std::hash::Hash + Clone + Send + Sync + 'static,
    {
        self.mount_with_site(descriptor).0
    }

    /// Attaches a child and returns a weak site for explicit acquisition.
    pub fn mount_with_site<Child, K>(
        mut self,
        descriptor: super::mount::OptionalMount<S, Child, K>,
    ) -> (
        Self,
        super::mount::MountSite<S, Child::State, Child::Action, A>,
    )
    where
        Child: crate::Feature + Send + 'static,
        Child::State: Send + 'static,
        Child::Action: Send + 'static,
        K: Eq + std::hash::Hash + Clone + Send + Sync + 'static,
    {
        let (declaration, site) = super::mount::attach(descriptor);
        self.inner.mounts.push(declaration);
        (self, site)
    }

    /// Attaches an optional child with an incarnation-bound child recipe.
    pub fn mount_with_children<Child, K>(
        mut self,
        descriptor: super::mount::OptionalMount<S, Child, K>,
        recipe: impl FnMut(
            &Child::State,
            &mut super::mount::children::MountChildren<'_, S, A, Child::State, Child::Action>,
        ) + Send
        + 'static,
    ) -> (
        Self,
        super::mount::MountSite<S, Child::State, Child::Action, A>,
    )
    where
        Child: crate::Feature + Send + 'static,
        Child::State: Send + 'static,
        Child::Action: Send + 'static,
        K: Eq + std::hash::Hash + Clone + Send + Sync + 'static,
    {
        let (owner, site) = super::mount::children::attach(descriptor, recipe);
        self.inner.mounts.push(owner);
        (self, site)
    }

    /// Attaches every keyed entry using fresh construction for each incarnation.
    ///
    /// Reorder/configuration retain Features and caches; new entry stamps and
    /// explicit scope leave rebuild them. Configuration changes expire captured
    /// authority. Selectors are relative to this builder's actual root State.
    #[must_use]
    pub fn keyed<Child, K, ChildState>(
        mut self,
        read: for<'s> fn(&'s S) -> &'s crate::KeyedState<K, ChildState>,
        write: for<'s> fn(&'s mut S) -> &'s mut crate::KeyedState<K, ChildState>,
        mut make: impl FnMut(&K, &ChildState) -> super::mount::EntryMount<Child> + Send + 'static,
    ) -> Self
    where
        Child: crate::Feature<State = ChildState> + Send + 'static,
        ChildState: Send + 'static,
        Child::Action: Send + 'static,
        K: Eq + std::hash::Hash + Clone + Send + Sync + 'static,
    {
        let owner = super::mount::children::keyed(
            super::mounted::OptionalPath::new(|state| Some(state), |state| Some(state)),
            read,
            write,
            0,
            move |key, state, _| {
                (
                    make(key, state),
                    None::<
                        fn(
                            &ChildState,
                            &mut super::mount::children::MountChildren<
                                '_,
                                S,
                                A,
                                ChildState,
                                Child::Action,
                            >,
                        ),
                    >,
                )
            },
        );
        self.inner.mounts.push(Box::new(owner));
        self
    }

    /// Attaches a keyed collection with fresh child recipes for each incarnation.
    ///
    /// Departing subtrees of this collection drain before new entries are made
    /// in keyed order. The resulting roster is then traversed depth-first;
    /// retained nested collections reconcile there, not in a global retirement
    /// pass. Reorder/configuration retain both Feature and recipe; replacement
    /// or explicit scope leave renews both. Returned callbacks own their captures.
    #[must_use]
    pub fn keyed_with_children<Child, K, G, ChildState>(
        mut self,
        read: for<'s> fn(&'s S) -> &'s crate::KeyedState<K, ChildState>,
        write: for<'s> fn(&'s mut S) -> &'s mut crate::KeyedState<K, ChildState>,
        mut make: impl FnMut(&K, &ChildState) -> (super::mount::EntryMount<Child>, G) + Send + 'static,
    ) -> Self
    where
        Child: crate::Feature<State = ChildState> + Send + 'static,
        ChildState: Send + 'static,
        Child::Action: Send + 'static,
        K: Eq + std::hash::Hash + Clone + Send + Sync + 'static,
        G: FnMut(
                &ChildState,
                &mut super::mount::children::MountChildren<'_, S, A, ChildState, Child::Action>,
            ) + Send
            + 'static,
    {
        let owner = super::mount::children::keyed(
            super::mounted::OptionalPath::new(|state| Some(state), |state| Some(state)),
            read,
            write,
            0,
            move |key, state, _| {
                let (entry, recipe) = make(key, state);
                (entry, Some(recipe))
            },
        );
        self.inner.mounts.push(Box::new(owner));
        self
    }

    /// Declares keyed root children with checked owning removal in each recipe.
    #[must_use]
    pub fn keyed_removable<Child, K, G>(
        mut self,
        read: for<'s> fn(&'s S) -> &'s crate::KeyedState<K, Child::State>,
        write: for<'s> fn(&'s mut S) -> &'s mut crate::KeyedState<K, Child::State>,
        make: impl FnMut(&K, &Child::State) -> (super::mount::EntryMount<Child>, G) + Send + 'static,
    ) -> Self
    where
        Child: crate::Feature + Send + 'static,
        Child::State: Send + 'static,
        Child::Action: Send + 'static,
        K: Eq + std::hash::Hash + Clone + Send + Sync + 'static,
        G: FnMut(
                &Child::State,
                &mut super::mount::children::MountChildren<'_, S, A, Child::State, Child::Action>,
                super::removal::RemovalSite<S, Child::State, A>,
            ) + Send
            + 'static,
    {
        let root: Arc<dyn super::output::HandlerRegistration<S, S, A, A>> = self.removal_root();
        let owner = super::mount::children::keyed_removable(
            super::mounted::OptionalPath::new(|state| Some(state), |state| Some(state)),
            Arc::downgrade(&root),
            read,
            write,
            0,
            make,
        );
        self.inner.mounts.push(Box::new(owner));
        self
    }

    /// Declares native stack children with checked pop-last removal in each recipe.
    #[must_use]
    pub fn stack_removable<Child, G>(
        mut self,
        read: for<'s> fn(&'s S) -> &'s crate::StackState<Child::State>,
        write: for<'s> fn(&'s mut S) -> &'s mut crate::StackState<Child::State>,
        make: impl FnMut(&crate::StackId, &Child::State) -> (super::mount::EntryMount<Child>, G)
        + Send
        + 'static,
    ) -> Self
    where
        Child: crate::Feature + Send + 'static,
        Child::State: Send + 'static,
        Child::Action: Send + 'static,
        G: FnMut(
                &Child::State,
                &mut super::mount::children::MountChildren<'_, S, A, Child::State, Child::Action>,
                super::removal::RemovalSite<S, Child::State, A>,
            ) + Send
            + 'static,
    {
        let root: Arc<dyn super::output::HandlerRegistration<S, S, A, A>> = self.removal_root();
        let owner = super::mount::children::stack_removable(
            super::mounted::OptionalPath::new(|state| Some(state), |state| Some(state)),
            Arc::downgrade(&root),
            read,
            write,
            0,
            make,
        );
        self.inner.mounts.push(Box::new(owner));
        self
    }

    /// Attaches one child with both ordinary and narrow command acquisition sites.
    ///
    /// The mapper runs only after actor validation and child selection. It may
    /// capture Send, non-Sync state; neither returned site owns its captures.
    #[expect(
        clippy::type_complexity,
        reason = "Construction returns the builder and independently typed child and command sites"
    )]
    pub fn mount_with_command<Child, K, C>(
        mut self,
        descriptor: super::mount::OptionalMount<S, Child, K>,
        mapper: impl FnMut(C) -> Child::Action + Send + 'static,
    ) -> (
        Self,
        super::mount::MountSite<S, Child::State, Child::Action, A>,
        super::command_port::CommandSite<S, C>,
    )
    where
        Child: crate::Feature + Send + 'static,
        Child::State: Send + 'static,
        Child::Action: Send + 'static,
        K: Eq + std::hash::Hash + Clone + Send + Sync + 'static,
        C: Send + 'static,
    {
        let (declaration, site, command_site) =
            super::mount::attach_command(descriptor, mapper, super::mounted::InputClass::Ordinary);
        self.inner.mounts.push(declaration);
        (self, site, command_site)
    }

    /// Attaches independently declared control input that bypasses only input closure.
    ///
    /// Ordinary `TaskContext` and effect sends remain gated, including work made
    /// by this control reducer. Stale authority never bypasses validation.
    #[expect(
        clippy::type_complexity,
        reason = "Construction returns the builder and independently typed child and command sites"
    )]
    pub fn mount_with_control<Child, K, C>(
        mut self,
        descriptor: super::mount::OptionalMount<S, Child, K>,
        mapper: impl FnMut(C) -> Child::Action + Send + 'static,
    ) -> (
        Self,
        super::mount::MountSite<S, Child::State, Child::Action, A>,
        super::command_port::CommandSite<S, C>,
    )
    where
        Child: crate::Feature + Send + 'static,
        Child::State: Send + 'static,
        Child::Action: Send + 'static,
        K: Eq + std::hash::Hash + Clone + Send + Sync + 'static,
        C: Send + 'static,
    {
        let (declaration, site, command_site) =
            super::mount::attach_command(descriptor, mapper, super::mounted::InputClass::Control);
        self.inner.mounts.push(declaration);
        (self, site, command_site)
    }

    /// Retains the feature and configuration without creating root metadata or an actor.
    pub(crate) fn new(state: S, feature: F, runtime: Runtime) -> Self {
        Self {
            inner: StoreBuilder::new(state, feature, runtime),
            root_mount: None,
            removal_root: None,
        }
    }

    /// Retains the root lifecycle declaration for attachment after explicit declarations.
    pub(super) fn with_root_mount(
        mut self,
        mount: Box<dyn super::mount::MountDeclaration<S, A> + Send>,
    ) -> Self {
        self.root_mount = Some(mount);
        self
    }

    /// Installs the same typed deterministic frontier used by TestStore.
    ///
    /// Claims the gate only during construction. See [`StoreBuilder::transaction_gate`].
    #[cfg(feature = "test-util")]
    #[doc(hidden)]
    #[must_use]
    pub fn transaction_gate(mut self, gate: crate::testing::TransactionGate<S, A>) -> Self {
        self.inner = self.inner.transaction_gate(gate);
        self
    }

    /// Sets bounded command capacity.
    ///
    /// Uses [`StoreBuilder::mailbox_capacity`] defaults and validation timing.
    #[must_use]
    pub fn mailbox_capacity(mut self, capacity: usize) -> Self {
        self.inner = self.inner.mailbox_capacity(capacity);
        self
    }

    /// Overrides one Store-owned dependency.
    #[must_use]
    pub fn with_dependency<K: DependencyKey>(mut self, value: K::Value) -> Self {
        self.inner = self.inner.with_dependency::<K>(value);
        self
    }

    /// Observes lifecycle facts synchronously on this Feature's actor.
    ///
    /// Uses [`StoreBuilder::with_lifecycle_diagnostics`] semantics, without
    /// requiring State, Action or Feature values to implement `Debug` or `Clone`.
    #[cfg(feature = "diagnostics")]
    #[must_use]
    pub fn with_lifecycle_diagnostics(
        mut self,
        sink: impl FnMut(&crate::diagnostics::LifecycleEvent) + Send + 'static,
    ) -> Self {
        self.inner = self.inner.with_lifecycle_diagnostics(sink);
        self
    }

    /// Replaces the owned dependency environment.
    ///
    /// Uses [`StoreBuilder::with_dependency_values`] replacement and ordering semantics.
    #[must_use]
    pub fn with_dependency_values(mut self, values: DependencyValues) -> Self {
        self.inner = self.inner.with_dependency_values(values);
        self
    }

    /// Enables Store-local reporting outside internal locks.
    ///
    /// Uses [`StoreBuilder::with_issue_handler`] replacement and recorder semantics.
    #[must_use]
    pub fn with_issue_handler(
        mut self,
        handler: impl Fn(&crate::Issue) + Send + Sync + 'static,
    ) -> Self {
        self.inner = self.inner.with_issue_handler(handler);
        self
    }

    /// Validates configuration and starts the shared Store actor.
    ///
    /// # Errors
    ///
    /// Returns [`StoreBuildError`] for zero capacity or a transaction gate that is
    /// attached or reserved by unfinished work. A failed build drops its owned inputs.
    ///
    /// # Panics
    ///
    /// Propagates panics from the runtime driver or captures discarded during gate attachment.
    pub fn build(mut self) -> Result<Store<S, A>, StoreBuildError> {
        if let Some(root) = self.root_mount.take() {
            self.inner.mounts.push(root);
        }
        self.inner.build_using(|feature| Box::new(feature))
    }

    /// Uses the compile-time-checked capacity without mount declarations or test gates.
    pub(super) fn build_default(state: S, feature: F, runtime: Runtime) -> Store<S, A> {
        StoreBuilder::new(state, feature, runtime).build_with_capacity(
            DEFAULT_NONZERO_MAILBOX_CAPACITY,
            #[cfg(feature = "test-util")]
            None,
            #[cfg(feature = "test-util")]
            None,
            |feature| Box::new(feature),
        )
    }
}

impl<S, A, F> fmt::Debug for FeatureStoreBuilder<S, A, F> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.inner.fmt(f)
    }
}
