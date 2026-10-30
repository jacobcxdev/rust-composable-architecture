//! Carries deferred typed tasks and their actor request authority.
//!
//! Endpoints freeze authority and admit owned requests. The transaction kernel
//! runs user code and stamps commits. Reply guards preserve the last known phase
//! when either stage unwinds. Shared and Local carriers use the same lifecycle
//! with different transfer bounds.

use std::fmt;
use std::hash::Hash;
use std::sync::{Arc, Weak};

use futures_channel::oneshot;

use super::command::{Command, Provenance};
use super::operation::Operation;
use super::scope::ScopeToken;
use super::{ActionTask, Ingress};

use crate::RuntimeFuture;
use crate::effect::CancellationIdentity;
use crate::effect::operation::{OperationDeclaration, OperationPolicy};

/// Declares deferred work with typed access to its feature.
pub struct Task<State, Action> {
    /// Owns captures until first poll invokes the factory or the task is dropped.
    run: Box<dyn FnOnce(TaskContext<State, Action>) -> RuntimeFuture + Send>,
    /// Selects named-generation registration without starting the factory.
    declaration: Option<OperationDeclaration>,
}

impl<S: 'static, A: 'static> Task<S, A> {
    /// Defers the factory and its future until the registered task is polled.
    ///
    /// Declaring a task does not call the factory. The first poll starts it
    /// after the originating transaction commits.
    pub fn run<F, Fut>(run: F) -> Self
    where
        F: FnOnce(TaskContext<S, A>) -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        Self {
            run: Box::new(move |context| Box::pin(async move { run(context).await })),
            declaration: None,
        }
    }

    /// Declares a named generation for one deferred typed task.
    pub fn run_named<K, F, Fut>(key: K, policy: OperationPolicy, run: F) -> Self
    where
        K: Hash + Eq + Send + Sync + 'static,
        F: FnOnce(TaskContext<S, A>) -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        let mut task = Self::run(run);
        task.declaration = Some(OperationDeclaration::new(key, policy));
        task
    }

    /// Separates registration policy from the still-unpolled task factory.
    pub(crate) fn into_parts(mut self) -> (Self, Option<OperationDeclaration>) {
        let declaration = self.declaration.take();
        (self, declaration)
    }

    /// Binds authority without calling the factory until the returned future is polled.
    pub(crate) fn start(self, context: TaskContext<S, A>) -> RuntimeFuture {
        (self.run)(context)
    }
}

impl<S, A> fmt::Debug for Task<S, A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Task").finish_non_exhaustive()
    }
}

/// Holds weak actor authority without owning causal work.
///
/// Cloning a context retains its captured authority, not the Store or unfinished
/// work. Child contexts keep their exact installed incarnation and do not
/// retarget after removal or replacement.
///
/// [`read`](Self::read), [`update`](Self::update) and [`send`](Self::send) submit
/// nothing until polled. Dropping their future before admission abandons the
/// request. After admission, dropping it only discards the reply. It does not
/// cancel or roll back actor work.
///
/// Receipts returned by `send` and `update` belong to the producing task's causal
/// lineage. Awaiting their completion from that task waits for its own completion.
pub struct TaskContext<State, Action> {
    /// Shares captured request authority without retaining a Store owner.
    endpoint: Arc<dyn TaskEndpoint<State, Action>>,
}

impl<S, A> Clone for TaskContext<S, A> {
    fn clone(&self) -> Self {
        Self {
            endpoint: Arc::clone(&self.endpoint),
        }
    }
}

impl<S, A> fmt::Debug for TaskContext<S, A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TaskContext").finish_non_exhaustive()
    }
}

/// Admits typed requests through root or projected authority without owning a Store.
pub(crate) trait TaskEndpoint<S, A>: Send + Sync {
    /// Validates a selected input boundary without executing its user callbacks.
    #[cfg(feature = "test-util")]
    fn inspect_input(
        &self,
        _: super::mounted::MountedInputIdentity,
        _: &super::scope::ScopeAuthority,
    ) -> Result<Option<super::mounted::SendInspection<A>>, TaskAccessError> {
        Ok(None)
    }

    /// Admits or rejects one owned request and leaves terminal reply ownership with it.
    fn submit(&self, request: TaskInput<S, A>) -> RuntimeFuture;

    /// Captures a named-operation view under this endpoint's frozen authority.
    fn operation(&self, identity: CancellationIdentity) -> Operation;

    /// Copies provenance and authority for output or structural access.
    fn output_source(&self) -> super::output::OutputSource;
}

#[cfg(feature = "test-util")]
impl<S, A> super::mounted::MountedInputInspection<A> for TaskContext<S, A> {
    fn inspect_input(
        &self,
        identity: super::mounted::MountedInputIdentity,
        authority: &super::scope::ScopeAuthority,
    ) -> Result<Option<super::mounted::SendInspection<A>>, TaskAccessError> {
        self.endpoint.inspect_input(identity, authority)
    }
}

impl<S, A> TaskContext<S, A> {
    /// Supplies the captured incarnation for checked structural removal.
    pub(super) fn removal_source(&self) -> super::output::OutputSource {
        self.endpoint.output_source()
    }

    /// Captures input control without reacquiring a replacement child.
    pub(super) fn input_gate(&self) -> super::InputGate {
        self.endpoint.output_source().authority.input_gate()
    }

    /// Lazily delivers an owned output from this context's captured incarnation.
    ///
    /// # Errors
    /// Reports the terminal failure and known progress. A recoverable value is
    /// returned in [`OutputError::Returned`](crate::OutputError::Returned).
    /// Earlier handlers may have run. Their mutations are not rolled back.
    pub fn output<O: Send + 'static>(
        &self,
        output: &super::output::Output<O>,
        value: O,
    ) -> impl Future<
        Output = Result<super::output::OutputDelivery<O>, super::output::OutputError<O>>,
    >
    + Send
    + 'static
    + use<S, A, O> {
        output.deliver(self.endpoint.output_source(), value)
    }

    /// Wraps a root or projected endpoint without strengthening its Store authority.
    pub(crate) fn from_endpoint(endpoint: Arc<dyn TaskEndpoint<S, A>>) -> Self {
        Self { endpoint }
    }

    /// Returns a weak named-operation view for this context's captured scope.
    ///
    /// Acquired child contexts retain their exact installed scope, incarnation
    /// and configured ancestor authority. They never retarget after removal or
    /// replacement. An arbitrary legacy `ScopedStore` projection does not carry
    /// this installed Feature authority.
    pub fn operation<K>(&self, key: K) -> Operation
    where
        K: Hash + Eq + Send + Sync + 'static,
    {
        self.endpoint.operation(OperationDeclaration::identity(key))
    }
}

/// Reports why typed actor access could not complete.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TaskAccessReason {
    /// The last Store owner closed admission.
    StoreClosed,
    /// Structural installation has not completed.
    NotReady,
    /// The current callback phase cannot perform this operation.
    PhaseDenied,
    /// The captured structural incarnation ended.
    ScopeEnded,
    /// The live captured target cannot satisfy the requested structural operation.
    /// For example, checked stack removal requires the target to be last.
    InvalidTarget,
    /// The destination input is closed.
    InputClosed,
    /// The producing task or its generation was cancelled.
    Cancelled,
    /// The actor failed before delivering a terminal reply.
    ActorFailed,
}

/// Describes how far this request progressed, without promising rollback.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TaskProgress {
    /// No selector, mapper or handler began.
    NotStarted,
    /// User code may have run, possibly mutating state.
    MayHaveRun,
    /// Mutation reached the irreversible revision stamp.
    Committed {
        /// Revision assigned to this mutation.
        revision: u64,
    },
}

/// A typed request's terminal failure and known progress.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TaskAccessError {
    /// Classifies rejection independently of whether user code already ran.
    reason: TaskAccessReason,
    /// Preserves the last known phase without promising rollback.
    progress: TaskProgress,
}

impl TaskAccessError {
    /// Combines a rejection category with the last phase the actor actually reached.
    pub(crate) const fn new(reason: TaskAccessReason, progress: TaskProgress) -> Self {
        Self { reason, progress }
    }

    /// Returns the terminal failure category.
    #[must_use]
    pub const fn reason(&self) -> TaskAccessReason {
        self.reason
    }

    /// Returns this request's last known progress.
    #[must_use]
    pub const fn progress(&self) -> TaskProgress {
        self.progress
    }
}

impl fmt::Display for TaskAccessError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "typed task access failed: {:?} ({:?})",
            self.reason, self.progress
        )
    }
}
impl std::error::Error for TaskAccessError {}

/// Recovers rejected actions, distinguishing failures after user entry.
pub enum TaskSendError<A> {
    /// User code never began; the original action is returned.
    Rejected {
        /// The same owned action supplied by the caller, without cloning.
        action: A,
        /// Explains the terminal failure before user entry.
        error: TaskAccessError,
    },
    /// User code may have run; no action recovery is promised.
    Failed {
        /// Reports the failure and last known phase after action recovery ended.
        error: TaskAccessError,
    },
}

impl<A> fmt::Debug for TaskSendError<A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Rejected { error, .. } => f
                .debug_struct("Rejected")
                .field("error", error)
                .finish_non_exhaustive(),
            Self::Failed { error } => f.debug_struct("Failed").field("error", error).finish(),
        }
    }
}
impl<A> fmt::Display for TaskSendError<A> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Rejected { error, .. } | Self::Failed { error } => error.fmt(f),
        }
    }
}
impl<A> std::error::Error for TaskSendError<A> {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Rejected { error, .. } | Self::Failed { error } => Some(error),
        }
    }
}

#[cfg(test)]
ordinary_test_items! {
mod error_tests {
    use super::{TaskAccessError, TaskAccessReason, TaskProgress, TaskSendError};
    use std::cell::Cell;
    use std::error::Error;

    #[test]
    fn send_failure_exposes_its_cause_without_constraining_the_action() {
        // This action is borrowed, non-Send and non-Debug. Error inspection must
        // not require stronger action bounds or consume the recoverable value.
        struct Action<'a>(&'a Cell<usize>);
        let state = Cell::new(7);
        let failures = [
            TaskSendError::Rejected {
                action: Action(&state),
                error: TaskAccessError::new(
                    TaskAccessReason::InputClosed,
                    TaskProgress::NotStarted,
                ),
            },
            TaskSendError::Failed {
                error: TaskAccessError::new(
                    TaskAccessReason::ActorFailed,
                    TaskProgress::Committed { revision: 9 },
                ),
            },
        ];

        for failure in &failures {
            let source = failure
                .source()
                .expect("the typed access failure remains in the standard error chain")
                .downcast_ref::<TaskAccessError>()
                .expect("the cause retains its concrete type");
            let expected = match failure {
                TaskSendError::Rejected { error, .. } | TaskSendError::Failed { error } => error,
            };
            assert!(
                std::ptr::eq(source, expected),
                "the cause is borrowed, not replaced"
            );
            assert!(source.source().is_none(), "the scalar cause ends the chain");
        }

        let [rejected, _failed] = failures;
        let TaskSendError::Rejected { action, error } = rejected else {
            panic!("cause inspection must preserve rejection ownership");
        };
        action.0.set(11);
        assert_eq!(state.get(), 11);
        assert_eq!(error.reason(), TaskAccessReason::InputClosed);
        assert_eq!(error.progress(), TaskProgress::NotStarted);
    }
}
}

#[cfg(test)]
ordinary_test_items! {
mod request_tests {
    use super::*;
    use crate::effect::lineage::LineageState;
    use crate::effect::operation::OperationAncestry;
    use crate::store::carriers::Shared;
    use crate::store::transaction::tests::Host;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::{Context, Waker};

    #[test]
    fn dropping_a_capacity_blocked_request_destroys_its_action_before_releasing_named_work() {
        struct Action {
            operation: Arc<LineageState>,
            drops: Arc<AtomicUsize>,
        }
        impl Drop for Action {
            fn drop(&mut self) {
                assert!(
                    !self.operation.is_finished(),
                    "named completion must follow concrete action destruction"
                );
                self.drops.fetch_add(1, Ordering::SeqCst);
            }
        }

        let mut host = Host::<(), Action, Shared>::new(
            (),
            Box::new(crate::Reduce::new(
                |(): &mut (), _: &Action, _: &mut crate::ReduceContext<'_>| {
                    panic!("the capacity-blocked action must never enter user code")
                },
            )),
        );
        let mut cx = Context::from_waker(Waker::noop());
        let mut filler = Box::pin(host.context().read(|()| ()));
        assert!(filler.as_mut().poll(&mut cx).is_pending());

        let lineage = LineageState::new(Weak::new());
        let operation = LineageState::new(Weak::new());
        let ancestry = OperationAncestry::default().with_generation(Arc::clone(&operation));
        let scope = host.scopes.root();
        let prepared = host.registry.prepare_with_operations(
            scope, Arc::clone(&lineage), &[], &ancestry,
        );
        let context = TaskContext::new(
            Weak::clone(&host.weak),
            scope,
            Provenance::Effect {
                scope,
                lineage: Arc::clone(&lineage),
                task: prepared.sender_token(),
                generations: prepared.generation_tokens(),
                operations: prepared.operation_ancestry(),
            },
            host.scopes.authority(scope),
        );
        let mut run = host.registry.register(prepared, std::future::pending());
        lineage.finish_registration();
        operation.finish_registration();
        assert!(run.as_mut().poll(&mut cx).is_pending());

        let drops = Arc::new(AtomicUsize::new(0));
        let mut send = Box::pin(context.send(Action {
            operation: Arc::clone(&operation),
            drops: Arc::clone(&drops),
        }));
        assert!(send.as_mut().poll(&mut cx).is_pending());
        lineage.cancel();
        drop(run);
        drop(host.collect());
        assert!(!operation.is_finished(), "the waiting request still owns named work");
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        drop(send);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert!(operation.is_finished());
        assert_eq!(host.version, 0);

        drop(host.next());
        assert!(filler.as_mut().poll(&mut cx).is_ready());
        assert!(matches!(host.receiver.try_recv(), Err(async_channel::TryRecvError::Empty)));
    }

    #[test]
    fn shared_read_is_lazy_and_survives_admitted_waiter_drop() {
        struct Payload(Arc<AtomicUsize>);
        impl Drop for Payload {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }

        for admitted in [false, true] {
            let mut host = Host::<usize, (), Shared>::new(
                7,
                Box::new(crate::Reduce::new(
                    |_: &mut usize, (): &(), _: &mut crate::ReduceContext<'_>| {
                        crate::Effect::none()
                    },
                )),
            );
            let calls = Arc::new(AtomicUsize::new(0));
            let drops = Arc::new(AtomicUsize::new(0));
            let queried = Arc::clone(&calls);
            let payload = Payload(Arc::clone(&drops));
            let mut read = Box::pin(host.context().read(move |state| {
                assert_eq!(*state, 7);
                queried.fetch_add(1, Ordering::SeqCst);
                payload
            }));
            assert!(matches!(host.receiver.try_recv(), Err(async_channel::TryRecvError::Empty)));
            assert_eq!(calls.load(Ordering::SeqCst), 0);
            if admitted {
                assert!(read.as_mut().poll(&mut Context::from_waker(Waker::noop())).is_pending());
            }
            drop(read);
            assert_eq!(calls.load(Ordering::SeqCst), 0);
            assert_eq!(drops.load(Ordering::SeqCst), usize::from(!admitted));
            if admitted {
                drop(host.next());
            }
            assert_eq!(calls.load(Ordering::SeqCst), usize::from(admitted));
            assert_eq!(drops.load(Ordering::SeqCst), 1);
            assert_eq!((host.state, host.version), (7, 0));
            assert!(matches!(host.receiver.try_recv(), Err(async_channel::TryRecvError::Empty)));
        }
    }

    #[cfg(feature = "local")]
    #[test]
    fn dropping_a_capacity_blocked_local_request_destroys_action_before_named_completion() {
        use crate::store::local;
        use std::cell::Cell;
        use std::rc::Rc;

        struct Action {
            operation: Arc<LineageState>,
            drops: Rc<Cell<usize>>,
            owner: std::thread::ThreadId,
        }
        impl Drop for Action {
            fn drop(&mut self) {
                assert_eq!(std::thread::current().id(), self.owner);
                assert!(
                    !self.operation.is_finished(),
                    "named completion must follow owner-local action destruction"
                );
                self.drops.set(self.drops.get() + 1);
            }
        }

        let mut host = Host::<(), Action, local::Local>::new(
            (),
            Box::new(local::Driver(local::Reduce::new(
                |(): &mut (), _: &Action, _: &mut crate::ReduceContext<'_>| {
                    panic!("the capacity-blocked local action must never enter user code")
                },
            ))),
        );
        let mut cx = Context::from_waker(Waker::noop());
        let mut filler = Box::pin(host.context().read(|()| ()));
        assert!(filler.as_mut().poll(&mut cx).is_pending());

        let lineage = LineageState::new(Weak::new());
        let operation = LineageState::new(Weak::new());
        let ancestry = OperationAncestry::default().with_generation(Arc::clone(&operation));
        let scope = host.scopes.root();
        let prepared = host.registry.prepare_with_operations(
            scope, Arc::clone(&lineage), &[], &ancestry,
        );
        let context = crate::local::TaskContext::new(
            Weak::clone(&host.weak),
            scope,
            Provenance::Effect {
                scope,
                lineage: Arc::clone(&lineage),
                task: prepared.sender_token(),
                generations: prepared.generation_tokens(),
                operations: prepared.operation_ancestry(),
            },
            host.scopes.authority(scope),
        );
        let mut run = host.registry.register_local(prepared, |_| std::future::pending());
        lineage.finish_registration();
        operation.finish_registration();
        assert!(run.as_mut().poll(&mut cx).is_pending());

        let drops = Rc::new(Cell::new(0));
        let mut send = Box::pin(context.send(Action {
            operation: Arc::clone(&operation),
            drops: Rc::clone(&drops),
            owner: std::thread::current().id(),
        }));
        assert!(send.as_mut().poll(&mut cx).is_pending());
        lineage.cancel();
        drop(run);
        drop(host.collect());
        assert!(!operation.is_finished(), "the waiting local request still owns named work");
        assert_eq!(drops.get(), 0);
        drop(send);
        assert_eq!(drops.get(), 1);
        assert!(operation.is_finished());
        assert_eq!(host.version, 0);

        drop(host.next());
        assert!(filler.as_mut().poll(&mut cx).is_ready());
        assert!(matches!(host.receiver.try_recv(), Err(async_channel::TryRecvError::Empty)));
    }

    #[cfg(feature = "local")]
    #[test]
    fn local_requests_are_lazy_and_survive_admitted_waiter_drop() {
        use crate::store::local;
        use std::cell::Cell;
        use std::rc::Rc;

        struct Payload(Rc<Cell<usize>>, std::thread::ThreadId);
        impl Drop for Payload {
            fn drop(&mut self) {
                assert_eq!(std::thread::current().id(), self.1);
                self.0.set(self.0.get() + 1);
            }
        }

        for admitted in [false, true] {
            let calls = Rc::new(Cell::new(0));
            let drops = Rc::new(Cell::new(0));
            let reduced = Rc::clone(&calls);
            let mut host = Host::<_, _, local::Local>::new(
                Rc::new(Cell::new(0)),
                Box::new(local::Driver(local::Reduce::new(
                    move |state: &mut Rc<Cell<usize>>,
                          _: &Payload,
                          _: &mut crate::ReduceContext<'_>| {
                        reduced.set(reduced.get() + 1);
                        state.set(state.get() + 3);
                        local::Effect::none()
                    },
                ))),
            );
            let context = host.context();
            let owner = std::thread::current().id();
            let entered = usize::from(admitted);
            let mut cx = Context::from_waker(Waker::noop());

            let queried = Rc::clone(&calls);
            let payload = Payload(Rc::clone(&drops), owner);
            let mut read = Box::pin(context.read(move |state| {
                assert_eq!(state.get(), 0);
                queried.set(queried.get() + 1);
                payload
            }));
            assert!(matches!(host.receiver.try_recv(), Err(async_channel::TryRecvError::Empty)));
            if admitted {
                assert!(read.as_mut().poll(&mut cx).is_pending());
            }
            drop(read);
            assert_eq!((calls.get(), drops.get()), (0, 1 - entered));
            if admitted {
                drop(host.next());
            }
            assert_eq!((calls.get(), drops.get()), (entered, 1));
            assert_eq!((host.state.get(), host.version), (0, 0));

            let updated = Rc::clone(&calls);
            let payload = Payload(Rc::clone(&drops), owner);
            let mut update = Box::pin(context.update(move |state| {
                updated.set(updated.get() + 1);
                state.set(state.get() + 2);
                payload
            }));
            assert!(matches!(host.receiver.try_recv(), Err(async_channel::TryRecvError::Empty)));
            if admitted {
                assert!(update.as_mut().poll(&mut cx).is_pending());
            }
            drop(update);
            assert_eq!((calls.get(), drops.get()), (entered, 2 - entered));
            if admitted {
                drop(host.next());
            }
            assert_eq!((calls.get(), drops.get()), (2 * entered, 2));
            assert_eq!((host.state.get(), host.version), (2 * entered, u64::from(admitted)));

            let mut send = Box::pin(context.send(Payload(Rc::clone(&drops), owner)));
            assert!(matches!(host.receiver.try_recv(), Err(async_channel::TryRecvError::Empty)));
            if admitted {
                assert!(send.as_mut().poll(&mut cx).is_pending());
            }
            drop(send);
            assert_eq!((calls.get(), drops.get()), (2 * entered, 3 - entered));
            if admitted {
                drop(host.next());
            }
            assert_eq!((calls.get(), drops.get()), (3 * entered, 3));
            assert_eq!((host.state.get(), host.version), (5 * entered, 2 * u64::from(admitted)));
            assert!(matches!(host.receiver.try_recv(), Err(async_channel::TryRecvError::Empty)));
        }
    }
}
}

/// Owns an update result, committed revision and causal task receipt.
#[derive(Debug)]
pub struct Committed<T> {
    /// Owns the synchronous update's result independently of Store state.
    value: T,
    /// Identifies the irreversible mutation stamp for this result.
    revision: u64,
    /// Tracks the inherited causal lineage, including the producing task.
    task: ActionTask,
}
impl<T> Committed<T> {
    /// Borrows the owned update result.
    pub const fn value(&self) -> &T {
        &self.value
    }

    /// Returns this update's committed revision.
    pub const fn revision(&self) -> u64 {
        self.revision
    }

    /// Borrows its causal task receipt.
    ///
    /// For an update issued by a task, this receipt includes that task's own
    /// completion. Await it outside that causal lineage.
    pub const fn task(&self) -> &ActionTask {
        &self.task
    }

    /// Returns the result, revision and causal receipt.
    pub fn into_parts(self) -> (T, u64, ActionTask) {
        (self.value, self.revision, self.task)
    }
}

impl<S: 'static, A: Send + 'static> TaskContext<S, A> {
    /// Reads state at an ordered mailbox position without creating a mutation.
    ///
    /// The query runs synchronously on the actor and returns an owned result.
    ///
    /// # Errors
    /// Returns the request's terminal failure with conservative progress.
    ///
    /// # Panics
    /// Panics if the internal owned-reply protocol is violated.
    pub fn read<T, F>(
        &self,
        query: F,
    ) -> impl Future<Output = Result<T, TaskAccessError>> + Send + 'static + use<T, F, S, A>
    where
        F: FnOnce(&S) -> T + Send + 'static,
        T: Send + 'static,
    {
        let endpoint = Arc::clone(&self.endpoint);
        async move {
            let (reply, receive) = ReplyGuard::channel();
            endpoint
                .submit(TaskInput::<S, A>::Read(Box::new(TypedRead {
                    reply,
                    query,
                })))
                .await;
            receive
                .await
                .expect("owned reply guard publishes a terminal result")
        }
    }

    /// Applies a synchronous update and returns its committed owned result.
    ///
    /// The update runs on the actor. Its result cannot borrow state.
    ///
    /// # Errors
    /// Returns the request's terminal failure. Mutation is never rolled back.
    ///
    /// # Panics
    /// Panics if the internal owned-reply protocol is violated.
    pub fn update<T, F>(
        &self,
        update: F,
    ) -> impl Future<Output = Result<Committed<T>, TaskAccessError>> + Send + 'static + use<T, F, S, A>
    where
        F: FnOnce(&mut S) -> T + Send + 'static,
        T: Send + 'static,
    {
        let endpoint = Arc::clone(&self.endpoint);
        async move {
            let (reply, receive) = ReplyGuard::channel();
            endpoint
                .submit(TaskInput::<S, A>::Update(Box::new(TypedUpdate {
                    reply,
                    update,
                })))
                .await;
            receive
                .await
                .expect("owned reply guard publishes a terminal result")
        }
    }

    /// Sends an action lazily, preserving ownership until user code begins.
    ///
    /// # Errors
    /// Returns the original action on rejection before entry, otherwise progress.
    ///
    /// # Panics
    /// Panics if the internal owned-reply protocol is violated.
    pub fn send(
        &self,
        action: A,
    ) -> impl Future<Output = Result<ActionTask, TaskSendError<A>>> + Send + 'static + use<S, A>
    {
        let endpoint = Arc::clone(&self.endpoint);
        async move {
            let (request, receive) = TypedSend::channel(action);
            // Close the receiver before dropping a capacity-blocked submission.
            // Otherwise its rejected action could outlive the named work lease.
            let mut pending = (receive, endpoint.submit(TaskRequest::Send(request)));
            pending.1.as_mut().await;
            let (receive, submitted) = pending;
            drop(submitted);
            receive
                .await
                .expect("owned reply guard publishes a terminal result")
        }
    }
}

/// One selected test boundary observes only its primary reply's scalar result.
#[cfg(feature = "test-util")]
#[derive(Clone, Default)]
pub(crate) struct TerminalReceipt(Arc<parking_lot::Mutex<Option<Result<(), TaskAccessError>>>>);
#[cfg(feature = "test-util")]
impl TerminalReceipt {
    /// Preserves the first reply even when later destruction also reports failure.
    pub(crate) fn record(&self, result: Result<(), TaskAccessError>) {
        self.0.lock().get_or_insert(result);
    }

    /// Copies the observed terminal scalar without waiting for completion.
    pub(crate) fn result(&self) -> Option<Result<(), TaskAccessError>> {
        *self.0.lock()
    }
}

#[cfg(all(
    test,
    not(loom),
    feature = "test-util",
    feature = "allocation-measurement"
))]
ordinary_test_items! {
mod terminal_receipt_tests {
    use super::{TaskAccessError, TaskAccessReason, TaskProgress, TerminalReceipt};

    fn measure<T>(run: impl FnOnce() -> T) -> (T, (u64, u64)) {
        let before = dhat::HeapStats::get();
        let result = run();
        let after = dhat::HeapStats::get();
        (result, (after.total_blocks - before.total_blocks, after.total_bytes - before.total_bytes))
    }

    #[test]
    fn terminal_receipt_first_access_is_allocation_free() {
        if crate::allocation_test::run_in_child(
            "store::task_context::terminal_receipt_tests::terminal_receipt_first_access_is_allocation_free",
        ) {
            return;
        }

        let profiler = dhat::Profiler::builder().testing().build();
        let (success, creation) = measure(TerminalReceipt::default);
        assert_eq!(creation.0, 1, "the selected boundary owns one receipt Arc");
        let ((), first_record) = measure(|| success.record(Ok(())));
        assert_eq!(first_record, (0, 0), "first record must not allocate another lock owner");
        let error = TaskAccessError::new(
            TaskAccessReason::ActorFailed,
            TaskProgress::Committed { revision: 7 },
        );
        let (success_result, later_access) = measure(|| {
            success.record(Err(error));
            success.result()
        });
        assert_eq!(success_result, Some(Ok(())), "the first success remains authoritative");
        assert_eq!(later_access, (0, 0));

        let (failure, creation) = measure(TerminalReceipt::default);
        assert_eq!(creation.0, 1);
        let (empty, first_read) = measure(|| failure.result());
        assert_eq!(empty, None);
        assert_eq!(first_read, (0, 0), "first observation must not allocate another lock owner");
        let (failure_result, later_access) = measure(|| {
            failure.record(Err(error));
            failure.record(Ok(()));
            failure.result()
        });
        assert_eq!(failure_result, Some(Err(error)), "the first exact error remains authoritative");
        assert_eq!(later_access, (0, 0));
        drop(success);
        drop(failure);
        drop(profiler);
    }
}
}

/// Supplies the same unwind publication for legacy replies and structural turns.
#[cfg(feature = "test-util")]
pub(crate) struct TerminalGuard {
    /// Optionally records the selected boundary's first terminal scalar.
    receipt: Option<TerminalReceipt>,
    /// Supplies the last observed phase if unwinding prevents normal publication.
    progress: TaskProgress,
}
#[cfg(feature = "test-util")]
impl TerminalGuard {
    /// Starts optional observation before any user entry.
    pub(crate) const fn new(receipt: Option<TerminalReceipt>) -> Self {
        Self {
            receipt,
            progress: TaskProgress::NotStarted,
        }
    }

    /// Selects the observer before the request enters its monitored boundary.
    pub(crate) fn attach(&mut self, receipt: TerminalReceipt) {
        self.receipt = Some(receipt);
    }

    /// Marks the last boundary where user code may begin without rollback.
    pub(crate) fn mark_entered(&mut self) {
        self.progress = TaskProgress::MayHaveRun;
    }

    /// Records the irreversible revision before publication can unwind.
    pub(crate) fn mark_committed(&mut self, revision: u64) {
        self.progress = TaskProgress::Committed { revision };
    }

    /// Publishes a scalar before waking any waiter for the corresponding reply.
    pub(crate) fn record(&self, result: Result<(), TaskAccessError>) {
        if let Some(receipt) = &self.receipt {
            receipt.record(result);
        }
    }

    /// Supplies the last phase when a legacy reply needs a typed failure.
    pub(crate) const fn progress(&self) -> TaskProgress {
        self.progress
    }

    /// Publishes unwind failure unless an earlier terminal result already won.
    pub(crate) fn fail(&self) {
        self.record(Err(TaskAccessError::new(
            TaskAccessReason::ActorFailed,
            self.progress,
        )));
    }
}
#[cfg(feature = "test-util")]
impl Drop for TerminalGuard {
    fn drop(&mut self) {
        self.fail();
    }
}

/// Publishes one terminal reply, including the last known phase during unwind.
pub(crate) struct ReplyGuard<T> {
    /// Taken before waking the receiver so a panicking waker cannot send twice.
    sender: Option<oneshot::Sender<Result<T, TaskAccessError>>>,
    /// Records how far user execution or commit progressed before a failure.
    progress: TaskProgress,
    #[cfg(feature = "test-util")]
    /// Observes terminal progress even when the response waiter has disappeared.
    terminal: Option<TerminalReceipt>,
}
impl<T> ReplyGuard<T> {
    /// Attaches scalar observation before the monitored actor boundary runs.
    #[cfg(feature = "test-util")]
    fn attach_terminal(&mut self, terminal: TerminalReceipt) {
        self.terminal = Some(terminal);
    }

    /// Advances progress before a projection or callback can run user code.
    pub(super) fn mark_entered(&mut self) {
        self.progress = TaskProgress::MayHaveRun;
    }

    /// Creates the sole reply owner and a freely droppable response waiter.
    pub(super) fn channel() -> (Self, oneshot::Receiver<Result<T, TaskAccessError>>) {
        let (sender, receiver) = oneshot::channel();
        (
            Self {
                sender: Some(sender),
                progress: TaskProgress::NotStarted,
                #[cfg(feature = "test-util")]
                terminal: None,
            },
            receiver,
        )
    }

    /// Publishes once, dropping the owned result if its waiter has gone away.
    pub(super) fn complete(mut self, result: Result<T, TaskAccessError>) {
        #[cfg(feature = "test-util")]
        if let Some(terminal) = &self.terminal {
            terminal.record(result.as_ref().map(|_| ()).map_err(|error| *error));
        }
        let sender = self
            .sender
            .take()
            .expect("consuming completion owns its reply");
        let _ = sender.send(result);
    }
}
impl<T> Drop for ReplyGuard<T> {
    fn drop(&mut self) {
        if let Some(sender) = self.sender.take() {
            let error = TaskAccessError::new(TaskAccessReason::ActorFailed, self.progress);
            #[cfg(feature = "test-util")]
            if let Some(terminal) = &self.terminal {
                terminal.record(Err(error));
            }
            let _ = sender.send(Err(error));
        }
    }
}

/// An actor-ready query whose callback never receives mutable state.
pub(crate) trait ReadCall<S> {
    /// Runs the synchronous query and publishes its owned result.
    fn execute(self: Box<Self>, state: &S);

    /// Publishes failure before destroying the uncalled query and its captures.
    fn reject(self: Box<Self>, error: TaskAccessError);
}

/// Marks user entry before an endpoint's mounted projection runs.
///
/// Actor-ready calls already own that boundary and cannot be projected again.
pub(crate) trait ReadInput<S>: ReadCall<S> {
    /// Preserves conservative progress if projection fails before the query runs.
    fn mark_entered(&mut self);
}

/// Owns the concrete query until actor execution or pre-entry rejection.
struct TypedRead<F, T> {
    /// Publishes the result or failure before uncalled captures are destroyed.
    reply: ReplyGuard<T>,
    /// Owns the one-shot query and its captures until actor execution or rejection.
    query: F,
}
impl<S, F: FnOnce(&S) -> T, T> ReadInput<S> for TypedRead<F, T> {
    fn mark_entered(&mut self) {
        self.reply.progress = TaskProgress::MayHaveRun;
    }
}
impl<S, F: FnOnce(&S) -> T, T> ReadCall<S> for TypedRead<F, T> {
    fn execute(self: Box<Self>, state: &S) {
        let Self { mut reply, query } = *self;
        reply.progress = TaskProgress::MayHaveRun;
        let value = query(state);
        reply.complete(Ok(value));
    }
    fn reject(self: Box<Self>, error: TaskAccessError) {
        let Self { reply, query } = *self;
        reply.complete(Err(error));
        drop(query);
    }
}

/// Carries an entered mutation's reply across commit stamping and publication.
pub(crate) trait CommitReply {
    /// Records the revision before later publication can fail.
    fn mark_committed(&mut self, revision: u64);

    /// Publishes the stamped result with its inherited causal completion receipt.
    fn complete(self, task: ActionTask);
}

/// Allows heterogeneous owned results to cross the same actor commit pipeline.
trait ErasedCommitReply {
    /// Records the revision without inspecting or copying the erased result.
    fn mark_committed(&mut self, revision: u64);

    /// Consumes the boxed result owner after the transaction commits.
    fn complete(self: Box<Self>, task: ActionTask);
}

/// Keeps erased Shared replies transferable without imposing `Sync` on results.
pub(crate) struct SharedReply(Box<dyn ErasedCommitReply + Send>);
impl SharedReply {
    /// Adapts an existing typed commit reply to the actor's erased carrier.
    pub(crate) fn new(reply: impl CommitReply + Send + 'static) -> Self {
        Self(Box::new(ReplyValue(reply)))
    }
}

/// Bridges a consuming typed reply to the object-safe boxed completion method.
struct ReplyValue<R>(R);
impl<R: CommitReply> ErasedCommitReply for ReplyValue<R> {
    fn mark_committed(&mut self, revision: u64) {
        self.0.mark_committed(revision);
    }
    fn complete(self: Box<Self>, task: ActionTask) {
        self.0.complete(task);
    }
}
impl CommitReply for SharedReply {
    fn mark_committed(&mut self, revision: u64) {
        self.0.mark_committed(revision);
    }
    fn complete(self, task: ActionTask) {
        self.0.complete(task);
    }
}

/// Holds an update result until the actor stamps and publishes its transaction.
struct PendingReply<T> {
    /// Preserves failure progress until the stamped result is published.
    reply: ReplyGuard<Committed<T>>,
    /// Keeps the update result owned while commit and observation finish.
    value: T,
    /// Stamped before completion. Zero is never a committed revision.
    revision: u64,
}
impl<T> ErasedCommitReply for PendingReply<T> {
    fn mark_committed(&mut self, revision: u64) {
        self.revision = revision;
        self.reply.progress = TaskProgress::Committed { revision };
    }
    fn complete(self: Box<Self>, task: ActionTask) {
        let Self {
            reply,
            value,
            revision,
        } = *self;
        reply.complete(Ok(Committed {
            value,
            revision,
            task,
        }));
    }
}

/// Owns a synchronous state mutation and its result through actor preparation.
pub(crate) trait UpdateCall<S> {
    /// Retains the result until the actor finishes stamping and publication.
    type Reply: CommitReply;

    /// Observes the selected boundary even if callback or publication unwinds.
    #[cfg(feature = "test-util")]
    fn attach_terminal(&mut self, terminal: TerminalReceipt);

    /// Marks entry before a mounted selector or reaction capture can run user code.
    fn mark_entered(&mut self);

    /// Mutates state once, or reports a checked projection rejection.
    fn apply(self: Box<Self>, state: &mut S) -> Apply<Self::Reply>;

    /// Publishes failure before destroying the uncalled update and its captures.
    fn reject(self: Box<Self>, error: TaskAccessError);
}

/// Separates a pending commit result from rejection by a checked projection.
pub(crate) enum Apply<R> {
    /// The owned result must still pass through commit stamping and publication.
    Applied(R),
    /// No commit reply remains, so the actor must not stamp a revision for it.
    Rejected(TaskAccessError),
}

/// Keeps the concrete update and reply together until the actor takes ownership.
struct TypedUpdate<F, T> {
    /// Carries failure progress until the update produces a pending commit reply.
    reply: ReplyGuard<Committed<T>>,
    /// Owns the one-shot mutation and its captures until actor entry or rejection.
    update: F,
}
impl<S, F: FnOnce(&mut S) -> T + Send, T: Send + 'static> UpdateCall<S> for TypedUpdate<F, T> {
    type Reply = SharedReply;
    #[cfg(feature = "test-util")]
    fn attach_terminal(&mut self, terminal: TerminalReceipt) {
        self.reply.attach_terminal(terminal);
    }
    fn mark_entered(&mut self) {
        self.reply.progress = TaskProgress::MayHaveRun;
    }
    fn apply(self: Box<Self>, state: &mut S) -> Apply<SharedReply> {
        let Self { mut reply, update } = *self;
        reply.progress = TaskProgress::MayHaveRun;
        let value = update(state);
        Apply::Applied(SharedReply(Box::new(PendingReply {
            reply,
            value,
            revision: 0,
        })))
    }
    fn reject(self: Box<Self>, error: TaskAccessError) {
        let Self { reply, update } = *self;
        reply.complete(Err(error));
        drop(update);
    }
}

/// Owns an action while rejection can still return it without cloning.
pub(crate) struct TypedSend<A> {
    /// Permits safe extraction from Drop. Only consuming methods take this payload,
    /// so an externally observable request always owns it.
    pending: Option<PendingSend<A>>,
    #[cfg(feature = "test-util")]
    /// Records the selected boundary before response publication can wake a waiter.
    terminal: Option<TerminalReceipt>,
}

/// Couples the recoverable action with its sole response sender.
struct PendingSend<A> {
    /// Remains recoverable until selectors, mappers or handlers may consume it.
    action: A,
    /// Transfers either the original action or the eventual causal receipt.
    reply: oneshot::Sender<Result<ActionTask, TaskSendError<A>>>,
}

/// Tracks progress after user entry, when the original action may be consumed.
pub(crate) struct EnteredSend<A> {
    /// Taken before publication so a panicking waker cannot trigger a second reply.
    reply: Option<oneshot::Sender<Result<ActionTask, TaskSendError<A>>>>,
    /// Distinguishes possible user execution from an irreversible commit stamp.
    progress: TaskProgress,
    #[cfg(feature = "test-util")]
    /// Preserves first-terminal observation independently of response ownership.
    terminal: Option<TerminalReceipt>,
}
impl<A> TypedSend<A> {
    /// Observes terminal progress without requiring the response waiter to survive.
    #[cfg(feature = "test-util")]
    pub(crate) fn attach_terminal(&mut self, terminal: TerminalReceipt) {
        self.terminal = Some(terminal);
    }

    /// Keeps the original action intact while admission and authority are checked.
    pub(super) fn channel(
        action: A,
    ) -> (
        Self,
        oneshot::Receiver<Result<ActionTask, TaskSendError<A>>>,
    ) {
        let (reply, receive) = oneshot::channel();
        (
            Self {
                pending: Some(PendingSend { action, reply }),
                #[cfg(feature = "test-util")]
                terminal: None,
            },
            receive,
        )
    }

    /// Recovers an unpublished immediate-admission failure's original action.
    ///
    /// No receipt has escaped, so dropping its reply cannot acknowledge work.
    pub(super) fn into_action(mut self) -> A {
        let PendingSend { action, reply } = self
            .pending
            .take()
            .expect("unadmitted request owns its payload");
        drop(reply);
        action
    }
    #[cfg(feature = "test-util")]
    /// Inspects the still-unentered action without consuming its recovery path.
    pub(crate) fn action(&self) -> &A {
        &self
            .pending
            .as_ref()
            .expect("borrowed request owns its payload")
            .action
    }

    /// Moves action ownership into user entry and permanently ends recovery.
    pub(crate) fn enter(mut self) -> (A, EnteredSend<A>) {
        let PendingSend { action, reply } = self
            .pending
            .take()
            .expect("consuming request owns its payload");
        (
            action,
            EnteredSend {
                reply: Some(reply),
                progress: TaskProgress::MayHaveRun,
                #[cfg(feature = "test-util")]
                terminal: self.terminal.take(),
            },
        )
    }

    /// Returns the original action without cloning or invoking user code.
    pub(crate) fn reject(mut self, error: TaskAccessError) {
        #[cfg(feature = "test-util")]
        if let Some(terminal) = &self.terminal {
            terminal.record(Err(error));
        }
        let PendingSend { action, reply } = self
            .pending
            .take()
            .expect("consuming rejection owns its payload");
        let _ = reply.send(Err(TaskSendError::Rejected { action, error }));
    }
}
impl<A> CommitReply for EnteredSend<A> {
    fn mark_committed(&mut self, revision: u64) {
        self.progress = TaskProgress::Committed { revision };
    }
    fn complete(mut self, task: ActionTask) {
        #[cfg(feature = "test-util")]
        if let Some(terminal) = &self.terminal {
            terminal.record(Ok(()));
        }
        let sender = self
            .reply
            .take()
            .expect("consuming completion owns its reply");
        let _ = sender.send(Ok(task));
    }
}
impl<A> EnteredSend<A> {
    /// Publishes an entered failure without claiming the action can be recovered.
    pub(crate) fn fail(mut self, error: TaskAccessError) {
        #[cfg(feature = "test-util")]
        if let Some(terminal) = &self.terminal {
            terminal.record(Err(error));
        }
        let reply = self.reply.take().expect("consuming failure owns its reply");
        let _ = reply.send(Err(TaskSendError::Failed { error }));
    }
}
impl<A> Drop for TypedSend<A> {
    fn drop(&mut self) {
        if let Some(PendingSend { action, reply }) = self.pending.take() {
            let error =
                TaskAccessError::new(TaskAccessReason::ActorFailed, TaskProgress::NotStarted);
            #[cfg(feature = "test-util")]
            if let Some(terminal) = &self.terminal {
                terminal.record(Err(error));
            }
            let _ = reply.send(Err(TaskSendError::Rejected { action, error }));
        }
    }
}
impl<A> Drop for EnteredSend<A> {
    fn drop(&mut self) {
        if let Some(reply) = self.reply.take() {
            let error = TaskAccessError::new(TaskAccessReason::ActorFailed, self.progress);
            #[cfg(feature = "test-util")]
            if let Some(terminal) = &self.terminal {
                terminal.record(Err(error));
            }
            let _ = reply.send(Err(TaskSendError::Failed { error }));
        }
    }
}

/// Shares admission and rejection across typed requests and execution carriers.
pub(crate) enum TaskRequest<
    S,
    A,
    C: super::carriers::Carriers<S, A> = super::carriers::Shared,
    Q: ReadCall<S> + ?Sized = <C as super::carriers::Carriers<S, A>>::Read,
> {
    /// Owns a query before or after endpoint projection, as selected by `Q`.
    Read(Box<Q>),
    /// Owns a mutation that will produce its result at the actor boundary.
    Update(Box<C::Update>),
    /// Keeps action recovery until the actor begins user entry.
    Send(TypedSend<A>),
}

/// Selects endpoint input queries, which must mark entry before projection.
pub(crate) type TaskInput<S, A, C = super::carriers::Shared> =
    TaskRequest<S, A, C, <C as super::carriers::Carriers<S, A>>::ReadInput>;

impl<S, A, C: super::carriers::Carriers<S, A>, Q: ReadCall<S> + ?Sized> TaskRequest<S, A, C, Q> {
    /// Publishes rejection while each concrete request still owns its captures.
    pub(crate) fn reject(self, error: TaskAccessError) {
        match self {
            Self::Read(call) => call.reject(error),
            Self::Update(call) => call.reject(error),
            Self::Send(call) => call.reject(error),
        }
    }
}

/// Captures root authority and producer identity without keeping ingress alive.
struct RootEndpoint<S, A> {
    /// Accesses admission without preventing final Store-owner shutdown.
    ingress: Weak<Ingress<S, A>>,
    /// Identifies the captured root installation for named-operation views.
    scope: ScopeToken,
    /// Carries the producing task, causal lineage and named generations.
    provenance: Provenance,
    /// Keeps frozen lifecycle and incarnation checks for each admitted request.
    authority: super::scope::ScopeAuthority,
}
impl<S: Send + 'static, A: Send + 'static> TaskEndpoint<S, A> for RootEndpoint<S, A> {
    fn output_source(&self) -> super::output::OutputSource {
        super::output::OutputSource {
            scope: self.scope,
            provenance: self.provenance.clone(),
            authority: self.authority.clone(),
        }
    }

    fn submit(&self, request: TaskInput<S, A>) -> RuntimeFuture {
        let ingress = Weak::clone(&self.ingress);
        let provenance = self.provenance.clone();
        let authority = self.authority.clone();
        Box::pin(submit_request(ingress, provenance, authority, request))
    }

    fn operation(&self, identity: CancellationIdentity) -> Operation {
        Operation::from_frozen_identity(
            Weak::clone(&self.ingress),
            self.provenance.clone(),
            self.scope,
            self.authority.clone(),
            identity,
        )
    }
}

/// Retains named work before bounded admission and releases it after the payload.
///
/// Cancellation is rechecked after retention. The actor checks captured authority
/// again before user entry, including requests that waited for queue capacity.
async fn submit_request<S, A, C: super::carriers::Carriers<S, A>>(
    ingress: Weak<super::ingress::CommandIngress<Command<S, A, C>>>,
    provenance: Provenance,
    authority: super::scope::ScopeAuthority,
    request: TaskInput<S, A, C>,
) {
    if let Err(error) = authority.admission() {
        request.reject(error);
        return;
    }
    if !provenance.is_live() {
        request.reject(TaskAccessError::new(
            TaskAccessReason::Cancelled,
            TaskProgress::NotStarted,
        ));
        return;
    }
    let operation_work = match provenance.operations().try_retain_work() {
        Ok(work) => work,
        Err((reason, partial_work)) => {
            request.reject(TaskAccessError::new(reason, TaskProgress::NotStarted));
            drop(partial_work);
            return;
        }
    };
    #[cfg(all(test, feature = "test-util"))]
    ordinary_test_items! {
        super::mounted::mounted_admission_test_hook::after_retain();
    }
    // Field order owns the concrete request before its completion leases while
    // bounded admission is suspended or the caller abandons this future.
    let owned = (request, provenance, authority, operation_work);
    if !owned.1.is_live() {
        let (request, _, _, operation_work) = owned;
        request.reject(TaskAccessError::new(
            TaskAccessReason::Cancelled,
            TaskProgress::NotStarted,
        ));
        drop(operation_work);
        return;
    }
    #[cfg(all(test, feature = "test-util"))]
    ordinary_test_items! {
        super::mounted::mounted_admission_test_hook::before_ingress();
    }
    let Some(ingress) = ingress.upgrade() else {
        let (request, _, _, operation_work) = owned;
        request.reject(TaskAccessError::new(
            TaskAccessReason::StoreClosed,
            TaskProgress::NotStarted,
        ));
        drop(operation_work);
        return;
    };
    if let Err(async_channel::SendError((request, _, _, operation_work))) = ingress
        .send_map(owned, |(request, provenance, authority, operation_work)| {
            Command::Task {
                request: match request {
                    TaskRequest::Read(call) => TaskRequest::Read(C::into_read(call)),
                    TaskRequest::Update(call) => TaskRequest::Update(call),
                    TaskRequest::Send(call) => TaskRequest::Send(call),
                },
                provenance,
                authority,
                operation_work,
            }
        })
        .await
    {
        request.reject(TaskAccessError::new(
            TaskAccessReason::StoreClosed,
            TaskProgress::NotStarted,
        ));
        drop(operation_work);
    }
}
impl<S: Send + 'static, A: Send + 'static> TaskContext<S, A> {
    /// Binds the root actor's frozen scope and producer identity to a weak endpoint.
    pub(crate) fn new(
        ingress: Weak<Ingress<S, A>>,
        scope: ScopeToken,
        provenance: Provenance,
        authority: super::scope::ScopeAuthority,
    ) -> Self {
        Self {
            endpoint: Arc::new(RootEndpoint {
                ingress,
                scope,
                provenance,
                authority,
            }),
        }
    }
}

#[cfg(feature = "local")]
pub(super) mod local {
    //! Keeps captures and results on their owner thread while sharing request phases.

    use super::{
        ActionTask, Apply, Command, CommitReply, Committed, ErasedCommitReply, PendingReply,
        Provenance, ReplyGuard, TaskAccessError, TaskInput, TaskProgress, TaskRequest,
        TaskSendError, TypedRead, TypedSend, UpdateCall, Weak, submit_request,
    };
    use crate::store::ingress::CommandIngress;
    use crate::store::local::{Local, LocalFuture};
    use std::rc::Rc;

    /// Owns asynchronous work that is polled and dropped on its host thread.
    pub struct Task<S, A> {
        /// Owns non-`Send` captures until the deferred factory runs or is dropped.
        run: Box<dyn FnOnce(TaskContext<S, A>) -> LocalFuture>,
        /// Selects named-generation registration without invoking the factory.
        declaration: Option<super::OperationDeclaration>,
    }
    impl<S: 'static, A: 'static> Task<S, A> {
        /// Defers the factory and its future until the registered task is polled.
        ///
        /// Declaring a task does not call the factory. The first poll starts it
        /// after the originating transaction commits.
        pub fn run<F, Fut>(run: F) -> Self
        where
            F: FnOnce(TaskContext<S, A>) -> Fut + 'static,
            Fut: Future<Output = ()> + 'static,
        {
            Self {
                run: Box::new(move |context| Box::pin(async move { run(context).await })),
                declaration: None,
            }
        }

        /// Declares a named generation for one deferred typed task.
        pub fn run_named<K, F, Fut>(key: K, policy: super::OperationPolicy, run: F) -> Self
        where
            K: std::hash::Hash + Eq + Send + Sync + 'static,
            F: FnOnce(TaskContext<S, A>) -> Fut + 'static,
            Fut: Future<Output = ()> + 'static,
        {
            let mut task = Self::run(run);
            task.declaration = Some(super::OperationDeclaration::new(key, policy));
            task
        }

        /// Separates registration policy from the still-unpolled local factory.
        pub(crate) fn into_parts(mut self) -> (Self, Option<super::OperationDeclaration>) {
            let declaration = self.declaration.take();
            (self, declaration)
        }

        /// Binds local authority without invoking the factory until first poll.
        pub(crate) fn start(self, context: TaskContext<S, A>) -> LocalFuture {
            (self.run)(context)
        }
    }

    /// Retains frozen owner-local task authority, not a Store owner.
    ///
    /// Cloning a context does not retain unfinished work. Child contexts keep
    /// their exact installed incarnation and do not retarget after replacement.
    ///
    /// [`read`](Self::read), [`update`](Self::update) and [`send`](Self::send)
    /// submit nothing until polled. Dropping their future before admission
    /// abandons the request. After admission, dropping it only discards the
    /// reply. It does not cancel or roll back actor work.
    ///
    /// Receipts returned by `send` and `update` belong to the producing task's
    /// causal lineage. Awaiting their completion from that task waits for its
    /// own completion.
    pub struct TaskContext<S, A> {
        /// Shares owner-local request authority without retaining a Store owner.
        endpoint: Rc<dyn Endpoint<S, A>>,
    }
    impl<S, A> std::fmt::Debug for Task<S, A> {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("Task").finish_non_exhaustive()
        }
    }
    impl<S, A> std::fmt::Debug for TaskContext<S, A> {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("TaskContext").finish_non_exhaustive()
        }
    }
    impl<S, A> Clone for TaskContext<S, A> {
        fn clone(&self) -> Self {
            Self {
                endpoint: Rc::clone(&self.endpoint),
            }
        }
    }

    /// Admits owner-local requests without requiring transferable captures or results.
    pub(crate) trait Endpoint<S, A> {
        /// Validates a selected input boundary without executing user callbacks.
        #[cfg(feature = "test-util")]
        fn inspect_input(
            &self,
            _: crate::store::mounted::MountedInputIdentity,
            _: &crate::store::scope::ScopeAuthority,
        ) -> Result<Option<crate::store::mounted::SendInspection<A>>, TaskAccessError> {
            Ok(None)
        }

        /// Admits or rejects an owned request while preserving terminal reply ownership.
        fn submit(&self, request: TaskInput<S, A, Local>) -> LocalFuture;

        /// Copies captured authority and provenance for output or structural access.
        fn output_source(&self) -> crate::store::output::OutputSource;

        /// Captures a named-operation view under this endpoint's frozen authority.
        fn operation(&self, identity: super::CancellationIdentity) -> crate::local::Operation;
    }
    #[cfg(feature = "test-util")]
    impl<S, A> crate::store::mounted::MountedInputInspection<A> for TaskContext<S, A> {
        fn inspect_input(
            &self,
            identity: crate::store::mounted::MountedInputIdentity,
            authority: &crate::store::scope::ScopeAuthority,
        ) -> Result<Option<crate::store::mounted::SendInspection<A>>, TaskAccessError> {
            self.endpoint.inspect_input(identity, authority)
        }
    }

    /// Keeps local root authority and producer identity without owning the Store.
    struct RootEndpoint<S, A> {
        /// Accesses local admission without keeping the Store alive.
        ingress: Weak<CommandIngress<Command<S, A, Local>>>,
        /// Identifies the captured root installation for named-operation views.
        scope: super::ScopeToken,
        /// Carries producer identity, causal lineage and named generations.
        provenance: Provenance,
        /// Retains frozen lifecycle and incarnation checks for local requests.
        authority: super::super::scope::ScopeAuthority,
    }
    impl<S: 'static, A: 'static> Endpoint<S, A> for RootEndpoint<S, A> {
        fn operation(&self, identity: super::CancellationIdentity) -> crate::local::Operation {
            crate::local::Operation::from_frozen_identity(
                Weak::clone(&self.ingress),
                self.provenance.clone(),
                self.scope,
                self.authority.clone(),
                identity,
            )
        }
        fn output_source(&self) -> crate::store::output::OutputSource {
            crate::store::output::OutputSource {
                scope: self.scope,
                provenance: self.provenance.clone(),
                authority: self.authority.clone(),
            }
        }
        fn submit(&self, request: TaskInput<S, A, Local>) -> LocalFuture {
            Box::pin(submit_request(
                Weak::clone(&self.ingress),
                self.provenance.clone(),
                self.authority.clone(),
                request,
            ))
        }
    }
    impl<S: 'static, A: 'static> TaskContext<S, A> {
        /// Returns a weak named-operation view for this context's captured scope.
        ///
        /// Acquired child contexts retain their installed incarnation and
        /// configured ancestor authority. They never retarget after replacement.
        pub fn operation<K>(&self, key: K) -> crate::local::Operation
        where
            K: std::hash::Hash + Eq + Send + Sync + 'static,
        {
            self.endpoint
                .operation(super::OperationDeclaration::identity(key))
        }

        /// Supplies the captured incarnation for checked structural removal.
        pub(in crate::store) fn removal_source(&self) -> crate::store::output::OutputSource {
            self.endpoint.output_source()
        }

        /// Uses captured input authority without reacquiring a replacement child.
        pub(in crate::store) fn input_gate(&self) -> crate::InputGate {
            self.endpoint.output_source().authority.input_gate()
        }

        /// Lazily delivers one owned value through captured owner-local ancestors.
        ///
        /// # Errors
        /// Reports the terminal failure and known progress. A recoverable value
        /// is returned in [`OutputError::Returned`](crate::OutputError::Returned).
        /// Earlier handlers may have run. Their mutations are not rolled back.
        pub fn output<O: 'static>(
            &self,
            output: &crate::local::Output<O>,
            value: O,
        ) -> impl Future<Output = Result<crate::OutputDelivery<O>, crate::OutputError<O>>>
        + 'static
        + use<O, S, A> {
            output.deliver(self.endpoint.output_source(), value)
        }

        /// Wraps a root or projected endpoint without strengthening Store ownership.
        pub(crate) fn from_endpoint(endpoint: Rc<dyn Endpoint<S, A>>) -> Self {
            Self { endpoint }
        }

        /// Binds frozen root scope and producer identity to a weak local endpoint.
        pub(crate) fn new(
            ingress: Weak<CommandIngress<Command<S, A, Local>>>,
            scope: super::ScopeToken,
            provenance: Provenance,
            authority: super::super::scope::ScopeAuthority,
        ) -> Self {
            Self {
                endpoint: Rc::new(RootEndpoint {
                    ingress,
                    scope,
                    provenance,
                    authority,
                }),
            }
        }

        /// Reads state at an ordered mailbox position without creating a mutation.
        ///
        /// The query runs synchronously on the actor and returns an owned result.
        ///
        /// # Errors
        /// Returns the request's terminal failure with conservative progress.
        ///
        /// # Panics
        /// Panics if an internal endpoint fails to publish its terminal reply.
        pub fn read<T, F>(
            &self,
            query: F,
        ) -> impl Future<Output = Result<T, TaskAccessError>> + 'static + use<T, F, S, A>
        where
            F: FnOnce(&S) -> T + 'static,
            T: 'static,
        {
            let endpoint = Rc::clone(&self.endpoint);
            async move {
                let (reply, receive) = ReplyGuard::channel();
                endpoint
                    .submit(TaskInput::<S, A, Local>::Read(Box::new(TypedRead {
                        reply,
                        query,
                    })))
                    .await;
                receive
                    .await
                    .expect("owned reply guard publishes a terminal result")
            }
        }

        /// Applies a synchronous update and returns its committed owned result.
        ///
        /// The update runs on the actor. Its result cannot borrow state.
        ///
        /// # Errors
        /// Returns the request's terminal failure. Mutation is never rolled back.
        ///
        /// # Panics
        /// Panics if an internal endpoint fails to publish its terminal reply.
        pub fn update<T, F>(
            &self,
            update: F,
        ) -> impl Future<Output = Result<Committed<T>, TaskAccessError>> + 'static + use<T, F, S, A>
        where
            F: FnOnce(&mut S) -> T + 'static,
            T: 'static,
        {
            let endpoint = Rc::clone(&self.endpoint);
            async move {
                let (reply, receive) = ReplyGuard::channel();
                endpoint
                    .submit(TaskInput::<S, A, Local>::Update(Box::new(LocalUpdate {
                        reply,
                        update,
                    })))
                    .await;
                receive
                    .await
                    .expect("owned reply guard publishes a terminal result")
            }
        }

        /// Sends an action lazily, preserving ownership until user code begins.
        ///
        /// # Errors
        /// Returns the original action on rejection before entry, otherwise progress.
        ///
        /// # Panics
        /// Panics if an internal endpoint fails to publish its terminal reply.
        pub fn send(
            &self,
            action: A,
        ) -> impl Future<Output = Result<ActionTask, TaskSendError<A>>> + 'static + use<S, A>
        {
            let endpoint = Rc::clone(&self.endpoint);
            async move {
                let (request, receive) = TypedSend::channel(action);
                // Close the receiver before dropping a capacity-blocked submission.
                // Otherwise its rejected action could outlive the named work lease.
                let mut pending = (receive, endpoint.submit(TaskRequest::Send(request)));
                pending.1.as_mut().await;
                let (receive, submitted) = pending;
                drop(submitted);
                receive
                    .await
                    .expect("owned reply guard publishes a terminal result")
            }
        }
    }

    /// Erases an owner-local result while reusing Shared commit and reply phases.
    pub(crate) struct LocalReply(Box<dyn ErasedCommitReply>);
    impl LocalReply {
        /// Adapts a typed reply without adding `Send` bounds or copying its result.
        pub(crate) fn new(reply: impl CommitReply + 'static) -> Self {
            Self(Box::new(super::ReplyValue(reply)))
        }
    }
    impl CommitReply for LocalReply {
        fn mark_committed(&mut self, revision: u64) {
            self.0.mark_committed(revision);
        }
        fn complete(self, task: ActionTask) {
            self.0.complete(task);
        }
    }

    /// Owns a non-`Send` callback and result until the local actor commits or rejects it.
    struct LocalUpdate<F, T> {
        /// Preserves failure progress until the result becomes a pending commit reply.
        reply: ReplyGuard<Committed<T>>,
        /// Owns the non-`Send` mutation and captures until actor entry or rejection.
        update: F,
    }
    impl<S, F: FnOnce(&mut S) -> T, T: 'static> UpdateCall<S> for LocalUpdate<F, T> {
        type Reply = LocalReply;
        #[cfg(feature = "test-util")]
        fn attach_terminal(&mut self, terminal: super::TerminalReceipt) {
            self.reply.attach_terminal(terminal);
        }
        fn mark_entered(&mut self) {
            self.reply.progress = TaskProgress::MayHaveRun;
        }
        fn apply(self: Box<Self>, state: &mut S) -> Apply<LocalReply> {
            let Self { mut reply, update } = *self;
            reply.progress = TaskProgress::MayHaveRun;
            let value = update(state);
            Apply::Applied(LocalReply(Box::new(PendingReply {
                reply,
                value,
                revision: 0,
            })))
        }
        fn reject(self: Box<Self>, error: TaskAccessError) {
            let Self { reply, update } = *self;
            reply.complete(Err(error));
            drop(update);
        }
    }
}
