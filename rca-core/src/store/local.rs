//! Owner-local authoring and Store frontends for the common actor engine.
//!
//! Local callbacks, actions, state and futures need not implement `Send` or `Sync`.
//! The caller's runtime polls and drops them on its owner thread. The common
//! kernel still owns admission, scope authority, cancellation and transaction order.
//! Dependency values, identity keys and synchronous issue handlers retain their
//! portable contracts. Local execution does not make blocking callbacks pre-emptible.

use std::pin::Pin;
use std::rc::Rc;
use std::sync::Weak;

use futures_util::future::AbortHandle;

use super::carriers::{Carriers, EffectLeaf, Execution};
use super::command::{Command, CommitReceiver, Provenance};
use super::ingress::CommandIngress;
use super::scope::{ScopeAuthority, ScopeToken};
use super::task_context::{ReadCall, ReadInput, UpdateCall};
use super::transaction::{EffectAdmission, admit_effect_action};
use crate::effect::leaves::{EffectLeaves, EffectValue};
use crate::effect::registry::{PreparedTask, TaskRegistry};
use crate::effect::{CancellationIdentity, EffectSendFuture, Program};
use crate::feature::{ReduceDriver, Work, WorkItem};
use crate::{EffectSendError, ReduceContext};

pub use super::command_port::local::{CommandPort, CommandReceipt, CommandSite, TryCommandError};
pub use super::fold::local::{Fold, FoldView};
pub use super::mount::local::children::MountChildren;
pub use super::mount::local::{
    EntryMount, LocalMountSite as MountSite, LocalOptionalMount as OptionalMount, RootMount,
};
pub use super::operation::local::Operation;
pub use super::output::local::{Output, OutputHandlers};

#[doc(inline)]
pub use super::removal::local::{RemovalSite, RemovedChild, RemovedOutputError};
pub(super) use super::task_context::local::LocalReply;
pub use super::task_context::local::{Task, TaskContext};

/// Owned runtime work that can retain owner-thread captures across suspension.
pub(super) type LocalFuture = Pin<Box<dyn Future<Output = ()>>>;

/// Suspends full-mailbox admission before returning the existing commit receiver.
pub(super) type AdmissionFuture =
    Pin<Box<dyn Future<Output = Result<CommitReceiver, EffectSendError>>>>;

/// Uses immediate admission outcomes without allocating a waiting future.
pub(super) type SendFuture = EffectSendFuture<AdmissionFuture>;

#[cfg(feature = "local")]
pub use frontend::{Store, StoreBuilder};

#[cfg(feature = "local")]
mod frontend {
    //! Connects local public handles and builders to the common actor and lifecycle.

    use std::fmt;
    use std::marker::PhantomData;
    use std::num::NonZeroUsize;
    use std::rc::Rc;
    use std::sync::Arc;

    use super::{Command, CommandIngress, Driver, Feature, Local};
    use crate::runtime::local::Runtime;
    use crate::store::command::TerminalReply;
    use crate::store::lifecycle::Lifecycle;
    use crate::{
        ActionReceipt, ActionTask, DependencyKey, DependencyValues, ShutdownError, StoreBuildError,
        StoreStopped, TrySendError,
    };

    /// Mailbox admission restricted by the local command payload types.
    type Ingress<S, A> = CommandIngress<Command<S, A, Local>>;

    /// Sends actions to one owner-thread feature actor.
    ///
    /// Clones share the actor and each retain logical Store ownership. Dropping
    /// the final owner closes authority; the host must still dispose of its tasks.
    /// State and actions need neither cloning nor thread-safe sharing.
    pub struct Store<S, A> {
        /// Command endpoint; retained ingress alone does not preserve Store authority.
        ingress: Arc<Ingress<S, A>>,
        /// Shared readiness, terminal status and logical owner count.
        lifecycle: Arc<Lifecycle>,
    }

    impl<S, A> Store<S, A> {
        /// Creates an owner-local feature Store with default configuration.
        ///
        /// # Panics
        ///
        /// Propagates a panic from the caller-supplied runtime driver.
        pub fn new<F: Feature<State = S, Action = A> + 'static>(
            state: S,
            feature: F,
            runtime: Runtime,
        ) -> Self
        where
            S: 'static,
            A: 'static,
        {
            StoreBuilder::new(state, feature, runtime).build_with_capacity(
                crate::store::builder::DEFAULT_NONZERO_MAILBOX_CAPACITY,
                #[cfg(feature = "test-util")]
                None,
            )
        }

        /// Starts configuration for an owner-local feature Store.
        pub fn builder<F: Feature<State = S, Action = A> + 'static>(
            state: S,
            feature: F,
            runtime: Runtime,
        ) -> StoreBuilder<S, A, F>
        where
            S: 'static,
            A: 'static,
        {
            StoreBuilder::new(state, feature, runtime)
        }

        /// Waits for installation without retaining a Store owner.
        ///
        /// # Errors
        ///
        /// Returns an error if the actor closes or fails before readiness.
        pub fn ready(
            &self,
        ) -> impl Future<Output = Result<(), crate::TaskAccessError>> + 'static + use<S, A>
        {
            Lifecycle::ready_weak(Arc::downgrade(&self.lifecycle))
        }

        /// Returns a weak named-operation view for this Store's root scope.
        ///
        /// Construction is inert. Requests are ordered when first polled and do
        /// not select, borrow or clone user state.
        pub fn operation<K>(&self, key: K) -> super::Operation
        where
            S: 'static,
            A: 'static,
            K: std::hash::Hash + Eq + Send + Sync + 'static,
        {
            super::Operation::root(
                Arc::downgrade(&self.ingress),
                Arc::downgrade(&self.lifecycle),
                key,
            )
        }

        /// Folds committed contributions at an ordered, non-mutating actor position.
        ///
        /// Construction is inert. First polling enqueues the read. A callback may
        /// first-poll another read, which runs after the current read finishes;
        /// synchronously blocking on it inside the actor is unsupported.
        /// Input admission gates do not remove contributions.
        ///
        /// # Errors
        ///
        /// Returns readiness, closure or authority failures with conservative progress.
        ///
        /// # Panics
        ///
        /// User callbacks may panic. The actor then fails and publishes a terminal error.
        pub fn fold<K: crate::FoldKey>(
            &self,
            fold: &super::Fold<S, K>,
        ) -> impl Future<Output = Result<K::Value, crate::TaskAccessError>> + 'static + use<S, A, K>
        where
            S: 'static,
            A: 'static,
        {
            crate::store::fold::local::read(
                fold.clone(),
                Arc::downgrade(&self.ingress),
                Arc::downgrade(&self.lifecycle),
            )
        }

        /// Checks root readiness without evaluating a fold or releasing the test frontier.
        #[cfg(feature = "test-util")]
        pub(crate) fn validate_fold_for_test(&self) -> Result<(), crate::TaskAccessError> {
            self.lifecycle.access()
        }

        /// Reads committed fold values through the held deterministic frontier.
        #[cfg(feature = "test-util")]
        pub(crate) fn fold_for_test<K: crate::FoldKey>(
            &self,
            fold: &super::Fold<S, K>,
            gate: crate::testing::TransactionGateCore<S, A, Local>,
        ) -> impl Future<Output = Result<K::Value, crate::TaskAccessError>> + 'static + use<S, A, K>
        where
            S: 'static,
            A: 'static,
        {
            crate::store::fold::local::read_for_test(
                fold.clone(),
                Arc::downgrade(&self.ingress),
                Arc::downgrade(&self.lifecycle),
                self.lifecycle.root_identity(),
                gate,
            )
        }

        /// Acquires fixed subtree authority through the held deterministic frontier.
        #[cfg(feature = "test-util")]
        pub(crate) fn acquire_fold_for_test<K: crate::FoldKey, CS: 'static, CA: 'static>(
            &self,
            fold: &super::Fold<S, K>,
            site: &super::MountSite<S, CS, CA, A>,
            gate: crate::testing::TransactionGateCore<S, A, Local>,
        ) -> impl Future<Output = Result<Option<super::FoldView<K>>, crate::TaskAccessError>>
        + 'static
        + use<S, A, K, CS, CA>
        where
            S: 'static,
            A: 'static,
        {
            crate::store::fold::local::acquire_for_test(
                fold.clone(),
                site.fold_registration(),
                Arc::downgrade(&self.ingress),
                Arc::downgrade(&self.lifecycle),
                gate,
            )
        }

        /// Reads an acquired fold after checking that it belongs to this root.
        #[cfg(feature = "test-util")]
        pub(crate) fn fold_view_for_test<K: crate::FoldKey>(
            &self,
            view: &super::FoldView<K>,
        ) -> impl Future<Output = Result<K::Value, crate::TaskAccessError>> + 'static + use<S, A, K>
        {
            view.read_for_test(self.lifecycle.root_identity())
        }

        /// Captures an installed subtree without evaluating contributions or selectors.
        ///
        /// Returns `None` for an absent installation in this Store. The view
        /// retains fixed authority and cannot retarget a later incarnation.
        ///
        /// # Errors
        ///
        /// Rejects another root, pre-readiness access, retired declarations and closed actors.
        pub fn acquire_fold<K: crate::FoldKey, CS: 'static, CA: 'static>(
            &self,
            fold: &super::Fold<S, K>,
            site: &super::MountSite<S, CS, CA, A>,
        ) -> impl Future<Output = Result<Option<super::FoldView<K>>, crate::TaskAccessError>>
        + 'static
        + use<S, A, K, CS, CA>
        where
            S: 'static,
            A: 'static,
        {
            crate::store::fold::local::acquire(
                fold.clone(),
                site.fold_registration(),
                Arc::downgrade(&self.ingress),
                Arc::downgrade(&self.lifecycle),
            )
        }

        /// Acquires the currently installed child without retaining this Store or site.
        ///
        /// # Errors
        ///
        /// Rejects foreign roots, pre-readiness access, closed actors and retired declarations.
        pub fn acquire<CS: 'static, CA: 'static>(
            &self,
            site: &super::MountSite<S, CS, CA, A>,
        ) -> impl Future<
            Output = Result<Option<super::TaskContext<CS, CA>>, crate::TaskAccessError>,
        >
        + 'static
        + use<S, A, CS, CA>
        where
            S: 'static,
            A: 'static,
        {
            let lifecycle = Arc::downgrade(&self.lifecycle);
            let acquisition = site.acquire(self.lifecycle.root_identity());
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

        /// Acquires an owner-local command port at its ordered actor position.
        ///
        /// # Errors
        ///
        /// Rejects foreign roots, inaccessible actors and retired declarations.
        pub fn command_port<C: 'static>(
            &self,
            site: &super::CommandSite<S, C>,
        ) -> impl Future<Output = Result<Option<super::CommandPort<C>>, crate::TaskAccessError>>
        + 'static
        + use<S, A, C>
        where
            S: 'static,
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

        /// Rejects inaccessible roots and foreign command sites before test acquisition.
        #[cfg(feature = "test-util")]
        pub(crate) fn validate_command_acquisition_for_test<C>(
            &self,
            site: &super::CommandSite<S, C>,
        ) -> Result<(), crate::TaskAccessError> {
            self.lifecycle.access()?;
            site.validate(&self.lifecycle.root_identity())
        }

        /// Routes a command acquisition through the test frontier when one is held.
        #[cfg(feature = "test-util")]
        pub(crate) fn command_port_for_test<C: 'static>(
            &self,
            site: &super::CommandSite<S, C>,
            gate: crate::testing::TransactionGateCore<S, A, Local>,
        ) -> impl Future<Output = Result<Option<super::CommandPort<C>>, crate::TaskAccessError>>
        + 'static
        + use<S, A, C>
        where
            S: 'static,
            A: 'static,
        {
            let query: crate::store::command_port::local::AcquireSubmit = Box::new(move |call| {
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

        /// Acquires parent-only input control without retaining this Store or site.
        ///
        /// Closing input preserves the installation and its live read, update,
        /// output and fold authority. Replacement requires explicit reacquisition.
        ///
        /// # Errors
        ///
        /// Rejects foreign roots, pre-readiness access and closed actors.
        pub fn input_gate<CS: 'static, CA: 'static>(
            &self,
            site: &super::MountSite<S, CS, CA, A>,
        ) -> impl Future<Output = Result<Option<crate::InputGate>, crate::TaskAccessError>>
        + 'static
        + use<S, A, CS, CA>
        where
            S: 'static,
            A: 'static,
        {
            let acquisition = self.acquire(site);
            async move { Ok(acquisition.await?.map(|context| context.input_gate())) }
        }

        /// Rejects inaccessible roots and foreign mount sites before test acquisition.
        #[cfg(feature = "test-util")]
        pub(crate) fn validate_acquisition_for_test<CS, CA>(
            &self,
            site: &super::MountSite<S, CS, CA, A>,
        ) -> Result<(), crate::TaskAccessError> {
            self.lifecycle.access()?;
            site.validate(&self.lifecycle.root_identity())
        }

        /// Acquires a child through the test frontier without retaining a Store owner.
        #[cfg(feature = "test-util")]
        pub(crate) fn acquire_for_test<CS: 'static, CA: 'static>(
            &self,
            site: &super::MountSite<S, CS, CA, A>,
            gate: crate::testing::TransactionGateCore<S, A, Local>,
        ) -> impl Future<
            Output = Result<Option<super::TaskContext<CS, CA>>, crate::TaskAccessError>,
        >
        + 'static
        + use<S, A, CS, CA>
        where
            S: 'static,
            A: 'static,
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

        /// Sends an action and waits for its commit.
        ///
        /// Admission starts on first poll. Dropping the waiter after admission
        /// discards its reply without retracting the action or its causal work.
        ///
        /// # Errors
        ///
        /// Returns an error if Store authority ends before commit.
        pub async fn send(&self, action: A) -> Result<ActionTask, StoreStopped> {
            crate::store::send_action(&self.ingress, action, None).await
        }

        /// Attempts immediate admission, retaining the action on failure.
        ///
        /// # Errors
        ///
        /// Returns the action when ingress is full or stopped.
        pub fn try_send(&self, action: A) -> Result<ActionReceipt, TrySendError<A>> {
            crate::store::try_send_action(&self.ingress, action)
        }

        /// Runs an owner-local query at an ordered actor position.
        ///
        /// Construction admits nothing. Before admission, waiter cancellation drops
        /// the query. After admission, waiter cancellation only discards the reply.
        /// Captures and results remain on the owner thread.
        ///
        /// # Errors
        ///
        /// Returns an error if admission fails or the reply channel closes empty.
        pub async fn read<F, T>(&self, query: F) -> Result<T, StoreStopped>
        where
            F: FnOnce(&S) -> T + 'static,
            T: 'static,
        {
            let (command, receive) = crate::store::read::new_local_read(query);
            crate::store::ordered_reply(&self.ingress, Command::Read(command), receive).await
        }

        /// Registers a clone-free owner-local projection at an ordered position.
        ///
        /// The selector and its results stay on the owner thread. Selection is
        /// synchronous; blocking a selector blocks that actor and runtime thread.
        ///
        /// # Errors
        ///
        /// Returns an error if Store authority ends before registration.
        pub async fn observe<F, V>(
            &self,
            selector: F,
        ) -> Result<crate::observation::local::Observation<V>, StoreStopped>
        where
            S: 'static,
            A: 'static,
            F: Fn(&S) -> V + 'static,
            V: 'static,
        {
            crate::store::register_observation(
                &self.ingress,
                move |signal| crate::store::observation::local::new_registration(selector, signal),
                |registration, signal| {
                    crate::observation::local::Observation::new(
                        registration.id,
                        signal,
                        registration.initial,
                        registration.version,
                        crate::store::observation::new_local_take_operation(
                            Arc::downgrade(&self.ingress),
                            registration.id,
                        ),
                    )
                },
            )
            .await
        }

        /// Registers a distinct owner-local projection at an ordered position.
        ///
        /// Clones selected values for equality tracking, not the Store's state.
        /// The selector and results remain on the owner thread.
        ///
        /// # Errors
        ///
        /// Returns an error if Store authority ends before registration.
        pub async fn observe_distinct<F, V>(
            &self,
            selector: F,
        ) -> Result<crate::observation::local::Observation<V>, StoreStopped>
        where
            S: 'static,
            A: 'static,
            F: Fn(&S) -> V + 'static,
            V: Clone + PartialEq + 'static,
        {
            crate::store::register_observation(
                &self.ingress,
                move |signal| {
                    crate::store::observation::local::new_distinct_registration(selector, signal)
                },
                |registration, signal| {
                    crate::observation::local::Observation::new(
                        registration.id,
                        signal,
                        registration.initial,
                        registration.version,
                        crate::store::observation::new_local_take_operation(
                            Arc::downgrade(&self.ingress),
                            registration.id,
                        ),
                    )
                },
            )
            .await
        }

        /// Waits until the current or a subsequent predicate selection is false.
        ///
        /// Registration starts on first poll. Values coalesce; ordinary drop
        /// releases observation without cancelling previously admitted work.
        pub fn wait_while<F>(&self, predicate: F) -> crate::observation::local::PredicateWait
        where
            S: 'static,
            A: 'static,
            F: Fn(&S) -> bool + 'static,
        {
            let store = self.clone();
            let terminal = crate::store::root_wait_terminal(&self.lifecycle);
            crate::observation::local::PredicateWait::new(
                async move {
                    let observation = store.observe(predicate).await;
                    drop(store);
                    match observation {
                        Ok(observation) => {
                            crate::observation::local::wait_for_false(observation, terminal).await
                        }
                        Err(_) => crate::PredicateWaitOutcome::StoreStopped,
                    }
                },
                None,
            )
        }

        /// Commits an action, then waits for a false predicate selection.
        ///
        /// Explicit cancellation controls only this action's causal work;
        /// ordinary drop never retracts an admitted action.
        pub fn send_and_wait_while<F>(
            &self,
            action: A,
            predicate: F,
        ) -> crate::observation::local::PredicateWait
        where
            S: 'static,
            A: 'static,
            F: Fn(&S) -> bool + 'static,
        {
            let store = self.clone();
            let terminal = crate::store::root_wait_terminal(&self.lifecycle);
            let cancellation = Arc::new(crate::observation::wait::WaitCancellation::default());
            let sending = Arc::clone(&cancellation);
            crate::observation::local::PredicateWait::new(
                async move {
                    if crate::store::send_action(&store.ingress, action, Some(sending))
                        .await
                        .is_err()
                    {
                        return crate::PredicateWaitOutcome::StoreStopped;
                    }
                    let observation = store.observe(predicate).await;
                    drop(store);
                    match observation {
                        Ok(observation) => {
                            crate::observation::local::wait_for_false(observation, terminal).await
                        }
                        Err(_) => crate::PredicateWaitOutcome::StoreStopped,
                    }
                },
                Some(cancellation),
            )
        }

        /// Performs ordered graceful shutdown after actor cleanup.
        ///
        /// Invalidates effect authority and issues aborts before returning. It
        /// does not wait for every runtime task wrapper to be polled to completion;
        /// the host must continue polling its runtime to dispose of those wrappers.
        ///
        /// # Errors
        ///
        /// Returns an error if authority already ended abortively.
        pub async fn shutdown(&self) -> Result<(), ShutdownError> {
            crate::store::shutdown(&self.ingress, &self.lifecycle).await
        }

        /// Returns the original owned state after ordered actor cleanup.
        ///
        /// Construction admits nothing. The first terminal command wins;
        /// abandoning an admitted reply does not retract shutdown.
        ///
        /// # Errors
        ///
        /// Returns an error on competing terminal commands or abortive closure.
        #[expect(
            clippy::manual_async_fn,
            reason = "The owned terminal future explicitly exposes its static lifetime"
        )]
        pub fn shutdown_into_state(self) -> impl Future<Output = Result<S, StoreStopped>> + 'static
        where
            S: 'static,
            A: 'static,
        {
            async move {
                let (reply, receive) = async_channel::bounded(1);
                let command = Command::<S, A, Local>::Shutdown(TerminalReply::State(Box::new(
                    move |state| {
                        let _ = reply.try_send(state);
                    },
                )));
                let result =
                    crate::store::ordered_reply(&self.ingress, command, receive.recv()).await;
                drop(self);
                result
            }
        }

        /// Returns whether Store authority has ended.
        #[must_use]
        pub fn is_closed(&self) -> bool {
            self.lifecycle.is_closed()
        }

        /// Waits for closure without retaining a Store owner.
        pub fn closed(&self) -> impl Future<Output = ()> + 'static + use<S, A> {
            Lifecycle::wait_weak(Arc::downgrade(&self.lifecycle))
        }

        /// Returns ordered diagnostics for deterministic testing.
        #[cfg(feature = "test-util")]
        #[doc(hidden)]
        pub async fn runtime_diagnostics(
            &self,
        ) -> Result<crate::testing::ActorDiagnostics, StoreStopped> {
            let (reply, receive) = async_channel::bounded(1);
            crate::store::ordered_reply(&self.ingress, Command::Snapshot(reply), receive.recv())
                .await
        }

        /// Returns ordered runtime counters for deterministic testing.
        #[cfg(feature = "test-util")]
        #[doc(hidden)]
        pub async fn runtime_snapshot(
            &self,
        ) -> Result<crate::testing::RuntimeSnapshot, StoreStopped> {
            self.runtime_diagnostics()
                .await
                .map(crate::testing::ActorDiagnostics::into_runtime)
        }

        /// Ends authority during deterministic test teardown.
        #[cfg(feature = "test-util")]
        #[doc(hidden)]
        pub fn abort_for_testing(&self) {
            self.lifecycle.abort(&self.ingress);
        }
    }

    impl<S, A> Clone for Store<S, A> {
        fn clone(&self) -> Self {
            self.lifecycle.add_owner();
            Self {
                ingress: Arc::clone(&self.ingress),
                lifecycle: Arc::clone(&self.lifecycle),
            }
        }
    }

    impl<S, A> Drop for Store<S, A> {
        fn drop(&mut self) {
            if self.lifecycle.drop_owner() {
                self.lifecycle.abort(&self.ingress);
            }
        }
    }

    impl<S, A> fmt::Debug for Store<S, A> {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.debug_struct("Store")
                .field("closed", &self.is_closed())
                .finish_non_exhaustive()
        }
    }

    /// Configures an owner-local Store using the common validated construction path.
    pub struct StoreBuilder<S, A, F> {
        /// Initial state transferred only after configuration validation succeeds.
        state: S,
        /// Owned feature, wrapped only after validation.
        feature: F,
        /// Owner-thread host that receives and disposes of the actor future.
        runtime: Runtime,
        /// Portable dependency environment owned by this Store.
        dependencies: DependencyValues,
        /// Reporting endpoint, including an installed deterministic recorder.
        issues: crate::IssueReporter,
        /// User callback combined with the reporting endpoint during construction.
        issue_handler: Option<crate::issue::IssueHandler>,
        /// Optional owner-local observer installed before the first actor poll.
        #[cfg(feature = "diagnostics")]
        lifecycle_sink: Option<Box<<Local as super::Carriers<S, A>>::LifecycleSink>>,
        /// Requested waiting-command capacity, validated by configurable construction.
        mailbox_capacity: usize,
        /// Owned declarations bound before spawning, in declaration order.
        mounts: Vec<Box<<Local as super::Carriers<S, A>>::Mount>>,
        /// Records the action type without owning an action instance.
        action: PhantomData<fn(A)>,
        /// Deterministic frontier claimed only after capacity validation succeeds.
        #[cfg(feature = "test-util")]
        transaction_gate: Option<crate::testing::local::TransactionGate<S, A>>,
        /// Shared on demand by root-output and removable-child declarations.
        removal_root: Option<Rc<crate::store::removal::local::root::RootRegistration<S, A>>>,
    }

    impl<S: 'static, A: 'static, F> StoreBuilder<S, A, F> {
        /// Retains construction inputs without wrapping behaviour or spawning work.
        fn new(state: S, feature: F, runtime: Runtime) -> Self {
            Self {
                state,
                feature,
                runtime,
                dependencies: DependencyValues::new(),
                issues: crate::IssueReporter::default(),
                issue_handler: None,
                #[cfg(feature = "diagnostics")]
                lifecycle_sink: None,
                mailbox_capacity: crate::store::DEFAULT_MAILBOX_CAPACITY,
                mounts: Vec::new(),
                action: PhantomData,
                #[cfg(feature = "test-util")]
                transaction_gate: None,
                removal_root: None,
            }
        }

        /// Sets bounded command capacity.
        ///
        /// Defaults to [`crate::DEFAULT_MAILBOX_CAPACITY`]. Construction rejects
        /// zero before claiming a test gate or spawning an actor.
        #[must_use]
        pub const fn mailbox_capacity(mut self, capacity: usize) -> Self {
            self.mailbox_capacity = capacity;
            self
        }

        /// Observes lifecycle facts synchronously on this Store's owner thread.
        ///
        /// Replaces and drops the previous callback without invocation. Delivery
        /// runs in debug and release builds; blocking blocks the actor and
        /// panics propagate. Captures need neither `Send` nor `Sync`. See
        /// [`LifecycleEvent`](crate::diagnostics::LifecycleEvent) for ordering.
        #[cfg(feature = "diagnostics")]
        #[must_use]
        pub fn with_lifecycle_diagnostics(
            mut self,
            sink: impl FnMut(&crate::diagnostics::LifecycleEvent) + 'static,
        ) -> Self {
            self.lifecycle_sink = Some(Box::new(sink));
            self
        }

        /// Overrides one Store-owned dependency using the portable key contract.
        #[must_use]
        pub fn with_dependency<K: DependencyKey>(mut self, value: K::Value) -> Self {
            self.dependencies.set::<K>(value);
            self
        }

        /// Replaces the owned dependency environment.
        ///
        /// Earlier overrides are replaced; later overrides apply to this environment.
        #[must_use]
        pub fn with_dependency_values(mut self, values: DependencyValues) -> Self {
            self.dependencies = values;
            self
        }

        /// Installs a synchronous portable issue handler, invoked outside locks.
        ///
        /// Replaces the previous handler without invoking it. An installed test
        /// recorder still records each issue before the handler runs.
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

        /// Prepares one production declaration for deferred test attachment.
        ///
        /// The closure owns the declaration; its site is weak. Applying the closure
        /// transfers the same declaration without constructing a child.
        #[cfg(feature = "test-util")]
        #[doc(hidden)]
        #[expect(
            clippy::type_complexity,
            reason = "The opaque attachment closure and typed weak site are returned together without an allocation"
        )]
        pub fn defer_mount<Child, K>(
            descriptor: super::OptionalMount<S, Child, K>,
        ) -> (
            impl FnOnce(Self) -> Self + 'static,
            super::MountSite<S, Child::State, Child::Action, A>,
        )
        where
            Child: Feature + 'static,
            Child::State: 'static,
            Child::Action: 'static,
            K: Eq + std::hash::Hash + Clone + Send + Sync + 'static,
        {
            let (owner, site) = crate::store::mount::local::attach_local(descriptor);
            (
                move |mut builder: Self| {
                    builder.mounts.push(owner);
                    builder
                },
                site,
            )
        }

        /// Prepares an incarnation-owned child recipe for deferred test attachment.
        ///
        /// The returned closure owns the declaration and recipe. Its site is weak.
        #[cfg(feature = "test-util")]
        #[doc(hidden)]
        #[expect(
            clippy::type_complexity,
            reason = "The opaque attachment closure and typed weak site are returned together without an allocation"
        )]
        pub fn defer_mount_with_children<Child, K>(
            descriptor: super::OptionalMount<S, Child, K>,
            recipe: impl FnMut(
                &Child::State,
                &mut super::MountChildren<'_, S, A, Child::State, Child::Action>,
            ) + 'static,
        ) -> (
            impl FnOnce(Self) -> Self + 'static,
            super::MountSite<S, Child::State, Child::Action, A>,
        )
        where
            Child: Feature + 'static,
            Child::State: 'static,
            Child::Action: 'static,
            K: Eq + std::hash::Hash + Clone + Send + Sync + 'static,
        {
            let (owner, site) = crate::store::mount::local::children::attach(descriptor, recipe);
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
        /// handlers until attachment; the output handle is weak.
        ///
        /// # Panics
        ///
        /// Propagates a panic from `configure`.
        #[cfg(feature = "test-util")]
        #[doc(hidden)]
        pub fn defer_output<O: 'static>(
            source: &super::MountSite<S, impl Sized + 'static, impl Sized + 'static, A>,
            configure: impl FnOnce(&mut super::OutputHandlers<S, A, O>),
        ) -> (impl FnOnce(Self) -> Self + 'static, super::Output<O>) {
            let (owner, output) =
                crate::store::output::local::attach(source.output_registration(), configure);
            (
                move |mut builder: Self| {
                    builder.mounts.push(owner);
                    builder
                },
                output,
            )
        }

        /// Installs the local typed deterministic transaction frontier.
        ///
        /// Replaces the previous gate handle without claiming an attachment.
        /// Construction claims the selected gate after capacity validation.
        #[cfg(feature = "test-util")]
        #[doc(hidden)]
        #[must_use]
        pub fn transaction_gate(
            mut self,
            gate: crate::testing::local::TransactionGate<S, A>,
        ) -> Self {
            self.transaction_gate = Some(gate);
            self
        }

        /// Validates configuration and spawns one owner-local actor.
        ///
        /// # Errors
        ///
        /// Rejects zero capacity and a gate attached or reserved by unfinished work.
        /// Failed construction drops its owned state, behaviour and callbacks.
        ///
        /// # Panics
        ///
        /// Propagates runtime-driver panics and panics from retired gate captures.
        pub fn build(self) -> Result<Store<S, A>, StoreBuildError>
        where
            F: Feature<State = S, Action = A> + 'static,
        {
            let capacity = NonZeroUsize::new(self.mailbox_capacity)
                .ok_or_else(StoreBuildError::zero_capacity)?;

            #[cfg(feature = "test-util")]
            let gate = self
                .transaction_gate
                .as_ref()
                .map(crate::testing::local::TransactionGate::attach)
                .transpose()?;
            Ok(self.build_with_capacity(
                capacity,
                #[cfg(feature = "test-util")]
                gate,
            ))
        }

        /// Transfers validated inputs to the common actor and owner-thread runtime.
        fn build_with_capacity(
            self,
            capacity: NonZeroUsize,
            #[cfg(feature = "test-util")] gate: Option<
                crate::testing::TransactionGateBinding<S, A, Local>,
            >,
        ) -> Store<S, A>
        where
            F: Feature<State = S, Action = A> + 'static,
        {
            let (actor, ingress, lifecycle) = crate::store::builder::prepare_actor::<S, A, Local>(
                self.state,
                Box::new(Driver(self.feature)),
                self.runtime.clone(),
                self.dependencies,
                self.issues,
                self.issue_handler,
                capacity,
                self.mounts,
                #[cfg(feature = "diagnostics")]
                self.lifecycle_sink,
                #[cfg(feature = "test-util")]
                None,
                #[cfg(feature = "test-util")]
                gate,
            );
            self.runtime.spawn(Box::pin(actor.run()));
            Store { ingress, lifecycle }
        }
    }

    impl<S, A, F> fmt::Debug for StoreBuilder<S, A, F> {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.debug_struct("StoreBuilder")
                .field("runtime", &self.runtime)
                .field("mailbox_capacity", &self.mailbox_capacity)
                .finish_non_exhaustive()
        }
    }

    impl<S: 'static, A: 'static, F: Feature<State = S, Action = A> + 'static> StoreBuilder<S, A, F> {
        /// Creates one root owner on demand and reuses its registration thereafter.
        fn removal_root(
            &mut self,
        ) -> Rc<crate::store::removal::local::root::RootRegistration<S, A>> {
            Rc::clone(self.removal_root.get_or_insert_with(|| {
                let registration = crate::store::removal::local::root::RootRegistration::new();
                self.mounts
                    .push(Box::new(crate::store::removal::local::root::RootOwner(
                        Rc::clone(&registration),
                    )));
                registration
            }))
        }

        /// Declares root output using the ordinary strict-ancestor routing law.
        ///
        /// The root has no ancestor. Its output returns unhandled, even with a root handler.
        /// Calls `configure` immediately and retains the handlers in the builder.
        ///
        /// # Panics
        ///
        /// Propagates a panic from `configure`.
        pub fn root_output<O: 'static>(
            mut self,
            configure: impl FnOnce(&mut super::OutputHandlers<S, A, O>),
        ) -> (Self, super::Output<O>) {
            let source: Rc<dyn crate::store::output::local::OutputRegistration> =
                self.removal_root();
            let (owner, output) =
                crate::store::output::local::attach(Rc::downgrade(&source), configure);
            self.mounts.push(owner);
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
            identity: impl FnMut(&S) -> Option<crate::MountIdentity<K>> + 'static,
            make: impl FnMut(&Child::State) -> Child + 'static,
        ) -> (
            Self,
            super::MountSite<S, Child::State, Child::Action, A>,
            super::RemovalSite<S, Child::State, A>,
        )
        where
            Child: Feature + 'static,
            Child::State: 'static,
            Child::Action: 'static,
            K: Eq + std::hash::Hash + Clone + Send + Sync + 'static,
        {
            let root = self.removal_root();
            let (owner, site) =
                crate::store::mount::local::attach_owned_optional(read, write, identity, make);
            let removal = crate::store::removal::local::root_optional(
                &root,
                site.output_registration(),
                write,
            );
            self.mounts.push(owner);
            (self, site, removal)
        }

        /// Declares owner-local handlers for one explicitly attached source.
        ///
        /// Calls `configure` immediately. The builder owns the registered handler
        /// captures; the returned output handle is weak and does not own them.
        ///
        /// # Panics
        ///
        /// Propagates a panic from `configure`.
        pub fn output<O: 'static>(
            self,
            source: &super::MountSite<S, impl Sized + 'static, impl Sized + 'static, A>,
            configure: impl FnOnce(&mut super::OutputHandlers<S, A, O>),
        ) -> (Self, super::Output<O>) {
            let (owner, output) =
                crate::store::output::local::attach(source.output_registration(), configure);
            (self.with_mount(owner), output)
        }

        /// Retains one declaration without reconciling or constructing its feature.
        pub(in crate::store) fn with_mount(
            mut self,
            owner: Box<<Local as super::Carriers<S, A>>::Mount>,
        ) -> Self {
            self.mounts.push(owner);
            self
        }

        /// Attaches an optional owner-local child to the common structural transaction.
        #[must_use]
        pub fn mount<Child, K>(self, descriptor: super::OptionalMount<S, Child, K>) -> Self
        where
            Child: Feature + 'static,
            Child::State: 'static,
            Child::Action: 'static,
            K: Eq + std::hash::Hash + Clone + Send + Sync + 'static,
        {
            self.mount_with_site(descriptor).0
        }

        /// Attaches a child and returns its weak site for explicit fresh acquisition.
        pub fn mount_with_site<Child, K>(
            mut self,
            descriptor: super::OptionalMount<S, Child, K>,
        ) -> (Self, super::MountSite<S, Child::State, Child::Action, A>)
        where
            Child: Feature + 'static,
            Child::State: 'static,
            Child::Action: 'static,
            K: Eq + std::hash::Hash + Clone + Send + Sync + 'static,
        {
            let (owner, site) = crate::store::mount::local::attach_local(descriptor);
            self.mounts.push(owner);
            (self, site)
        }

        /// Attaches an optional child with an incarnation-bound child recipe.
        pub fn mount_with_children<Child, K>(
            mut self,
            descriptor: super::OptionalMount<S, Child, K>,
            recipe: impl FnMut(
                &Child::State,
                &mut crate::store::mount::local::children::MountChildren<
                    '_,
                    S,
                    A,
                    Child::State,
                    Child::Action,
                >,
            ) + 'static,
        ) -> (Self, super::MountSite<S, Child::State, Child::Action, A>)
        where
            Child: Feature + 'static,
            Child::State: 'static,
            Child::Action: 'static,
            K: Eq + std::hash::Hash + Clone + Send + Sync + 'static,
        {
            let (owner, site) = crate::store::mount::local::children::attach(descriptor, recipe);
            self.mounts.push(owner);
            (self, site)
        }

        /// Attaches every keyed entry using fresh owner-local construction.
        ///
        /// Reorder/configuration retain Features and caches; a new entry stamp
        /// or explicit scope leave rebuilds them. Configuration changes expire
        /// captured authority. Selectors use this builder's actual root State.
        #[must_use]
        pub fn keyed<Child, K, ChildState>(
            mut self,
            read: for<'s> fn(&'s S) -> &'s crate::KeyedState<K, ChildState>,
            write: for<'s> fn(&'s mut S) -> &'s mut crate::KeyedState<K, ChildState>,
            mut make: impl FnMut(&K, &ChildState) -> crate::store::mount::local::EntryMount<Child>
            + 'static,
        ) -> Self
        where
            Child: Feature<State = ChildState> + 'static,
            ChildState: 'static,
            Child::Action: 'static,
            K: Eq + std::hash::Hash + Clone + 'static,
        {
            let owner = crate::store::mount::local::children::keyed(
                crate::store::mounted::OptionalPath::new(|state| Some(state), |state| Some(state)),
                read,
                write,
                0,
                move |key, state, _| {
                    (
                        make(key, state),
                        None::<
                            fn(
                                &ChildState,
                                &mut crate::store::mount::local::children::MountChildren<
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
            self.mounts.push(Box::new(owner));
            self
        }

        /// Attaches a keyed collection with fresh child recipes for each incarnation.
        ///
        /// Departing subtrees of this collection drain before new entries are
        /// made in keyed order, followed by depth-first roster traversal. Nested
        /// retained collections reconcile there, not in a global retirement
        /// pass. Reorder/configuration preserve Features and recipes; replacement
        /// or explicit scope leave renews both. Returned callbacks own captures.
        #[must_use]
        pub fn keyed_with_children<Child, K, G, ChildState>(
            mut self,
            read: for<'s> fn(&'s S) -> &'s crate::KeyedState<K, ChildState>,
            write: for<'s> fn(&'s mut S) -> &'s mut crate::KeyedState<K, ChildState>,
            mut make: impl FnMut(&K, &ChildState) -> (crate::store::mount::local::EntryMount<Child>, G)
            + 'static,
        ) -> Self
        where
            Child: Feature<State = ChildState> + 'static,
            ChildState: 'static,
            Child::Action: 'static,
            K: Eq + std::hash::Hash + Clone + 'static,
            G: FnMut(
                    &ChildState,
                    &mut crate::store::mount::local::children::MountChildren<
                        '_,
                        S,
                        A,
                        ChildState,
                        Child::Action,
                    >,
                ) + 'static,
        {
            let owner = crate::store::mount::local::children::keyed(
                crate::store::mounted::OptionalPath::new(|state| Some(state), |state| Some(state)),
                read,
                write,
                0,
                move |key, state, _| {
                    let (entry, recipe) = make(key, state);
                    (entry, Some(recipe))
                },
            );
            self.mounts.push(Box::new(owner));
            self
        }

        /// Declares keyed root children with checked owning removal in each recipe.
        #[must_use]
        pub fn keyed_removable<Child, K, G>(
            mut self,
            read: for<'s> fn(&'s S) -> &'s crate::KeyedState<K, Child::State>,
            write: for<'s> fn(&'s mut S) -> &'s mut crate::KeyedState<K, Child::State>,
            make: impl FnMut(&K, &Child::State) -> (crate::local::EntryMount<Child>, G) + 'static,
        ) -> Self
        where
            Child: Feature + 'static,
            Child::State: 'static,
            Child::Action: 'static,
            K: Eq + std::hash::Hash + Clone + 'static,
            G: FnMut(
                    &Child::State,
                    &mut crate::store::mount::local::children::MountChildren<
                        '_,
                        S,
                        A,
                        Child::State,
                        Child::Action,
                    >,
                    crate::local::RemovalSite<S, Child::State, A>,
                ) + 'static,
        {
            let root: Rc<dyn crate::store::output::local::HandlerRegistration<S, S, A, A>> =
                self.removal_root();
            let owner = crate::store::mount::local::children::keyed_removable(
                crate::store::mounted::OptionalPath::new(|state| Some(state), |state| Some(state)),
                Rc::downgrade(&root),
                read,
                write,
                0,
                make,
            );
            self.mounts.push(Box::new(owner));
            self
        }

        /// Declares native stack children with checked pop-last removal in each recipe.
        #[must_use]
        pub fn stack_removable<Child, G>(
            mut self,
            read: for<'s> fn(&'s S) -> &'s crate::StackState<Child::State>,
            write: for<'s> fn(&'s mut S) -> &'s mut crate::StackState<Child::State>,
            make: impl FnMut(&crate::StackId, &Child::State) -> (crate::local::EntryMount<Child>, G)
            + 'static,
        ) -> Self
        where
            Child: Feature + 'static,
            Child::State: 'static,
            Child::Action: 'static,
            G: FnMut(
                    &Child::State,
                    &mut crate::store::mount::local::children::MountChildren<
                        '_,
                        S,
                        A,
                        Child::State,
                        Child::Action,
                    >,
                    crate::local::RemovalSite<S, Child::State, A>,
                ) + 'static,
        {
            let root: Rc<dyn crate::store::output::local::HandlerRegistration<S, S, A, A>> =
                self.removal_root();
            let owner = crate::store::mount::local::children::stack_removable(
                crate::store::mounted::OptionalPath::new(|state| Some(state), |state| Some(state)),
                Rc::downgrade(&root),
                read,
                write,
                0,
                make,
            );
            self.mounts.push(Box::new(owner));
            self
        }
    }
}

/// Selects non-`Send` command, callback and runtime types for the common kernel.
pub(crate) struct Local;

/// Selects owner-local callable leaves within the common effect program.
pub(crate) struct LocalEffects;

/// Describes owner-local work interpreted by the Store actor.
pub struct Effect<A> {
    /// Owned declarations interpreted after the current reduction commits.
    pub(super) program: Program<A, LocalEffects>,
    /// Optional originating scope retained through composition and mapping.
    pub(super) scope: Option<ScopeToken>,
}

impl<A> Default for Effect<A> {
    fn default() -> Self {
        Self::none()
    }
}

/// Sends causal actions without retaining a Store owner.
pub struct EffectSender<A> {
    /// Shared local sender closure retaining causal authority, not Store ownership.
    send: Rc<dyn Fn(A) -> SendFuture>,
}

impl<A> std::fmt::Debug for Effect<A> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Effect").finish_non_exhaustive()
    }
}

impl<A> std::fmt::Debug for EffectSender<A> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EffectSender").finish_non_exhaustive()
    }
}

impl<A> Clone for EffectSender<A> {
    fn clone(&self) -> Self {
        Self {
            send: Rc::clone(&self.send),
        }
    }
}

impl<A> EffectSender<A> {
    /// Retains an admission operation that clones without duplicating its captures.
    pub(super) fn new(send: impl Fn(A) -> SendFuture + 'static) -> Self {
        Self {
            send: Rc::new(send),
        }
    }

    /// Sends a causal action and waits for its commit.
    ///
    /// # Errors
    ///
    /// Returns an error when the originating authority has ended.
    pub async fn send(&self, action: A) -> Result<(), EffectSendError> {
        (self.send)(action).await
    }
}

impl<A> EffectLeaves<A> for LocalEffects {
    type Effect = Effect<A>;
    type Run = Box<dyn FnOnce(EffectSender<A>) -> LocalFuture>;
    type FallibleRun = Box<dyn FnOnce(EffectSender<A>, Provenance, AbortHandle) -> LocalFuture>;
    type Future = Pin<Box<dyn Future<Output = Option<A>>>>;
    type Interval = Box<dyn FnMut() -> A>;
}

impl<A> EffectValue<A, LocalEffects> for Effect<A> {
    fn from_parts(program: Program<A, LocalEffects>, scope: Option<ScopeToken>) -> Self {
        Self { program, scope }
    }

    fn into_parts(self) -> (Program<A, LocalEffects>, Option<ScopeToken>) {
        (self.program, self.scope)
    }

    fn parts_mut(&mut self) -> (&mut Program<A, LocalEffects>, &mut Option<ScopeToken>) {
        (&mut self.program, &mut self.scope)
    }

    fn is_none(&self) -> bool {
        matches!(self.program, Program::None)
    }
}

impl<A> Effect<A> {
    /// Associates this work with a named operation in its originating scope.
    #[must_use]
    pub fn named<K>(self, key: K, policy: crate::OperationPolicy) -> Self
    where
        K: std::hash::Hash + Eq + Send + Sync + 'static,
    {
        Self {
            program: Program::Named {
                effect: Box::new(self),
                declaration: crate::effect::operation::OperationDeclaration::new(key, policy),
            },
            scope: None,
        }
    }

    /// Produces no work.
    #[must_use]
    pub const fn none() -> Self {
        Self {
            program: Program::None,
            scope: None,
        }
    }

    /// Merges non-empty effects in source order for concurrent execution.
    ///
    /// Consumes the iterator synchronously without retaining its borrows.
    /// Returns no work without allocating when every effect is empty.
    pub fn merge(effects: impl IntoIterator<Item = Self>) -> Self {
        crate::effect::leaves::merge::<A, LocalEffects>(effects)
    }
}

impl<A: 'static> Effect<A> {
    /// Excludes cancellation identities inherited from the triggering action.
    ///
    /// Explicit cancellation wrappers remain effective regardless of modifier
    /// order. Root action-task cancellation, structural scope removal and Store
    /// shutdown still own this work. This does not detach a task or let an
    /// obsolete triggering action pass admission.
    ///
    /// Wrap an entire sequence to transfer its continuation. Wrapping one
    /// element does not protect its enclosing sequence or later elements.
    #[must_use]
    pub fn without_inherited_cancellation(self) -> Self {
        if matches!(self.program, Program::None) {
            return self;
        }
        Self {
            scope: self.scope,
            program: Program::WithoutInheritedCancellation(Box::new(self)),
        }
    }

    /// Sends one causal action.
    pub fn send(action: A) -> Self {
        Self {
            program: Program::Send(action),
            scope: None,
        }
    }

    /// Starts each effect after its predecessor finishes.
    pub fn concatenate(effects: impl IntoIterator<Item = Self>) -> Self {
        Self {
            program: Program::Concatenate(effects.into_iter().collect()),
            scope: None,
        }
    }

    /// Cancels every live generation matching `id` in this reducer scope.
    ///
    /// Keys must obey the identity stability law of [`Self::cancellable`],
    /// starting when they are passed to this constructor.
    pub fn cancel<ID>(id: ID) -> Self
    where
        ID: std::hash::Hash + Eq + Send + Sync + 'static,
    {
        Self {
            program: Program::Cancel(CancellationIdentity::local(id)),
            scope: None,
        }
    }

    /// Associates this effect with one typed cancellation identity.
    ///
    /// Independently created equal keys of the same Rust type match within the
    /// current reducer scope. Other types and reducer scopes remain distinct.
    ///
    /// Keep `Eq` and `Hash` coherent and the key's identity stable from this call
    /// until the effect and its registered work release the key. Owning a key
    /// does not freeze shared interior state: its hash is cached at construction,
    /// but equality still observes the stored value. Changing identity during
    /// this interval is invalid input; cancellation behaviour is not guaranteed.
    #[must_use]
    pub fn cancellable<ID>(self, id: ID) -> Self
    where
        ID: std::hash::Hash + Eq + Send + Sync + 'static,
    {
        Self {
            program: Program::Cancellable {
                effect: Box::new(self),
                identity: CancellationIdentity::local(id),
                cancel_in_flight: false,
            },
            scope: None,
        }
    }

    /// Replaces live effects sharing one typed cancellation identity.
    ///
    /// Keys must obey the identity stability law of [`Self::cancellable`],
    /// starting when they are passed to this constructor.
    #[must_use]
    pub fn cancellable_in_flight<ID>(self, id: ID) -> Self
    where
        ID: std::hash::Hash + Eq + Send + Sync + 'static,
    {
        Self {
            program: Program::Cancellable {
                effect: Box::new(self),
                identity: CancellationIdentity::local(id),
                cancel_in_flight: true,
            },
            scope: None,
        }
    }

    /// Sends one causal action after the runtime's monotonic delay.
    pub fn send_after(duration: std::time::Duration, action: A) -> Self {
        Self {
            program: Program::SendAfter { duration, action },
            scope: None,
        }
    }

    /// Sends one action no earlier than an absolute monotonic deadline.
    ///
    /// Use the Store runtime's clock origin, for example a deadline derived from
    /// [`crate::local::Runtime::now`]. Clock origins are not checked or translated.
    /// Delaying or sequencing this effect does not rebase its deadline. An elapsed
    /// deadline becomes eligible through the normal actor queue, not inline.
    /// [`crate::MonotonicInstant::MAX`] stays pending until cancelled or shut down.
    /// Mapping preserves the deadline. Task, scope and cancellation ownership
    /// remain the same as for [`Self::send_after`].
    pub fn send_at(deadline: crate::MonotonicInstant, action: A) -> Self {
        Self {
            program: Program::SendAt { deadline, action },
            scope: None,
        }
    }

    /// Delays execution of this effect program.
    ///
    /// Waiting starts when the Store reaches this program. Construction and
    /// mapping remain eager; put external operations inside [`Self::run`].
    /// Zero duration returns this effect unchanged; an empty effect stays empty.
    /// Positive durations use saturating monotonic deadlines.
    ///
    /// Cancellation wrappers outside the delay own waiting and execution; inner
    /// wrappers register after waiting. For debounce, use
    /// `effect.delay(duration).cancellable_in_flight(id)`. Root task cancellation,
    /// structural removal and shutdown retain ownership of both phases. A
    /// surrounding sequence waits for the child program, not only its timer.
    /// Place an inherited-cancellation boundary outside the delay to transfer
    /// both phases; an inner boundary cannot rescue an already cancelled wait.
    #[must_use]
    pub fn delay(self, duration: std::time::Duration) -> Self {
        if duration.is_zero() || matches!(self.program, Program::None) {
            return self;
        }
        let scope = self.scope;
        let wait = Self {
            program: Program::Wait(duration),
            scope: None,
        };
        Self {
            program: Program::Concatenate([wait, self].into()),
            scope,
        }
    }

    /// Generates fixed-rate actions until cancelled.
    ///
    /// # Panics
    ///
    /// Panics when `period` is zero.
    pub fn interval<F>(period: std::time::Duration, make_action: F) -> Self
    where
        F: FnMut() -> A + 'static,
    {
        assert!(!period.is_zero(), "interval period must be non-zero");
        Self {
            program: Program::<A, LocalEffects>::Interval {
                period,
                make_action: Box::new(make_action),
            },
            scope: None,
        }
    }

    /// Starts owner-local asynchronous work after the transaction commits.
    ///
    /// The factory runs on the registered body's first poll, not at construction.
    /// Cancellation cannot interrupt a poll that blocks or spins indefinitely.
    /// Long-running work must await or yield so the owner thread can make progress.
    pub fn run<F, Fut>(run: F) -> Self
    where
        F: FnOnce(EffectSender<A>) -> Fut + 'static,
        Fut: Future<Output = ()> + 'static,
    {
        Self {
            program: Program::<A, LocalEffects>::Run(Box::new(move |sender| Box::pin(run(sender)))),
            scope: None,
        }
    }

    /// Maps emitted actions without changing work ordering.
    ///
    /// Immediate and delayed-send actions map synchronously. Direct eager and
    /// action-free variants need no callback storage. Nested and deferred programs
    /// use shared callback storage. Deferred work retains the mapper until its
    /// last owner drops. Captures can be owner-local.
    pub fn map<B: 'static>(self, map: impl Fn(A) -> B + 'static) -> Effect<B> {
        crate::effect::map::map_with::<A, B, LocalEffects, LocalEffects, _, _>(self, map, |map| {
            Rc::new(map) as Rc<dyn Fn(A) -> B>
        })
    }

    /// Emits the optional result of an owner-local future.
    pub fn future(future: impl Future<Output = Option<A>> + 'static) -> Self {
        Self {
            program: Program::<A, LocalEffects>::Future(Box::pin(future)),
            scope: None,
        }
    }

    /// Runs fallible work and handles its error under the same causal authority.
    pub fn try_run<E, F, Fut, H, HFut>(run: F, handler: H) -> Self
    where
        F: FnOnce(EffectSender<A>) -> Fut + 'static,
        Fut: Future<Output = Result<(), crate::EffectError<E>>> + 'static,
        H: FnOnce(E, EffectSender<A>) -> HFut + 'static,
        HFut: Future<Output = ()> + 'static,
    {
        Self {
            program: Program::<A, LocalEffects>::FallibleRun(Box::new(
                move |sender, provenance, cancellation| {
                    Box::pin(async move {
                        let next = crate::effect::fallible::handle_result(
                            run(sender.clone()).await,
                            sender,
                            &provenance,
                            &cancellation,
                            handler,
                        );
                        drop(provenance);
                        drop(cancellation);
                        if let Some(next) = next {
                            next.await;
                        }
                    })
                },
            )),
            scope: None,
        }
    }

    /// Reports live failures while retaining their original typed causes.
    ///
    /// Uses the same live-at-result guard and reporting contract as
    /// [`crate::Effect::try_run_reported`], without requiring owner-local captures,
    /// futures or causes to implement `Send`. The explicit reporter remains
    /// portable. Success, explicit cancellation and invalidated authority invoke
    /// neither the issue factory nor the outcome callback.
    ///
    /// For a live failure, the lazy issue factory borrows the original cause.
    /// Its location is replaced with this constructor's caller location. The
    /// synchronous outcome callback receives the cause and independent delivery
    /// evidence even if the reporter is disabled or closed. No ambient reporter
    /// is consulted.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::rc::Rc;
    /// use rca_core::{EffectError, Issue, IssueReporter, local::Effect};
    /// let cause = Rc::new(7);
    /// let effect: Effect<()> = Effect::try_run_reported(
    ///     move |_| async move { Err(EffectError::Failed(cause)) },
    ///     IssueReporter::default(),
    ///     |_| Issue::new("load.failed", "loading failed"),
    ///     |report| assert_eq!(*report.result.unwrap_err(), 7),
    /// );
    /// ```
    ///
    /// # Panics
    ///
    /// Polling propagates operation, factory, reporter and outcome callback panics.
    /// A panicking reporter prevents the outcome callback from running.
    #[track_caller]
    pub fn try_run_reported<Error, Operation, OperationFuture, MakeIssue, Outcome>(
        operation: Operation,
        reporter: crate::IssueReporter,
        issue: MakeIssue,
        outcome: Outcome,
    ) -> Self
    where
        Operation: FnOnce(EffectSender<A>) -> OperationFuture + 'static,
        OperationFuture: Future<Output = Result<(), crate::EffectError<Error>>> + 'static,
        MakeIssue: FnOnce(&Error) -> crate::Issue + 'static,
        Outcome:
            FnOnce(crate::issue::OperationReport<(), Error, std::convert::Infallible>) + 'static,
    {
        let location = std::panic::Location::caller();
        Self {
            program: Program::<A, LocalEffects>::FallibleRun(Box::new(
                move |sender, provenance, cancellation| {
                    Box::pin(async move {
                        crate::effect::fallible::report_result(
                            operation(sender).await,
                            &provenance,
                            &cancellation,
                            &reporter,
                            location,
                            issue,
                            outcome,
                        );
                    })
                },
            )),
            scope: None,
        }
    }
}

impl<S: 'static, A: 'static> crate::effect::map::MapLeaves<S, A, LocalEffects, LocalEffects>
    for Rc<dyn Fn(S) -> A>
{
    fn action(&self, action: S) -> A {
        self(action)
    }

    fn run(
        self,
        run: <LocalEffects as EffectLeaves<S>>::Run,
    ) -> <LocalEffects as EffectLeaves<A>>::Run {
        Box::new(move |sender| run(EffectSender::new(move |source| (sender.send)(self(source)))))
    }

    fn fallible(
        self,
        run: <LocalEffects as EffectLeaves<S>>::FallibleRun,
    ) -> <LocalEffects as EffectLeaves<A>>::FallibleRun {
        Box::new(move |sender, provenance, cancellation| {
            run(
                EffectSender::new(move |source| (sender.send)(self(source))),
                provenance,
                cancellation,
            )
        })
    }

    fn future(
        self,
        future: <LocalEffects as EffectLeaves<S>>::Future,
    ) -> <LocalEffects as EffectLeaves<A>>::Future {
        Box::pin(async move { future.await.map(|action| self(action)) })
    }

    fn interval(
        self,
        mut interval: <LocalEffects as EffectLeaves<S>>::Interval,
    ) -> <LocalEffects as EffectLeaves<A>>::Interval {
        Box::new(move || self(interval()))
    }
}

/// Reduces owner-local State and Action values synchronously.
///
/// Mutable captures need no `Send` or `Sync`. Reduction runs on the actor thread.
/// Blocking reduction prevents cancellation and other work on that thread.
///
/// Every reducer also implements [`Feature`], so Stores accept reducers directly.
pub trait Reducer {
    /// Actor-owned state.
    type State;

    /// Actions admitted to the actor.
    type Action;

    /// Mutates state and returns causal work.
    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &Self::Action,
        context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action>;
}

/// Creates an owner-local reducer from a closure.
pub struct Reduce<S, A, F> {
    /// Owned callback invoked synchronously for each reduction.
    run: F,
    /// Names the domain without owning state or actions or imposing their auto traits.
    marker: std::marker::PhantomData<fn(S, A)>,
}

impl<S, A, F> std::fmt::Debug for Reduce<S, A, F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Reduce").finish_non_exhaustive()
    }
}

impl<S, A, F> Reduce<S, A, F> {
    /// Wraps a synchronous reducing closure.
    pub fn new(run: F) -> Self {
        Self {
            run,
            marker: std::marker::PhantomData,
        }
    }
}

impl<S, A, F> Reducer for Reduce<S, A, F>
where
    F: FnMut(&mut S, &A, &mut ReduceContext<'_>) -> Effect<A>,
{
    type State = S;
    type Action = A;

    fn reduce(&mut self, state: &mut S, action: &A, context: &mut ReduceContext<'_>) -> Effect<A> {
        (self.run)(state, action, context)
    }
}

/// Reduces owner-local values and declares typed causal tasks.
///
/// Reductions are synchronous. Effect and task factories are retained as owned
/// declarations and start after commit, when the runtime polls their bodies.
///
/// [`Reducer`] implements this trait automatically for action-and-effect logic.
/// A direct implementation can also declare tasks with typed state access.
/// Implement either trait on a type, not both. Both use the same Store constructors.
pub trait Feature {
    /// Actor-owned state.
    type State;

    /// Actions admitted to the actor.
    type Action;

    /// Mutates state and appends causal work to this transaction.
    fn update(
        &mut self,
        state: &mut Self::State,
        action: &Self::Action,
        context: &mut FeatureContext<'_, Self::State, Self::Action>,
    );
}

/// Collects owner-local work during one reduction.
pub struct FeatureContext<'a, S, A> {
    /// Borrowed dependencies, scope authority and synchronous transaction services.
    reducer: ReduceContext<'a>,
    /// Owned nonempty effects and tasks, kept in declaration order until transfer.
    work: Work<Effect<A>, Task<S, A>>,
}

impl<S, A> std::fmt::Debug for FeatureContext<'_, S, A> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FeatureContext").finish_non_exhaustive()
    }
}

impl<'a, S, A> FeatureContext<'a, S, A> {
    /// Borrows one reduction's services and starts with no registered work.
    pub(super) fn new(reducer: ReduceContext<'a>) -> Self {
        Self {
            reducer,
            work: Work::default(),
        }
    }

    /// Releases synchronous context borrows and transfers owned work to the interpreter.
    pub(super) fn into_work(self) -> Work<Effect<A>, Task<S, A>> {
        self.work
    }

    /// Returns the synchronous dependency and issue-reporting context.
    pub fn reducer_context(&mut self) -> &mut ReduceContext<'a> {
        &mut self.reducer
    }

    /// Changes a direct child's input admission without ending its lifetime.
    ///
    /// # Errors
    ///
    /// Rejects a foreign, stale or non-child gate before changing admission.
    pub fn set_input_open(
        &mut self,
        gate: &crate::InputGate,
        open: bool,
    ) -> Result<(), crate::TaskAccessError> {
        self.reducer.set_input_open(gate, open)
    }

    /// Appends an effect to this transaction's ordered work.
    pub fn effect(&mut self, effect: Effect<A>) {
        if !effect.is_none() {
            self.work.push(WorkItem::Effect(effect));
        }
    }

    /// Appends a typed task to this transaction's ordered work.
    pub fn task(&mut self, task: Task<S, A>) {
        self.work.push(WorkItem::Task(task));
    }
}

impl<R: Reducer + ?Sized> Feature for R {
    type State = R::State;
    type Action = R::Action;

    fn update(
        &mut self,
        state: &mut R::State,
        action: &R::Action,
        context: &mut FeatureContext<'_, R::State, R::Action>,
    ) {
        let effect = Reducer::reduce(self, state, action, context.reducer_context());
        context.effect(effect);
    }
}

/// Owns a local feature and adapts its context to the common reduction driver.
pub(super) struct Driver<F>(pub(super) F);

impl<S, A, F: Feature<State = S, Action = A>> ReduceDriver<S, A> for Driver<F> {
    type Effect = Effect<A>;
    type Task = Task<S, A>;

    fn reduce(
        &mut self,
        state: &mut S,
        action: &A,
        context: ReduceContext<'_>,
    ) -> Work<Self::Effect, Self::Task> {
        let mut context = FeatureContext::new(context);
        self.0.update(state, action, &mut context);
        context.into_work()
    }
}

impl<S, A> Carriers<S, A> for Local {
    #[cfg(feature = "test-util")]
    type OutputAnchor = std::rc::Weak<dyn std::any::Any>;
    type Runtime = crate::runtime::local::Runtime;
    type LegacyRead = dyn super::read::ReadCommand<S>;
    type CheckedRead = dyn super::read::CheckedReadCommand<S>;
    type HandleSelector = dyn super::scope::HandleSelector<S>;
    type ObservationEntry = dyn super::observation::ErasedObservation<S>;
    type ObservationCommand = dyn super::observation::local::ObservationCommand<S>;
    type OperationMetadata = std::rc::Rc<dyn super::operation::OperationMetadata>;
    type Acquire = dyn super::mount::AcquireCall;
    type Fold = dyn super::fold::FoldCall<S, A, Self>;
    type Mount = dyn super::mount::MountDeclaration<S, A, Local>;
    type Future = LocalFuture;
    type Context = TaskContext<S, A>;
    type Effects = LocalEffects;
    type Task = Task<S, A>;
    type RootInitialCallback =
        Box<dyn for<'a> FnMut(&S, crate::ReduceContext<'a>) -> Option<Self::Task>>;
    type RootReaction = super::reaction::LocalHook<S, A>;
    type RootConfiguration = Box<dyn FnMut(&S) -> u64>;
    type Driver = dyn ReduceDriver<S, A, Effect = Effect<A>, Task = Task<S, A>>;
    type Read = dyn ReadCall<S>;
    type ReadInput = dyn ReadInput<S>;

    fn into_read(input: Box<Self::ReadInput>) -> Box<Self::Read> {
        input
    }

    type Update = dyn UpdateCall<S, Reply = LocalReply>;
    type Reply = LocalReply;
    type Terminal = dyn FnOnce(S);

    #[cfg(feature = "diagnostics")]
    type LifecycleSink = dyn FnMut(&crate::diagnostics::LifecycleEvent);

    #[cfg(feature = "diagnostics")]
    fn lifecycle_sink(
        sink: &mut Self::LifecycleSink,
    ) -> &mut dyn FnMut(&crate::diagnostics::LifecycleEvent) {
        sink
    }

    #[cfg(feature = "test-util")]
    type Query = dyn FnOnce(&S);
    type Mounted = dyn super::mounted::MountedCall<S, A, Self>;
    type Output = dyn super::output::OutputCall<S, A, Self>;
    type MountedWork = dyn super::mounted::MountedWork<S, A, Self>;
    type Deferred = dyn super::mounted::DeferredProgram<S, A, Self>;
    type Timer = dyn super::mounted::RoutedTimer<S, A, Self>;
    type Node = dyn super::mounted::InstalledNode;
}

impl<S: 'static, A: 'static> Execution<S, A> for Local {
    fn now(runtime: &Self::Runtime) -> crate::MonotonicInstant {
        runtime.now()
    }

    fn yield_now(runtime: &Self::Runtime) -> Self::Future {
        runtime.yield_now()
    }

    fn sleep_until(runtime: &Self::Runtime, deadline: crate::MonotonicInstant) -> Self::Future {
        runtime.sleep_until(deadline)
    }

    fn spawn(runtime: &Self::Runtime, future: Self::Future) {
        runtime.spawn(future);
    }

    fn observe(
        command: Box<Self::ObservationCommand>,
        state: &S,
        registry: &mut super::observation::ObservationRegistryCore<S, Self::ObservationEntry>,
        version: u64,
        scope: super::scope::ScopeToken,
        handle: Option<super::scope::WeakHandleScope>,
        validation: &super::scope::SelectedChild<'_>,
    ) {
        command.execute(state, registry, version, scope, handle, validation);
    }

    fn task_parts(
        task: Self::Task,
    ) -> (
        Self::Task,
        Option<crate::effect::operation::OperationDeclaration>,
    ) {
        task.into_parts()
    }

    fn context(
        ingress: Weak<CommandIngress<Command<S, A, Self>>>,
        scope: super::scope::ScopeToken,
        provenance: Provenance,
        authority: ScopeAuthority,
    ) -> Self::Context {
        TaskContext::new(ingress, scope, provenance, authority)
    }

    fn register_task(
        registry: &mut TaskRegistry,
        prepared: PreparedTask,
        task: Self::Task,
        context: Self::Context,
    ) -> Self::Future {
        registry.register_local(prepared, move |_| task.start(context))
    }

    fn register_effect(
        registry: &mut TaskRegistry,
        prepared: PreparedTask,
        leaf: EffectLeaf<A, LocalEffects>,
        ingress: Weak<CommandIngress<Command<S, A, Self>>>,
        provenance: Provenance,
    ) -> Self::Future {
        let sender =
            move |authority| move |action| send_effect_action(&ingress, &authority, action);

        match leaf {
            EffectLeaf::Send(action) => {
                let send = sender(provenance);
                registry.register_local(prepared, move |_| async move {
                    let _ = send(action).await;
                })
            }
            EffectLeaf::Run(operation) => {
                let sender = EffectSender::new(sender(provenance));
                registry.register_local(prepared, move |_| async move {
                    operation(sender).await;
                })
            }
            EffectLeaf::Fallible(operation) => {
                // Sending and failure routing each retain the same authority.
                let sender = EffectSender::new(sender(provenance.clone()));
                registry.register_local(prepared, move |registration| {
                    let cancellation = registration.handle();
                    async move {
                        operation(sender, provenance, cancellation).await;
                    }
                })
            }
            EffectLeaf::Future(future) => {
                let send = sender(provenance);
                registry.register_local(prepared, move |_| async move {
                    if let Some(action) = future.await {
                        let _ = send(action).await;
                    }
                })
            }
        }
    }
}

/// Attempts causal admission immediately, allocating a wait only for a full mailbox.
///
/// The waiting branch retains ingress until admission, then releases it before
/// awaiting commit. Cancelled or stopped authority never creates that future.
fn send_effect_action<S: 'static, A: 'static>(
    ingress: &Weak<CommandIngress<Command<S, A, Local>>>,
    provenance: &Provenance,
    action: A,
) -> SendFuture {
    match admit_effect_action::<S, A, Local>(ingress, provenance, action) {
        EffectAdmission::Receiving(receive) => EffectSendFuture::Receiving(receive),
        EffectAdmission::Cancelled => EffectSendFuture::Cancelled,
        EffectAdmission::Stopped => EffectSendFuture::StoreStopped,
        EffectAdmission::Full {
            ingress,
            command,
            receive,
        } => EffectSendFuture::Admitting(Box::pin(async move {
            ingress
                .send(command)
                .await
                .map_err(|_closed| EffectSendError::store_stopped())?;
            drop(ingress);
            Ok(receive)
        })),
    }
}

#[cfg(test)]
ordinary_test_items! {
mod tests {
    use std::cell::Cell;
    use std::rc::Rc;

    use super::{Effect, Feature, FeatureContext, Reduce, Reducer, WorkItem};
    use crate::store::scope::{ScopeRegistry, ScopeTransitionBatch};
    use crate::{DependencyValues, MonotonicInstant, ReduceContext};

    #[test]
    fn reducer_is_a_feature_with_borrowed_owner_local_values() {
        let calls = Cell::new(0);
        let value = Rc::new(Cell::new(2));
        let amount = 3;
        let mut state = &value;
        let mut reducer = Reduce::new(
            |state: &mut &Rc<Cell<usize>>, action: &usize, _: &mut ReduceContext<'_>| {
                calls.set(calls.get() + 1);
                state.set(state.get() + *action);
                Effect::send(*action)
            },
        );

        let mut dependencies = DependencyValues::new();
        let (_lifecycle, mut scopes) = ScopeRegistry::for_test();
        let mut transitions = ScopeTransitionBatch::default();
        let mut context = ReduceContext::new(
            &mut dependencies,
            MonotonicInstant::ZERO,
            &mut scopes,
            &mut transitions,
        );

        let effect = reducer.reduce(&mut state, &amount, &mut context);
        assert!(matches!(effect.program, crate::effect::Program::Send(action) if action == amount));

        let mut context = FeatureContext::new(context);
        reducer.update(&mut state, &amount, &mut context);
        let erased: &mut dyn Reducer<State = &Rc<Cell<usize>>, Action = usize> = &mut reducer;
        erased.update(&mut state, &amount, &mut context);
        context.effect(Effect::none());
        let mut work = context.into_work().into_iter();

        assert_eq!(calls.get(), 3);
        assert_eq!(state.get(), 11);
        assert!(matches!(work.next(), Some(WorkItem::Effect(effect))
            if matches!(effect.program, crate::effect::Program::Send(action) if action == amount)));
        assert!(matches!(work.next(), Some(WorkItem::Effect(effect))
            if matches!(effect.program, crate::effect::Program::Send(action) if action == amount)));
        assert!(work.next().is_none());
        assert!(transitions.is_empty());
    }
}
}
