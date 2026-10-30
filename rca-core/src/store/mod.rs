//! Implements actor-backed stores.
//!
//! Store methods prepare typed commands for one bounded, ordered mailbox.
//! The actor owns state and executes reducers, queries and observation selectors.
//! Shared stores require a caller-supplied runtime. The optional Local family
//! uses the same command and transaction contracts on its owning thread.
//!
//! Store clones are owners. The last owner ends admission and requests abortive
//! cleanup. Scoped handles, observations and task endpoints do not add owners.
//! Graceful shutdown is explicit and does not drain every pending effect.

mod actor;
mod builder;
pub(crate) mod carriers;
pub(crate) mod command;
mod fold;
mod ingress;
mod lifecycle;
#[cfg(feature = "local")]
pub(crate) mod local;
#[cfg(feature = "local")]
pub(crate) use local::Local;
mod command_port;
mod mailbox;
mod mount;
pub(crate) mod mounted;
mod observation;
mod operation;
pub(crate) mod output;
pub(crate) mod reaction;
mod read;
mod receipt;
mod removal;
pub(crate) mod scope;
mod scoped;
pub(crate) mod task_context;
mod transaction;

#[cfg(all(test, feature = "allocation-measurement"))]
pub(crate) use actor::allocation_tests::{
    direct_future_launch_avoids_erased_sender_allocation,
    timer_command_avoids_discarded_reply_allocation,
};
#[cfg(all(test, not(loom), feature = "allocation-measurement"))]
pub(crate) use scoped::allocation_tests::{
    distinct_projection_publication_costs, scoped_observation_adapter_storage_costs,
};
#[cfg(all(test, feature = "allocation-measurement"))]
pub(crate) use transaction::allocation_tests::retiring_delayed_suffixes_prunes_waits_with_constant_allocation_budget;

#[cfg(all(test, feature = "allocation-measurement"))]
pub(crate) use mount::allocation_tests::mount_declaration_and_acquisition_costs;
#[cfg(all(test, feature = "allocation-measurement"))]
pub(crate) use mount::collection_costs::keyed_collection_costs;

#[cfg(all(test, feature = "allocation-measurement"))]
pub(crate) use mounted::allocation_tests::route_and_roster_costs;

#[cfg(all(test, feature = "allocation-measurement"))]
pub(crate) use mounted::allocation_tests::mounted_storage_and_work_costs;

use std::fmt;
use std::hash::Hash;
use std::sync::Arc;

use actor::Actor;
pub use builder::{DEFAULT_MAILBOX_CAPACITY, StoreBuilder};
use command::{ActionCommand, Command, CommitError, OwnedActionCommand, TerminalReply};
pub use command_port::{CommandPort, CommandReceipt, CommandSite, TryCommandError};
#[cfg(feature = "test-util")]
pub(crate) use fold::FoldCall;
#[doc(inline)]
pub use fold::{Fold, FoldKey, FoldView};
pub(crate) use ingress::Ingress;
#[cfg(feature = "test-util")]
pub(crate) use lifecycle::Lifecycle as TestLifecycle;
use lifecycle::{CloseState, Lifecycle};
#[cfg(feature = "test-util")]
pub(crate) use mount::AcquireCall;
#[doc(inline)]
pub use mount::children::MountChildren;
#[doc(inline)]
pub use mount::{EntryMount, MountIdentity, MountSite, OptionalMount, RootMount};
use observation::{new_distinct_registration, new_registration, new_take_operation};
pub use operation::Operation;
pub use output::{Output, OutputDelivery, OutputError, OutputFlow, OutputHandlers};
use read::new_read;
pub use receipt::{ActionReceipt, ActionReceiptFuture, ActionTask};
pub use removal::{RemovalSite, RemovedChild, RemovedOutputError};
#[doc(inline)]
pub use scope::{InputGate, RemovalOrigin};
pub use scoped::{ScopedStore, WeakScopedStore};

use crate::observation::ObservationSignal;
use crate::observation::wait::{
    PredicateWait, PredicateWaitOutcome, WaitCancellation, WaitTerminal, wait_for_false,
};
use crate::{
    ActionPath, CaseStatePath, Feature, KeyedAction, KeyedActionPath, KeyedState, KeyedStatePath,
    Observation, OptionalStatePath, Runtime, ScopedStoreEnded, ShutdownError, StackAction, StackId,
    StackState, StatePath, StoreStopped, TrySendError,
};

/// Sends actions to one serial feature actor.
///
/// Cloning this handle shares ownership, not state. Dropping the last owner
/// closes admission and requests abortive cleanup. [`Self::shutdown`] requests
/// graceful closure; [`Self::shutdown_into_state`] also returns the owned state.
///
/// [`Self::send`] waits for a committed action and returns its [`ActionTask`].
/// Call [`ActionTask::finish`] to wait for that action's causal work. Ordered
/// reads and observations borrow actor-owned state without requiring `State: Clone`.
/// The [`Self::ready`] future waits for structural installation.
///
/// Creating a scope-registration future prepares its projection and embedding
/// synchronously, dropping unused mutable projections and action extractors.
/// Only polling the future admits registration to the actor's mailbox.
pub struct Store<State, Action> {
    /// Shared admission endpoint; retaining it alone does not count as Store ownership.
    ingress: Arc<Ingress<State, Action>>,
    /// Counts Store owners and publishes terminal state independently of admission storage.
    lifecycle: Arc<Lifecycle>,
}

/// Checks root closure through weak metadata without adding a Store owner.
fn root_wait_terminal(lifecycle: &Arc<Lifecycle>) -> WaitTerminal {
    let lifecycle = Arc::downgrade(lifecycle);
    Box::new(move || {
        lifecycle
            .upgrade()
            .is_none_or(|root| root.is_closed())
            .then_some(PredicateWaitOutcome::StoreStopped)
    })
}

impl<State, Action> Store<State, Action> {
    /// Selects named work in this Store's permanent root scope.
    ///
    /// The metadata-only handle does not retain the Store or admit a command
    /// until one of its asynchronous methods is first polled.
    /// Use an acquired [`crate::TaskContext::operation`] for a structurally
    /// installed child; arbitrary [`ScopedStore`] projections do not carry
    /// named Feature authority.
    pub fn operation<K>(&self, key: K) -> Operation
    where
        State: Send + 'static,
        Action: Send + 'static,
        K: std::hash::Hash + Eq + Send + Sync + 'static,
    {
        Operation::root(
            Arc::downgrade(&self.ingress),
            Arc::downgrade(&self.lifecycle),
            key,
        )
    }

    /// Waits for structural installation without retaining a Store owner.
    ///
    /// # Errors
    /// Returns an error if the actor closes or fails before readiness.
    pub fn ready(
        &self,
    ) -> impl Future<Output = Result<(), crate::TaskAccessError>> + Send + 'static + use<State, Action>
    {
        Lifecycle::ready_weak(Arc::downgrade(&self.lifecycle))
    }

    /// Acquires current child authority at an ordered actor position.
    ///
    /// Construction admits nothing. First polling before readiness rejects;
    /// retained contexts never retarget after configuration or replacement.
    ///
    /// # Errors
    /// Returns an error for another root, unavailable installation or closed actor.
    pub fn acquire<S: Send + 'static, A: Send + 'static>(
        &self,
        site: &MountSite<State, S, A, Action>,
    ) -> impl Future<Output = Result<Option<crate::TaskContext<S, A>>, crate::TaskAccessError>>
    + Send
    + 'static
    + use<S, A, State, Action>
    where
        State: 'static,
        Action: 'static,
    {
        let lifecycle = Arc::downgrade(&self.lifecycle);
        let acquisition = site.acquire(self.lifecycle.root_identity());
        async move {
            // Caller metadata survives actor-owned declaration destruction and
            // distinguishes a failed actor from ordinary closure. Neither weak
            // capture retains a Store owner or a user callback across the wait.
            let root = lifecycle.upgrade().ok_or_else(|| {
                crate::TaskAccessError::new(
                    crate::TaskAccessReason::StoreClosed,
                    crate::TaskProgress::NotStarted,
                )
            })?;
            root.access()?;
            drop(root);
            acquisition.await
        }
    }

    /// Acquires parent-only input control at an ordered actor position.
    ///
    /// Closing input does not remount the child or invalidate existing handles.
    /// Replacement or configuration refresh requires explicit reacquisition.
    ///
    /// # Errors
    /// Rejects another root, an inaccessible phase or a closed actor.
    pub fn input_gate<S: Send + 'static, A: Send + 'static>(
        &self,
        site: &MountSite<State, S, A, Action>,
    ) -> impl Future<Output = Result<Option<InputGate>, crate::TaskAccessError>>
    + Send
    + 'static
    + use<State, Action, S, A>
    where
        State: 'static,
        Action: 'static,
    {
        let acquisition = self.acquire(site);
        async move { Ok(acquisition.await?.map(|context| context.input_gate())) }
    }

    /// Checks root readiness and site identity without submitting a test acquisition.
    #[cfg(feature = "test-util")]
    pub(crate) fn validate_acquisition_for_test<S, A>(
        &self,
        site: &MountSite<State, S, A, Action>,
    ) -> Result<(), crate::TaskAccessError> {
        self.lifecycle.access()?;
        site.validate(&self.lifecycle.root_identity())
    }

    /// Acquires through the test transaction gate after the ordinary root access check.
    #[cfg(feature = "test-util")]
    pub(crate) fn acquire_for_test<S: Send + 'static, A: Send + 'static>(
        &self,
        site: &MountSite<State, S, A, Action>,
        gate: crate::testing::TransactionGateCore<State, Action>,
    ) -> impl Future<Output = Result<Option<crate::TaskContext<S, A>>, crate::TaskAccessError>>
    + Send
    + 'static
    + use<S, A, State, Action>
    where
        State: 'static,
        Action: Send + 'static,
    {
        let lifecycle = Arc::downgrade(&self.lifecycle);
        let acquisition = site.acquire_for_test(self.lifecycle.root_identity(), gate);
        async move {
            let root = lifecycle.upgrade().ok_or_else(|| {
                crate::TaskAccessError::new(
                    crate::TaskAccessReason::StoreClosed,
                    crate::TaskProgress::NotStarted,
                )
            })?;
            root.access()?;
            drop(root);
            acquisition.await
        }
    }

    /// Acquires a narrow command port at an ordered actor position.
    ///
    /// Construction admits nothing. Reacquire explicitly after configuration
    /// refresh or replacement; a previously acquired port never retargets.
    /// Returns `Ok(None)` when the declaration has no current installation.
    ///
    /// # Errors
    /// Returns an error for another root, an inaccessible phase or a closed actor.
    pub fn command_port<C: Send + 'static>(
        &self,
        site: &CommandSite<State, C>,
    ) -> impl Future<Output = Result<Option<CommandPort<C>>, crate::TaskAccessError>>
    + Send
    + 'static
    + use<C, State, Action>
    where
        State: 'static,
    {
        let lifecycle = Arc::downgrade(&self.lifecycle);
        let acquisition = site.acquire(
            self.lifecycle.root_identity(),
            #[cfg(feature = "test-util")]
            None,
        );
        async move {
            let root = lifecycle.upgrade().ok_or_else(|| {
                crate::TaskAccessError::new(
                    crate::TaskAccessReason::StoreClosed,
                    crate::TaskProgress::NotStarted,
                )
            })?;
            root.access()?;
            drop(root);
            acquisition.await
        }
    }

    /// Checks root readiness and command-site identity without entering the test gate.
    #[cfg(feature = "test-util")]
    pub(crate) fn validate_command_acquisition_for_test<C>(
        &self,
        site: &CommandSite<State, C>,
    ) -> Result<(), crate::TaskAccessError> {
        self.lifecycle.access()?;
        site.validate(&self.lifecycle.root_identity())
    }

    /// Routes a command acquisition through a configured test transaction boundary.
    #[cfg(feature = "test-util")]
    pub(crate) fn command_port_for_test<C: Send + 'static>(
        &self,
        site: &CommandSite<State, C>,
        gate: crate::testing::TransactionGateCore<State, Action>,
    ) -> impl Future<Output = Result<Option<CommandPort<C>>, crate::TaskAccessError>>
    + Send
    + 'static
    + use<State, Action, C>
    where
        State: 'static,
        Action: Send + 'static,
    {
        let query: command_port::AcquireSubmit = Box::new(move |call| {
            Box::pin(async move {
                if let Some(boundary) = gate.boundary() {
                    gate.acquire(boundary, call).await;
                    None
                } else {
                    Some(call)
                }
            })
        });
        let lifecycle = Arc::downgrade(&self.lifecycle);
        let acquisition = site.acquire(self.lifecycle.root_identity(), Some(query));
        async move {
            let root = lifecycle.upgrade().ok_or_else(|| {
                crate::TaskAccessError::new(
                    crate::TaskAccessReason::StoreClosed,
                    crate::TaskProgress::NotStarted,
                )
            })?;
            root.access()?;
            drop(root);
            acquisition.await
        }
    }

    /// Creates a Store with default configuration.
    ///
    /// The runtime receives the actor before this method returns. No executor
    /// is installed. Structural installation starts on the first actor poll.
    /// [`Self::ready`] waits for installation. [`Self::builder`] configures dependencies,
    /// capacity and child declarations. Every [`crate::Reducer`] is also a [`Feature`].
    #[must_use]
    pub fn new<F>(state: State, feature: F, runtime: Runtime) -> Self
    where
        State: Send + 'static,
        Action: Send + 'static,
        F: Feature<State = State, Action = Action> + Send + 'static,
    {
        StoreBuilder::build_default(state, feature, runtime)
    }

    /// Starts configuring a Store.
    pub fn builder<F>(state: State, feature: F, runtime: Runtime) -> StoreBuilder<State, Action, F>
    where
        State: Send + 'static,
        Action: Send + 'static,
        F: Feature<State = State, Action = Action> + Send + 'static,
    {
        StoreBuilder::new(state, feature, runtime)
    }

    /// Wraps parts whose lifecycle already counts this initial Store owner.
    pub(crate) fn from_parts(
        ingress: Arc<Ingress<State, Action>>,
        lifecycle: Arc<Lifecycle>,
    ) -> Self {
        Self { ingress, lifecycle }
    }

    /// Sends an action and waits for its commit.
    ///
    /// Construction admits nothing. Dropping the future before admission drops
    /// the action. After admission, dropping it does not retract the command.
    /// The returned task tracks causal work separately from commit completion.
    ///
    /// # Errors
    ///
    /// Returns [`StoreStopped`] when Store authority ends before commit.
    pub async fn send(&self, action: Action) -> Result<ActionTask, StoreStopped> {
        send_action(&self.ingress, action, None).await
    }

    /// Waits until the current or a subsequent predicate selection is false.
    ///
    /// Registration begins on first poll. Selections coalesce to the latest
    /// state, so transient false values can be missed. Initial false completes
    /// without requiring an action. See [`PredicateWait`] for cancellation,
    /// closure, and ordinary-drop semantics.
    pub fn wait_while<F>(&self, predicate: F) -> PredicateWait
    where
        State: Send + 'static,
        Action: Send + 'static,
        F: Fn(&State) -> bool + Send + 'static,
    {
        let store = self.clone();
        let terminal = root_wait_terminal(&self.lifecycle);
        PredicateWait::new(
            async move {
                let observation = store.observe(predicate).await;
                drop(store);
                match observation {
                    Ok(observation) => wait_for_false(observation, terminal).await,
                    Err(_) => PredicateWaitOutcome::StoreStopped,
                }
            },
            None,
        )
    }

    /// Sends an action, then waits until its predicate selection is false.
    ///
    /// The action commits before predicate registration. Synchronous post-send
    /// false completes without another action; pre-send false cannot satisfy
    /// this wait. Later selections coalesce, rather than latching transitions.
    /// Explicit cancellation controls only this action's causal work, including
    /// cancellation racing admission or its receipt. Ordinary drop does not
    /// cancel admitted work. See [`PredicateWait::cancel`].
    pub fn send_and_wait_while<F>(&self, action: Action, predicate: F) -> PredicateWait
    where
        State: Send + 'static,
        Action: Send + 'static,
        F: Fn(&State) -> bool + Send + 'static,
    {
        let store = self.clone();
        let terminal = root_wait_terminal(&self.lifecycle);
        let cancellation = Arc::new(WaitCancellation::default());
        let sending = Arc::clone(&cancellation);
        PredicateWait::new(
            async move {
                if store.send_owned(action, sending).await.is_err() {
                    return PredicateWaitOutcome::StoreStopped;
                }
                let observation = store.observe(predicate).await;
                drop(store);
                match observation {
                    Ok(observation) => wait_for_false(observation, terminal).await,
                    Err(_) => PredicateWaitOutcome::StoreStopped,
                }
            },
            Some(cancellation),
        )
    }

    /// Attaches explicit predicate-wait cancellation before action admission.
    async fn send_owned(
        &self,
        action: Action,
        cancellation: Arc<WaitCancellation>,
    ) -> Result<ActionTask, StoreStopped> {
        send_action(&self.ingress, action, Some(cancellation)).await
    }

    /// Attempts immediate action admission.
    ///
    /// # Errors
    ///
    /// Retains `action` when ingress is full or stopped.
    pub fn try_send(&self, action: Action) -> Result<ActionReceipt, TrySendError<Action>> {
        try_send_action(&self.ingress, action)
    }

    /// Creates a required child handle sharing this Store's actor and mailbox.
    #[must_use]
    pub fn scope<ChildState, ChildAction, Get, GetMut, Extract, Embed>(
        &self,
        state_path: StatePath<State, ChildState, Get, GetMut>,
        action_path: ActionPath<Action, ChildAction, Extract, Embed>,
    ) -> ScopedStore<ChildState, ChildAction>
    where
        State: Send + 'static,
        Action: Send + 'static,
        ChildState: Send + 'static,
        ChildAction: Send + 'static,
        Get: for<'a> Fn(&'a State) -> &'a ChildState + Send + Sync + 'static,
        Embed: Fn(ChildAction) -> Action + Send + Sync + 'static,
    {
        scoped::scope(self, state_path, action_path)
    }

    /// Creates a replacement-aware required child handle.
    ///
    /// # Errors
    ///
    /// Returns [`ScopedStoreEnded`] when Store authority ends before registration.
    pub fn scope_identified<ChildState, ChildAction, Get, GetMut, Extract, Embed, Identity, Id>(
        &self,
        state_path: StatePath<State, ChildState, Get, GetMut>,
        action_path: ActionPath<Action, ChildAction, Extract, Embed>,
        identity: Identity,
    ) -> impl Future<Output = Result<ScopedStore<ChildState, ChildAction>, ScopedStoreEnded>> + Send + '_
    where
        State: Send + 'static,
        Action: Send + 'static,
        ChildState: Send + 'static,
        ChildAction: Send + 'static,
        Get: for<'a> Fn(&'a State) -> &'a ChildState + Send + 'static,
        Embed: Fn(ChildAction) -> Action + Send + Sync + 'static,
        Identity: for<'a> Fn(&'a ChildState) -> &'a Id + Send + 'static,
        Id: Eq + Hash + Clone + Send + Sync + 'static,
    {
        scoped::scope_identified(self, state_path, action_path, identity)
    }

    /// Creates a handle for the current optional-child lifetime.
    ///
    /// # Errors
    ///
    /// Returns [`ScopedStoreEnded`] when the child is absent or Store authority ends.
    pub fn scope_some<ChildState, ChildAction, Get, GetMut, Extract, Embed>(
        &self,
        state_path: OptionalStatePath<State, ChildState, Get, GetMut>,
        action_path: ActionPath<Action, ChildAction, Extract, Embed>,
    ) -> impl Future<Output = Result<ScopedStore<ChildState, ChildAction>, ScopedStoreEnded>> + Send + '_
    where
        State: Send + 'static,
        Action: Send + 'static,
        ChildState: Send + 'static,
        ChildAction: Send + 'static,
        Get: for<'a> Fn(&'a State) -> Option<&'a ChildState> + Send + 'static,
        Embed: Fn(ChildAction) -> Action + Send + Sync + 'static,
    {
        scoped::scope_some(self, state_path, action_path)
    }

    /// Creates an identity-aware optional-child handle.
    ///
    /// # Errors
    ///
    /// Returns [`ScopedStoreEnded`] when the child is absent or Store authority ends.
    pub fn scope_some_identified<
        ChildState,
        ChildAction,
        Get,
        GetMut,
        Extract,
        Embed,
        Identity,
        Id,
    >(
        &self,
        state_path: OptionalStatePath<State, ChildState, Get, GetMut>,
        action_path: ActionPath<Action, ChildAction, Extract, Embed>,
        identity: Identity,
    ) -> impl Future<Output = Result<ScopedStore<ChildState, ChildAction>, ScopedStoreEnded>> + Send + '_
    where
        State: Send + 'static,
        Action: Send + 'static,
        ChildState: Send + 'static,
        ChildAction: Send + 'static,
        Get: for<'a> Fn(&'a State) -> Option<&'a ChildState> + Send + 'static,
        Embed: Fn(ChildAction) -> Action + Send + Sync + 'static,
        Identity: for<'a> Fn(&'a ChildState) -> &'a Id + Send + 'static,
        Id: Eq + Hash + Clone + Send + Sync + 'static,
    {
        scoped::scope_some_identified(self, state_path, action_path, identity)
    }

    /// Creates a handle for the current matching enum-case lifetime.
    ///
    /// # Errors
    ///
    /// Returns [`ScopedStoreEnded`] when the case is absent or Store authority ends.
    pub fn scope_case<ChildState, ChildAction, Get, GetMut, Extract, Embed>(
        &self,
        state_path: CaseStatePath<State, ChildState, Get, GetMut>,
        action_path: ActionPath<Action, ChildAction, Extract, Embed>,
    ) -> impl Future<Output = Result<ScopedStore<ChildState, ChildAction>, ScopedStoreEnded>> + Send + '_
    where
        State: Send + 'static,
        Action: Send + 'static,
        ChildState: Send + 'static,
        ChildAction: Send + 'static,
        Get: for<'a> Fn(&'a State) -> Option<&'a ChildState> + Send + 'static,
        Embed: Fn(ChildAction) -> Action + Send + Sync + 'static,
    {
        scoped::scope_case(self, state_path, action_path)
    }

    /// Creates an identity-aware matching enum-case handle.
    ///
    /// # Errors
    ///
    /// Returns [`ScopedStoreEnded`] when the case is absent or Store authority ends.
    pub fn scope_case_identified<
        ChildState,
        ChildAction,
        Get,
        GetMut,
        Extract,
        Embed,
        Identity,
        Id,
    >(
        &self,
        state_path: CaseStatePath<State, ChildState, Get, GetMut>,
        action_path: ActionPath<Action, ChildAction, Extract, Embed>,
        identity: Identity,
    ) -> impl Future<Output = Result<ScopedStore<ChildState, ChildAction>, ScopedStoreEnded>> + Send + '_
    where
        State: Send + 'static,
        Action: Send + 'static,
        ChildState: Send + 'static,
        ChildAction: Send + 'static,
        Get: for<'a> Fn(&'a State) -> Option<&'a ChildState> + Send + 'static,
        Embed: Fn(ChildAction) -> Action + Send + Sync + 'static,
        Identity: for<'a> Fn(&'a ChildState) -> &'a Id + Send + 'static,
        Id: Eq + Hash + Clone + Send + Sync + 'static,
    {
        scoped::scope_case_identified(self, state_path, action_path, identity)
    }

    /// Creates a handle for one current keyed-child lifetime.
    ///
    /// # Errors
    ///
    /// Returns [`ScopedStoreEnded`] when the key is absent or Store authority ends.
    pub fn scope_keyed<ChildState, ChildAction, Get, GetMut, Extract, Embed, Id>(
        &self,
        state_path: KeyedStatePath<State, Id, ChildState, Get, GetMut>,
        action_path: KeyedActionPath<Action, Id, ChildAction, Extract, Embed>,
        id: Id,
    ) -> impl Future<Output = Result<ScopedStore<ChildState, ChildAction>, ScopedStoreEnded>> + Send + '_
    where
        State: Send + 'static,
        Action: Send + 'static,
        ChildState: Send + 'static,
        ChildAction: Send + 'static,
        Get: for<'a> Fn(&'a State) -> &'a KeyedState<Id, ChildState> + Send + 'static,
        Embed: Fn(KeyedAction<Id, ChildAction>) -> Action + Send + Sync + 'static,
        Id: Eq + Hash + Clone + Send + Sync + 'static,
    {
        scoped::scope_keyed(self, state_path, action_path, id)
    }

    /// Creates a handle for one current stack-entry lifetime.
    ///
    /// Removing and reinserting the same logical ID does not retarget this
    /// handle. Unused mutable projections and extractors are dropped before
    /// the registration future is returned.
    ///
    /// # Errors
    ///
    /// Returns [`ScopedStoreEnded`] when the entry is absent or Store authority ends.
    pub fn scope_stack<ChildState, ChildAction, Push, Get, GetMut, Extract, Embed>(
        &self,
        state_path: StatePath<State, StackState<ChildState>, Get, GetMut>,
        action_path: ActionPath<Action, StackAction<Push, ChildAction>, Extract, Embed>,
        id: StackId,
    ) -> impl Future<Output = Result<ScopedStore<ChildState, ChildAction>, ScopedStoreEnded>> + Send + '_
    where
        State: Send + 'static,
        Action: Send + 'static,
        ChildState: Send + 'static,
        ChildAction: Send + 'static,
        Get: for<'a> Fn(&'a State) -> &'a StackState<ChildState> + Send + 'static,
        Embed: Fn(StackAction<Push, ChildAction>) -> Action + Send + Sync + 'static,
    {
        scoped::scope_stack(self, state_path, action_path, id)
    }

    /// Runs a query at its ordered mailbox position.
    ///
    /// Construction admits nothing. Before admission, waiter cancellation drops
    /// the query. After admission, waiter cancellation only discards the reply.
    ///
    /// # Errors
    ///
    /// Returns [`StoreStopped`] if admission fails or the reply channel closes empty.
    pub async fn read<F, Output>(&self, query: F) -> Result<Output, StoreStopped>
    where
        Action: Send + 'static,
        F: FnOnce(&State) -> Output + Send + 'static,
        Output: Send + 'static,
    {
        let (command, receive) = new_read(query);
        ordered_reply(&self.ingress, Command::Read(command), receive).await
    }

    /// Registers a clone-free projection at its ordered mailbox position.
    ///
    /// # Errors
    ///
    /// Returns [`StoreStopped`] when Store authority ends before registration.
    pub async fn observe<F, Value>(&self, selector: F) -> Result<Observation<Value>, StoreStopped>
    where
        State: 'static,
        Action: Send + 'static,
        F: Fn(&State) -> Value + Send + 'static,
        Value: Send + 'static,
    {
        register_observation(
            &self.ingress,
            move |signal| new_registration(selector, signal),
            |registration, signal| {
                Observation::new(
                    registration.id,
                    signal,
                    registration.initial,
                    registration.version,
                    new_take_operation(Arc::downgrade(&self.ingress), registration.id),
                )
            },
        )
        .await
    }

    /// Registers a distinct projection at its ordered mailbox position.
    ///
    /// The initial value is emitted immediately. Later commits emit only when
    /// the projected value differs from the last published value.
    ///
    /// # Errors
    ///
    /// Returns [`StoreStopped`] when Store authority ends before registration.
    pub async fn observe_distinct<F, Value>(
        &self,
        selector: F,
    ) -> Result<Observation<Value>, StoreStopped>
    where
        State: 'static,
        Action: Send + 'static,
        F: Fn(&State) -> Value + Send + 'static,
        Value: Clone + PartialEq + Send + 'static,
    {
        register_observation(
            &self.ingress,
            move |signal| new_distinct_registration(selector, signal),
            |registration, signal| {
                Observation::new(
                    registration.id,
                    signal,
                    registration.initial,
                    registration.version,
                    new_take_operation(Arc::downgrade(&self.ingress), registration.id),
                )
            },
        )
        .await
    }

    /// Performs ordered graceful shutdown.
    ///
    /// Shutdown invalidates causal work and issues task aborts before returning,
    /// but does not wait for every runtime task wrapper to be cooperatively
    /// polled to completion. A future that spins forever in one poll can starve
    /// shutdown on a current-thread runtime.
    ///
    /// # Errors
    ///
    /// Returns [`ShutdownError`] when authority already ended abortively.
    pub async fn shutdown(&self) -> Result<(), ShutdownError>
    where
        Action: Send + 'static,
    {
        shutdown(&self.ingress, &self.lifecycle).await
    }

    /// Shuts down the Store and returns its owned state after actor cleanup.
    ///
    /// The first terminal command processed wins, even when other owning handles
    /// exist. Earlier commands follow existing commit rules; later commands are
    /// discarded. Effects are cancelled, not drained. Cleanup issues aborts
    /// without waiting for every cooperative runtime task wrapper to finish.
    ///
    /// The future retains this owner until completion or cancellation. Creating
    /// it submits nothing. Dropping it before admission drops only that owner.
    /// If this was the final owner, ordinary abortive cleanup applies.
    /// After admission, abandoning the receiver does not retract the command.
    /// With a surviving owner, it still closes the Store and drops unclaimed
    /// state once. Final-owner or host loss can instead cause abortive cleanup.
    ///
    /// Pending effects and timers are cancelled. Observations and scoped handles
    /// close. If the result needs completed effects, establish job completion
    /// before extraction. Ordinary [`Self::shutdown`] still drops state before
    /// acknowledgement and remains idempotent after graceful closure.
    ///
    /// # Examples
    ///
    /// A finite job returns its non-Clone product without replacing actor state.
    /// The caller awaits the root causal task before requesting extraction.
    ///
    /// ```
    /// use rca_core::{Effect, ReduceContext, Reducer, Store};
    /// # mod support {
    /// #     include!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/support/mod.rs"));
    /// # }
    /// # let host = support::ManualRuntime::new();
    /// # let runtime = host.runtime();
    /// struct Product(Box<[u8]>);
    /// struct Build;
    /// impl Reducer for Build {
    ///     type State = Product;
    ///     type Action = ();
    ///     fn reduce(
    ///         &mut self,
    ///         state: &mut Product,
    ///         _: &(),
    ///         _: &mut ReduceContext<'_>,
    ///     ) -> Effect<()> {
    ///         for byte in &mut state.0 { *byte *= 2; }
    ///         Effect::none()
    ///     }
    /// }
    /// let product = Product(vec![1, 2, 3].into_boxed_slice());
    /// let allocation = product.0.as_ptr();
    /// let store = Store::new(product, Build, runtime);
    /// # host.start_all();
    /// # futures_executor::block_on(async {
    /// store.send(()).await?.finish().await;
    /// let product = store.shutdown_into_state().await?;
    /// assert_eq!(&*product.0, &[2, 4, 6]);
    /// assert_eq!(product.0.as_ptr(), allocation);
    /// # Ok::<(), rca_core::StoreStopped>(())
    /// # }).unwrap();
    /// # for result in host.join_all() { result.unwrap(); }
    /// ```
    ///
    /// # Errors
    ///
    /// Returns [`StoreStopped`] if another terminal command, host loss, or
    /// abortive closure prevents the handoff. Extraction is not idempotent.
    #[expect(
        clippy::manual_async_fn,
        reason = "the public contract explicitly guarantees a Send and static future"
    )]
    pub fn shutdown_into_state(
        self,
    ) -> impl std::future::Future<Output = Result<State, StoreStopped>> + Send + 'static
    where
        State: Send + 'static,
        Action: Send + 'static,
    {
        async move {
            let (reply, receive) = async_channel::bounded(1);
            self.ingress
                .send(Command::Shutdown(TerminalReply::<
                    <carriers::Shared as carriers::Carriers<State, Action>>::Terminal,
                >::State(Box::new(
                    move |state| {
                        let _ = reply.try_send(state);
                    },
                ))))
                .await
                .map_err(|_closed| StoreStopped::new())?;
            let result = receive.recv().await.map_err(|_closed| StoreStopped::new());
            drop(self);
            result
        }
    }

    /// Returns whether Store authority ended.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.lifecycle.is_closed()
    }

    /// Waits until Store authority ends without retaining it.
    pub fn closed(&self) -> impl Future<Output = ()> + Send + 'static + use<State, Action> {
        Lifecycle::wait_weak(Arc::downgrade(&self.lifecycle))
    }

    /// Returns ordered actor diagnostics for deterministic testing.
    ///
    /// # Errors
    ///
    /// Returns [`StoreStopped`] if admission fails or the actor drops its reply.
    #[cfg(feature = "test-util")]
    #[doc(hidden)]
    pub async fn runtime_snapshot(&self) -> Result<crate::testing::RuntimeSnapshot, StoreStopped>
    where
        Action: Send + 'static,
    {
        let (reply, receive) = async_channel::bounded(1);
        self.ingress
            .send(Command::Snapshot(reply))
            .await
            .map_err(|_closed| StoreStopped::new())?;
        receive
            .recv()
            .await
            .map(crate::testing::ActorDiagnostics::into_runtime)
            .map_err(|_closed| StoreStopped::new())
    }

    /// Returns ordered actor diagnostics for deterministic testing.
    ///
    /// # Errors
    ///
    /// Returns [`StoreStopped`] if admission fails or the actor drops its reply.
    #[cfg(feature = "test-util")]
    #[doc(hidden)]
    pub async fn runtime_diagnostics(
        &self,
    ) -> Result<crate::testing::ActorDiagnostics, StoreStopped>
    where
        Action: Send + 'static,
    {
        let (reply, receive) = async_channel::bounded(1);
        self.ingress
            .send(Command::Snapshot(reply))
            .await
            .map_err(|_closed| StoreStopped::new())?;
        receive.recv().await.map_err(|_closed| StoreStopped::new())
    }

    /// Ends Store authority for deterministic test teardown.
    #[cfg(feature = "test-util")]
    #[doc(hidden)]
    pub fn abort_for_testing(&self) {
        self.lifecycle.abort(&self.ingress);
    }
}

/// Admits one owned action and waits for its commit, not its causal completion.
///
/// The optional cancellation handle follows the command through admission.
/// Losing the receipt does not retract an admitted action.
async fn send_action<S, A, C: carriers::Carriers<S, A>>(
    ingress: &ingress::CommandIngress<Command<S, A, C>>,
    action: A,
    cancellation: Option<Arc<WaitCancellation>>,
) -> Result<ActionTask, StoreStopped> {
    let (reply, receive) = command::commit_channel();
    let command = ActionCommand::external(action, reply);
    let command = match cancellation {
        Some(cancellation) => Command::OwnedAction(Box::new(OwnedActionCommand {
            command,
            cancellation,
        })),
        None => Command::Action(command),
    };
    ingress.send(command).await.map_err(|closed| {
        let error = StoreStopped::new();
        drop(closed);
        error
    })?;
    match receive.await {
        Ok(Ok(task)) => Ok(task),
        Ok(Err(CommitError::Cancelled | CommitError::ScopeEnded | CommitError::InputClosed))
        | Err(_) => Err(StoreStopped::new()),
    }
}

/// Preserves the original action when immediate typed admission fails.
fn try_send_action<S, A, C: carriers::Carriers<S, A>>(
    ingress: &ingress::CommandIngress<Command<S, A, C>>,
    action: A,
) -> Result<ActionReceipt, TrySendError<A>> {
    let (reply, receive) = command::commit_channel();
    match ingress.try_send_map((action, reply), |(action, reply)| {
        Command::Action(ActionCommand::external(action, reply))
    }) {
        Ok(()) => Ok(ActionReceipt::new(receive)),
        Err(async_channel::TrySendError::Full((action, _))) => Err(TrySendError::full(action)),
        Err(async_channel::TrySendError::Closed((action, _))) => {
            Err(TrySendError::store_stopped(action))
        }
    }
}

/// Admits a command before polling its reply, normalising either closure to `StoreStopped`.
async fn ordered_reply<S, A, C: carriers::Carriers<S, A>, T, E>(
    ingress: &ingress::CommandIngress<Command<S, A, C>>,
    command: Command<S, A, C>,
    receive: impl Future<Output = Result<T, E>>,
) -> Result<T, StoreStopped> {
    ingress.send(command).await.map_err(|closed| {
        let error = StoreStopped::new();
        drop(closed);
        error
    })?;
    receive.await.map_err(|_closed| StoreStopped::new())
}

/// Requests ordered shutdown, accepting an already completed graceful closure.
///
/// A concurrent terminal command can still win after the initial state checks.
async fn shutdown<S, A, C: carriers::Carriers<S, A>>(
    ingress: &ingress::CommandIngress<Command<S, A, C>>,
    lifecycle: &Lifecycle,
) -> Result<(), ShutdownError> {
    if lifecycle.was_graceful() {
        return Ok(());
    }
    if lifecycle.is_closed() {
        return Err(ShutdownError::store_stopped());
    }
    let (reply, receive) = async_channel::bounded(1);
    ingress
        .send(Command::Shutdown(TerminalReply::Shutdown(reply)))
        .await
        .map_err(|_closed| ShutdownError::store_stopped())?;
    receive
        .recv()
        .await
        .unwrap_or_else(|_closed| Err(ShutdownError::store_stopped()))
}

/// Transfers observation cancellation from a registration guard to its returned handle.
///
/// The guard retires registration on failed admission, lost reply or cancellation.
/// It stays armed through handle construction so a construction panic also retires it.
async fn register_observation<S, A, C: carriers::Carriers<S, A>, V, O>(
    ingress: &Arc<ingress::CommandIngress<Command<S, A, C>>>,
    prepare: impl FnOnce(
        Arc<ObservationSignal>,
    ) -> (
        Box<C::ObservationCommand>,
        futures_channel::oneshot::Receiver<observation::RegistrationReply<V>>,
    ),
    finish: impl FnOnce(observation::RegistrationReply<V>, Arc<ObservationSignal>) -> O,
) -> Result<O, StoreStopped> {
    let signal = ObservationSignal::new(Arc::downgrade(&ingress.control));
    let mut guard = RegistrationGuard::new(Arc::clone(&signal));
    let (command, receive) = prepare(Arc::clone(&signal));
    ingress
        .send(Command::Observation(command))
        .await
        .map_err(|closed| {
            let error = StoreStopped::new();
            drop(closed);
            error
        })?;
    let registration = receive.await.map_err(|_closed| StoreStopped::new())?;
    let observation = finish(registration, signal);
    guard.disarm();
    Ok(observation)
}

/// Retires an unfinished observation registration when its waiter disappears.
struct RegistrationGuard {
    /// Shared liveness flag used by the actor and the eventual observation handle.
    signal: Arc<ObservationSignal>,
    /// Remains true until a returned observation takes responsibility for retirement.
    armed: bool,
}

impl RegistrationGuard {
    /// Takes responsibility for retiring the new registration on early exit.
    fn new(signal: Arc<ObservationSignal>) -> Self {
        Self {
            signal,
            armed: true,
        }
    }

    /// Transfers retirement responsibility after handle construction succeeds.
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for RegistrationGuard {
    fn drop(&mut self) {
        if self.armed {
            self.signal.deactivate();
        }
    }
}

impl<State, Action> Clone for Store<State, Action> {
    fn clone(&self) -> Self {
        self.lifecycle.add_owner();
        Self {
            ingress: Arc::clone(&self.ingress),
            lifecycle: Arc::clone(&self.lifecycle),
        }
    }
}

impl<State, Action> Drop for Store<State, Action> {
    fn drop(&mut self) {
        if self.lifecycle.drop_owner() {
            self.lifecycle.abort(&self.ingress);
        }
    }
}

impl<State, Action> fmt::Debug for Store<State, Action> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Store")
            .field("closed", &self.is_closed())
            .finish_non_exhaustive()
    }
}
