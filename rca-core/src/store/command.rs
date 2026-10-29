//! Owns requests between mailbox admission and serial actor dispatch.
//!
//! Envelopes carry authority and completion ownership, but do not validate state
//! or invoke callbacks. The actor checks authority before materialising deferred
//! actions. Payloads and reply channels precede causal work guards so discarding
//! an unprocessed command cannot announce completion before its payload dies.

use std::sync::Arc;
use std::sync::Weak;

use super::ActionTask;
use crate::ShutdownError;
use crate::effect::lineage::LineageState;
use crate::effect::operation::{OperationAncestry, OperationWork};
use crate::effect::registry::{GenerationToken, TaskCompletion};
use crate::observation::wait::WaitCancellation;
use crate::store::scope::{HandleRegistrationCommandCore, HandleScope, ScopeToken};

/// A committed action's causal task, or a legacy acknowledgement failure.
pub(crate) type CommitResult = Result<ActionTask, CommitError>;

/// Publishes one commit result, with optional test-visible progress metadata.
///
/// Losing the receiver does not cancel the action or its causal work.
pub(crate) struct CommitSender(
    /// Absent for actor-owned timers, which have no waiting caller.
    Option<futures_channel::oneshot::Sender<CommitResult>>,
    /// Records terminal progress before channel destruction can wake a waiter.
    #[cfg(feature = "test-util")]
    super::task_context::TerminalGuard,
);

/// Waits for commit acknowledgement, not for the returned task to finish.
pub(crate) struct CommitReceiver(
    /// Owns the receive side; dropping it only abandons acknowledgement.
    futures_channel::oneshot::Receiver<CommitResult>,
);

// Preserve the former commit channel's unwind traits at this fixed-payload
// boundary. Sending consumes the sender; receiving moves the result out, never
// exposing mutable payload references. futures-channel 0.3.34 releases its
// internal lock guards on unwind and marks completion before waking a receiver.
// A panicking waker cannot expose partially mutated ActionTask state here.
impl std::panic::UnwindSafe for CommitSender {}
impl std::panic::RefUnwindSafe for CommitSender {}
impl std::panic::UnwindSafe for CommitReceiver {}
impl std::panic::RefUnwindSafe for CommitReceiver {}

/// Creates a single-use acknowledgement channel without retaining store ownership.
pub(crate) fn commit_channel() -> (CommitSender, CommitReceiver) {
    let (sender, receiver) = futures_channel::oneshot::channel();
    (
        CommitSender(
            Some(sender),
            #[cfg(feature = "test-util")]
            super::task_context::TerminalGuard::new(None),
        ),
        CommitReceiver(receiver),
    )
}

impl CommitSender {
    /// Creates a reply sink for actor-owned timers, which have no commit waiter.
    pub(crate) const fn disconnected() -> Self {
        Self(
            None,
            #[cfg(feature = "test-util")]
            super::task_context::TerminalGuard::new(None),
        )
    }

    /// Attaches the assertion driver's terminal record before actor execution.
    #[cfg(feature = "test-util")]
    pub(crate) fn attach_terminal(&mut self, terminal: super::task_context::TerminalReceipt) {
        self.1.attach(terminal);
    }

    /// Marks possible user-code entry; rejection can no longer imply rollback.
    #[cfg(feature = "test-util")]
    pub(crate) fn mark_entered(&mut self) {
        self.1.mark_entered();
    }

    /// Records the irreversible revision stamp before publishing acknowledgement.
    #[cfg(feature = "test-util")]
    pub(crate) fn mark_committed(&mut self, revision: u64) {
        self.1.mark_committed(revision);
    }

    /// Publishes a more precise terminal outcome than channel loss alone provides.
    #[cfg(feature = "test-util")]
    pub(crate) fn record_terminal(&self, result: Result<(), super::task_context::TaskAccessError>) {
        self.1.record(result);
    }

    /// Publishes acknowledgement, returning the unchanged result if no waiter exists.
    ///
    /// Test metadata is recorded first. A receiver waker may panic after the channel
    /// has accepted the result; that unwind does not retract the committed task.
    pub(crate) fn send(self, result: CommitResult) -> Result<(), CommitResult> {
        #[cfg(feature = "test-util")]
        self.1.record(result.as_ref().map(|_| ()).map_err(|error| {
            use super::task_context::{TaskAccessError, TaskAccessReason};
            TaskAccessError::new(
                match error {
                    CommitError::Cancelled => TaskAccessReason::Cancelled,
                    CommitError::ScopeEnded => TaskAccessReason::ScopeEnded,
                    CommitError::InputClosed => TaskAccessReason::InputClosed,
                },
                self.1.progress(),
            )
        }));
        #[cfg(feature = "test-util")]
        let sender = {
            let mut this = self;
            this.0.take()
        };
        #[cfg(not(feature = "test-util"))]
        let sender = self.0;
        match sender {
            Some(sender) => sender.send(result),
            None => Err(result),
        }
    }
}

#[cfg(feature = "test-util")]
impl Drop for CommitSender {
    fn drop(&mut self) {
        // Publish metadata before dropping the channel can wake its receiver.
        self.1.fail();
    }
}

impl std::future::Future for CommitReceiver {
    type Output = Result<CommitResult, futures_channel::oneshot::Canceled>;

    fn poll(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        std::pin::Pin::new(&mut self.get_mut().0).poll(cx)
    }
}

#[cfg(test)]
ordinary_test_items! {
impl CommitReceiver {
    pub(crate) fn try_recv(
        &mut self,
    ) -> Result<Option<CommitResult>, futures_channel::oneshot::Canceled> {
        self.0.try_recv()
    }
}
}

/// Rejections understood by the legacy action acknowledgement protocol.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CommitError {
    /// Effect authority ended, or another failure was mapped to legacy cancellation.
    Cancelled,
    /// The captured scoped handle or installed destination is no longer current.
    ScopeEnded,
    /// The installed destination no longer accepts this input.
    InputClosed,
}

/// Separates external handle validation from effect cancellation authority.
///
/// Cloning this metadata does not retain causal work. Commands carry a separate
/// [`OperationWork`] guard when their lifetime must delay named completion.
#[derive(Clone)]
pub(crate) enum Provenance {
    /// External submissions are not tied to a producing task's lifetime.
    External {
        /// An optional legacy state-selection incarnation, checked by the actor.
        handle: Option<HandleScope>,
    },
    /// Effect submissions must pass captured task and generation liveness checks.
    Effect {
        /// Installation incarnation checked against the actor's scope registry.
        scope: ScopeToken,
        /// Root causal cancellation state; this reference alone adds no pending work.
        lineage: Arc<LineageState>,
        /// Weak authority prevents queued commands from extending a finished task.
        task: Weak<TaskCompletion>,
        /// Immutable cancellation generations captured when the task was registered.
        generations: Box<[GenerationToken]>,
        /// Named-operation ancestry used when admitting descendant work.
        operations: OperationAncestry,
    },
}

impl Provenance {
    /// Borrows external handle authority without applying its state selector.
    pub(crate) fn handle(&self) -> Option<&HandleScope> {
        match self {
            Self::External { handle } => handle.as_ref(),
            Self::Effect { .. } => None,
        }
    }

    /// Distinguishes effect-originated actions for test-side admission rules.
    #[cfg(feature = "test-util")]
    pub(crate) fn is_effect(&self) -> bool {
        matches!(self, Self::Effect { .. })
    }

    /// Borrows causal cancellation state without adding a work lease.
    pub(crate) fn lineage(&self) -> Option<&Arc<LineageState>> {
        match self {
            Self::External { .. } => None,
            Self::Effect { lineage, .. } => Some(lineage),
        }
    }

    /// Borrows the effect's captured generations; external submissions have none.
    pub(crate) fn generations(&self) -> &[GenerationToken] {
        match self {
            Self::External { .. } => &[],
            Self::Effect { generations, .. } => generations,
        }
    }

    /// Shares named ancestry without itself retaining pending operation work.
    pub(crate) fn operations(&self) -> OperationAncestry {
        match self {
            Self::External { .. } => OperationAncestry::default(),
            Self::Effect { operations, .. } => operations.clone(),
        }
    }

    /// Returns an effect's installation token for the actor's registry check.
    pub(crate) fn scope(&self) -> Option<ScopeToken> {
        match self {
            Self::External { .. } => None,
            Self::Effect { scope, .. } => Some(*scope),
        }
    }

    /// Samples task and generation liveness without validating installation state.
    ///
    /// The actor must separately check `scope` and `handle`. This is a rejection
    /// check, not a reservation: concurrent cancellation can follow a live result.
    pub(crate) fn is_live(&self) -> bool {
        match self {
            // Handle validity is checked separately to preserve ScopeEnded replies.
            Self::External { .. } => true,
            Self::Effect {
                lineage,
                task,
                generations,
                ..
            } => {
                !lineage.is_cancelled()
                    && task.upgrade().is_some_and(|task| task.is_running())
                    && generations.iter().all(GenerationToken::is_live)
            }
        }
    }
}

/// One admitted actor request, with carrier-specific callback thread bounds.
///
/// Named work guards follow their payloads so ordinary field destruction keeps
/// completion behind payload cleanup. [`Self::close`] preserves the same order
/// when it explicitly rejects typed requests during shutdown.
pub(crate) enum Command<
    State,
    Action,
    C: super::carriers::Carriers<State, Action> = super::carriers::Shared,
> {
    /// Reconciles installed structure without a user action.
    Structural,
    /// Acquires a currently installed endpoint without invoking its handler.
    Acquire(Box<C::Acquire>),
    /// Performs an ordered structural read over installed state.
    Fold(Box<C::Fold>),
    /// Delivers a typed output to its captured parent authority.
    Output {
        /// Owns the output and its terminal reply until dispatch or rejection.
        call: Box<C::Output>,
        /// Captures the producing task's cancellation and generation checks.
        provenance: Provenance,
        /// Captures the destination installation; it must not retarget a replacement.
        authority: super::scope::ScopeAuthority,
        /// Keeps named completion pending until the output and reply are released.
        operation_work: OperationWork,
    },
    /// Invokes a captured installed Feature through its typed dispatch carrier.
    Mounted {
        /// Retains the input and deferred mapper until actor-side validation.
        call: Box<C::Mounted>,
        /// Distinguishes external input from task-bound cancellation authority.
        provenance: Provenance,
        /// Identifies the installed destination and required ancestor authority.
        authority: super::scope::ScopeAuthority,
        /// Outlives input destruction and successor-work registration.
        operation_work: OperationWork,
    },
    /// Executes a typed read, update or send against captured task authority.
    Task {
        /// Owns the callback or input and its typed reply.
        request: super::task_context::TaskRequest<State, Action, C>,
        /// Preserves the producing task's cancellation checks.
        provenance: Provenance,
        /// Restricts access to the captured installation incarnation.
        authority: super::scope::ScopeAuthority,
        /// Retains named work until request cleanup or successor registration.
        operation_work: OperationWork,
    },
    /// Performs an ordered named-operation query or cancellation.
    Operation {
        /// Owns the requested operation and its terminal reply.
        request: super::operation::OperationRequest,
        /// Carries execution-family-specific source authority and ancestry.
        metadata: C::OperationMetadata,
    },
    /// Reduces an action after checking its provenance.
    Action(ActionCommand<Action>),
    /// Adds caller-owned cancellation to an otherwise ordinary action command.
    OwnedAction(Box<OwnedActionCommand<Action>>),
    /// Runs an unscoped legacy read after earlier admitted commands.
    Read(Box<C::LegacyRead>),
    /// Runs a legacy read only while its captured handle remains current.
    ScopedRead {
        /// Validated before the read callback is invoked.
        scope: HandleScope,
        /// Owns the checked read callback and response channel.
        command: Box<C::CheckedRead>,
    },
    /// Registers a legacy scoped handle against actor-owned state.
    RegisterHandle(HandleRegistrationCommandCore<C::HandleSelector>),
    /// Registers or queries an unscoped observation in actor order.
    Observation(Box<C::ObservationCommand>),
    /// Applies an observation request only while its handle remains current.
    ScopedObservation {
        /// Validated before registration; observations retain only weak authority.
        scope: HandleScope,
        /// Owns the observation request and its response channel.
        command: Box<C::ObservationCommand>,
    },
    /// Returns actor diagnostics at this point in command order.
    #[cfg(feature = "test-util")]
    Snapshot(async_channel::Sender<crate::testing::ActorDiagnostics>),
    /// Closes admission, rejects queued commands and waits for actor teardown.
    Shutdown(TerminalReply<C::Terminal>),
}

/// Erases a shared embedder without requiring child action storage to be shared.
pub(crate) trait ActionEmbed<ChildAction, Action>: Send + Sync {
    /// Consumes a child action after the actor has validated its scoped handle.
    fn embed(&self, action: ChildAction) -> Action;
}

impl<State, Action, C: super::carriers::Carriers<State, Action>> Command<State, Action, C> {
    /// Discards a closed command, preserving typed terminal results and ownership.
    pub(super) fn close(self) {
        use super::fold::FoldCall;
        use super::mount::AcquireCall;
        use super::mounted::MountedCall;
        use super::output::OutputCall;
        match self {
            Self::Acquire(call) => call.close(),
            Self::Fold(call) => call.reject(super::task_context::TaskAccessError::new(
                super::task_context::TaskAccessReason::StoreClosed,
                super::task_context::TaskProgress::NotStarted,
            )),
            Self::Output {
                call,
                operation_work,
                ..
            } => {
                call.reject(super::task_context::TaskAccessError::new(
                    super::task_context::TaskAccessReason::StoreClosed,
                    super::task_context::TaskProgress::NotStarted,
                ));
                drop(operation_work);
            }
            Self::Mounted {
                call,
                operation_work,
                ..
            } => {
                call.reject(super::task_context::TaskAccessError::new(
                    super::task_context::TaskAccessReason::StoreClosed,
                    super::task_context::TaskProgress::NotStarted,
                ));
                drop(operation_work);
            }
            Self::Task {
                request,
                operation_work,
                ..
            } => {
                request.reject(super::task_context::TaskAccessError::new(
                    super::task_context::TaskAccessReason::StoreClosed,
                    super::task_context::TaskProgress::NotStarted,
                ));
                drop(operation_work);
            }
            Self::Operation { request, .. } => {
                request.reject(super::task_context::TaskAccessError::new(
                    super::task_context::TaskAccessReason::StoreClosed,
                    super::task_context::TaskProgress::NotStarted,
                ));
            }
            // Existing commands retain their established destruction protocol.
            command => drop(command),
        }
    }
}

/// Shares a child-to-parent callback without retaining any child action.
pub(crate) struct ScopedActionEmbed<ChildAction, Embed> {
    /// User callback; construction and mailbox admission must never invoke it.
    embed: Embed,
    /// Models consumed actions, not shared action storage or its auto-trait bounds.
    child: std::marker::PhantomData<fn(ChildAction)>,
}

impl<ChildAction, Embed> ScopedActionEmbed<ChildAction, Embed> {
    /// Stores the callback without running it or owning a child action.
    pub(crate) fn new(embed: Embed) -> Self {
        Self {
            embed,
            child: std::marker::PhantomData,
        }
    }

    /// Runs the embedder once, consuming the original child action.
    pub(crate) fn apply<Action>(&self, action: ChildAction) -> Action
    where
        Embed: Fn(ChildAction) -> Action,
    {
        (self.embed)(action)
    }
}

impl<Action, ChildAction, Embed> ActionEmbed<ChildAction, Action>
    for ScopedActionEmbed<ChildAction, Embed>
where
    ChildAction: Send + 'static,
    Embed: Fn(ChildAction) -> Action + Send + Sync,
{
    fn embed(&self, action: ChildAction) -> Action {
        self.apply(action)
    }
}

/// Retains a typed action and its embedder until actor-side validation.
///
/// Field order drops an uncalled child action before the embedder capture,
/// including stale-handle rejection and shutdown drains.
pub(super) struct DeferredAction<ChildAction, Embed: ?Sized> {
    /// Original owned input, recovered unchanged when admission fails.
    pub(super) action: ChildAction,
    /// Shared callback ownership released only after the uncalled input is dropped.
    pub(super) embed: Arc<Embed>,
}

impl<ChildAction, Embed: ?Sized> DeferredAction<ChildAction, Embed> {
    /// Releases envelope storage before returning ownership or invoking the embedder.
    #[expect(
        clippy::boxed_local,
        reason = "consume the allocation before invoking the user embedder"
    )]
    pub(super) fn into_inner(self: Box<Self>) -> Self {
        *self
    }
}

/// Owns an erased child action without embedding it during admission.
pub(crate) trait DeferredActionCall<Action>: Send {
    /// Releases envelope storage, then embeds the action after actor validation.
    fn into_action(self: Box<Self>) -> Action;
}

impl<Action, ChildAction, Embed> DeferredActionCall<Action> for DeferredAction<ChildAction, Embed>
where
    ChildAction: Send,
    Embed: ActionEmbed<ChildAction, Action> + ?Sized,
{
    fn into_action(self: Box<Self>) -> Action {
        let Self { action, embed } = self.into_inner();
        embed.embed(action)
    }
}

/// Separates validation from the point at which embedding can run user code.
pub(crate) enum ActionPayload<Action> {
    /// Already expressed in the root reducer's action type.
    Ready(Action),
    /// Still owns a child action and an uncalled embedder.
    Deferred(Box<dyn DeferredActionCall<Action>>),
}

impl<Action> ActionPayload<Action> {
    /// Consumes the payload, invoking a deferred embedder only on the actor.
    pub(crate) fn into_action(self) -> Action {
        match self {
            Self::Ready(action) => action,
            Self::Deferred(action) => action.into_action(),
        }
    }
}

/// Stores ready actions or external scoped input without duplicating provenance.
///
/// Deferred payloads only originate at external scoped handles. Their separate
/// variant avoids reserving both an embedder and effect provenance in every slot.
pub(crate) enum ActionInput<Action> {
    /// An action whose caller has already selected the root action type.
    Ready {
        /// Owned reducer input, never cloned by the transport.
        action: Action,
        /// Authority that the actor must check before reduction.
        provenance: Provenance,
    },
    /// External scoped input whose embedder must wait for actor validation.
    Deferred {
        /// Owns both the child input and the uncalled embedder.
        action: Box<dyn DeferredActionCall<Action>>,
        /// Optional installed handle checked before embedding.
        handle: Option<HandleScope>,
    },
}

impl<Action> ActionInput<Action> {
    /// Separates payload and authority without invoking the deferred embedder.
    pub(crate) fn into_parts(self) -> (ActionPayload<Action>, Provenance) {
        match self {
            Self::Ready { action, provenance } => (ActionPayload::Ready(action), provenance),
            Self::Deferred { action, handle } => (
                ActionPayload::Deferred(action),
                Provenance::External { handle },
            ),
        }
    }
}

/// Carries action ownership through validation, reduction and acknowledgement.
pub(crate) struct ActionCommand<Action> {
    /// Drops before reply and named-work guards on unprocessed-command cleanup.
    pub(crate) input: ActionInput<Action>,
    /// Acknowledges commit; abandoning the receiver does not retract admission.
    pub(crate) reply: CommitSender,
    /// Lets the assertion gate observe the same committed causal task.
    #[cfg(feature = "test-util")]
    pub(crate) assertion_reply:
        Option<async_channel::Sender<Result<ActionTask, crate::StoreStopped>>>,
    /// Last field: named completion follows payload and reply destruction.
    pub(crate) operation_work: OperationWork,
}

/// Holds only materialised actions while the assertion gate controls admission.
///
/// This invariant prevents test-side inspection from embedding scoped input.
#[cfg(feature = "test-util")]
pub(crate) struct ReadyActionCommand<Action> {
    /// The already-embedded input available for assertion inspection.
    pub(crate) action: Action,
    /// Original authority, preserved while the gate delays actor processing.
    pub(crate) provenance: Provenance,
    /// Original commit waiter, not a second submission.
    pub(crate) reply: CommitSender,
    /// Assertion-side recipient of the committed causal task.
    pub(crate) assertion_reply:
        Option<async_channel::Sender<Result<ActionTask, crate::StoreStopped>>>,
    /// Keeps named completion pending through gate retention and payload cleanup.
    pub(crate) operation_work: OperationWork,
}

#[cfg(feature = "test-util")]
impl<Action> ReadyActionCommand<Action> {
    /// Restores the ordinary envelope without embedding or cloning the action.
    pub(crate) fn into_command(self) -> ActionCommand<Action> {
        ActionCommand {
            input: ActionInput::Ready {
                action: self.action,
                provenance: self.provenance,
            },
            reply: self.reply,
            assertion_reply: self.assertion_reply,
            operation_work: self.operation_work,
        }
    }
}

/// Associates an admitted action with its caller-owned cancellation signal.
pub(crate) struct OwnedActionCommand<Action> {
    /// Owns the original input, acknowledgement and causal work.
    pub(crate) command: ActionCommand<Action>,
    /// Checked before reduction and propagated to the resulting action task.
    pub(crate) cancellation: Arc<WaitCancellation>,
}

impl<Action> ActionCommand<Action> {
    /// Wraps unscoped external input without effect authority or named-work leases.
    pub(crate) fn external(action: Action, reply: CommitSender) -> Self {
        Self {
            input: ActionInput::Ready {
                action,
                provenance: Provenance::External { handle: None },
            },
            reply,
            #[cfg(feature = "test-util")]
            assertion_reply: None,
            operation_work: OperationWork::default(),
        }
    }

    /// Moves captured effect authority and its existing work lease into the command.
    #[expect(
        clippy::too_many_arguments,
        reason = "explicit effect provenance preserves effect-only construction and operation-work ownership"
    )]
    pub(crate) fn effect(
        action: Action,
        reply: CommitSender,
        scope: ScopeToken,
        lineage: Arc<LineageState>,
        task: Weak<TaskCompletion>,
        generations: Box<[GenerationToken]>,
        operations: OperationAncestry,
        operation_work: OperationWork,
    ) -> Self {
        Self {
            input: ActionInput::Ready {
                action,
                provenance: Provenance::Effect {
                    scope,
                    lineage,
                    task,
                    generations,
                    operations,
                },
            },
            reply,
            #[cfg(feature = "test-util")]
            assertion_reply: None,
            operation_work,
        }
    }

    /// Allocates a typed pair while keeping embedding deferred until actor validation.
    pub(crate) fn scoped<ChildAction, Embed>(
        action: ChildAction,
        reply: CommitSender,
        embed: Arc<Embed>,
        handle: Option<HandleScope>,
    ) -> Self
    where
        ChildAction: Send + 'static,
        Embed: ActionEmbed<ChildAction, Action> + ?Sized + 'static,
    {
        Self::scoped_boxed(Box::new(DeferredAction { action, embed }), reply, handle)
    }

    /// Moves a preallocated pair into its erased envelope without invoking user code.
    ///
    /// Typed admission calls this while holding the queue lock, so it must only
    /// unsize and move storage; it must not run the embedder or drop either input.
    pub(super) fn scoped_boxed<ChildAction, Embed>(
        action: Box<DeferredAction<ChildAction, Embed>>,
        reply: CommitSender,
        handle: Option<HandleScope>,
    ) -> Self
    where
        ChildAction: Send + 'static,
        Embed: ActionEmbed<ChildAction, Action> + ?Sized + 'static,
    {
        Self {
            input: ActionInput::Deferred { action, handle },
            reply,
            #[cfg(feature = "test-util")]
            assertion_reply: None,
            operation_work: OperationWork::default(),
        }
    }
}

/// Selects how teardown reports completion after releasing actor-owned resources.
pub(crate) enum TerminalReply<Callback: ?Sized> {
    /// Reports shutdown success or failure without returning state.
    Shutdown(async_channel::Sender<Result<(), ShutdownError>>),
    /// Receives the final owned state; dropping an unprocessed command skips it.
    State(Box<Callback>),
}

#[cfg(all(test, not(loom)))]
ordinary_test_items! {
mod reply_contract_tests {
    use super::*;
    use std::future::Future;
    use std::mem::{align_of, size_of, size_of_val};
    use std::pin::Pin;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::{Context, Poll, Wake, Waker};

    #[cfg(feature = "local")]
    #[test]
    fn local_terminal_callback_keeps_original_state_and_owner_drops() {
        use std::cell::Cell;
        use std::rc::Rc;

        struct Owned {
            owner: std::thread::ThreadId,
            drops: Rc<Cell<usize>>,
        }
        impl Drop for Owned {
            fn drop(&mut self) {
                assert_eq!(std::thread::current().id(), self.owner);
                self.drops.set(self.drops.get() + 1);
            }
        }

        let owner = std::thread::current().id();
        let state_drops = Rc::new(Cell::new(0));
        let capture_drops = Rc::new(Cell::new(0));
        let calls = Rc::new(Cell::new(0));
        let state = Rc::new(Owned { owner, drops: Rc::clone(&state_drops) });
        let original = Rc::as_ptr(&state).addr();
        let capture = Owned { owner, drops: Rc::clone(&capture_drops) };
        let seen = Rc::clone(&calls);
        let callback = Box::new(move |state: Rc<Owned>| {
            assert_eq!(std::thread::current().id(), owner);
            assert_eq!(Rc::as_ptr(&state).addr(), original);
            assert_eq!(Rc::strong_count(&state), 1);
            seen.set(seen.get() + 1);
            drop(capture);
        });
        let allocation = std::ptr::from_ref(&*callback).cast::<()>();
        let payload_size = size_of_val(&*callback);
        let command: Command<Rc<Owned>, (), super::super::local::Local> =
            Command::Shutdown(TerminalReply::<<super::super::local::Local as super::super::carriers::Carriers<Rc<Owned>, ()>>::Terminal>::State(callback));
        let Command::Shutdown(TerminalReply::State(callback)) = command else {
            panic!("state reply must retain its variant");
        };
        assert_eq!(std::ptr::from_ref(&*callback).cast::<()>(), allocation);
        assert_eq!(size_of_val(&*callback), payload_size);
        assert_eq!(state_drops.get(), 0);
        assert_eq!(capture_drops.get(), 0);
        callback(state);
        assert_eq!(calls.get(), 1);
        assert_eq!(capture_drops.get(), 1);
        assert_eq!(state_drops.get(), 1);
    }

    #[cfg(feature = "local")]
    #[test]
    fn closed_local_terminal_command_drops_capture_without_calling_it() {
        use std::cell::Cell;
        use std::rc::Rc;

        struct Capture {
            owner: std::thread::ThreadId,
            drops: Rc<Cell<usize>>,
        }
        impl Drop for Capture {
            fn drop(&mut self) {
                assert_eq!(std::thread::current().id(), self.owner);
                self.drops.set(self.drops.get() + 1);
            }
        }
        type Callback =
            <super::super::local::Local as super::super::carriers::Carriers<(), ()>>::Terminal;
        let drops = Rc::new(Cell::new(0));
        let calls = Rc::new(Cell::new(0));
        let capture = Rc::new(Capture {
            owner: std::thread::current().id(),
            drops: Rc::clone(&drops),
        });
        let witness = Rc::downgrade(&capture);
        let seen = Rc::clone(&calls);
        let command: Command<(), (), super::super::local::Local> =
            Command::Shutdown(TerminalReply::<Callback>::State(Box::new(move |()| {
                seen.set(seen.get() + 1);
                drop(capture);
            })));
        command.close();
        assert_eq!(calls.get(), 0);
        assert_eq!(drops.get(), 1);
        assert!(witness.upgrade().is_none());
    }

    #[test]
    fn shared_terminal_command_keeps_send_and_original_allocations() {
        fn assert_send<T: Send>(_: &T) {}
        type Callback =
            <super::super::carriers::Shared as super::super::carriers::Carriers<Vec<u8>, ()>>::Terminal;
        let state = vec![3, 5, 8];
        let state_address = state.as_ptr().addr();
        let calls = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&calls);
        let callback = Box::new(move |state: Vec<u8>| {
            assert_eq!(state.as_ptr().addr(), state_address);
            assert_eq!(state, [3, 5, 8]);
            seen.fetch_add(1, Ordering::SeqCst);
        });
        let allocation = std::ptr::from_ref(&*callback).cast::<()>();
        let payload_size = size_of_val(&*callback);
        let command: Command<Vec<u8>, ()> =
            Command::Shutdown(TerminalReply::<Callback>::State(callback));
        assert_send(&command);
        let Command::Shutdown(TerminalReply::State(callback)) = command else {
            panic!("state reply must retain its variant");
        };
        assert_eq!(std::ptr::from_ref(&*callback).cast::<()>(), allocation);
        assert_eq!(size_of_val(&*callback), payload_size);
        callback(state);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[cfg(all(feature = "allocation-measurement", feature = "local"))]
    #[test]
    fn terminal_callbacks_allocate_one_existing_payload_box() {
        fn measure<T>(make: impl FnOnce() -> T) -> (u64, u64) {
            let before = dhat::HeapStats::get();
            let value = std::hint::black_box(make());
            let after = dhat::HeapStats::get();
            drop(value);
            (
                after.total_blocks - before.total_blocks,
                after.total_bytes - before.total_bytes,
            )
        }
        type SharedCallback =
            <super::super::carriers::Shared as super::super::carriers::Carriers<u64, ()>>::Terminal;
        type LocalCallback =
            <super::super::local::Local as super::super::carriers::Carriers<u64, ()>>::Terminal;
        if crate::allocation_test::run_in_child(
            "store::command::reply_contract_tests::terminal_callbacks_allocate_one_existing_payload_box",
        ) {
            return;
        }

        let _profiler = dhat::Profiler::builder().testing().build();
        let shared = measure(|| {
            let captured = std::hint::black_box(7_u64);
            Command::<u64, ()>::Shutdown(TerminalReply::<SharedCallback>::State(Box::new(
                move |state| {
                    std::hint::black_box(captured + state);
                },
            )))
        });
        let local = measure(|| {
            let captured = std::hint::black_box(7_u64);
            Command::<u64, (), super::super::local::Local>::Shutdown(
                TerminalReply::<LocalCallback>::State(Box::new(move |state| {
                    std::hint::black_box(captured + state);
                })),
            )
        });
        assert_eq!(shared, (1, 8));
        assert_eq!(local, (1, 8));
        println!("terminal_callback_allocations shared={shared:?} local={local:?}");
    }

    #[test]
    fn action_payload_dies_before_final_named_completion_release() {
        struct Payload {
            lineage: Arc<LineageState>,
            destroyed: Arc<AtomicUsize>,
        }
        impl Drop for Payload {
            fn drop(&mut self) {
                assert!(!self.lineage.is_finished());
                self.destroyed.fetch_add(1, Ordering::SeqCst);
            }
        }
        let control = crate::effect::lineage::ControlWake::new();
        let named = LineageState::new(Arc::downgrade(&control));
        let registration = crate::effect::operation::OperationRegistration::new(Arc::clone(&named));
        let operations = OperationAncestry::default().with_generation(Arc::clone(&named));
        let operation_work = operations.retain_work();
        drop(registration);
        let destroyed = Arc::new(AtomicUsize::new(0));
        let scope = crate::store::scope::ScopeRegistry::for_test().1.root();
        let command = ActionCommand::effect(
            Payload { lineage: Arc::clone(&named), destroyed: Arc::clone(&destroyed) },
            CommitSender::disconnected(),
            scope,
            LineageState::new(Weak::new()),
            Weak::new(),
            Box::default(),
            operations,
            operation_work,
        );
        drop(command);
        assert_eq!(destroyed.load(Ordering::SeqCst), 1);
        assert!(named.is_finished());
        assert_eq!(control.epoch(), 1);
    }

    fn assert_rejected_results(make_sender: impl Fn() -> CommitSender) {
        for error in [CommitError::Cancelled, CommitError::ScopeEnded] {
            assert_eq!(
                make_sender()
                    .send(Err(error))
                    .expect_err("no waiter")
                    .expect_err("same error"),
                error
            );
        }
        let completed = make_sender()
            .send(Ok(ActionTask::completed()))
            .expect_err("no waiter")
            .unwrap();
        assert!(completed.is_finished());
        assert!(!completed.is_cancelled());

        let lineage = LineageState::new(Weak::new());
        let witness = Arc::downgrade(&lineage);
        let returned = make_sender()
            .send(Ok(ActionTask::live(lineage)))
            .expect_err("no waiter")
            .unwrap();
        assert_eq!(
            witness.strong_count(),
            1,
            "the returned task owns the original lineage"
        );
        assert!(!returned.is_finished());
        assert!(!returned.is_cancelled());
        let inspect = witness.upgrade().unwrap();
        drop(returned);
        assert!(
            !inspect.is_cancelled(),
            "ordinary task drop is non-cancelling"
        );
        assert_eq!(witness.strong_count(), 1);
        drop(inspect);
        assert!(witness.upgrade().is_none());
    }

    #[test]
    fn receiver_loss_returns_exact_result_without_cancelling_work() {
        assert_rejected_results(|| {
            let (sender, receiver) = commit_channel();
            drop(receiver);
            sender
        });
    }

    #[test]
    fn disconnected_reply_returns_exact_result_without_cancelling_work() {
        assert_rejected_results(CommitSender::disconnected);
    }

    struct WakeProbe {
        calls: AtomicUsize,
        panic: bool,
    }

    impl Wake for WakeProbe {
        fn wake(self: Arc<Self>) {
            self.calls.fetch_add(1, Ordering::SeqCst);
            assert!(!self.panic, "deliberate receiver wake panic");
        }
    }

    #[test]
    fn connected_reply_wakes_and_keeps_committed_task_through_wake_unwind() {
        for panic in [false, true] {
            let probe = Arc::new(WakeProbe {
                calls: AtomicUsize::new(0),
                panic,
            });
            let waker = Waker::from(Arc::clone(&probe));
            let mut context = Context::from_waker(&waker);
            let (sender, mut receiver) = commit_channel();
            assert!(Pin::new(&mut receiver).poll(&mut context).is_pending());
            let lineage = LineageState::new(Weak::new());
            let witness = Arc::downgrade(&lineage);
            let sent = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                sender.send(Ok(ActionTask::live(lineage)))
            }));
            if panic {
                let payload = sent.expect_err("receiver wake panics");
                assert_eq!(
                    payload.downcast_ref::<&str>(),
                    Some(&"deliberate receiver wake panic")
                );
            } else {
                sent.expect("nonpanicking wake").expect("connected reply");
            }
            assert_eq!(probe.calls.load(Ordering::SeqCst), 1);
            let Poll::Ready(Ok(Ok(task))) = Pin::new(&mut receiver).poll(&mut context) else {
                panic!("committed task survives sender completion");
            };
            assert_eq!(witness.strong_count(), 1);
            assert!(!task.is_finished());
            assert!(!task.is_cancelled());
            drop(task);
            assert!(witness.upgrade().is_none());

            // Dropping an unused sender still wakes the receiver with cancellation.
            let (sender, mut receiver) = commit_channel();
            let probe = Arc::new(WakeProbe {
                calls: AtomicUsize::new(0),
                panic: false,
            });
            let waker = Waker::from(Arc::clone(&probe));
            let mut context = Context::from_waker(&waker);
            assert!(Pin::new(&mut receiver).poll(&mut context).is_pending());
            drop(sender);
            assert_eq!(probe.calls.load(Ordering::SeqCst), 1);
            assert!(matches!(
                Pin::new(&mut receiver).poll(&mut context),
                Poll::Ready(Err(_))
            ));
        }
    }

    #[test]
    fn reply_traits_and_common_layouts_remain_bounded() {
        fn reply_traits<
            T: Send + Sync + Unpin + std::panic::UnwindSafe + std::panic::RefUnwindSafe + 'static,
        >() {
        }
        fn public_traits<T: Send + Unpin + 'static>() {}
        fn layout<T>(name: &str) {
            println!("{name} size={} align={}", size_of::<T>(), align_of::<T>());
        }
        #[cfg(not(feature = "test-util"))]
        type ExpectedSender = futures_channel::oneshot::Sender<CommitResult>;
        #[cfg(feature = "test-util")]
        type ExpectedSender = (
            Option<futures_channel::oneshot::Sender<CommitResult>>,
            super::super::task_context::TerminalGuard,
        );
        reply_traits::<CommitSender>();
        reply_traits::<CommitReceiver>();
        public_traits::<crate::ActionReceipt>();
        public_traits::<crate::ActionReceiptFuture>();
        public_traits::<ActionTask>();
        public_traits::<ActionCommand<std::cell::Cell<u8>>>();
        assert_eq!(
            size_of::<CommitSender>(),
            size_of::<ExpectedSender>()
        );
        assert_eq!(
            align_of::<CommitSender>(),
            align_of::<ExpectedSender>()
        );
        layout::<CommitSender>("sender");
        layout::<Option<CommitSender>>("optional_sender");
        layout::<CommitReceiver>("receiver");
        layout::<ActionCommand<()>>("action_command");
        layout::<Command<(), ()>>("command");
        layout::<OwnedActionCommand<()>>("owned_command");
        layout::<crate::ActionReceipt>("receipt");
        layout::<crate::ActionReceiptFuture>("receipt_future");
    }
}
}
