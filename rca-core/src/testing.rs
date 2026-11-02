//! Exposes sealed hooks used by RCA deterministic testing.
//!
//! These controls are available only with `test-util`. They pause production
//! actors at assertion boundaries without replacing reduction or effect handling.
//! [`AssertionGate`] holds effect-produced actions. [`TransactionGate`] also
//! holds structural, update and output boundaries for typed features.
//! Prefer the `rca-test` crate for application tests.

use std::any::Any;
use std::collections::VecDeque;
use std::fmt;
use std::sync::{Arc, Mutex};

use event_listener::{Event, EventListener};

#[doc(hidden)]
pub use crate::issue::IssueRecorder;
use crate::store::command::ReadyActionCommand;
use crate::{ActionTask, MonotonicInstant, StoreStopped};

mod transaction_gate;
pub(crate) use transaction_gate::Pending as PendingTransaction;
#[cfg(feature = "local")]
#[doc(hidden)]
pub use transaction_gate::local;
pub use transaction_gate::{TransactionBoundary, TransactionGate, TransactionKind};
pub(crate) use transaction_gate::{TransactionGateBinding, TransactionGateCore};

/// Owns a one-shot assertion whose state type is checked at actor execution.
type ErasedStateAssertion = Box<dyn FnOnce(&dyn Any) + Send + 'static>;

/// Identifies one actor-owned cancellation entry without retaining its value.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CancellationIdentitySnapshot {
    /// Store-local identifier, stable while the cancellation entry remains live.
    pub diagnostic_id: u64,
    /// Structural reducer scope identifier.
    pub scope_id: u64,
    /// Concrete Rust type name of the cancellation value.
    pub type_name: &'static str,
}

/// Summarises actor-owned work without exposing domain payloads.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeSnapshot {
    /// Latest committed action version.
    pub commit_version: u64,
    /// Live effect task identifiers in ascending order.
    pub live_task_ids: Vec<u64>,
    /// Number of active cancellation identities.
    pub cancellation_count: usize,
    /// Actor timer deadlines in stable order.
    pub timer_deadlines: Vec<MonotonicInstant>,
    /// Number of active observation registrations.
    pub observation_count: usize,
}

/// Captures public runtime counters and opaque cancellation diagnostics atomically.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ActorDiagnostics {
    /// Counters captured at the same actor boundary as the identity list.
    runtime: RuntimeSnapshot,
    /// Inert identity metadata, never cancellation values or domain payloads.
    cancellation_identities: Vec<CancellationIdentitySnapshot>,
    /// Scoped handles registered when the actor captured these diagnostics.
    handle_registration_count: usize,
}

impl ActorDiagnostics {
    /// Combines diagnostics captured by one actor turn without further inspection.
    pub(crate) const fn new(
        runtime: RuntimeSnapshot,
        cancellation_identities: Vec<CancellationIdentitySnapshot>,
        handle_registration_count: usize,
    ) -> Self {
        Self {
            runtime,
            cancellation_identities,
            handle_registration_count,
        }
    }

    /// Returns stable actor-owned counters.
    #[must_use]
    pub const fn runtime(&self) -> &RuntimeSnapshot {
        &self.runtime
    }

    /// Consumes these diagnostics and returns their runtime counters.
    ///
    /// Moves the runtime vectors without cloning them. The cancellation identity
    /// list and handle-registration count are discarded.
    #[must_use]
    pub fn into_runtime(self) -> RuntimeSnapshot {
        self.runtime
    }

    /// Returns active cancellation identities in store-local diagnostic order.
    #[must_use]
    pub fn cancellation_identities(&self) -> &[CancellationIdentitySnapshot] {
        &self.cancellation_identities
    }

    /// Returns the number of actor-owned scoped-handle registrations.
    #[must_use]
    pub const fn handle_registration_count(&self) -> usize {
        self.handle_registration_count
    }
}

/// Pauses one effect-produced action before production commit.
///
/// Clones control the same gate. A gate can belong to only one live Store actor;
/// after that actor is dropped, it can be installed in another Store.
pub struct AssertionGate<Action> {
    /// Shared control state, independent of the actor's exclusive binding.
    shared: Arc<GateShared<ReadyActionCommand<Action>, StateAssertionCommand>>,
}

impl<Action> Clone for AssertionGate<Action> {
    fn clone(&self) -> Self {
        Self {
            shared: Arc::clone(&self.shared),
        }
    }
}

impl<Action> AssertionGate<Action> {
    /// Creates an empty assertion gate.
    #[must_use]
    pub fn new() -> Self {
        Self {
            shared: Arc::new(GateShared {
                state: Mutex::new(GateState {
                    attachment: GateAttachment::Fresh,
                    pending: None,
                    inspection: None,
                    decision: None,
                    assertion_reply: None,
                    pending_snapshot: None,
                    latest_snapshot: None,
                    state_assertions: VecDeque::new(),
                    metadata: (),
                }),
                changed: Event::new(),
            }),
        }
    }

    /// Claims the gate for one actor and drops retired captures after unlocking.
    ///
    /// The new binding exists before retired captures drop. If a destructor
    /// panics, unwinding releases that claim instead of leaving an attached gate.
    pub(crate) fn attach(&self) -> Result<AssertionGateBinding<Action>, crate::StoreBuildError> {
        let retired = {
            let mut state = self
                .shared
                .state
                .lock()
                .expect("assertion gate lock must not be poisoned");
            if state.attachment == GateAttachment::Attached {
                drop(state);
                return Err(crate::StoreBuildError::assertion_gate_attached());
            }
            let retired = (state.attachment == GateAttachment::Detached).then(|| {
                state.decision = None;
                state.inspection = None;
                state.pending_snapshot = None;
                state.latest_snapshot = None;
                (
                    state.pending.take(),
                    state.assertion_reply.take(),
                    std::mem::take(&mut state.state_assertions),
                )
            });
            state.attachment = GateAttachment::Attached;
            retired
        };
        let binding = AssertionGateBinding { gate: self.clone() };
        // Retired callbacks and actions may re-enter the gate or panic. The new
        // binding is already owned, and no user value is dropped under its lock.
        drop(retired);
        Ok(binding)
    }

    /// Returns whether an effect action awaits a decision.
    #[must_use]
    pub fn has_pending(&self) -> bool {
        let state = self
            .shared
            .state
            .lock()
            .expect("assertion gate lock must not be poisoned");
        state.pending.is_some() || state.inspection.is_some()
    }

    /// Waits until an effect-produced action reaches the assertion gate.
    ///
    /// This does not drive the actor or finish when its binding detaches.
    #[doc(hidden)]
    pub async fn wait_for_pending(&self) {
        while !self.has_pending() {
            event_listener::listener!(self.shared.changed => listener);
            if self.has_pending() {
                break;
            }
            listener.await;
        }
    }

    /// Returns diagnostics captured immediately before pending action commit.
    #[doc(hidden)]
    #[must_use]
    pub fn pending_snapshot(&self) -> Option<RuntimeSnapshot> {
        self.shared
            .state
            .lock()
            .expect("assertion gate lock must not be poisoned")
            .pending_snapshot
            .as_ref()
            .map(|diagnostics| diagnostics.runtime().clone())
    }

    /// Returns diagnostics captured immediately before pending action commit.
    #[doc(hidden)]
    #[must_use]
    pub fn pending_diagnostics(&self) -> Option<ActorDiagnostics> {
        self.shared
            .state
            .lock()
            .expect("assertion gate lock must not be poisoned")
            .pending_snapshot
            .clone()
    }

    /// Returns latest stable actor-owned diagnostics.
    #[doc(hidden)]
    #[must_use]
    pub fn latest_snapshot(&self) -> Option<RuntimeSnapshot> {
        self.shared
            .state
            .lock()
            .expect("assertion gate lock must not be poisoned")
            .latest_snapshot
            .as_ref()
            .map(|diagnostics| diagnostics.runtime().clone())
    }

    /// Returns latest stable actor-owned diagnostics.
    #[doc(hidden)]
    #[must_use]
    pub fn latest_diagnostics(&self) -> Option<ActorDiagnostics> {
        self.shared
            .state
            .lock()
            .expect("assertion gate lock must not be poisoned")
            .latest_snapshot
            .clone()
    }

    /// Borrows the pending action outside the gate lock without cloning it.
    ///
    /// The exact command is restored even if the callback panics. While it is
    /// borrowed, nested or concurrent inspection returns `None`; decisions remain
    /// queued until inspection finishes. Gate queries may be called from the callback.
    ///
    /// # Panics
    /// Propagates callback panics after restoring the pending command unless cleanup removed it.
    pub fn inspect_pending<Output>(
        &self,
        inspect: impl FnOnce(&Action) -> Output,
    ) -> Option<Output> {
        let inspection = {
            let mut state = self
                .shared
                .state
                .lock()
                .expect("assertion gate lock must not be poisoned");
            let command = state.pending.take()?;
            let token = Arc::new(());
            state.inspection = Some(Arc::clone(&token));
            PendingInspection {
                shared: &self.shared,
                token,
                command: Some(command),
            }
        };
        Some(inspect(
            &inspection
                .command
                .as_ref()
                .expect("inspection owns its command")
                .action,
        ))
    }

    /// Releases pending action into production commit path.
    #[must_use]
    pub fn release_pending(&self) -> bool {
        self.decide(GateDecision::Release, None)
    }

    /// Releases pending action and returns its production commit receipt.
    #[doc(hidden)]
    #[must_use]
    pub fn release_pending_with_receipt(&self) -> Option<AssertionReceipt> {
        let (reply, receive) = async_channel::bounded(1);
        self.decide(GateDecision::Release, Some(reply))
            .then_some(AssertionReceipt { receive })
    }

    /// Discards pending action without reduction.
    #[must_use]
    pub fn skip_pending(&self) -> bool {
        self.decide(GateDecision::Skip, None)
    }

    /// Records at most one decision, including during out-of-lock inspection.
    fn decide(
        &self,
        decision: GateDecision,
        assertion_reply: Option<async_channel::Sender<Result<ActionTask, StoreStopped>>>,
    ) -> bool {
        let changed = {
            let mut state = self
                .shared
                .state
                .lock()
                .expect("assertion gate lock must not be poisoned");
            if (state.pending.is_none() && state.inspection.is_none()) || state.decision.is_some() {
                false
            } else {
                state.decision = Some(decision);
                state.assertion_reply = assertion_reply;
                true
            }
        };
        if changed {
            self.shared.changed.notify(usize::MAX);
        }
        changed
    }

    /// Queues clone-free state inspection while an effect action is paused.
    ///
    /// This legacy gate queues immediately. The typed [`TransactionGate`] instead
    /// bounds admission to one diagnostic query. Dropping the receipt prevents
    /// execution of an assertion that the actor has not started.
    ///
    /// # Panics
    ///
    /// Actor execution panics if `State` differs from its state type or the
    /// assertion panics. Enqueueing does not execute the assertion.
    #[doc(hidden)]
    #[must_use]
    pub fn assert_state<State>(
        &self,
        assertion: impl FnOnce(&State) + Send + 'static,
    ) -> AssertionStateReceipt
    where
        State: Send + 'static,
    {
        let (reply, receive) = async_channel::bounded(1);
        let command = StateAssertionCommand {
            assertion: Box::new(move |state| {
                let state = state
                    .downcast_ref::<State>()
                    .expect("assertion state type must match Store state");
                assertion(state);
            }),
            reply,
        };
        self.shared
            .state
            .lock()
            .expect("assertion gate lock must not be poisoned")
            .state_assertions
            .push_back(command);
        self.shared.changed.notify(usize::MAX);
        AssertionStateReceipt {
            receive,
            admission: std::future::ready(()),
        }
    }

    /// Publishes the actor's next gated command after the previous command leaves.
    pub(crate) fn begin(&self, command: ReadyActionCommand<Action>, snapshot: ActorDiagnostics) {
        let mut state = self
            .shared
            .state
            .lock()
            .expect("assertion gate lock must not be poisoned");
        state.pending = Some(command);
        state.latest_snapshot = Some(snapshot.clone());
        state.pending_snapshot = Some(snapshot);
        drop(state);
        self.shared.changed.notify(usize::MAX);
    }

    /// Replaces inert diagnostics without changing a pending command or decision.
    pub(crate) fn update_snapshot(&self, snapshot: ActorDiagnostics) {
        self.shared
            .state
            .lock()
            .expect("assertion gate lock must not be poisoned")
            .latest_snapshot = Some(snapshot);
    }

    /// Transfers a decided command to the actor after its inspection ends.
    pub(crate) fn take_decided(&self) -> Option<GateOutcome<Action>> {
        let mut state = self
            .shared
            .state
            .lock()
            .expect("assertion gate lock must not be poisoned");
        if state.inspection.is_some() {
            return None;
        }
        let decision = state.decision.take()?;
        let mut command = state
            .pending
            .take()
            .expect("gate decision requires pending action");
        state.pending_snapshot = None;
        if decision == GateDecision::Release {
            command.assertion_reply = state.assertion_reply.take();
        } else {
            debug_assert!(
                state.assertion_reply.is_none(),
                "skip has no commit receipt"
            );
        }
        Some(GateOutcome { command, decision })
    }

    /// Removes the oldest query for execution outside the gate lock.
    pub(crate) fn take_state_assertion(&self) -> Option<StateAssertionCommand> {
        self.shared
            .state
            .lock()
            .expect("assertion gate lock must not be poisoned")
            .state_assertions
            .pop_front()
    }

    /// Reports queued queries without consuming them.
    pub(crate) fn has_state_assertions(&self) -> bool {
        !self
            .shared
            .state
            .lock()
            .expect("assertion gate lock must not be poisoned")
            .state_assertions
            .is_empty()
    }

    /// Defers liveness checks while inspection owns the command.
    pub(crate) fn pending_is_live(&self) -> bool {
        let state = self
            .shared
            .state
            .lock()
            .expect("assertion gate lock must not be poisoned");
        // The actor cannot access the borrowed command; recheck provenance after restoration.
        state.inspection.is_some()
            || state
                .pending
                .as_ref()
                .is_some_and(|command| command.provenance.is_live())
    }

    /// Reports an actionable decision or cancellation after inspection restores ownership.
    pub(crate) fn is_decided(&self) -> bool {
        let state = self
            .shared
            .state
            .lock()
            .expect("assertion gate lock must not be poisoned");
        // Also report cancellation readiness so restoring an invalidated command
        // between the actor's liveness check and listener registration cannot lose a wake.
        state.inspection.is_none()
            && (state.decision.is_some()
                || state
                    .pending
                    .as_ref()
                    .is_none_or(|command| !command.provenance.is_live()))
    }

    /// Removes pending ownership and drops query captures outside the lock.
    ///
    /// Invalidating the inspection token prevents a borrowed command from
    /// returning to the slot. Its guard then reports cancellation on return.
    pub(crate) fn force_skip(&self) -> Option<ReadyActionCommand<Action>> {
        let mut state = self
            .shared
            .state
            .lock()
            .expect("assertion gate lock must not be poisoned");
        state.decision = None;
        let assertion_reply = state.assertion_reply.take();
        state.pending_snapshot = None;
        state.inspection = None;
        let assertions = std::mem::take(&mut state.state_assertions);
        let pending = state.pending.take();
        drop(state);
        drop(assertion_reply);
        drop(assertions);
        pending
    }

    /// Registers before the actor rechecks readiness to avoid a lost wake.
    pub(crate) fn listen(&self) -> EventListener {
        self.shared.changed.listen()
    }
}

impl<Action> Default for AssertionGate<Action> {
    fn default() -> Self {
        Self::new()
    }
}

impl<Action> fmt::Debug for AssertionGate<Action> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AssertionGate")
            .field("pending", &self.has_pending())
            .finish_non_exhaustive()
    }
}

/// Shares gate bookkeeping while keeping notifications outside the state lock.
struct GateShared<Pending, Query, Metadata = (), Inspection = Option<Arc<()>>> {
    /// Serialises ownership transfers, never user callbacks or payload destruction.
    state: Mutex<GateState<Pending, Query, Metadata, Inspection>>,
    /// Wakes actor and diagnostic waiters after a relevant state transition.
    changed: Event,
}

/// Stores common gate slots with carrier-specific frontier and inspection metadata.
struct GateState<Pending, Query, Metadata, Inspection = Option<Arc<()>>> {
    /// Tracks the actor's exclusive binding separately from cloned control handles.
    attachment: GateAttachment,
    /// Owns the command except while an inspection guard holds it.
    pending: Option<Pending>,
    /// Marks out-of-lock inspection, with generation tokens for the legacy gate.
    inspection: Inspection,
    /// Defers an accepted release or skip until command ownership returns.
    decision: Option<GateDecision>,
    /// Optional commit acknowledgement, transferred only on release.
    assertion_reply: Option<async_channel::Sender<Result<ActionTask, StoreStopped>>>,
    /// Diagnostics from immediately before the pending mutation.
    pending_snapshot: Option<ActorDiagnostics>,
    /// Most recent diagnostics published by the actor.
    latest_snapshot: Option<ActorDiagnostics>,
    /// Queued state work, capacity-limited by the typed gate's admission policy.
    state_assertions: VecDeque<Query>,
    /// Additional typed-frontier debt and retirement state, or `()` for legacy gates.
    metadata: Metadata,
}

/// Separates an unused gate from an actor-owned or previously released gate.
#[derive(Eq, PartialEq)]
enum GateAttachment {
    /// No actor has claimed this gate.
    Fresh,
    /// One live actor binding owns the gate.
    Attached,
    /// The previous binding dropped, but retired values can remain until reattachment.
    Detached,
}

/// Retains one actor's exclusive gate claim through its complete destruction.
pub(crate) struct AssertionGateBinding<Action> {
    /// Keeps the shared gate alive until this actor releases its claim.
    gate: AssertionGate<Action>,
}

impl<Action> AssertionGateBinding<Action> {
    /// Borrows controls without creating a second actor claim.
    pub(crate) fn gate(&self) -> &AssertionGate<Action> {
        &self.gate
    }
}

impl<Action> Drop for AssertionGateBinding<Action> {
    fn drop(&mut self) {
        let mut state = self
            .gate
            .shared
            .state
            .lock()
            .expect("assertion gate lock must not be poisoned");
        state.attachment = GateAttachment::Detached;
        state.latest_snapshot = None;
        state.pending_snapshot = None;
    }
}

/// Owns a command during user code and restores it on return or unwind.
///
/// A token prevents abortive cleanup from resurrecting a removed command or
/// replacing a newer one when the callback returns.
struct PendingInspection<'a, Action> {
    /// Gate that lent the command, borrowed for the callback's duration.
    shared: &'a GateShared<ReadyActionCommand<Action>, StateAssertionCommand>,
    /// Unique identity retained until this inspection finishes.
    token: Arc<()>,
    /// Command temporarily removed from the gate, then restored or cancelled.
    command: Option<ReadyActionCommand<Action>>,
}

impl<Action> Drop for PendingInspection<'_, Action> {
    fn drop(&mut self) {
        let mut command = self.command.take();
        {
            let mut state = self
                .shared
                .state
                .lock()
                .expect("assertion gate lock must not be poisoned");
            if state
                .inspection
                .as_ref()
                .is_some_and(|token| Arc::ptr_eq(token, &self.token))
            {
                state.pending = command.take();
                state.inspection = None;
            }
        }
        if let Some(command) = command {
            let _ = command
                .reply
                .send(Err(crate::store::command::CommitError::Cancelled));
        }
        self.shared.changed.notify(usize::MAX);
    }
}

/// Selects whether the actor reduces or discards a gated mutation.
#[derive(Clone, Copy, Eq, PartialEq)]
pub(crate) enum GateDecision {
    /// Continue through the production commit path.
    Release,
    /// Discard the command before reduction.
    Skip,
}

/// Transfers a command and its decision together to the actor.
pub(crate) struct GateOutcome<Action> {
    /// Exact owned command that the gate previously held.
    pub(crate) command: ReadyActionCommand<Action>,
    /// Accepted decision for this command.
    pub(crate) decision: GateDecision,
}

/// Waits for exact gated action to complete production commit.
#[doc(hidden)]
pub struct AssertionReceipt {
    /// One production commit result, or channel closure if the actor stops.
    receive: async_channel::Receiver<Result<ActionTask, StoreStopped>>,
}

impl AssertionReceipt {
    /// Waits for production commit.
    ///
    /// # Errors
    ///
    /// Returns [`StoreStopped`] if the actor stops before returning a commit task.
    pub async fn finish(self) -> Result<ActionTask, StoreStopped> {
        self.receive
            .recv()
            .await
            .unwrap_or_else(|_closed| Err(StoreStopped::new()))
    }
}

impl fmt::Debug for AssertionReceipt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AssertionReceipt").finish_non_exhaustive()
    }
}

/// Waits for clone-free gated state inspection.
#[doc(hidden)]
pub struct AssertionStateReceipt<Admission = std::future::Ready<()>> {
    /// Completes only after the actor runs the assertion successfully.
    receive: async_channel::Receiver<()>,
    /// Owns deferred query admission, or an already-ready legacy admission.
    admission: Admission,
}

impl<Admission: Future<Output = ()>> AssertionStateReceipt<Admission> {
    /// Waits for state inspection to run.
    ///
    /// # Errors
    ///
    /// Returns [`StoreStopped`] if admission fails or the assertion reply closes.
    pub async fn finish(self) -> Result<(), StoreStopped> {
        self.admission.await;
        self.receive
            .recv()
            .await
            .map_err(|_closed| StoreStopped::new())
    }
}

impl<Admission> fmt::Debug for AssertionStateReceipt<Admission> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AssertionStateReceipt")
            .finish_non_exhaustive()
    }
}

/// Owns a legacy state assertion and its completion acknowledgement.
pub(crate) struct StateAssertionCommand {
    /// Erased callback, invoked at most once after the receiver-liveness check.
    assertion: ErasedStateAssertion,
    /// Acknowledges completion without copying state.
    reply: async_channel::Sender<()>,
}

impl StateAssertionCommand {
    /// Runs the assertion only if its receipt remains open.
    pub(crate) fn execute<State: 'static>(self, state: &State) {
        if self.reply.is_closed() {
            return;
        }
        (self.assertion)(state);
        let _ = self.reply.try_send(());
    }
}

#[cfg(test)]
ordinary_test_items! {
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::panic::{AssertUnwindSafe, catch_unwind};

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

    #[test]
    fn matcher_panic_restores_exact_move_only_command() {
        let gate = AssertionGate::new();
        let action = Box::new(Cell::new(7));
        let identity = std::ptr::from_ref(action.as_ref());
        let (reply, mut receive) = crate::store::command::commit_channel();
        gate.begin(
            ReadyActionCommand {
                action,
                provenance: crate::store::command::Provenance::External { handle: None },
                reply,
                assertion_reply: None,
                operation_work: crate::effect::operation::OperationWork::default(),
            },
            diagnostics(),
        );
        assert_eq!(gate.inspect_pending(|_| false), Some(false));
        let panic = catch_unwind(AssertUnwindSafe(|| {
            gate.inspect_pending(|action| {
                assert_eq!(std::ptr::from_ref(action.as_ref()), identity);
                panic!("matcher panic");
            })
        }));
        let panic = panic.unwrap_err();
        assert_eq!(panic.downcast_ref::<&str>(), Some(&"matcher panic"));
        assert!(gate.has_pending());
        assert_eq!(
            gate.inspect_pending(|action| std::ptr::from_ref(action.as_ref())),
            Some(identity)
        );
        assert!(matches!(receive.try_recv(), Ok(None)));
        assert!(gate.release_pending());
        let outcome = gate.take_decided().unwrap();
        assert_eq!(
            std::ptr::from_ref(outcome.command.action.as_ref()),
            identity
        );
        drop(outcome);
        receive
            .try_recv()
            .expect_err("dropping the command closes its commit reply");
    }

    #[test]
    fn matcher_runs_unlocked_and_defers_concurrent_release() {
        let gate = AssertionGate::new();
        let (reply, _receive) = crate::store::command::commit_channel();
        gate.begin(
            ReadyActionCommand {
                action: Cell::new(7),
                provenance: crate::store::command::Provenance::External { handle: None },
                reply,
                assertion_reply: None,
                operation_work: crate::effect::operation::OperationWork::default(),
            },
            diagnostics(),
        );
        assert_eq!(
            gate.inspect_pending(|action| {
                assert!(
                    gate.shared.state.try_lock().is_ok(),
                    "matcher held gate lock"
                );
                assert!(gate.has_pending());
                assert!(gate.pending_diagnostics().is_some());
                assert!(gate.inspect_pending(|_| ()).is_none());
                std::thread::scope(|scope| {
                    scope
                        .spawn(|| {
                            assert!(gate.release_pending());
                            assert!(gate.take_decided().is_none());
                            assert!(!gate.is_decided());
                        })
                        .join()
                        .unwrap();
                });
                action.get()
            }),
            Some(7)
        );
        assert!(gate.is_decided());
        assert_eq!(gate.take_decided().unwrap().command.action.get(), 7);
    }

    #[test]
    fn force_skip_during_inspection_does_not_resurrect_old_command() {
        let gate = AssertionGate::new();
        let (reply, mut receive) = crate::store::command::commit_channel();
        gate.begin(
            ReadyActionCommand {
                action: Cell::new(7),
                provenance: crate::store::command::Provenance::External { handle: None },
                reply,
                assertion_reply: None,
                operation_work: crate::effect::operation::OperationWork::default(),
            },
            diagnostics(),
        );
        gate.inspect_pending(|_| {
            assert!(
                gate.shared.state.try_lock().is_ok(),
                "matcher held gate lock"
            );
            assert!(gate.force_skip().is_none());
            assert!(!gate.has_pending());
            let (reply, _receive) = crate::store::command::commit_channel();
            gate.begin(
                ReadyActionCommand {
                    action: Cell::new(9),
                    provenance: crate::store::command::Provenance::External { handle: None },
                    reply,
                    assertion_reply: None,
                    operation_work: crate::effect::operation::OperationWork::default(),
                },
                diagnostics(),
            );
        })
        .unwrap();
        assert!(matches!(
            receive.try_recv(),
            Ok(Some(Err(crate::store::command::CommitError::Cancelled)))
        ));
        assert_eq!(gate.inspect_pending(Cell::get), Some(9));
    }

    #[test]
    fn cancellation_during_inspection_remains_ready_after_restoration() {
        use crate::effect::lineage::{ControlWake, LineageState};
        use crate::effect::registry::TaskRegistry;
        use crate::store::scope::ScopeRegistry;

        let control = ControlWake::new();
        let lineage = LineageState::new(Arc::downgrade(&control));
        let mut registry = TaskRegistry::new(control);
        let scope = ScopeRegistry::for_test().1.root();
        let prepared = registry.prepare(scope, Arc::clone(&lineage), &[]);
        let gate = AssertionGate::new();
        let (reply, _receive) = crate::store::command::commit_channel();
        gate.begin(
            ReadyActionCommand {
                action: Cell::new(7),
                provenance: crate::store::command::Provenance::Effect {
                    scope,
                    lineage: Arc::clone(&lineage),
                    task: prepared.sender_token(),
                    generations: Box::default(),
                    operations: crate::effect::operation::OperationAncestry::default(),
                },
                reply,
                assertion_reply: None,
                operation_work: crate::effect::operation::OperationWork::default(),
            },
            diagnostics(),
        );
        assert!(gate.pending_is_live());
        gate.inspect_pending(|_| {
            lineage.cancel();
            assert!(gate.pending_is_live());
            assert!(!gate.is_decided());
        })
        .unwrap();
        assert!(!gate.pending_is_live());
        // The actor checks readiness after registering its listener, closing the
        // race where restoration notified just before listener registration.
        assert!(gate.is_decided());
        assert!(gate.force_skip().is_some());
    }
}
}
