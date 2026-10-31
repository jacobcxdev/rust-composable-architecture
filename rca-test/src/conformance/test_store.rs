//! Checks exhaustive TestStore assertions and deterministic drive limits.

use std::future::pending;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::Poll;
use std::time::Duration;

use rca_core::{Effect, MonotonicInstant, ReduceContext, Reducer};

use crate::{DeterministicRuntime, TestStore};

/// Records the source of each action in the assertion-gate scenario.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Action {
    /// Starts an immediate response followed by a virtual timer.
    Start,
    /// Pauses at the effect assertion gate before the timer can start.
    Response,
    /// Records completion of the virtual timer.
    Timed,
    /// Attempts an external send while an effect still requires acknowledgement.
    External,
}

/// Records actions and emits the response-then-timer sequence.
struct Feature;

impl Reducer for Feature {
    type State = Vec<Action>;
    type Action = Action;

    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &Self::Action,
        _context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        state.push(*action);
        match action {
            Action::Start => Effect::concatenate([
                Effect::send(Action::Response),
                Effect::send_after(Duration::from_millis(5), Action::Timed),
            ]),
            Action::Response | Action::Timed | Action::External => Effect::none(),
        }
    }
}

/// Runs TestStore-applicable state, effect, time, and assertion-gate contracts.
///
/// Each scenario owns its deterministic runtime and uses explicit drive budgets.
/// Expected teardown panics require panic unwinding. An incomplete assertion stays
/// terminal. Teardown does not reset it.
///
/// # Panics
///
/// Panics when `TestStore` violates a conformance contract.
pub async fn run_test_store() {
    assertion_gate_and_virtual_time().await;
    source_ordering().await;
    cancellation_diagnostics().await;
    deterministic_budget_success().await;
    deterministic_budget_failure().await;
    deterministic_overdue_budget_failure();
}

/// Checks that state changes occur only after each explicit effect acknowledgement.
async fn assertion_gate_and_virtual_time() {
    let mut store = TestStore::new(Vec::new(), Feature);

    let _start = store
        .send_with(Action::Start, |state| state.push(Action::Start))
        .await
        .expect("start commit");
    let pending = store
        .send(Action::External)
        .await
        .expect_err("receive must precede external send");
    assert!(pending.is_unreceived_action());
    store
        .assert_state(|state| assert_eq!(state, &[Action::Start]))
        .await
        .expect("state while gate paused");
    let _response = store
        .receive_with(
            |action| *action == Action::Response,
            |state| state.push(Action::Response),
        )
        .await
        .expect("response commit");
    store
        .advance(Duration::from_millis(5))
        .await
        .expect("advance virtual timer");
    let _timed = store
        .receive_with(
            |action| *action == Action::Timed,
            |state| state.push(Action::Timed),
        )
        .await
        .expect("timer commit");
    store
        .assert_state_eq(vec![Action::Start, Action::Response, Action::Timed])
        .await
        .expect("final state");
    store.finish().await.expect("exhaustive finish");
}

/// Selects opposite source orders for otherwise equivalent cancellation programs.
#[derive(Clone, Copy)]
enum SourceAction {
    /// Registers work before cancellation, so its output must never commit.
    StartThenCancel,
    /// Cancels first, so subsequently registered work remains live.
    CancelThenStart,
    /// Records output from the surviving effect.
    Output(u8),
}

/// Makes cancellation order visible through distinct output values.
struct SourceOrder;

impl Reducer for SourceOrder {
    type State = Vec<u8>;
    type Action = SourceAction;

    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &Self::Action,
        _context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        match action {
            SourceAction::StartThenCancel => Effect::merge([
                Effect::send(SourceAction::Output(1)).cancellable(1_u8),
                Effect::cancel(1_u8),
            ]),
            SourceAction::CancelThenStart => Effect::merge([
                Effect::cancel(2_u8),
                Effect::send(SourceAction::Output(2)).cancellable(2_u8),
            ]),
            SourceAction::Output(value) => {
                state.push(*value);
                Effect::none()
            }
        }
    }
}

/// Checks source-ordered cancellation and completion of both action lifetimes.
async fn source_ordering() {
    let mut store = TestStore::new(Vec::new(), SourceOrder);
    let start_then_cancel = store
        .send(SourceAction::StartThenCancel)
        .await
        .expect("start-before-cancel");
    let cancel_then_start = store
        .send(SourceAction::CancelThenStart)
        .await
        .expect("cancel-before-start");
    let output = store
        .receive_with(
            |action| matches!(action, SourceAction::Output(2)),
            |state| state.push(2),
        )
        .await
        .expect("live ordered output");
    start_then_cancel
        .finish()
        .await
        .expect("cancelled branch finished");
    cancel_then_start
        .finish()
        .await
        .expect("live branch finished");
    output.finish().await.expect("output finished");
    store
        .assert_state(|state| assert_eq!(state, &[2]))
        .await
        .expect("source-ordered state");
    store.finish().await.expect("source-order finish");
}

/// Starts the unfinished effect used to inspect cancellation diagnostics.
#[derive(Clone, Copy)]
enum PendingAction {
    /// Registers pending work under a typed cancellation identity.
    Start,
}

/// Produces work that can finish only through cancellation or teardown.
struct PendingFeature;

/// Distinguishes the pending effect's cancellation type in diagnostics.
#[derive(Eq, Hash, PartialEq)]
struct PendingCancellation;

impl Reducer for PendingFeature {
    type State = ();
    type Action = PendingAction;

    fn reduce(
        &mut self,
        _state: &mut Self::State,
        _action: &Self::Action,
        _context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        Effect::run(|_sender| pending()).cancellable(PendingCancellation)
    }
}

/// Checks unfinished-work diagnostics, then explicitly cancels and finishes that work.
async fn cancellation_diagnostics() {
    let mut store = TestStore::new((), PendingFeature);
    let task = store
        .send(PendingAction::Start)
        .await
        .expect("pending effect");
    let error = store.finish().await.expect_err("pending cancellation");
    let diagnostics = error.finish_diagnostics().expect("finish diagnostics");
    assert_eq!(diagnostics.cancellation_identities().len(), 1);
    assert_eq!(
        diagnostics.cancellation_identities()[0].type_name(),
        std::any::type_name::<PendingCancellation>()
    );

    task.cancel().await.expect("causal cancellation");
    assert!(task.is_cancelled());
    store.finish().await.expect("cancellation finish");
}

/// Requires eight effect polls, allowing budgets on both sides of completion.
struct SelfWakeFeature;

impl Reducer for SelfWakeFeature {
    type State = ();
    type Action = ();

    fn reduce(
        &mut self,
        _state: &mut Self::State,
        _action: &Self::Action,
        _context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        Effect::run(|_sender| async move {
            let mut polls = 0;
            std::future::poll_fn(move |context| {
                polls += 1;
                if polls == 8 {
                    Poll::Ready(())
                } else {
                    context.waker().wake_by_ref();
                    Poll::Pending
                }
            })
            .await;
        })
    }
}

/// Permits enough polls for finite self-waking work and all actor acknowledgements.
async fn deterministic_budget_success() {
    let mut store = TestStore::with_step_budget((), SelfWakeFeature, 32);
    let task = store
        .send(())
        .await
        .expect("finite self-waking effect commits");
    task.finish()
        .await
        .expect("finite self-waking effect finishes");
    store
        .finish()
        .await
        .expect("finite work leaves no unfinished effects");
}

/// Checks exact budget diagnostics and the terminal incomplete-assertion contract.
async fn deterministic_budget_failure() {
    let mut store = TestStore::with_step_budget((), SelfWakeFeature, 7);
    let error = store.send(()).await.expect_err("step budget exhaustion");
    let drive = error.drive_error().expect("drive diagnostics");
    assert_eq!(drive.polls(), 7);
    assert_eq!(drive.step_budget(), 7);
    let error = store
        .finish()
        .await
        .expect_err("admitted assertion remains incomplete");
    assert!(error.is_incomplete_assertion());
    assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(store))).is_err());
}

/// Checks partial virtual-time progress and retry after a bounded drive failure.
fn deterministic_overdue_budget_failure() {
    /// Leaves one of eight successive deadlines pending after the first drive.
    const BUDGET: usize = 7;
    let runtime = DeterministicRuntime::with_step_budget(BUDGET);
    let task_runtime = runtime.clone();
    let completed = Arc::new(AtomicBool::new(false));
    let task_completed = Arc::clone(&completed);
    runtime.spawn_task(Box::pin(async move {
        for millis in 1..=(BUDGET + 1) {
            task_runtime
                .sleep_until(MonotonicInstant::from_elapsed(Duration::from_millis(
                    millis as u64,
                )))
                .await;
        }
        task_completed.store(true, Ordering::Release);
    }));

    let error = runtime
        .advance(Duration::from_millis((BUDGET + 1) as u64))
        .expect_err("overdue work must exhaust step budget");

    assert_eq!(error.polls(), BUDGET);
    assert_eq!(error.step_budget(), BUDGET);
    assert_eq!(error.ready_task_ids(), [0]);
    assert_eq!(error.pending_sleeps(), [] as [crate::PendingSleep; 0]);
    assert_eq!(
        error.current_time(),
        MonotonicInstant::from_elapsed(Duration::from_millis(BUDGET as u64))
    );
    assert!(!completed.load(Ordering::Acquire));

    runtime
        .advance(Duration::from_millis(1))
        .expect("remaining overdue work completes on the next drive");
    assert!(completed.load(Ordering::Acquire));
    assert_eq!(runtime.pending_sleeps(), [] as [crate::PendingSleep; 0]);
    assert_eq!(
        runtime
            .run_until_stalled()
            .expect("completed runtime is idle")
            .polls(),
        0
    );
}
