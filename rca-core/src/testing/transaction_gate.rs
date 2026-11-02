//! One typed mutation frontier with capacity-one diagnostic inspection.
//!
//! The actor owns mutation execution. This gate retains one mutation or its
//! unacknowledged result, while diagnostic queries use a separate capacity-one
//! slot. Callbacks and owned payload destruction run outside the gate lock.
//! Inspection guards delay retirement until borrowed values return.

use super::{
    ActionTask, ActorDiagnostics, Arc, AssertionReceipt, AssertionStateReceipt, Event,
    EventListener, GateAttachment, GateDecision, GateShared, GateState, Mutex, RuntimeSnapshot,
    StoreStopped, VecDeque, fmt,
};
use crate::effect::operation::OperationWork;
use crate::store::AcquireCall;
use crate::store::FoldCall;
use crate::store::carriers::{Carriers, Shared};
use crate::store::command::{ActionCommand, ActionInput, Command, Provenance, ReadyActionCommand};
use crate::store::mounted::MountedCall;
use crate::store::output::OutputCall;
use crate::store::task_context::UpdateCall;
use crate::store::task_context::{
    TaskAccessError, TaskAccessReason, TaskProgress, TaskRequest, TerminalReceipt,
};

#[cfg(test)]
ordinary_test_items! { mod owned_queries; }
#[cfg(test)]
ordinary_test_items! { mod fold_reference_costs; }

/// Diagnostic work that cannot mutate or acknowledge the held boundary.
pub(crate) enum GateQuery<S, A, C: Carriers<S, A>> {
    /// One-shot read of actor-owned state.
    State(Box<C::Query>),
    /// Typed authority acquisition with an owned failure reply.
    Acquire(AcquireQuery<C::Acquire>),
    /// Typed state fold with an owned failure reply.
    Fold(FoldQuery<S, A, C>),
}

/// Rejects an unexecuted fold when its admission future or queue entry drops.
pub(crate) struct FoldQuery<S, A, C: Carriers<S, A>> {
    /// Taken before execution or rejection so Drop cannot reply twice.
    call: Option<Box<C::Fold>>,
}

impl<S, A, C: Carriers<S, A>> FoldQuery<S, A, C> {
    /// Transfers the exact failure to the original fold request.
    fn reject(mut self, error: TaskAccessError) {
        self.call.take().expect("query owns fold").reject(error);
    }
}
impl<S, A, C: Carriers<S, A>> Drop for FoldQuery<S, A, C> {
    fn drop(&mut self) {
        if let Some(call) = self.call.take() {
            call.reject(TaskAccessError::new(
                TaskAccessReason::StoreClosed,
                TaskProgress::NotStarted,
            ));
        }
    }
}
/// Closes an unexecuted acquisition when its admission or queue owner drops.
pub(crate) struct AcquireQuery<T: ?Sized + AcquireCall> {
    /// Taken before execution or rejection, leaving no duplicate cleanup owner.
    call: Option<Box<T>>,
}

impl<T: ?Sized + AcquireCall> AcquireQuery<T> {
    /// Transfers a phase rejection to the original acquisition request.
    fn reject(mut self, error: TaskAccessError) {
        self.call
            .take()
            .expect("query owns acquisition")
            .reject(error);
    }
}
impl<T: ?Sized + AcquireCall> Drop for AcquireQuery<T> {
    fn drop(&mut self) {
        if let Some(call) = self.call.take() {
            call.close();
        }
    }
}
impl<S, A, C: Carriers<S, A>> GateQuery<S, A, C> {
    /// Executes a dequeued query against the actor's current committed state.
    pub(crate) fn execute(
        self,
        state: &S,
        scopes: &crate::store::scope::ScopeRegistry,
        mounts: &[Box<C::Mount>],
        lifecycle: &crate::store::TestLifecycle,
    ) {
        match self {
            Self::State(query) => query(state),
            Self::Acquire(mut query) => query
                .call
                .take()
                .expect("query owns acquisition")
                .execute(scopes, lifecycle),
            Self::Fold(mut query) => query
                .call
                .take()
                .expect("query owns fold")
                .execute(state, scopes, mounts, lifecycle),
        }
    }
}

/// Identifies the mutation awaiting an explicit expectation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransactionKind {
    /// Installation is waiting for its structural expectation.
    Structural,
    /// An action is waiting for reduction.
    Action,
    /// A synchronous typed update is waiting to run.
    Update,
    /// An owned typed output is waiting for delivery.
    Output,
}

/// An inert identity for one selected transaction in one Store instance.
///
/// Capturing or cloning this token does not retain the Store or its work and
/// never acknowledges the transaction. Tokens cannot select a later boundary.
#[derive(Clone, Debug)]
pub struct TransactionBoundary {
    /// Inert root identity, distinct across Store instances.
    root: Arc<()>,
    /// Monotonic generation retained across gate reattachments.
    id: u64,
    /// Mutation category captured with this generation.
    kind: TransactionKind,
}

/// Records independent obligations and retirement progress for one mutation.
///
/// Commitment, failed comparison and terminal rejection can coexist. Acknowledging
/// one obligation never clears another. Root reservations cover mailbox admission
/// before a boundary exists. Inspection and retirement keep that generation fixed.
#[expect(
    clippy::struct_excessive_bools,
    reason = "commitment, acknowledgement, release, matcher debt, query access and reservation are independent facts"
)]
struct Frontier<Anchor> {
    /// Last issued boundary number, never reset by reattachment.
    next: u64,
    /// Current boundary, retained until every applicable obligation clears.
    active: Option<u64>,
    /// Actor root identity bound before publishing a boundary.
    root: Option<Arc<()>>,
    /// Category of the active mutation or retained result.
    kind: Option<TransactionKind>,
    /// Output declaration and weak identity anchor, without retaining its payload.
    output: Option<(crate::store::output::DeclarationId, Anchor)>,
    /// The actor completed a mutation successfully.
    committed: bool,
    /// The test acknowledged the committed state component.
    commit_acknowledged: bool,
    /// The actor took this mutation for execution, including incomplete execution.
    released: bool,
    /// A false or unwinding matcher left a non-discardable assertion obligation.
    matcher_failed: bool,
    /// Exact rejection metadata retained until terminal acknowledgement.
    terminal: Option<TaskAccessError>,
    /// Receives inert success or rejection metadata from the production reply owner.
    terminal_receipt: Option<TerminalReceipt>,
    /// The test acknowledged the exact terminal component.
    terminal_acknowledged: bool,
    /// The live actor still permits diagnostic query admission.
    accepts_queries: bool,
    /// One root expectation reserved the next mutation before mailbox admission.
    root_reserved: bool,
    /// Prevents replacement or acknowledgement before retired captures finish dropping.
    retirement: Retirement,
}
impl<Anchor> Default for Frontier<Anchor> {
    fn default() -> Self {
        Self {
            next: 0,
            active: None,
            root: None,
            kind: None,
            output: None,
            committed: false,
            commit_acknowledged: false,
            released: false,
            matcher_failed: false,
            terminal: None,
            terminal_receipt: None,
            terminal_acknowledged: false,
            root_reserved: false,
            accepts_queries: false,
            retirement: Retirement::Active,
        }
    }
}
impl<Anchor> Frontier<Anchor> {
    /// Requires the same root, generation and mutation category.
    fn matches(&self, boundary: &TransactionBoundary) -> bool {
        self.active == Some(boundary.id)
            && self.kind == Some(boundary.kind)
            && self
                .root
                .as_ref()
                .is_some_and(|root| Arc::ptr_eq(root, &boundary.root))
    }
    /// Clears inert boundary metadata only after all applicable obligations clear.
    fn complete(&mut self) {
        if (!self.committed || self.commit_acknowledged)
            && !self.matcher_failed
            && (self.terminal.is_none() || self.terminal_acknowledged)
        {
            self.active = None;
            self.kind = None;
            self.output = None;
            self.terminal = None;
            self.terminal_receipt = None;
        }
    }
}

/// Keeps a generation reserved through outside-lock capture destruction.
#[derive(Default)]
enum Retirement {
    /// The actor can still execute or skip the selected mutation.
    #[default]
    Active,
    /// An inspection guard must return its command before cleanup starts.
    Requested {
        /// Cleanup must release the actor's exclusive gate attachment.
        detach: bool,
    },
    /// Retired payloads and diagnostic captures are dropping outside the lock.
    Draining {
        /// Attachment release follows capture destruction.
        detach: bool,
    },
    /// Cleanup completed, while assertion or terminal obligations can remain.
    Finished,
}

/// Shared slots specialised for typed commands and carrier-specific output identity.
type FrontierShared<S, A, C> = GateShared<
    Pending<S, A, C>,
    GateQuery<S, A, C>,
    Frontier<<C as Carriers<S, A>>::OutputAnchor>,
    bool,
>;

/// Implements one frontier policy for both Shared and Local carrier families.
pub(crate) struct TransactionGateCore<S, A, C: Carriers<S, A> = Shared> {
    /// Cloned controls share bookkeeping, but only one actor binding can attach.
    shared: Arc<FrontierShared<S, A, C>>,
}
impl<S, A, C: Carriers<S, A>> Clone for TransactionGateCore<S, A, C> {
    fn clone(&self) -> Self {
        Self {
            shared: Arc::clone(&self.shared),
        }
    }
}
impl<S, A, C: Carriers<S, A>> Default for TransactionGateCore<S, A, C> {
    fn default() -> Self {
        Self::new()
    }
}
impl<S, A, C: Carriers<S, A>> fmt::Debug for TransactionGateCore<S, A, C> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TransactionGate")
            .field("boundary", &self.boundary())
            .finish_non_exhaustive()
    }
}

/// Owns one selected mutation, including its original reply and causal lifetime.
pub(crate) enum Pending<S, A, C: Carriers<S, A> = Shared> {
    /// Initial structural installation, which tests can release but cannot skip.
    Structural,
    /// Typed output whose payload stays in its declaration's rendezvous slot.
    Output {
        /// Exact output request and its reply endpoint.
        call: Box<C::Output>,
        /// Causal liveness and cancellation ancestry of this request.
        provenance: Provenance,
        /// Captured scope authority for the later production commit.
        authority: crate::store::scope::ScopeAuthority,
        /// Keeps named-operation work alive through release or rejection.
        operation_work: OperationWork,
    },
    /// Mounted action, update or command that retains its original typed input.
    Mounted {
        /// Carrier-specific mounted request and reply.
        call: Box<C::Mounted>,
        /// Causal liveness checked before production execution.
        provenance: Provenance,
        /// Authority for the selected mounted incarnation.
        authority: crate::store::scope::ScopeAuthority,
        /// Named-operation lifetime retained while this request is gated.
        operation_work: OperationWork,
    },
    /// An already-owned root action or effect-produced root action.
    Action(ReadyActionCommand<A>),
    /// A typed task send that has not entered root reduction.
    Send {
        /// Owned action and its typed reply.
        call: crate::store::task_context::TypedSend<A>,
        /// Task liveness and inherited cancellation metadata.
        provenance: Provenance,
        /// Scope authority that production execution must still check.
        authority: crate::store::scope::ScopeAuthority,
        /// Named-operation lifetime carried across the gate.
        operation_work: OperationWork,
    },
    /// A synchronous state update that has not entered its callback.
    Update {
        /// Owned update callback and typed result reply.
        call: Box<C::Update>,
        /// Task liveness and inherited cancellation metadata.
        provenance: Provenance,
        /// Scope authority checked before entering the update.
        authority: crate::store::scope::ScopeAuthority,
        /// Named-operation lifetime retained through callback disposition.
        operation_work: OperationWork,
    },
}
impl<S, A, C: Carriers<S, A>> Pending<S, A, C> {
    /// Lets the original reply owner publish inert terminal metadata.
    fn attach_terminal(&mut self, receipt: TerminalReceipt) {
        match self {
            Self::Structural => {}
            Self::Action(command) => command.reply.attach_terminal(receipt),
            Self::Send { call, .. } => call.attach_terminal(receipt),
            Self::Update { call, .. } => call.attach_terminal(receipt),
            Self::Mounted { call, .. } => call.attach_terminal(receipt),
            Self::Output { call, .. } => call.attach_terminal(receipt),
        }
    }
    /// Selects gateable mutations while returning other commands without copying them.
    #[expect(
        clippy::result_large_err,
        reason = "Unrecognised commands must retain their original owned payload and reply without allocating an error box"
    )]
    pub(crate) fn from_command(command: Command<S, A, C>) -> Result<Self, Command<S, A, C>> {
        match command {
            Command::Output {
                call,
                provenance,
                authority,
                operation_work,
            } => Ok(Self::Output {
                call,
                provenance,
                authority,
                operation_work,
            }),
            Command::Mounted {
                call,
                provenance,
                authority,
                operation_work,
            } => Ok(Self::Mounted {
                call,
                provenance,
                authority,
                operation_work,
            }),
            Command::Action(ActionCommand {
                input: ActionInput::Ready { action, provenance },
                reply,
                assertion_reply,
                operation_work,
            }) => Ok(Self::Action(ReadyActionCommand {
                action,
                provenance,
                reply,
                assertion_reply,
                operation_work,
            })),
            Command::Task {
                request: TaskRequest::Send(call),
                provenance,
                authority,
                operation_work,
            } => Ok(Self::Send {
                call,
                provenance,
                authority,
                operation_work,
            }),
            Command::Task {
                request: TaskRequest::Update(call),
                provenance,
                authority,
                operation_work,
            } => Ok(Self::Update {
                call,
                provenance,
                authority,
                operation_work,
            }),
            other => Err(other),
        }
    }
    /// Restores the original production command without changing its payload or reply.
    pub(crate) fn into_command(self) -> Command<S, A, C> {
        match self {
            Self::Structural => Command::Structural,
            Self::Output {
                call,
                provenance,
                authority,
                operation_work,
            } => Command::Output {
                call,
                provenance,
                authority,
                operation_work,
            },
            Self::Mounted {
                call,
                provenance,
                authority,
                operation_work,
            } => Command::Mounted {
                call,
                provenance,
                authority,
                operation_work,
            },
            Self::Action(action) => Command::Action(action.into_command()),
            Self::Send {
                call,
                provenance,
                authority,
                operation_work,
            } => Command::Task {
                request: TaskRequest::Send(call),
                provenance,
                authority,
                operation_work,
            },
            Self::Update {
                call,
                provenance,
                authority,
                operation_work,
            } => Command::Task {
                request: TaskRequest::Update(call),
                provenance,
                authority,
                operation_work,
            },
        }
    }
    /// Identifies externally admitted root actions eligible for a reserved release.
    pub(crate) fn is_root(&self) -> bool {
        matches!(self, Self::Action(action) if !action.provenance.is_effect())
    }
    /// Cancels a selected mutation before user code begins.
    pub(crate) fn reject(self) {
        self.cancel();
    }
    /// Classifies a request without evaluating its user callback.
    fn kind(&self) -> TransactionKind {
        match self {
            Self::Structural => TransactionKind::Structural,
            Self::Output { .. } => TransactionKind::Output,
            Self::Update { .. } => TransactionKind::Update,
            Self::Mounted { call, .. } if call.is_update() => TransactionKind::Update,
            Self::Action(_) | Self::Send { .. } | Self::Mounted { .. } => TransactionKind::Action,
        }
    }
    /// Borrows only root-typed payloads, never erasing a mounted action's type.
    fn action(&self) -> Option<&A> {
        match self {
            Self::Action(command) => Some(&command.action),
            Self::Send { call, .. } => Some(call.action()),
            Self::Structural | Self::Update { .. } | Self::Mounted { .. } | Self::Output { .. } => {
                None
            }
        }
    }
    /// Checks captured provenance without entering the mutation.
    fn live(&self) -> bool {
        match self {
            Self::Structural => true,
            Self::Action(command) => command.provenance.is_live(),
            Self::Send { provenance, .. }
            | Self::Update { provenance, .. }
            | Self::Mounted { provenance, .. }
            | Self::Output { provenance, .. } => provenance.is_live(),
        }
    }
}
impl<S, A, C: Carriers<S, A>> Pending<S, A, C> {
    /// Reports cancellation with the guarantee that user code did not start.
    fn cancel(self) {
        let error = TaskAccessError::new(TaskAccessReason::Cancelled, TaskProgress::NotStarted);
        self.reject_with_error(error);
    }
    /// Completes the original reply before releasing operation ownership.
    fn reject_with_error(self, error: TaskAccessError) {
        match self {
            Self::Structural => {}
            Self::Output {
                call,
                operation_work,
                ..
            } => {
                call.reject(error);
                drop(operation_work);
            }
            Self::Action(command) => {
                command.reply.record_terminal(Err(error));
                // An action still owned by the gate can only be cancelled or
                // closed. Scope/input rejection has already consumed its reply.
                if error.reason() == TaskAccessReason::Cancelled {
                    let _ = command
                        .reply
                        .send(Err(crate::store::command::CommitError::Cancelled));
                }
            }
            Self::Send {
                call,
                operation_work,
                ..
            } => {
                call.reject(error);
                drop(operation_work);
            }
            Self::Update {
                call,
                operation_work,
                ..
            } => {
                call.reject(error);
                drop(operation_work);
            }
            Self::Mounted {
                call,
                operation_work,
                ..
            } => {
                call.reject(error);
                drop(operation_work);
            }
        }
    }
}

/// Owns the typed command outside the gate lock and pins its generation.
///
/// Returning the command restores the boundary or drains deferred retirement.
/// Acknowledgement and reattachment cannot replace it while this guard is live.
struct PendingInspection<'a, S, A, C: Carriers<S, A>> {
    /// Gate whose inspection reservation protects this command or terminal match.
    shared: &'a FrontierShared<S, A, C>,
    /// Temporarily removed mutation, or `None` during an inert terminal comparison.
    command: Option<Pending<S, A, C>>,
}

/// Publishes retirement completion after outside-lock payload destruction.
///
/// Declaring this guard before retired owners preserves cleanup order on unwind.
struct InspectionCompletion<'a, S, A, C: Carriers<S, A>> {
    /// Reserved frontier whose cleanup is in progress.
    shared: &'a FrontierShared<S, A, C>,
    /// Remembers attachment release requested when cleanup began.
    detach: bool,
}

impl<S, A, C: Carriers<S, A>> Drop for InspectionCompletion<'_, S, A, C> {
    fn drop(&mut self) {
        {
            let mut state = self.shared.state.lock().expect("gate lock");
            // Draining retirement reserves this generation until cleanup ends.
            state.inspection = false;
            state.decision = None;
            state.pending_snapshot = None;
            let detach = matches!(
                state.metadata.retirement,
                Retirement::Draining { detach: true }
            );
            state.metadata.complete();
            state.metadata.root_reserved = false;
            state.metadata.retirement = Retirement::Finished;
            if self.detach || detach {
                state.attachment = GateAttachment::Detached;
            }
        }
        self.shared.changed.notify(usize::MAX);
    }
}

impl<S, A, C: Carriers<S, A>> Drop for PendingInspection<'_, S, A, C> {
    fn drop(&mut self) {
        let mut completion = None;
        let mut command = self.command.take();
        let mut retired = None;
        let mut error = None;
        {
            let mut state = self.shared.state.lock().expect("gate lock");
            if let Retirement::Requested { detach } = state.metadata.retirement {
                state.metadata.retirement = Retirement::Draining { detach };
                error = state.metadata.terminal;
                completion = Some(InspectionCompletion {
                    shared: self.shared,
                    detach,
                });
                retired = Some((
                    state.assertion_reply.take(),
                    std::mem::take(&mut state.state_assertions),
                ));
            } else {
                state.pending = command.take();
                state.inspection = false;
            }
        }
        if let Some(command) = command {
            if let Some(error) = error {
                command.reject_with_error(error);
            } else {
                command.cancel();
            }
        }
        drop(retired);
        drop(completion);
        self.shared.changed.notify(usize::MAX);
    }
}

impl<S, A, C: Carriers<S, A>> TransactionGateCore<S, A, C> {
    /// Creates an unattached frontier.
    #[must_use]
    pub fn new() -> Self {
        Self {
            shared: Arc::new(GateShared {
                state: Mutex::new(GateState {
                    attachment: GateAttachment::Fresh,
                    pending: None,
                    inspection: false,
                    decision: None,
                    assertion_reply: None,
                    pending_snapshot: None,
                    latest_snapshot: None,
                    state_assertions: VecDeque::new(),
                    metadata: Frontier::default(),
                }),
                changed: Event::new(),
            }),
        }
    }
    /// Returns the current boundary, including committed assertion debt.
    #[must_use]
    pub fn boundary(&self) -> Option<u64> {
        self.shared.state.lock().expect("gate lock").metadata.active
    }
    /// Captures inert identity for the active mutation or its unacknowledged result.
    pub fn pending_boundary(&self) -> Option<TransactionBoundary> {
        let state = self.shared.state.lock().expect("gate lock");
        let id = state.metadata.active?;
        // Actors bind their root before begin publishes a boundary and its kind.
        Some(TransactionBoundary {
            root: Arc::clone(
                state
                    .metadata
                    .root
                    .as_ref()
                    .expect("active boundary has a root"),
            ),
            id,
            kind: state.metadata.kind.expect("active boundary has a kind"),
        })
    }
    /// Binds the actor's inert root identity before it publishes any boundary.
    pub(crate) fn bind_root(&self, root: Arc<()>) {
        self.shared.state.lock().expect("gate lock").metadata.root = Some(root);
    }
    /// Shares the current production reply's inert result cell.
    pub(crate) fn terminal_receipt(&self) -> Option<TerminalReceipt> {
        self.shared
            .state
            .lock()
            .expect("gate lock")
            .metadata
            .terminal_receipt
            .clone()
    }
    /// Reports comparison debt that release, skip and acknowledgement cannot erase.
    pub fn has_failed_match(&self) -> bool {
        self.shared
            .state
            .lock()
            .expect("gate lock")
            .metadata
            .matcher_failed
    }
    /// Compares a fully retired exact boundary outside the lock, retaining failures.
    pub fn match_terminal(
        &self,
        boundary: &TransactionBoundary,
        matcher: impl FnOnce(&TaskAccessError) -> bool,
    ) -> Option<bool> {
        let (error, inspection) = {
            let mut state = self.shared.state.lock().expect("gate lock");
            if !state.metadata.matches(boundary)
                || !matches!(state.metadata.retirement, Retirement::Finished)
                || state.inspection
                || state.metadata.terminal_acknowledged
            {
                return None;
            }
            let error = state.metadata.terminal?;
            state.inspection = true;
            (
                error,
                PendingInspection {
                    shared: &self.shared,
                    command: None,
                },
            )
        };
        let result = self.match_value(|| matcher(&error));
        drop(inspection);
        Some(result)
    }
    /// Selects historical boundary identity by its output declaration, not payload equality.
    pub fn output_boundary<O>(
        &self,
        output: &impl crate::store::output::OutputInspection<O>,
    ) -> Option<TransactionBoundary> {
        let state = self.shared.state.lock().expect("gate lock");
        if state.metadata.output.as_ref()?.0 != output.declaration() {
            return None;
        }
        // Output identity is published and cleared with the same boundary.
        Some(TransactionBoundary {
            root: Arc::clone(
                state
                    .metadata
                    .root
                    .as_ref()
                    .expect("output boundary has a root"),
            ),
            id: state.metadata.active.expect("output retains its boundary"),
            kind: state.metadata.kind.expect("output boundary has a kind"),
        })
    }
    /// Acknowledges terminal debt only after all inspection and retirement owners finish.
    pub fn acknowledge_terminal(&self, boundary: &TransactionBoundary) -> bool {
        let mut state = self.shared.state.lock().expect("gate lock");
        if !state.metadata.matches(boundary)
            || state.metadata.terminal.is_none()
            || state.metadata.terminal_acknowledged
            || !matches!(state.metadata.retirement, Retirement::Finished)
            || state.inspection
        {
            return false;
        }
        state.metadata.terminal_acknowledged = true;
        state.metadata.complete();
        drop(state);
        self.shared.changed.notify(usize::MAX);
        true
    }
    /// Records false or unwinding comparisons against the currently pinned boundary.
    ///
    /// Callers retain `PendingInspection` until after `Outcome` drops, so no
    /// acknowledgement, replacement or retirement can change the selected boundary.
    fn match_value(&self, matcher: impl FnOnce() -> bool) -> bool {
        /// Marks the pinned boundary on false return or callback unwind.
        struct Outcome<'a, S, A, C: Carriers<S, A>> {
            /// Gate whose inspection guard prevents generation replacement.
            gate: &'a TransactionGateCore<S, A, C>,
            /// Becomes true only after a successful matcher return.
            matched: bool,
        }
        impl<S, A, C: Carriers<S, A>> Drop for Outcome<'_, S, A, C> {
            fn drop(&mut self) {
                if !self.matched {
                    let mut state = self.gate.shared.state.lock().expect("gate lock");
                    state.metadata.matcher_failed = true;
                }
            }
        }
        let mut outcome = Outcome {
            gate: self,
            matched: false,
        };
        outcome.matched = matcher();
        outcome.matched
    }
    /// Matches the exact typed output while preserving comparison debt on failure.
    pub fn match_output<O: 'static>(
        &self,
        output: &impl crate::store::output::OutputInspection<O>,
        matcher: impl FnOnce(&O) -> bool,
    ) -> Option<bool> {
        self.inspect_output(output, |value| self.match_value(|| matcher(value)))
    }
    /// Matches an exact mounted request after its authority and input identity checks.
    fn match_mounted<X>(
        &self,
        context: &impl crate::store::mounted::MountedInputInspection<X>,
        matcher: impl FnOnce(&X) -> bool,
    ) -> Result<Option<bool>, TaskAccessError> {
        self.inspect_mounted(context, |value| self.match_value(|| matcher(value)))
    }
    /// Reports a waiting mutation or an active out-of-lock inspection.
    ///
    /// An active terminal matcher counts as inspection. Idle committed or
    /// terminal debt alone does not make this true.
    #[must_use]
    pub fn has_pending(&self) -> bool {
        let state = self.shared.state.lock().expect("gate lock");
        state.pending.is_some() || state.inspection
    }
    /// Returns the pending mutation kind without invoking a matcher.
    pub fn pending_kind(&self) -> Option<TransactionKind> {
        self.shared
            .state
            .lock()
            .expect("gate lock")
            .pending
            .as_ref()
            .map(Pending::kind)
    }
    /// Borrows a pending root-typed action outside the lock, restoring it on unwind.
    /// Typed reads, updates and mounted actions return `None` without releasing
    /// or consuming their pending frontier.
    pub fn inspect_pending<T>(&self, inspect: impl FnOnce(&A) -> T) -> Option<T> {
        let inspection = {
            let mut state = self.shared.state.lock().expect("gate lock");
            // Non-action frontiers never enter the out-of-lock inspection slot.
            // In particular a racing skip must still see Structural metadata.
            state.pending.as_ref()?.action()?;
            let command = state
                .pending
                .take()
                .expect("checked pending action under the same lock");
            state.inspection = true;
            PendingInspection {
                shared: &self.shared,
                command: Some(command),
            }
        };
        Some(inspect(
            inspection
                .command
                .as_ref()
                .expect("inspection owns its command")
                .action()
                .expect("inspection retains the checked action"),
        ))
    }

    /// Borrows an exact output value while its command and rendezvous remain pinned.
    pub fn inspect_output<O: 'static, T>(
        &self,
        output: &impl crate::store::output::OutputInspection<O>,
        inspect: impl FnOnce(&O) -> T,
    ) -> Option<T> {
        self.inspect_output_slot(output, |value| inspect(value))
    }

    /// Reads removal metadata without moving the pending output's payload.
    pub fn output_origin<O: 'static>(
        &self,
        output: &impl crate::store::output::OutputInspection<O>,
    ) -> Option<crate::RemovalOrigin> {
        let inspection = self.pin_output(output)?;
        // The exact pending request stays pinned, but its owned O never moves.
        // Endpoint access and metadata destruction remain outside the gate lock.
        let origin = output.inspect_origin();
        drop(inspection);
        origin
    }

    /// Moves the matching output command into a generation-preserving inspection guard.
    fn pin_output<O>(
        &self,
        output: &impl crate::store::output::OutputInspection<O>,
    ) -> Option<PendingInspection<'_, S, A, C>> {
        Some({
            let mut state = self.shared.state.lock().expect("gate lock");
            let Pending::Output { call, .. } = state.pending.as_ref()? else {
                return None;
            };
            if call.declaration() != output.declaration() {
                return None;
            }
            let command = state
                .pending
                .take()
                .expect("checked pending output under the same lock");
            state.inspection = true;
            PendingInspection {
                shared: &self.shared,
                command: Some(command),
            }
        })
    }

    /// Restores the output value before command restoration or retirement can run.
    fn inspect_output_slot<O: 'static, T>(
        &self,
        output: &impl crate::store::output::OutputInspection<O>,
        inspect: impl FnOnce(&crate::store::output::ValueInspection<O>) -> T,
    ) -> Option<T> {
        let inspection = self.pin_output(output)?;
        // Construct the value guard last, so it restores O before pending
        // cancellation/retirement can destroy its command or publish a reply.
        let value = output.inspect_value()?;
        let result = inspect(&value);
        drop(value);
        drop(inspection);
        Some(result)
    }
    /// Pins command ownership, then borrows its typed input outside the gate lock.
    fn inspect_mounted<X, T>(
        &self,
        context: &impl crate::store::mounted::MountedInputInspection<X>,
        inspect: impl FnOnce(&X) -> T,
    ) -> Result<Option<T>, TaskAccessError> {
        let (inspection, identity, authority) = {
            let mut state = self.shared.state.lock().expect("gate lock");
            let Some(Pending::Mounted {
                call, authority, ..
            }) = state.pending.as_ref()
            else {
                return Ok(None);
            };
            let Some(identity) = call.input_identity() else {
                return Ok(None);
            };
            let authority = authority.clone();
            let command = state.pending.take().expect("mounted frontier owns command");
            state.inspection = true;
            (
                PendingInspection {
                    shared: &self.shared,
                    command: Some(command),
                },
                identity,
                authority,
            )
        };
        // Last-created request guard restores the exact action AND reply before
        // PendingInspection can honour deferred cancellation, including unwind.
        let Some(request) = context.inspect_input(identity, &authority)? else {
            return Ok(None);
        };
        let result = inspect(request.action());
        drop(request);
        drop(inspection);
        Ok(Some(result))
    }
    /// Reserves root action expectations before mailbox admission.
    #[must_use]
    pub fn reserve_root(&self) -> bool {
        let mut state = self.shared.state.lock().expect("gate lock");
        if state.metadata.active.is_some()
            || state.metadata.root_reserved
            || !matches!(
                state.metadata.retirement,
                Retirement::Active | Retirement::Finished
            )
        {
            return false;
        }
        state.metadata.root_reserved = true;
        true
    }
    /// Cancels a reservation when root admission fails.
    pub fn cancel_root_reservation(&self) {
        self.shared
            .state
            .lock()
            .expect("gate lock")
            .metadata
            .root_reserved = false;
    }
    /// Acknowledges the committed component without erasing other boundary debt.
    #[must_use]
    pub fn acknowledge(&self, boundary: u64) -> bool {
        let mut state = self.shared.state.lock().expect("gate lock");
        if state.metadata.active != Some(boundary)
            || !state.metadata.committed
            || state.metadata.commit_acknowledged
        {
            return false;
        }
        state.metadata.commit_acknowledged = true;
        state.metadata.complete();
        drop(state);
        self.shared.changed.notify(usize::MAX);
        true
    }
    /// Releases the pending mutation and waits for its production receipt.
    #[must_use]
    pub fn release_pending_with_receipt(&self) -> Option<AssertionReceipt> {
        let (reply, receive) = async_channel::bounded(1);
        self.decide(GateDecision::Release, Some(reply))
            .then_some(AssertionReceipt { receive })
    }
    /// Releases a pending mutation without a separate test receipt.
    #[must_use]
    pub fn release_pending(&self) -> bool {
        self.decide(GateDecision::Release, None)
    }
    /// Explicitly skips a mutation before user code begins.
    /// Structural installation cannot be skipped: returns false and retains its debt.
    #[must_use]
    pub fn skip_pending(&self) -> bool {
        self.decide(GateDecision::Skip, None)
    }
    /// Queues one disposition without bypassing structural or failed-match obligations.
    fn decide(
        &self,
        decision: GateDecision,
        reply: Option<async_channel::Sender<Result<ActionTask, StoreStopped>>>,
    ) -> bool {
        let mut state = self.shared.state.lock().expect("gate lock");
        if (state.pending.is_none() && !state.inspection)
            || state.decision.is_some()
            || !matches!(state.metadata.retirement, Retirement::Active)
        {
            return false;
        }
        if decision == GateDecision::Skip
            && (state.metadata.matcher_failed || matches!(state.pending, Some(Pending::Structural)))
        {
            return false;
        }
        state.decision = Some(decision);
        state.assertion_reply = reply;
        drop(state);
        self.shared.changed.notify(usize::MAX);
        true
    }
    /// Waits for one diagnostic slot while retaining the query in its caller's future.
    ///
    /// A stale boundary or closed phase returns the unchanged query for typed
    /// rejection. Listener registration precedes each capacity check. Cancellation
    /// drops the caller-owned query without admitting it to a later generation.
    fn admit_query(
        &self,
        boundary: Option<u64>,
        query: GateQuery<S, A, C>,
    ) -> impl Future<Output = Option<GateQuery<S, A, C>>> + use<S, A, C> {
        let gate = self.clone();
        async move {
            loop {
                let listener = gate.listen();
                {
                    let mut state = gate.shared.state.lock().expect("gate lock");
                    if boundary.is_none()
                        || state.metadata.active != boundary
                        || state.attachment != GateAttachment::Attached
                        || !state.metadata.accepts_queries
                        || !matches!(
                            state.metadata.retirement,
                            Retirement::Active | Retirement::Finished
                        )
                    {
                        return Some(query);
                    }
                    if state.state_assertions.is_empty() {
                        state.state_assertions.push_back(query);
                        drop(state);
                        gate.shared.changed.notify(usize::MAX);
                        return None;
                    }
                }
                listener.await;
            }
        }
    }
    /// Admits typed acquisition or rejects its original reply with `PhaseDenied`.
    pub(crate) fn acquire(
        &self,
        boundary: u64,
        call: Box<C::Acquire>,
    ) -> impl Future<Output = ()> + use<S, A, C> {
        let admission = self.admit_query(
            Some(boundary),
            GateQuery::Acquire(AcquireQuery { call: Some(call) }),
        );
        async move {
            if let Some(GateQuery::Acquire(query)) = admission.await {
                query.reject(TaskAccessError::new(
                    TaskAccessReason::PhaseDenied,
                    TaskProgress::NotStarted,
                ));
            }
        }
    }
    /// Admits a typed fold or rejects its original reply with `PhaseDenied`.
    pub(crate) fn fold(
        &self,
        boundary: u64,
        call: Box<C::Fold>,
    ) -> impl Future<Output = ()> + use<S, A, C> {
        let admission = self.admit_query(
            Some(boundary),
            GateQuery::Fold(FoldQuery { call: Some(call) }),
        );
        async move {
            if let Some(GateQuery::Fold(query)) = admission.await {
                query.reject(TaskAccessError::new(
                    TaskAccessReason::PhaseDenied,
                    TaskProgress::NotStarted,
                ));
            }
        }
    }
    /// Returns diagnostics at the pending boundary.
    #[must_use]
    pub fn pending_diagnostics(&self) -> Option<ActorDiagnostics> {
        self.shared
            .state
            .lock()
            .expect("gate lock")
            .pending_snapshot
            .clone()
    }
    /// Returns the latest published diagnostics.
    #[must_use]
    pub fn latest_diagnostics(&self) -> Option<ActorDiagnostics> {
        self.shared
            .state
            .lock()
            .expect("gate lock")
            .latest_snapshot
            .clone()
    }
    /// Returns counters at the pending boundary.
    #[must_use]
    pub fn pending_snapshot(&self) -> Option<RuntimeSnapshot> {
        self.shared
            .state
            .lock()
            .expect("gate lock")
            .pending_snapshot
            .as_ref()
            .map(|diagnostics| diagnostics.runtime().clone())
    }
    /// Returns the latest published counters.
    #[must_use]
    pub fn latest_snapshot(&self) -> Option<RuntimeSnapshot> {
        self.shared
            .state
            .lock()
            .expect("gate lock")
            .latest_snapshot
            .as_ref()
            .map(|diagnostics| diagnostics.runtime().clone())
    }
    /// Waits for a mutation awaiting an expectation.
    ///
    /// This does not drive the actor or complete merely because the gate closes.
    pub async fn wait_for_pending(&self) {
        loop {
            let listener = self.listen();
            if self.has_pending() {
                return;
            }
            listener.await;
        }
    }

    /// Claims an idle gate while preserving its monotonic generation counter.
    ///
    /// Retired captures drop outside the lock after the new binding exists.
    /// Active debt, inspection and unfinished retirement prevent reattachment.
    pub(crate) fn attach(&self) -> Result<TransactionGateBinding<S, A, C>, crate::StoreBuildError> {
        let mut state = self.shared.state.lock().expect("gate lock");
        if state.attachment == GateAttachment::Attached
            || state.inspection
            || state.metadata.active.is_some()
            || matches!(
                state.metadata.retirement,
                Retirement::Requested { .. } | Retirement::Draining { .. }
            )
        {
            return Err(crate::StoreBuildError::assertion_gate_attached());
        }
        let retired = (
            state.pending.take(),
            state.assertion_reply.take(),
            std::mem::take(&mut state.state_assertions),
        );
        state.attachment = GateAttachment::Attached;
        state.inspection = false;
        state.decision = None;
        state.metadata = Frontier {
            next: state.metadata.next,
            accepts_queries: true,
            ..Frontier::default()
        };
        state.pending_snapshot = None;
        state.latest_snapshot = None;
        drop(state);
        let binding = TransactionGateBinding { gate: self.clone() };
        drop(retired);
        Ok(binding)
    }
    /// Publishes one mutation and its typed rendezvous before notifying waiters.
    ///
    /// The owning actor supplies a fresh boundary only after previous debt clears.
    /// Reserved root expectations release automatically without skipping commitment.
    pub(crate) fn begin(
        &self,
        mut command: Pending<S, A, C>,
        snapshot: ActorDiagnostics,
        root: bool,
    ) {
        let terminal_receipt = TerminalReceipt::default();
        command.attach_terminal(terminal_receipt.clone());
        // The typed rendezvous must exist before the pending frontier becomes
        // visible. No user callback or value destruction occurs here.
        if let Pending::Output { call, .. } = &command {
            call.publish_pending();
        }
        if let Pending::Mounted { call, .. } = &command {
            call.publish_input();
        }
        let kind = command.kind();
        let output = match &command {
            Pending::Output { call, .. } => Some((call.declaration(), call.declaration_anchor())),
            _ => None,
        };
        let mut state = self.shared.state.lock().expect("gate lock");
        assert!(
            state.metadata.active.is_none(),
            "a boundary cannot overwrite debt"
        );
        state.metadata.next = state
            .metadata
            .next
            .checked_add(1)
            .expect("boundary overflow");
        state.metadata.active = Some(state.metadata.next);
        state.metadata.kind = Some(kind);
        state.metadata.output = output;
        state.metadata.committed = false;
        state.metadata.commit_acknowledged = false;
        state.metadata.released = false;
        state.metadata.matcher_failed = false;
        state.metadata.terminal = None;
        state.metadata.terminal_receipt = Some(terminal_receipt);
        state.metadata.terminal_acknowledged = false;
        state.metadata.retirement = Retirement::Active;
        state.pending = Some(command);
        if root && state.metadata.root_reserved {
            state.decision = Some(GateDecision::Release);
            state.metadata.root_reserved = false;
        }
        state.pending_snapshot = Some(snapshot.clone());
        state.latest_snapshot = Some(snapshot);
        drop(state);
        self.shared.changed.notify(usize::MAX);
    }
    /// Transfers a decided command after inspection returns and failed-match checks pass.
    pub(crate) fn take_decided(&self) -> Option<(Pending<S, A, C>, GateDecision)> {
        let mut state = self.shared.state.lock().expect("gate lock");
        let decision = state.decision?;
        if decision == GateDecision::Skip && state.metadata.matcher_failed {
            state.decision = None;
            return None;
        }
        // During inspection the guard owns the pending mutation. Leave its
        // decision queued until the guard restores that mutation.
        let pending = state.pending.take()?;
        state.decision = None;
        state.metadata.released = decision == GateDecision::Release;
        Some((pending, decision))
    }
    /// Records completed state and publishes its causal task without acknowledging debt.
    pub(crate) fn committed(&self, task: ActionTask, snapshot: ActorDiagnostics) {
        let reply = {
            let mut state = self.shared.state.lock().expect("gate lock");
            state.metadata.committed = true;
            state.latest_snapshot = Some(snapshot);
            state.pending_snapshot = None;
            state.assertion_reply.take()
        };
        if let Some(reply) = reply {
            let _ = reply.try_send(Ok(task));
        }
        self.shared.changed.notify(usize::MAX);
    }
    /// Replaces inert diagnostics without altering the held boundary.
    pub(crate) fn update_snapshot(&self, snapshot: ActorDiagnostics) {
        self.shared.state.lock().expect("gate lock").latest_snapshot = Some(snapshot);
    }
    /// Frees the diagnostic slot before returning its query for outside-lock execution.
    pub(crate) fn take_query(&self) -> Option<GateQuery<S, A, C>> {
        let query = self
            .shared
            .state
            .lock()
            .expect("gate lock")
            .state_assertions
            .pop_front();
        if query.is_some() {
            self.shared.changed.notify(usize::MAX);
        }
        query
    }
    /// Defers provenance inspection while a callback owns the pending command.
    pub(crate) fn pending_is_live(&self) -> bool {
        let state = self.shared.state.lock().expect("gate lock");
        state.inspection || state.pending.as_ref().is_none_or(Pending::live)
    }
    /// Retires skipped work without inventing terminal failure metadata.
    pub(crate) fn clear(&self) {
        self.retire(false, None);
    }
    /// Retires a rejected mutation while retaining its exact failure for assertion.
    pub(crate) fn rejected(&self, error: TaskAccessError) {
        self.retire(false, Some(error));
    }
    /// Stops query admission and retires the actor's unfinished frontier.
    pub(crate) fn close(&self) {
        self.shared
            .state
            .lock()
            .expect("gate lock")
            .metadata
            .accepts_queries = false;
        self.retire(false, Some(self.closure_error()));
    }
    /// Distinguishes requests held before execution from requests already released.
    fn closure_error(&self) -> TaskAccessError {
        let state = self.shared.state.lock().expect("gate lock");
        TaskAccessError::new(
            TaskAccessReason::StoreClosed,
            if state.metadata.released {
                TaskProgress::MayHaveRun
            } else {
                TaskProgress::NotStarted
            },
        )
    }
    /// Reports whether no deferred inspection or capture destruction blocks actor progress.
    pub(crate) fn retirement_finished(&self) -> bool {
        matches!(
            self.shared
                .state
                .lock()
                .expect("gate lock")
                .metadata
                .retirement,
            Retirement::Active | Retirement::Finished
        )
    }
    /// Preserves reply facts, then disposes of selected work outside the gate lock.
    ///
    /// Inspection defers disposal to its guard. Otherwise the completion guard
    /// releases the reservation after payloads and queries drop, including unwind.
    fn retire(&self, detach: bool, error: Option<TaskAccessError>) {
        let receipt = {
            let state = self.shared.state.lock().expect("gate lock");
            state.metadata.terminal_receipt.clone()
        };
        // The reply owner records only inert result metadata. Read its lock
        // outside the gate lock. Only the owning actor can begin another
        // generation; acknowledgements can clear this one while we read.
        let actual = receipt.and_then(|receipt| receipt.result());
        let completion;
        let retired = {
            let mut state = self.shared.state.lock().expect("gate lock");
            if detach {
                state.metadata.accepts_queries = false;
            }
            if state.metadata.active.is_some() {
                match actual {
                    Some(Ok(())) => state.metadata.committed = true,
                    Some(Err(actual)) => {
                        if state.metadata.terminal.is_none() {
                            state.metadata.terminal = Some(actual);
                        }
                    }
                    // Successful mutations record their receipt before becoming committed.
                    None if state.metadata.terminal.is_none() => {
                        state.metadata.terminal = error;
                    }
                    None => {}
                }
            }
            if let Retirement::Draining { .. } = state.metadata.retirement {
                // The actor waits for its drain to finish. Only destruction of
                // its unique binding can re-enter during outside-lock cleanup.
                state.metadata.retirement = Retirement::Draining { detach: true };
                return;
            }
            if state.inspection {
                let previous_detach = match state.metadata.retirement {
                    Retirement::Requested { detach } => detach,
                    _ => false,
                };
                state.metadata.retirement = Retirement::Requested {
                    detach: detach || previous_detach,
                };
                drop(state);
                self.shared.changed.notify(usize::MAX);
                return;
            }
            state.metadata.retirement = Retirement::Draining { detach };
            completion = InspectionCompletion {
                shared: &self.shared,
                detach,
            };
            (
                state.pending.take(),
                state.assertion_reply.take(),
                std::mem::take(&mut state.state_assertions),
            )
        };
        if let Some(pending) = retired.0 {
            if let Some(error) = error {
                pending.reject_with_error(error);
            } else {
                pending.cancel();
            }
        }
        drop((retired.1, retired.2));
        drop(completion);
    }
    /// Registers a waiter before its caller rechecks gate readiness.
    pub(crate) fn listen(&self) -> EventListener {
        self.shared.changed.listen()
    }
}
/// Retains one actor's exclusive attachment through frontier and capture retirement.
pub(crate) struct TransactionGateBinding<S, A, C: Carriers<S, A> = Shared> {
    /// Shared controls whose claim this unique binding releases on drop.
    gate: TransactionGateCore<S, A, C>,
}
impl<S, A, C: Carriers<S, A>> TransactionGateBinding<S, A, C> {
    /// Borrows controls without duplicating the exclusive actor claim.
    pub(crate) fn gate(&self) -> &TransactionGateCore<S, A, C> {
        &self.gate
    }
}
impl<S, A, C: Carriers<S, A>> Drop for TransactionGateBinding<S, A, C> {
    fn drop(&mut self) {
        self.gate.retire(true, Some(self.gate.closure_error()));
    }
}

/// Generates carrier-specific public controls with one shared frontier policy.
macro_rules! gate_facade {
    ($family:ty $(, $send:ident)?) => {
        /// Controls the single typed assertion frontier of a deterministic Store.
        ///
        /// Clones share one frontier. The attached actor owns mutation execution.
        /// Inspection never releases a mutation or acknowledges its result.
        /// Failed comparisons retain assertion debt until the test ends.
        pub struct TransactionGate<S, A> {
            /// Carrier-specific storage with common admission and retirement policy.
            core: TransactionGateCore<S, A, $family>,
        }

        impl<S, A> Clone for TransactionGate<S, A> {
            fn clone(&self) -> Self {
                Self {
                    core: self.core.clone(),
                }
            }
        }

        impl<S, A> Default for TransactionGate<S, A> {
            fn default() -> Self {
                Self::new()
            }
        }

        impl<S, A> fmt::Debug for TransactionGate<S, A> {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.core.fmt(f)
            }
        }

        impl<S, A> TransactionGate<S, A> {
            /// Creates an unattached frontier.
            #[must_use]
            pub fn new() -> Self {
                Self {
                    core: TransactionGateCore::new(),
                }
            }

            /// Returns the current boundary, including committed assertion debt.
            #[must_use]
            pub fn boundary(&self) -> Option<u64> {
                self.core.boundary()
            }

            /// Captures the exact selected live or terminal boundary without acknowledging it.
            #[must_use]
            pub fn pending_boundary(&self) -> Option<TransactionBoundary> {
                self.core.pending_boundary()
            }

            /// Reports a failed exact-source payload or terminal comparison.
            #[must_use]
            pub fn has_failed_match(&self) -> bool {
                self.core.has_failed_match()
            }

            /// Matches the terminal result for this exact boundary outside locks.
            ///
            /// Wrong roots, stale tokens and live requests invoke no callback.
            ///
            /// # Panics
            ///
            /// Propagates matcher panics and retains failed-comparison debt.
            pub fn match_terminal(
                &self,
                boundary: &TransactionBoundary,
                matcher: impl FnOnce(&TaskAccessError) -> bool,
            ) -> Option<bool> {
                self.core.match_terminal(boundary, matcher)
            }

            /// Completes only the exact terminal component, retaining any other debt.
            #[must_use]
            pub fn acknowledge_terminal(&self, boundary: &TransactionBoundary) -> bool {
                self.core.acknowledge_terminal(boundary)
            }

            /// Reports a waiting mutation or an active out-of-lock inspection.
            ///
            /// An active terminal matcher counts as inspection. Idle committed
            /// or terminal debt alone does not make this true.
            #[must_use]
            pub fn has_pending(&self) -> bool {
                self.core.has_pending()
            }

            /// Returns the pending mutation kind without invoking a matcher.
            pub fn pending_kind(&self) -> Option<TransactionKind> {
                self.core.pending_kind()
            }

            /// Borrows a pending root-typed action outside the lock, restoring it on unwind.
            ///
            /// Typed reads, updates and mounted actions return `None` without releasing
            /// or consuming their pending frontier.
            ///
            /// # Panics
            ///
            /// Propagates callback panics after restoring or retiring the original command.
            pub fn inspect_pending<T>(&self, inspect: impl FnOnce(&A) -> T) -> Option<T> {
                self.core.inspect_pending(inspect)
            }

            /// Reserves root action expectations before mailbox admission.
            #[must_use]
            pub fn reserve_root(&self) -> bool {
                self.core.reserve_root()
            }

            /// Cancels a reservation when root admission fails.
            pub fn cancel_root_reservation(&self) {
                self.core.cancel_root_reservation();
            }

            /// Acknowledges the committed component without erasing other boundary debt.
            #[must_use]
            pub fn acknowledge(&self, boundary: u64) -> bool {
                self.core.acknowledge(boundary)
            }

            /// Releases the pending mutation and waits for its production receipt.
            #[must_use]
            pub fn release_pending_with_receipt(&self) -> Option<AssertionReceipt> {
                self.core.release_pending_with_receipt()
            }

            /// Releases a pending mutation without a separate test receipt.
            #[must_use]
            pub fn release_pending(&self) -> bool {
                self.core.release_pending()
            }

            /// Explicitly skips a mutation before user code begins.
            ///
            /// Structural installation cannot be skipped: returns false and retains its debt.
            #[must_use]
            pub fn skip_pending(&self) -> bool {
                self.core.skip_pending()
            }

            /// Inspects typed state after waiting for the single diagnostic slot.
            ///
            /// Admission starts when the receipt is awaited. Waiting callers retain
            /// their queries outside the actor and never acknowledge a boundary.
            pub fn assert_state(
                &self,
                assertion: impl FnOnce(&S) $(+ $send)? + 'static,
            ) -> AssertionStateReceipt<impl Future<Output = ()> $(+ $send)? + 'static>
            where
                S: 'static,
                A: $($send +)? 'static,
            {
                let (reply, receive) = async_channel::bounded(1);
                let query: Box<<$family as Carriers<S, A>>::Query> = Box::new(move |state| {
                    if !reply.is_closed() {
                        assertion(state);
                        let _ = reply.try_send(());
                    }
                });
                let admission = self
                    .core
                    .admit_query(self.core.boundary(), GateQuery::State(query));

                AssertionStateReceipt {
                    receive,
                    admission: async move {
                        drop(admission.await);
                    },
                }
            }

            /// Returns diagnostics at the pending boundary.
            #[must_use]
            pub fn pending_diagnostics(&self) -> Option<ActorDiagnostics> {
                self.core.pending_diagnostics()
            }

            /// Returns the latest published diagnostics.
            #[must_use]
            pub fn latest_diagnostics(&self) -> Option<ActorDiagnostics> {
                self.core.latest_diagnostics()
            }

            /// Returns counters at the pending boundary.
            #[must_use]
            pub fn pending_snapshot(&self) -> Option<RuntimeSnapshot> {
                self.core.pending_snapshot()
            }

            /// Returns the latest published counters.
            #[must_use]
            pub fn latest_snapshot(&self) -> Option<RuntimeSnapshot> {
                self.core.latest_snapshot()
            }

            /// Waits for a mutation awaiting an expectation.
            ///
            /// This neither drives the actor nor completes merely because it closes.
            pub fn wait_for_pending(&self) -> impl Future<Output = ()> {
                self.core.wait_for_pending()
            }

            /// Claims one actor attachment, preserving outstanding boundary obligations.
            pub(crate) fn attach(
                &self,
            ) -> Result<TransactionGateBinding<S, A, $family>, crate::StoreBuildError> {
                self.core.attach()
            }
        }
    };
}

gate_facade!(Shared, Send);

impl<S, A> TransactionGate<S, A> {
    /// Checks readiness without driving Initial work or admitting a fold.
    ///
    /// # Errors
    ///
    /// Returns the root lifecycle's access error, such as an unfinished Initial
    /// phase or a closed Store. No diagnostic query is submitted.
    pub fn validate_fold(&self, store: &crate::Store<S, A>) -> Result<(), TaskAccessError>
    where
        S: Send + 'static,
        A: Send + 'static,
    {
        store.validate_fold_for_test()
    }
    /// Folds the held committed state without releasing mutation or assertion debt.
    ///
    /// # Errors
    ///
    /// Reports lifecycle, gate admission or fold execution failures with their
    /// known progress. The query never retargets a replacement boundary.
    pub fn fold<K: crate::FoldKey>(
        &self,
        store: &crate::Store<S, A>,
        fold: &crate::Fold<S, K>,
    ) -> impl Future<Output = Result<K::Value, TaskAccessError>> + Send + 'static + use<S, A, K>
    where
        S: Send + 'static,
        A: Send + 'static,
        K::Value: Send,
    {
        store.fold_for_test(fold, self.core.clone())
    }
    /// Captures typed subtree authority and the matching diagnostic gate.
    ///
    /// # Errors
    ///
    /// Reports root, declaration or admission failures. A valid site with no
    /// installed child returns `Ok(None)`, not an error.
    pub fn acquire_fold<K: crate::FoldKey, CS: 'static, CA: 'static>(
        &self,
        store: &crate::Store<S, A>,
        fold: &crate::Fold<S, K>,
        site: &crate::MountSite<S, CS, CA, A>,
    ) -> impl Future<Output = Result<Option<crate::FoldView<K>>, TaskAccessError>>
    + Send
    + 'static
    + use<S, A, K, CS, CA>
    where
        S: Send + 'static,
        A: Send + 'static,
        K::Value: Send,
    {
        store.acquire_fold_for_test(fold, site, self.core.clone())
    }
    /// Reads a view acquired through this root's diagnostic endpoint.
    ///
    /// Ordinary-produced views return `PhaseDenied` rather than queue behind a frontier.
    ///
    /// # Errors
    ///
    /// Reports foreign or expired authority, a non-diagnostic view, admission
    /// failure or a fold execution failure. It does not reacquire a stale view.
    pub fn fold_view<K: crate::FoldKey>(
        &self,
        store: &crate::Store<S, A>,
        view: &crate::FoldView<K>,
    ) -> impl Future<Output = Result<K::Value, TaskAccessError>> + Send + 'static + use<S, A, K>
    where
        S: Send + 'static,
        A: Send + 'static,
    {
        store.fold_view_for_test(view)
    }
    /// Checks actual lifecycle and weak declaration metadata without driving the actor.
    ///
    /// Test hosts perform this before settling queued Initial work.
    ///
    /// # Errors
    ///
    /// Returns the root access error or rejects a foreign or retired declaration.
    pub fn validate_acquisition<CS, CA>(
        &self,
        store: &crate::Store<S, A>,
        site: &crate::MountSite<S, CS, CA, A>,
    ) -> Result<(), TaskAccessError> {
        store.validate_acquisition_for_test(site)
    }
    /// Acquires current child authority through this frontier's diagnostic slot.
    ///
    /// With no frontier, uses the ordinary ordered ingress. Never releases a mutation.
    ///
    /// # Errors
    ///
    /// Reports root, declaration or admission failures. A valid site with no
    /// installed child returns `Ok(None)`.
    pub fn acquire<CS: Send + 'static, CA: Send + 'static>(
        &self,
        store: &crate::Store<S, A>,
        site: &crate::MountSite<S, CS, CA, A>,
    ) -> impl Future<Output = Result<Option<crate::TaskContext<CS, CA>>, TaskAccessError>>
    + Send
    + 'static
    + use<S, A, CS, CA>
    where
        S: 'static,
        A: Send + 'static,
    {
        store.acquire_for_test(site, self.core.clone())
    }
    /// Borrows a mounted action through the same installed incarnation.
    ///
    /// The original request is restored before cancellation, including on unwind.
    ///
    /// # Errors
    ///
    /// Returns the endpoint's authority error. A different pending input returns
    /// `Ok(None)` without invoking the callback.
    ///
    /// # Panics
    ///
    /// Propagates callback panics after restoring the original request.
    pub fn inspect_mounted<CS, CA, T>(
        &self,
        context: &crate::TaskContext<CS, CA>,
        inspect: impl FnOnce(&CA) -> T,
    ) -> Result<Option<T>, TaskAccessError> {
        self.core.inspect_mounted(context, inspect)
    }
    /// Matches an exact mounted action, retaining false or unwinding comparison debt.
    ///
    /// # Errors
    ///
    /// Returns the authority errors described by [`Self::inspect_mounted`].
    ///
    /// # Panics
    ///
    /// Propagates matcher panics and retains failed-comparison debt.
    pub fn match_mounted<CS, CA>(
        &self,
        context: &crate::TaskContext<CS, CA>,
        matcher: impl FnOnce(&CA) -> bool,
    ) -> Result<Option<bool>, TaskAccessError> {
        self.core.match_mounted(context, matcher)
    }
    /// Borrows an original command before mapping, restoring its reply on unwind.
    ///
    /// # Errors
    ///
    /// Returns the port's authority error. A different pending input returns
    /// `Ok(None)` without invoking the callback or command mapper.
    ///
    /// # Panics
    ///
    /// Propagates callback panics after restoring the original request.
    pub fn inspect_command<C, T>(
        &self,
        port: &crate::CommandPort<C>,
        inspect: impl FnOnce(&C) -> T,
    ) -> Result<Option<T>, TaskAccessError> {
        self.core.inspect_mounted(port, inspect)
    }
    /// Matches an exact command, retaining false or unwinding comparison debt.
    ///
    /// # Errors
    ///
    /// Returns the authority errors described by [`Self::inspect_command`].
    ///
    /// # Panics
    ///
    /// Propagates matcher panics and retains failed-comparison debt.
    pub fn match_command<C>(
        &self,
        port: &crate::CommandPort<C>,
        matcher: impl FnOnce(&C) -> bool,
    ) -> Result<Option<bool>, TaskAccessError> {
        self.core.match_mounted(port, matcher)
    }
    /// Checks actual lifecycle and command declaration metadata before driving work.
    ///
    /// # Errors
    ///
    /// Returns the root access error or rejects a foreign or retired declaration.
    pub fn validate_command_acquisition<C>(
        &self,
        store: &crate::Store<S, A>,
        site: &crate::CommandSite<S, C>,
    ) -> Result<(), TaskAccessError> {
        store.validate_command_acquisition_for_test(site)
    }
    /// Acquires a command port through the existing ordered diagnostic slot.
    ///
    /// # Errors
    ///
    /// Reports root, declaration or admission failures. A valid declaration
    /// without an installed command route returns `Ok(None)`.
    pub fn command_port<C: Send + 'static>(
        &self,
        store: &crate::Store<S, A>,
        site: &crate::CommandSite<S, C>,
    ) -> impl Future<Output = Result<Option<crate::CommandPort<C>>, TaskAccessError>>
    + Send
    + 'static
    + use<S, A, C>
    where
        S: 'static,
        A: Send + 'static,
    {
        store.command_port_for_test(site, self.core.clone())
    }
    /// Borrows the exact typed output outside locks, restoring it on unwind.
    ///
    /// # Panics
    ///
    /// Propagates callback panics after restoring the payload, before its command
    /// can finish any deferred retirement.
    pub fn inspect_output<O: Send + 'static, T>(
        &self,
        output: &crate::Output<O>,
        inspect: impl FnOnce(&O) -> T,
    ) -> Option<T> {
        self.core.inspect_output(output, inspect)
    }
    /// Matches an exact output, retaining false or unwinding comparison debt.
    ///
    /// # Panics
    ///
    /// Propagates matcher panics after restoring the payload and retaining
    /// failed-comparison debt.
    pub fn match_output<O: Send + 'static>(
        &self,
        output: &crate::Output<O>,
        matcher: impl FnOnce(&O) -> bool,
    ) -> Option<bool> {
        self.core.match_output(output, matcher)
    }
    /// Captures the historical boundary for this exact output declaration.
    #[must_use]
    pub fn output_boundary<O: Send + 'static>(
        &self,
        output: &crate::Output<O>,
    ) -> Option<TransactionBoundary> {
        self.core.output_boundary(output)
    }

    /// Returns historical removal metadata for this exact pending output request.
    #[must_use]
    pub fn output_origin<O: Send + 'static>(
        &self,
        output: &crate::Output<O>,
    ) -> Option<crate::RemovalOrigin> {
        self.core.output_origin(output)
    }
}

#[cfg(feature = "local")]
pub mod local {
    //! Owner-thread typed frontier handles for deterministic testing.
    use super::{
        ActorDiagnostics, AssertionReceipt, AssertionStateReceipt, Carriers, GateQuery,
        RuntimeSnapshot, TaskAccessError, TransactionBoundary, TransactionGateBinding,
        TransactionGateCore, TransactionKind, fmt,
    };
    gate_facade!(crate::store::Local);

    #[cfg(test)]
    ordinary_test_items! {
        mod fold_reference_costs {
            use std::sync::Arc;
            use crate::local as domain;
            use std::rc::Rc as Ownership;
            use std::cell::RefCell as Storage;
            type Gate = super::TransactionGate<Option<usize>, ()>;
            super::super::fold_reference_costs::endpoint_reference_case!(borrow_mut);
        }
    }

    impl<S, A> TransactionGate<S, A> {
        /// Checks readiness without driving Initial work or admitting a fold.
        ///
        /// # Errors
        ///
        /// Returns the root access errors of [`super::TransactionGate::validate_fold`].
        pub fn validate_fold(
            &self,
            store: &crate::local::Store<S, A>,
        ) -> Result<(), TaskAccessError> {
            store.validate_fold_for_test()
        }
        /// Folds owner-local held state without releasing mutation or assertion debt.
        ///
        /// # Errors
        ///
        /// Returns the lifecycle, admission and execution errors of
        /// [`super::TransactionGate::fold`], without requiring `Send` values.
        pub fn fold<K: crate::FoldKey>(
            &self,
            store: &crate::local::Store<S, A>,
            fold: &crate::local::Fold<S, K>,
        ) -> impl Future<Output = Result<K::Value, TaskAccessError>> + 'static + use<S, A, K>
        where
            S: 'static,
            A: 'static,
        {
            store.fold_for_test(fold, self.core.clone())
        }
        /// Captures owner-local subtree authority and the matching diagnostic gate.
        ///
        /// # Errors
        ///
        /// Returns the authority and admission errors of
        /// [`super::TransactionGate::acquire_fold`]. An absent child is `Ok(None)`.
        pub fn acquire_fold<K: crate::FoldKey, CS: 'static, CA: 'static>(
            &self,
            store: &crate::local::Store<S, A>,
            fold: &crate::local::Fold<S, K>,
            site: &crate::local::MountSite<S, CS, CA, A>,
        ) -> impl Future<Output = Result<Option<crate::local::FoldView<K>>, TaskAccessError>>
        + 'static
        + use<S, A, K, CS, CA>
        where
            S: 'static,
            A: 'static,
        {
            store.acquire_fold_for_test(fold, site, self.core.clone())
        }
        /// Reads a diagnostically acquired local view after exact-root validation.
        ///
        /// # Errors
        ///
        /// Returns the authority, phase, admission and execution errors of
        /// [`super::TransactionGate::fold_view`]. Stale views are not reacquired.
        pub fn fold_view<K: crate::FoldKey>(
            &self,
            store: &crate::local::Store<S, A>,
            view: &crate::local::FoldView<K>,
        ) -> impl Future<Output = Result<K::Value, TaskAccessError>> + 'static + use<S, A, K>
        {
            store.fold_view_for_test(view)
        }
        /// Checks lifecycle and weak declaration metadata without driving the actor.
        ///
        /// # Errors
        ///
        /// Returns the access and declaration errors of
        /// [`super::TransactionGate::validate_acquisition`].
        pub fn validate_acquisition<CS, CA>(
            &self,
            store: &crate::local::Store<S, A>,
            site: &crate::local::MountSite<S, CS, CA, A>,
        ) -> Result<(), TaskAccessError> {
            store.validate_acquisition_for_test(site)
        }
        /// Acquires owner-local child authority without releasing the pending mutation.
        ///
        /// # Errors
        ///
        /// Returns the authority and admission errors of
        /// [`super::TransactionGate::acquire`]. An absent child is `Ok(None)`.
        pub fn acquire<CS: 'static, CA: 'static>(
            &self,
            store: &crate::local::Store<S, A>,
            site: &crate::local::MountSite<S, CS, CA, A>,
        ) -> impl Future<
            Output = Result<Option<crate::local::TaskContext<CS, CA>>, TaskAccessError>,
        >
        + 'static
        + use<S, A, CS, CA>
        where
            S: 'static,
            A: 'static,
        {
            store.acquire_for_test(site, self.core.clone())
        }
        /// Borrows a mounted owner-local action, restoring its original reply on unwind.
        ///
        /// # Errors
        ///
        /// Returns the endpoint authority errors of
        /// [`super::TransactionGate::inspect_mounted`]. A different input is `Ok(None)`.
        ///
        /// # Panics
        ///
        /// Propagates callback panics after restoring the original request.
        pub fn inspect_mounted<CS, CA, T>(
            &self,
            context: &crate::local::TaskContext<CS, CA>,
            inspect: impl FnOnce(&CA) -> T,
        ) -> Result<Option<T>, TaskAccessError> {
            self.core.inspect_mounted(context, inspect)
        }
        /// Matches an exact mounted action, retaining false or unwinding comparison debt.
        ///
        /// # Errors
        ///
        /// Returns the authority errors described by [`Self::inspect_mounted`].
        ///
        /// # Panics
        ///
        /// Propagates matcher panics and retains failed-comparison debt.
        pub fn match_mounted<CS, CA>(
            &self,
            context: &crate::local::TaskContext<CS, CA>,
            matcher: impl FnOnce(&CA) -> bool,
        ) -> Result<Option<bool>, TaskAccessError> {
            self.core.match_mounted(context, matcher)
        }
        /// Borrows an original owner-local command, restoring its reply on unwind.
        ///
        /// # Errors
        ///
        /// Returns the port authority errors of [`super::TransactionGate::inspect_command`].
        /// A different input is `Ok(None)` and does not invoke the command mapper.
        ///
        /// # Panics
        ///
        /// Propagates callback panics after restoring the original request.
        pub fn inspect_command<C, T>(
            &self,
            port: &crate::local::CommandPort<C>,
            inspect: impl FnOnce(&C) -> T,
        ) -> Result<Option<T>, TaskAccessError> {
            self.core.inspect_mounted(port, inspect)
        }
        /// Matches an exact command, retaining false or unwinding comparison debt.
        ///
        /// # Errors
        ///
        /// Returns the authority errors described by [`Self::inspect_command`].
        ///
        /// # Panics
        ///
        /// Propagates matcher panics and retains failed-comparison debt.
        pub fn match_command<C>(
            &self,
            port: &crate::local::CommandPort<C>,
            matcher: impl FnOnce(&C) -> bool,
        ) -> Result<Option<bool>, TaskAccessError> {
            self.core.match_mounted(port, matcher)
        }
        /// Checks actual lifecycle and command metadata before driving owner-local work.
        ///
        /// # Errors
        ///
        /// Returns the access and declaration errors of
        /// [`super::TransactionGate::validate_command_acquisition`].
        pub fn validate_command_acquisition<C>(
            &self,
            store: &crate::local::Store<S, A>,
            site: &crate::local::CommandSite<S, C>,
        ) -> Result<(), TaskAccessError> {
            store.validate_command_acquisition_for_test(site)
        }
        /// Acquires an owner-local command port without releasing the pending mutation.
        ///
        /// # Errors
        ///
        /// Returns the authority and admission errors of
        /// [`super::TransactionGate::command_port`]. An absent route is `Ok(None)`.
        pub fn command_port<C: 'static>(
            &self,
            store: &crate::local::Store<S, A>,
            site: &crate::local::CommandSite<S, C>,
        ) -> impl Future<Output = Result<Option<crate::local::CommandPort<C>>, TaskAccessError>>
        + 'static
        + use<S, A, C>
        where
            S: 'static,
            A: 'static,
        {
            store.command_port_for_test(site, self.core.clone())
        }
        /// Borrows an owner-local output, restoring it before frontier retirement.
        ///
        /// # Panics
        ///
        /// Propagates callback panics after restoring the payload.
        pub fn inspect_output<O: 'static, T>(
            &self,
            output: &crate::local::Output<O>,
            inspect: impl FnOnce(&O) -> T,
        ) -> Option<T> {
            self.core.inspect_output(output, inspect)
        }
        /// Matches an exact output, retaining false or unwinding comparison debt.
        ///
        /// # Panics
        ///
        /// Propagates matcher panics after restoring the payload and retaining
        /// failed-comparison debt.
        pub fn match_output<O: 'static>(
            &self,
            output: &crate::local::Output<O>,
            matcher: impl FnOnce(&O) -> bool,
        ) -> Option<bool> {
            self.core.match_output(output, matcher)
        }
        /// Captures the historical boundary for this exact output declaration.
        #[must_use]
        pub fn output_boundary<O: 'static>(
            &self,
            output: &crate::local::Output<O>,
        ) -> Option<TransactionBoundary> {
            self.core.output_boundary(output)
        }

        /// Returns historical removal metadata for this exact pending output request.
        #[must_use]
        pub fn output_origin<O: 'static>(
            &self,
            output: &crate::local::Output<O>,
        ) -> Option<crate::RemovalOrigin> {
            self.core.output_origin(output)
        }
    }
}

#[cfg(test)]
ordinary_test_items! {
impl<S, A, C: Carriers<S, A>> TransactionGateCore<S, A, C> {
    /// Checks guard availability before same-thread test re-entry can block.
    ///
    /// This probe releases the guard before returning. It does not establish
    /// availability across concurrent access from another thread.
    pub(crate) fn test_guard_available(&self) -> bool {
        self.shared.state.try_lock().is_ok()
    }
}

mod tests {
    use super::*;

    #[test]
    fn guard_probe_distinguishes_held_and_released_mutex() {
        let gate = TransactionGateCore::<(), ()>::new();
        assert!(gate.test_guard_available());

        let guard = gate.shared.state.lock().expect("gate lock");
        assert!(!gate.test_guard_available());
        drop(guard);

        assert!(gate.test_guard_available());
        assert!(!gate.has_pending());
    }

    #[cfg(feature = "local")]
    mod local_controls {
        use super::*;
        use crate::store::Local;
        use std::cell::Cell;
        use std::rc::Rc;
        type Domain = Rc<usize>;
        type Gate = local::TransactionGate<Domain, Domain>;
        struct Owner {
            gate: TransactionGateCore<Domain, Domain, Local>,
            rejected: Rc<Cell<usize>>,
            dropped: Rc<Cell<usize>>,
            thread: std::thread::ThreadId,
        }
        impl Owner {
            fn reject(&self, error: TaskAccessError) {
                assert_eq!(error.reason(), TaskAccessReason::Cancelled);
                assert_eq!(error.progress(), TaskProgress::NotStarted);
                assert_eq!(std::thread::current().id(), self.thread);
                assert!(
                    self.gate.shared.state.try_lock().is_ok(),
                    "reject outside gate lock"
                );
                self.rejected.set(self.rejected.get() + 1);
            }
        }
        impl Drop for Owner {
            fn drop(&mut self) {
                assert_eq!(std::thread::current().id(), self.thread);
                assert!(
                    self.gate.shared.state.try_lock().is_ok(),
                    "drop outside gate lock"
                );
                self.dropped.set(self.dropped.get() + 1);
            }
        }
        struct Update(Owner);
        impl UpdateCall<Domain> for Update {
            type Reply = <Local as Carriers<Domain, Domain>>::Reply;
            fn attach_terminal(&mut self, _: TerminalReceipt) {}
            fn mark_entered(&mut self) {
                panic!("gate must not enter update");
            }
            fn apply(
                self: Box<Self>,
                _: &mut Domain,
            ) -> crate::store::task_context::Apply<Self::Reply> {
                panic!("gate must not apply update");
            }
            fn reject(self: Box<Self>, error: TaskAccessError) {
                self.0.reject(error);
            }
        }
        struct Mounted(Owner);
        impl MountedCall<Domain, Domain, Local> for Mounted {
            fn attach_terminal(&mut self, _: TerminalReceipt) {}
            fn input_rejection(
                &self,
                _: &crate::store::scope::ScopeRegistry,
            ) -> Option<TaskAccessError> {
                None
            }
            fn apply(
                self: Box<Self>,
                _: &mut Domain,
                _: crate::ReduceContext<'_>,
                _: Option<&mut crate::store::reaction::BeforeMutation<'_, Domain>>,
            ) -> crate::store::mounted::MountedApply<
                <Local as Carriers<Domain, Domain>>::Reply,
                <Local as Carriers<Domain, Domain>>::MountedWork,
            > {
                panic!("gate must not apply mounted callback");
            }
            fn reject(self: Box<Self>, error: TaskAccessError) {
                self.0.reject(error);
            }
        }
        #[test]
        fn local_query_retirement_drops_capture_on_owner_after_unlock() {
            let gate = Gate::new();
            let binding = gate.attach().unwrap();
            let core = binding.gate().clone();
            binding.gate().bind_root(Arc::new(()));
            binding
                .gate()
                .begin(Pending::Structural, diagnostics(), false);
            let rejected = Rc::new(Cell::new(0));
            let dropped = Rc::new(Cell::new(0));
            let owner = Owner {
                gate: binding.gate().clone(),
                rejected: Rc::clone(&rejected),
                dropped: Rc::clone(&dropped),
                thread: std::thread::current().id(),
            };
            let mut receipt = Box::pin(gate.assert_state(move |_| drop(owner)).finish());
            let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
            assert!(receipt.as_mut().poll(&mut cx).is_pending());
            assert_eq!(dropped.get(), 0);
            drop(binding);
            assert_eq!(dropped.get(), 1);
            assert_eq!(rejected.get(), 0);
            assert!(matches!(
                receipt.as_mut().poll(&mut cx),
                std::task::Poll::Ready(Err(_))
            ));
            acknowledge_closed(&core);
            assert!(gate.boundary().is_none());
        }
        #[test]
        fn local_update_and_mount_preserve_owned_commands_until_out_of_lock_rejection() {
            let gate = Gate::new();
            let binding = gate.attach().unwrap();
            let rejected = Rc::new(Cell::new(0));
            let dropped = Rc::new(Cell::new(0));
            let owner = || Owner {
                gate: binding.gate().clone(),
                rejected: Rc::clone(&rejected),
                dropped: Rc::clone(&dropped),
                thread: std::thread::current().id(),
            };
            let (lifecycle, scopes) = crate::store::scope::ScopeRegistry::for_test();
            drop(lifecycle);
            let commands: [Command<Domain, Domain, Local>; 2] = [
                Command::Task {
                    request: TaskRequest::<Domain, Domain, Local>::Update(Box::new(
                        Update(owner()),
                    )),
                    provenance: Provenance::External { handle: None },
                    authority: scopes.authority(scopes.root()),
                    operation_work: OperationWork::default(),
                },
                Command::Mounted {
                    call: Box::new(Mounted(owner())),
                    provenance: Provenance::External { handle: None },
                    authority: scopes.authority(scopes.root()),
                    operation_work: OperationWork::default(),
                },
            ];
            for (index, command) in commands.into_iter().enumerate() {
                let pending = Pending::from_command(command).ok().unwrap();
                let pending = Pending::from_command(pending.into_command()).ok().unwrap();
                binding.gate().begin(pending, diagnostics(), false);
                assert_eq!(
                    gate.pending_kind(),
                    Some(if index == 0 {
                        TransactionKind::Update
                    } else {
                        TransactionKind::Action
                    })
                );
                assert!(
                    gate.inspect_pending(|_| panic!("non-action matcher must not run"))
                        .is_none()
                );
                assert_eq!(rejected.get(), index);
                assert_eq!(dropped.get(), index);
                binding.gate().clear();
                assert_eq!(rejected.get(), index + 1);
                assert_eq!(dropped.get(), index + 1);
            }
            drop(binding);
            gate.attach()
                .as_ref()
                .expect("released gate can attach again");
        }
        #[test]
        fn local_action_inspection_restores_original_rc_after_unwind() {
            let gate = Gate::new();
            let binding = gate.attach().unwrap();
            let action = Rc::new(19);
            let (reply, _) = crate::store::command::commit_channel();
            binding.gate().begin(
                Pending::Action(ReadyActionCommand {
                    action: Rc::clone(&action),
                    provenance: Provenance::External { handle: None },
                    reply,
                    assertion_reply: None,
                    operation_work: OperationWork::default(),
                }),
                diagnostics(),
                false,
            );
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                gate.inspect_pending(|_| panic!("matcher panic"))
            }))
            .as_ref()
            .expect_err("matcher must panic");
            assert_eq!(
                gate.inspect_pending(|value| Rc::ptr_eq(value, &action)),
                Some(true)
            );
            assert_eq!(Rc::strong_count(&action), 2);
            drop(binding);
            assert_eq!(Rc::strong_count(&action), 1);
        }
    }
    #[cfg(feature = "local")]
    #[test]
    fn local_queries_keep_rc_captures_bounded_without_acknowledging() {
        use std::cell::Cell;
        use std::rc::Rc;
        let gate = local::TransactionGate::<Rc<usize>, Rc<usize>>::new();
        let binding = gate.attach().unwrap();
        let core = binding.gate().clone();
        core.bind_root(Arc::new(()));
        core.begin(Pending::Structural, diagnostics(), false);
        let boundary = gate.boundary();
        let calls = Rc::new(Cell::new(0));
        let capture = Rc::clone(&calls);
        let reentrant = gate.clone();
        let owner = std::thread::current().id();
        let mut first = Box::pin(
            gate.assert_state(move |state| {
                assert_eq!(std::thread::current().id(), owner);
                assert_eq!(**state, 7);
                assert_eq!(reentrant.boundary(), boundary);
                capture.set(capture.get() + 1);
            })
            .finish(),
        );
        let capture = Rc::clone(&calls);
        let mut second = Box::pin(gate.assert_state(move |_| capture.set(99)).finish());
        let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
        assert!(first.as_mut().poll(&mut cx).is_pending());
        assert!(second.as_mut().poll(&mut cx).is_pending());
        assert_eq!(Rc::strong_count(&calls), 3);
        drop(second);
        assert_eq!(Rc::strong_count(&calls), 2);
        let GateQuery::State(query) = core.take_query().unwrap() else {
            panic!("state query");
        };
        query(&Rc::new(7));
        assert!(matches!(
            first.as_mut().poll(&mut cx),
            std::task::Poll::Ready(Ok(()))
        ));
        assert_eq!(calls.get(), 1);
        assert_eq!(Rc::strong_count(&calls), 1);
        assert!(core.take_query().is_none());
        assert_eq!(gate.boundary(), boundary);
        assert!(!gate.skip_pending());
        drop(binding);
        acknowledge_closed(&core);
        assert!(gate.boundary().is_none());
    }
    use std::sync::atomic::{AtomicUsize, Ordering};
    struct WakeCount(AtomicUsize);
    impl std::task::Wake for WakeCount {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }
    struct Dropped(Arc<AtomicUsize>);
    impl Drop for Dropped {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }
    fn diagnostics() -> ActorDiagnostics {
        ActorDiagnostics::new(
            RuntimeSnapshot {
                commit_version: 0,
                live_task_ids: Vec::new(),
                cancellation_count: 0,
                timer_deadlines: Vec::new(),
                observation_count: 0,
            },
            Vec::new(),
            0,
        )
    }
    fn pending(action: String) -> Pending<(), String> {
        let (reply, _) = crate::store::command::commit_channel();
        Pending::Action(ReadyActionCommand {
            action,
            provenance: Provenance::External { handle: None },
            reply,
            assertion_reply: None,
            operation_work: OperationWork::default(),
        })
    }
    fn acknowledge_closed<S, A, C: Carriers<S, A>>(gate: &TransactionGateCore<S, A, C>) {
        let boundary = gate
            .pending_boundary()
            .expect("retirement retains terminal debt");
        assert!(
            gate.attach().is_err(),
            "terminal debt reserves the attachment"
        );
        assert_eq!(
            gate.match_terminal(&boundary, |error| {
                *error
                    == TaskAccessError::new(TaskAccessReason::StoreClosed, TaskProgress::NotStarted)
            }),
            Some(true)
        );
        assert!(gate.acknowledge_terminal(&boundary));
    }

    #[test]
    fn terminal_requests_reject_stale_generation_without_consuming_current_debt() {
        let gate = TransactionGate::<(), String>::new();
        let binding = gate.attach().unwrap();
        gate.core.bind_root(Arc::new(()));
        gate.core
            .begin(pending("earlier request".into()), diagnostics(), false);
        let stale = gate.pending_boundary().unwrap();
        assert_eq!(gate.boundary(), Some(1));
        gate.core.clear();
        assert_eq!(gate.boundary(), None);

        gate.core
            .begin(pending("current request".into()), diagnostics(), false);
        let current = gate.pending_boundary().unwrap();
        let rejected =
            TaskAccessError::new(TaskAccessReason::Cancelled, TaskProgress::NotStarted);
        gate.core.rejected(rejected);
        assert_eq!(gate.boundary(), Some(2));

        let mut matcher_called = false;
        assert_eq!(
            gate.match_terminal(&stale, |_| {
                matcher_called = true;
                true
            }),
            None
        );
        assert!(!matcher_called);
        assert!(!gate.acknowledge_terminal(&stale));
        assert_eq!(gate.boundary(), Some(2));
        assert_eq!(
            gate.match_terminal(&current, |error| *error == rejected),
            Some(true)
        );
        assert_eq!(gate.boundary(), Some(2));
        assert!(gate.acknowledge_terminal(&current));
        assert_eq!(gate.boundary(), None);
        assert!(!gate.acknowledge_terminal(&current));
        drop(binding);
    }

    #[test]
    fn terminal_requests_reject_foreign_kind_at_the_same_generation() {
        let gate = TransactionGate::<(), String>::new();
        let other = TransactionGate::<(), String>::new();
        let binding = gate.attach().unwrap();
        let other_binding = other.attach().unwrap();
        gate.core.bind_root(Arc::new(()));
        other.core.bind_root(Arc::new(()));
        gate.core
            .begin(pending("action request".into()), diagnostics(), false);
        other.core.begin(Pending::Structural, diagnostics(), false);
        let current = gate.pending_boundary().unwrap();
        let foreign = other.pending_boundary().unwrap();
        assert_eq!(gate.boundary(), Some(1));
        assert_eq!(other.boundary(), Some(1));
        assert_eq!(gate.pending_kind(), Some(TransactionKind::Action));
        assert_eq!(other.pending_kind(), Some(TransactionKind::Structural));

        let rejected =
            TaskAccessError::new(TaskAccessReason::Cancelled, TaskProgress::NotStarted);
        gate.core.rejected(rejected);
        let mut matcher_called = false;
        assert_eq!(
            gate.match_terminal(&foreign, |_| {
                matcher_called = true;
                true
            }),
            None
        );
        assert!(!matcher_called);
        assert!(!gate.acknowledge_terminal(&foreign));
        assert_eq!(gate.boundary(), Some(1));
        assert_eq!(
            gate.match_terminal(&current, |error| *error == rejected),
            Some(true)
        );
        assert_eq!(gate.boundary(), Some(1));
        assert!(gate.acknowledge_terminal(&current));
        assert_eq!(gate.boundary(), None);
        assert!(!gate.acknowledge_terminal(&current));
        drop((binding, other_binding));
    }

    #[test]
    fn phase_acknowledged_commit_with_failed_match_does_not_acquire_terminal_failure_on_close() {
        let gate = TransactionGate::<(), String>::new();
        let binding = gate.attach().unwrap();
        gate.core.bind_root(Arc::new(()));
        gate.core
            .begin(pending("matched request".into()), diagnostics(), false);
        let boundary = gate.pending_boundary().unwrap();
        assert_eq!(
            gate.inspect_pending(|_| gate.core.match_value(|| false)),
            Some(false)
        );
        assert!(gate.release_pending());
        let (Pending::Action(command), _) = gate.core.take_decided().unwrap() else {
            panic!("action");
        };
        let _ = command.reply.send(Ok(ActionTask::completed()));
        gate.core.committed(ActionTask::completed(), diagnostics());
        assert!(gate.acknowledge(gate.boundary().unwrap()));
        assert!(!gate.acknowledge(gate.boundary().unwrap()));
        assert!(gate.has_failed_match());
        gate.core.close();
        assert_eq!(
            gate.match_terminal(&boundary, |_| panic!(
                "committed request has no terminal failure"
            )),
            None
        );
        assert!(
            gate.boundary().is_some(),
            "the failed comparison remains debt"
        );
        drop(binding);
        assert!(gate.attach().is_err());
    }
    #[test]
    fn phase_queued_skip_cannot_erase_a_later_failed_match() {
        for panics in [false, true] {
            let gate = TransactionGate::<(), String>::new();
            let binding = gate.attach().unwrap();
            gate.core
                .begin(pending("original request".into()), diagnostics(), false);
            let boundary = gate.boundary();
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                gate.inspect_pending(|_| {
                    gate.core.match_value(|| {
                        assert!(gate.skip_pending(), "skip can be queued while matching");
                        assert!(!panics, "matcher unwound after queuing skip");
                        false
                    })
                })
                .unwrap()
            }));
            assert_eq!(result.is_err(), panics);
            assert!(gate.has_failed_match());
            assert!(
                gate.core.take_decided().is_none(),
                "failed matching must invalidate the queued skip"
            );
            assert_eq!(gate.boundary(), boundary);
            assert_eq!(
                gate.inspect_pending(Clone::clone).as_deref(),
                Some("original request")
            );
            assert!(!gate.skip_pending());
            drop(binding);
        }
    }
    #[test]
    fn terminal_retirement_preserves_borrowed_frontier_until_inspection_returns() {
        for close_before_detach in [false, true] {
            let gate = TransactionGate::<(), String>::new();
            let mut binding = Some(gate.attach().unwrap());
            gate.core.bind_root(Arc::new(()));
            let original = String::from("still borrowed");
            let identity = original.as_ptr();
            gate.core.begin(pending(original), diagnostics(), false);
            let boundary = gate.boundary();
            gate.inspect_pending(|action| {
                if close_before_detach {
                    gate.core.close();
                    assert!(matches!(
                        gate.core.shared.state.lock().unwrap().metadata.retirement,
                        Retirement::Requested { detach: false }
                    ));
                }
                drop(binding.take());
                assert!(matches!(
                    gate.core.shared.state.lock().unwrap().metadata.retirement,
                    Retirement::Requested { detach: true }
                ));
                assert_eq!(
                    gate.boundary(),
                    boundary,
                    "retirement must preserve the borrowed boundary"
                );
                assert!(gate.has_pending(), "inspection remains the pending owner");
                let Err(error) = gate.attach() else {
                    panic!("a retiring inspection reserves its attachment");
                };
                assert!(error.is_assertion_gate_attached());
                assert!(
                    error.to_string().starts_with(
                        "test gate is still attached or reserved by unfinished work\n"
                    )
                );
                assert_eq!(action.as_ptr(), identity);
                assert_eq!(action, "still borrowed");
            })
            .unwrap();
            assert!(!gate.has_pending());
            acknowledge_closed(&gate.core);
            assert!(gate.boundary().is_none());
            drop(gate.attach().unwrap());
        }
    }
    #[test]
    fn terminal_acknowledgement_waits_for_unborrowed_capture_disposal() {
        struct Capture {
            gate: TransactionGate<(), String>,
            boundary: TransactionBoundary,
            drops: Arc<AtomicUsize>,
        }
        impl Drop for Capture {
            fn drop(&mut self) {
                {
                    let state = self
                        .gate
                        .core
                        .shared
                        .state
                        .try_lock()
                        .expect("capture disposal runs outside the gate lock");
                    assert!(matches!(
                        state.metadata.retirement,
                        Retirement::Draining { detach: true }
                    ));
                    assert!(
                        !state.inspection,
                        "no borrowed inspection may mask the retirement guard"
                    );
                    assert!(state.pending.is_none());
                    assert!(state.metadata.matches(&self.boundary));
                    assert_eq!(
                        state.metadata.terminal,
                        Some(TaskAccessError::new(
                            TaskAccessReason::StoreClosed,
                            TaskProgress::NotStarted
                        ))
                    );
                    assert!(!state.metadata.terminal_acknowledged);
                };
                assert!(!self.gate.has_pending());
                assert!(
                    !self.gate.acknowledge_terminal(&self.boundary),
                    "terminal acknowledgement must wait for capture disposal"
                );
                assert_eq!(self.gate.boundary(), Some(self.boundary.id));
                assert!(
                    self.gate.attach().is_err(),
                    "the draining generation still owns its attachment"
                );
                assert_eq!(self.drops.fetch_add(1, Ordering::SeqCst), 0);
            }
        }
        let gate = TransactionGate::<(), String> {
            core: TransactionGateCore::default(),
        };
        assert_eq!(
            gate.inspect_pending(|_| panic!("an unattached gate has no pending action")),
            None::<()>
        );
        let binding = gate.attach().unwrap();
        gate.core.bind_root(Arc::new(()));
        gate.core.begin(
            pending("retired before execution".into()),
            diagnostics(),
            false,
        );
        let boundary = gate.pending_boundary().unwrap();
        let drops = Arc::new(AtomicUsize::new(0));
        let capture = Capture {
            gate: gate.clone(),
            boundary: boundary.clone(),
            drops: Arc::clone(&drops),
        };
        let mut query = Box::pin(
            gate.assert_state(move |()| {
                drop(capture);
                panic!("retirement must reject, not execute, the queued state query");
            })
            .finish(),
        );
        let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
        assert!(query.as_mut().poll(&mut cx).is_pending());
        assert_eq!(drops.load(Ordering::SeqCst), 0);

        // Dropping the actual actor binding drains the queued capture without an inspection lease.
        drop(binding);

        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert!(gate.core.retirement_finished());
        assert!(!gate.has_pending());
        assert_eq!(
            gate.match_terminal(&boundary, |error| *error
                == TaskAccessError::new(
                    TaskAccessReason::StoreClosed,
                    TaskProgress::NotStarted
                )),
            Some(true)
        );
        assert!(gate.acknowledge_terminal(&boundary));
        assert!(gate.boundary().is_none());
        assert!(matches!(
            query.as_mut().poll(&mut cx),
            std::task::Poll::Ready(Err(_))
        ));
        drop(gate.attach().unwrap());
    }
    #[expect(
        clippy::too_many_lines,
        reason = "one ordered witness keeps matcher lifetime, destructor re-entry and terminal completion visible"
    )]
    fn retirement_capture_control(
        matcher_panics: bool,
        capture_panics: bool,
        binding_in_capture: bool,
    ) {
        use std::sync::atomic::AtomicBool;
        struct Active(Arc<AtomicBool>);
        impl Drop for Active {
            fn drop(&mut self) {
                self.0.store(false, Ordering::SeqCst);
            }
        }
        struct Capture {
            gate: TransactionGate<(), String>,
            active: Arc<AtomicBool>,
            drops: Arc<AtomicUsize>,
            panic: bool,
            binding: Option<TransactionGateBinding<(), String>>,
        }
        impl Drop for Capture {
            fn drop(&mut self) {
                assert!(
                    !self.active.load(Ordering::SeqCst),
                    "matcher must return first"
                );
                assert!(
                    self.gate.core.shared.state.try_lock().is_ok(),
                    "no payload Drop under gate lock"
                );
                assert!(
                    !self.gate.core.retirement_finished(),
                    "destruction precedes terminal completion"
                );
                assert!(
                    self.gate.attach().is_err(),
                    "destruction still reserves the attachment"
                );
                if let Some(binding) = self.binding.take() {
                    assert!(matches!(
                        self.gate
                            .core
                            .shared
                            .state
                            .lock()
                            .unwrap()
                            .metadata
                            .retirement,
                        Retirement::Draining { detach: false }
                    ));
                    drop(binding);
                    assert!(matches!(
                        self.gate
                            .core
                            .shared
                            .state
                            .lock()
                            .unwrap()
                            .metadata
                            .retirement,
                        Retirement::Draining { detach: true }
                    ));
                    assert!(
                        self.gate.attach().is_err(),
                        "detachment must wait for capture cleanup"
                    );
                }
                self.drops.fetch_add(1, Ordering::SeqCst);
                assert!(!self.panic, "deliberate retirement capture panic");
            }
        }
        let gate = TransactionGate::<(), String>::new();
        let mut binding = Some(gate.attach().unwrap());
        gate.core.bind_root(Arc::new(()));
        let original = String::from("original");
        let identity = original.as_ptr();
        gate.core.begin(pending(original), diagnostics(), false);
        let boundary = gate.boundary();
        let active = Arc::new(AtomicBool::new(false));
        let drops = Arc::new(AtomicUsize::new(0));
        let capture = Capture {
            gate: gate.clone(),
            active: Arc::clone(&active),
            drops: Arc::clone(&drops),
            panic: capture_panics,
            binding: if binding_in_capture {
                binding.take()
            } else {
                None
            },
        };
        let mut query = Box::pin(gate.assert_state(move |()| drop(capture)).finish());
        let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
        assert!(query.as_mut().poll(&mut cx).is_pending());
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            gate.inspect_pending(|action| {
                active.store(true, Ordering::SeqCst);
                let _active = Active(Arc::clone(&active));
                if binding_in_capture {
                    gate.core.close();
                } else {
                    drop(binding.take());
                }
                assert_eq!(gate.boundary(), boundary);
                assert_eq!(drops.load(Ordering::SeqCst), 0);
                assert!(!gate.release_pending());
                assert!(!gate.skip_pending());
                assert_eq!(action.as_ptr(), identity);
                assert_eq!(action, "original");
                assert!(
                    !matcher_panics,
                    "deliberate matcher panic during retirement"
                );
            })
            .unwrap();
        }));
        assert_eq!(result.is_err(), matcher_panics || capture_panics);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert!(gate.core.retirement_finished());
        acknowledge_closed(&gate.core);
        assert!(gate.boundary().is_none());
        assert!(!gate.has_pending());
        assert!(
            matches!(query.as_mut().poll(&mut cx), std::task::Poll::Ready(Err(_))),
            "retirement must reject, not execute, the queued state query"
        );
        drop(gate.attach().unwrap());
    }
    #[test]
    fn terminal_retirement_drains_after_matcher_unwind_outside_lock() {
        retirement_capture_control(true, false, false);
    }
    #[test]
    fn terminal_retirement_finishes_after_capture_drop_unwind() {
        retirement_capture_control(false, true, false);
    }
    #[test]
    fn terminal_retirement_detaches_during_queued_query_capture_drop() {
        retirement_capture_control(false, false, true);
    }
    #[test]
    fn typed_frontier_inspection_restores_on_panic_and_clear_invalidates_restoration() {
        let gate = TransactionGate::<(), String>::default();
        let binding = gate.attach().unwrap();
        assert!(gate.attach().is_err());
        gate.core
            .begin(pending("owned".into()), diagnostics(), false);
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            gate.inspect_pending(|_| panic!("matcher panic"))
        }))
        .as_ref()
        .expect_err("matcher must panic");
        assert_eq!(gate.inspect_pending(Clone::clone).as_deref(), Some("owned"));
        gate.inspect_pending(|_| gate.core.clear()).unwrap();
        assert!(!gate.has_pending());
        assert!(gate.boundary().is_none());
        gate.core.begin(
            pending("release after inspection".into()),
            diagnostics(),
            false,
        );
        assert!(gate.core.take_decided().is_none());
        gate.inspect_pending(|_| {
            assert!(gate.release_pending());
            assert!(gate.core.take_decided().is_none());
        })
        .unwrap();
        let (command, decision) = gate.core.take_decided().unwrap();
        assert!(matches!(decision, GateDecision::Release));
        assert_eq!(command.action().unwrap(), "release after inspection");
        gate.core.clear();
        drop(command);
        drop(binding);
        drop(gate.attach().unwrap());
    }
    #[test]
    fn typed_diagnostic_queue_is_bounded_and_never_acknowledges() {
        fn send_static<T: Send + 'static>(value: T) -> T {
            value
        }
        let gate = TransactionGate::<std::cell::Cell<usize>, String>::new();
        let binding = gate.attach().unwrap();
        let (reply, _) = crate::store::command::commit_channel();
        gate.core.begin(
            Pending::Action(ReadyActionCommand {
                action: String::new(),
                provenance: Provenance::External { handle: None },
                reply,
                assertion_reply: None,
                operation_work: OperationWork::default(),
            }),
            diagnostics(),
            false,
        );
        let boundary = gate.boundary();
        let mut first =
            Box::pin(send_static(gate.assert_state(|value| assert_eq!(value.get(), 7))).finish());
        let mut second =
            Box::pin(send_static(gate.assert_state(|value| assert_eq!(value.get(), 8))).finish());
        let wakes = Arc::new(WakeCount(AtomicUsize::new(0)));
        let task_waker = std::task::Waker::from(Arc::clone(&wakes));
        let mut context = std::task::Context::from_waker(&task_waker);
        assert!(first.as_mut().poll(&mut context).is_pending());
        assert!(
            second.as_mut().poll(&mut context).is_pending(),
            "a full diagnostic slot must wait, not report StoreStopped"
        );
        assert_eq!(
            wakes.0.load(Ordering::Relaxed),
            0,
            "saturation must not self-wake"
        );
        let GateQuery::State(query) = gate.core.take_query().unwrap() else {
            panic!("state query");
        };
        assert!(
            wakes.0.load(Ordering::Relaxed) > 0,
            "freeing capacity wakes the owner"
        );
        query(&std::cell::Cell::new(7));
        assert!(matches!(
            first.as_mut().poll(&mut context),
            std::task::Poll::Ready(Ok(()))
        ));
        assert!(second.as_mut().poll(&mut context).is_pending());
        let GateQuery::State(query) = gate.core.take_query().unwrap() else {
            panic!("state query");
        };
        query(&std::cell::Cell::new(8));
        assert!(matches!(
            second.as_mut().poll(&mut context),
            std::task::Poll::Ready(Ok(()))
        ));
        assert_eq!(gate.boundary(), boundary);
        assert!(gate.has_pending());
        drop(binding);
    }
    #[test]
    fn diagnostic_waiter_owns_capture_and_cannot_cross_gate_reincarnation() {
        let gate = TransactionGate::<(), String>::new();
        let binding = gate.attach().unwrap();
        gate.core.bind_root(Arc::new(()));
        gate.core
            .begin(pending("first".into()), diagnostics(), false);
        let original = gate.boundary();
        let drops = Arc::new(AtomicUsize::new(0));
        let mut context = std::task::Context::from_waker(std::task::Waker::noop());
        let reentrant = gate.clone();
        let mut first = Box::pin(
            gate.assert_state(move |()| assert_eq!(reentrant.boundary(), original))
                .finish(),
        );
        assert!(first.as_mut().poll(&mut context).is_pending());
        let capture = Dropped(Arc::clone(&drops));
        let mut cancelled = Box::pin(gate.assert_state(move |()| drop(capture)).finish());
        assert!(cancelled.as_mut().poll(&mut context).is_pending());
        assert_eq!(drops.load(Ordering::Relaxed), 0);
        drop(cancelled);
        assert_eq!(drops.load(Ordering::Relaxed), 1);
        let GateQuery::State(query) = gate.core.take_query().unwrap() else {
            panic!("state query");
        };
        query(&());
        assert!(matches!(
            first.as_mut().poll(&mut context),
            std::task::Poll::Ready(Ok(()))
        ));
        assert!(gate.core.take_query().is_none());
        let mut occupying = Box::pin(gate.assert_state(|()| {}).finish());
        assert!(occupying.as_mut().poll(&mut context).is_pending());
        let capture = Dropped(Arc::clone(&drops));
        let mut old = Box::pin(gate.assert_state(move |()| drop(capture)).finish());
        assert!(old.as_mut().poll(&mut context).is_pending());
        drop(binding);
        acknowledge_closed(&gate.core);
        let replacement = gate.attach().unwrap();
        gate.core
            .begin(pending("replacement".into()), diagnostics(), false);
        assert_ne!(gate.boundary(), original);
        assert!(matches!(
            old.as_mut().poll(&mut context),
            std::task::Poll::Ready(Err(_))
        ));
        assert!(matches!(
            occupying.as_mut().poll(&mut context),
            std::task::Poll::Ready(Err(_))
        ));
        assert_eq!(drops.load(Ordering::Relaxed), 2);
        assert!(gate.core.take_query().is_none());
        assert!(gate.has_pending());
        drop(replacement);
    }
    #[test]
    fn query_admission_rejects_reentrant_capture_during_skip_retirement() {
        struct Capture {
            gate: TransactionGate<(), String>,
            drops: Arc<AtomicUsize>,
        }
        impl Drop for Capture {
            fn drop(&mut self) {
                let mut query = Box::pin(
                    self.gate
                        .assert_state(|()| {
                            panic!("a query admitted during retirement must not execute");
                        })
                        .finish(),
                );
                let mut context = std::task::Context::from_waker(std::task::Waker::noop());
                assert!(matches!(
                    query.as_mut().poll(&mut context),
                    std::task::Poll::Ready(Err(_))
                ));
                assert_eq!(self.drops.fetch_add(1, Ordering::SeqCst), 0);
            }
        }
        let gate = TransactionGate::<(), String>::new();
        let binding = gate.attach().unwrap();
        gate.core.bind_root(Arc::new(()));
        gate.core
            .begin(pending("skipped".into()), diagnostics(), false);
        assert!(gate.core.take_query().is_none());
        assert!(gate.skip_pending());
        let (pending, decision) = gate.core.take_decided().unwrap();
        assert!(matches!(decision, GateDecision::Skip));

        // A Shared caller may queue this query after the actor's query drain,
        // but before the actor clears the skipped boundary.
        let drops = Arc::new(AtomicUsize::new(0));
        let capture = Capture {
            gate: gate.clone(),
            drops: Arc::clone(&drops),
        };
        let mut query = Box::pin(
            gate.assert_state(move |()| {
                drop(capture);
                panic!("retirement must discard, not execute, the queued query");
            })
            .finish(),
        );
        let mut context = std::task::Context::from_waker(std::task::Waker::noop());
        assert!(query.as_mut().poll(&mut context).is_pending());
        assert_eq!(drops.load(Ordering::SeqCst), 0);

        gate.core.clear();
        pending.cancel();

        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert!(matches!(
            query.as_mut().poll(&mut context),
            std::task::Poll::Ready(Err(_))
        ));
        assert!(gate.core.retirement_finished());
        assert!(gate.core.take_query().is_none());
        assert!(!gate.has_pending());
        assert!(gate.boundary().is_none());
        drop(binding);
        drop(gate.attach().unwrap());
    }
}
}
