//! Checks asserted commits, bounded driving and causal cleanup through public APIs.

use rca_core::testing::AssertionGate;
use rca_core::{Effect, ReduceContext, Reducer, Store};
use rca_test::{DeterministicRuntime, Exhaustivity, TestStore};
use std::task::Poll;
use std::time::Duration;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Action {
    Start,
    Response,
    External,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum State {
    Idle,
    Started,
    Responded,
}

struct Feature;

impl Reducer for Feature {
    type State = State;
    type Action = Action;

    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &Self::Action,
        _context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        match action {
            Action::Start => {
                *state = State::Started;
                Effect::send(Action::Response)
            }
            Action::Response => {
                *state = State::Responded;
                Effect::none()
            }
            Action::External => Effect::none(),
        }
    }
}

#[test]
fn exhaustive_store_requires_receive_before_next_send() {
    let mut store = TestStore::new(State::Idle, Feature);

    futures_executor::block_on(async {
        let _start = store
            .send_with(Action::Start, |state| *state = State::Started)
            .await
            .expect("start commit");
        let error = store
            .send(Action::External)
            .await
            .expect_err("pending receive");
        assert!(error.is_unreceived_action());
        assert!(
            error
                .to_string()
                .starts_with("receive pending effect action before sending external action at ")
        );
        assert!(format!("{error:?}").contains("UnreceivedAction"));

        let _response = store
            .receive_with(
                |action| *action == Action::Response,
                |state| *state = State::Responded,
            )
            .await
            .expect("response commit");
        let _external = store.send(Action::External).await.expect("external commit");
        store.finish().await.expect("exhaustive finish");
        store
            .finish()
            .await
            .expect("repeated finish remains successful");
    });
}

#[test]
fn receive_mismatch_leaves_exact_action_pending() {
    let mut store = TestStore::new(State::Idle, Feature);

    futures_executor::block_on(async {
        let _start = store
            .send_with(Action::Start, |state| *state = State::Started)
            .await
            .expect("start commit");
        let error = store
            .receive(|action| *action == Action::External)
            .await
            .expect_err("mismatched action");
        assert!(error.is_action_mismatch());
        assert!(
            error
                .to_string()
                .starts_with("pending effect action did not match receive at ")
        );
        assert!(format!("{error:?}").contains("ActionMismatch"));
        let _response = store
            .receive_with(
                |action| *action == Action::Response,
                |state| *state = State::Responded,
            )
            .await
            .expect("response remains pending");
        store.finish().await.expect("exhaustive finish");
    });
}

#[test]
fn state_assertion_runs_while_effect_action_is_gated() {
    let mut store = TestStore::new(State::Idle, Feature);

    futures_executor::block_on(async {
        let _start = store
            .send_with(Action::Start, |state| *state = State::Started)
            .await
            .expect("start commit");
        store
            .assert_state(|state| assert_eq!(*state, State::Started))
            .await
            .expect("paused state assertion");
        let _response = store
            .receive_with(
                |action| *action == Action::Response,
                |state| *state = State::Responded,
            )
            .await
            .expect("response commit");
        store
            .assert_state_eq(State::Responded)
            .await
            .expect("committed state assertion");
        store.finish().await.expect("exhaustive finish");
    });
}

#[test]
fn exhaustive_skip_rejection_preserves_pending_action_and_state() {
    // Rejecting a skip must not silently consume or reduce the pending response.
    let mut store = TestStore::new(State::Idle, Feature);

    futures_executor::block_on(async {
        let _start = store
            .send_with(Action::Start, |state| *state = State::Started)
            .await
            .expect("start commit");
        let origin_line = line!() + 1;
        let skip = store.skip_received();
        let error = skip.await.expect_err("exhaustive mode rejects skipping");
        assert!(error.is_skip_requires_non_exhaustive());
        assert!(format!("{error:?}").contains("SkipRequiresNonExhaustive"));
        assert_eq!(error.origin().file(), file!());
        assert_eq!(error.origin().line(), origin_line);
        assert!(
            error
                .to_string()
                .starts_with("effect actions may be skipped only in non-exhaustive mode at ")
        );
        assert!(std::error::Error::source(&error).is_none());
        store
            .assert_state_eq(State::Started)
            .await
            .expect("rejected skip did not reduce the response");
        let _response = store
            .receive_with(
                |action| *action == Action::Response,
                |state| *state = State::Responded,
            )
            .await
            .expect("original response remains pending");
        store.finish().await.expect("exhaustive finish");
    });
}

#[test]
fn non_exhaustive_store_can_explicitly_skip_received_action() {
    let mut store = TestStore::with_exhaustivity(State::Idle, Feature, Exhaustivity::NonExhaustive);

    futures_executor::block_on(async {
        let _start = store
            .send_with(Action::Start, |state| *state = State::Started)
            .await
            .expect("start commit");
        let error = store
            .send(Action::External)
            .await
            .expect_err("explicit skip required");
        assert!(error.is_unreceived_action());
        store.skip_received().await.expect("explicit skip");
        store
            .assert_state_eq(State::Started)
            .await
            .expect("skipped action did not reduce");
        store.finish().await.expect("non-exhaustive finish");
    });
}

#[test]
fn test_store_public_futures_are_send() {
    fn assert_send<T: Send>(_: T) {}

    let mut store = TestStore::new(State::Idle, Feature);
    assert_send(store.send(Action::Start));

    let mut store = TestStore::new(State::Idle, Feature);
    assert_send(store.receive(|action| *action == Action::Response));

    let mut store = TestStore::with_exhaustivity(State::Idle, Feature, Exhaustivity::NonExhaustive);
    assert_send(store.skip_received());

    let mut store = TestStore::new(State::Idle, Feature);
    assert_send(store.advance(Duration::from_secs(1)));

    let store = TestStore::new(State::Idle, Feature);
    assert_send(store.assert_state(|_state| {}));

    let mut store = TestStore::new(State::Idle, Feature);
    assert_send(store.finish());
}

#[test]
fn finish_reports_pending_action_and_actor_work_at_caller() {
    let mut store = TestStore::new(State::Idle, Feature);

    futures_executor::block_on(async {
        let _start = store
            .send_with(Action::Start, |state| *state = State::Started)
            .await
            .expect("start commit");
        let finish_line = line!() + 1;
        let error = store.finish().await.expect_err("pending response");
        assert_eq!(error.origin().line(), finish_line);
        let diagnostics = error.finish_diagnostics().expect("finish diagnostics");
        assert!(diagnostics.has_pending_action());
        assert_eq!(diagnostics.runtime().commit_version, 1);
        assert_eq!(diagnostics.runtime().live_task_ids, [0]);
        assert_eq!(diagnostics.runtime().cancellation_count, 0);
        assert!(diagnostics.runtime().timer_deadlines.is_empty());

        let _response = store
            .receive_with(
                |action| *action == Action::Response,
                |state| *state = State::Responded,
            )
            .await
            .expect("response commit");
        store.finish().await.expect("clean finish");
    });
}

#[test]
fn finish_retains_pending_snapshot_after_cancellation_and_retry() {
    let mut store = TestStore::new(State::Idle, Feature);
    let task = finish_without_external_wake(
        store.send_with(Action::Start, |state| *state = State::Started),
    )
    .expect("start commit");

    let error = finish_without_external_wake(store.finish()).expect_err("pending response");
    assert!(!error.is_no_received_action());
    let snapshot = error.finish_diagnostics().expect("unfinished work").clone();
    assert!(snapshot.has_pending_action());
    assert_eq!(snapshot.runtime().commit_version, 1);
    assert_eq!(snapshot.runtime().live_task_ids, [0]);
    assert_eq!(snapshot.runtime().cancellation_count, 0);
    assert!(snapshot.runtime().timer_deadlines.is_empty());
    assert!(snapshot.pending_sleeps().is_empty());
    assert_eq!(snapshot.current_time(), rca_core::MonotonicInstant::ZERO);

    finish_without_external_wake(task.cancel()).expect("cancel the gated response");
    assert!(task.is_cancelled());
    assert!(task.is_finished());
    finish_without_external_wake(store.assert_state_eq(State::Started))
        .expect("cancellation never commits the response");
    assert_eq!(store.now(), rca_core::MonotonicInstant::ZERO);
    assert!(
        finish_without_external_wake(store.finish())
            .unwrap_err()
            .is_incomplete_assertion()
    );
    let boundary = store
        .pending_boundary()
        .expect("cancelled response retains its terminal component");
    finish_without_external_wake(store.receive_terminal(boundary, |error| {
        error.reason() == rca_core::TaskAccessReason::Cancelled
    }))
    .expect("acknowledge cancelled response");
    finish_without_external_wake(store.finish())
        .expect("retry sees acknowledged cancellation cleanup");
    assert_eq!(error.finish_diagnostics(), Some(&snapshot));
}

struct MoveOnlyState(usize);

struct MoveOnlyAction;

struct MoveOnlyFeature;

impl Reducer for MoveOnlyFeature {
    type State = MoveOnlyState;
    type Action = MoveOnlyAction;

    fn reduce(
        &mut self,
        state: &mut Self::State,
        _action: &Self::Action,
        _context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        state.0 += 1;
        Effect::none()
    }
}

#[test]
fn projected_test_store_requires_no_domain_utility_traits() {
    fn count_snapshot(state: &MoveOnlyState) -> usize {
        state.0
    }
    let mut store = TestStore::projected(MoveOnlyState(0), MoveOnlyFeature, count_snapshot);

    futures_executor::block_on(async {
        let _task = store
            .send_with(MoveOnlyAction, |count| *count = 1)
            .await
            .expect("move-only commit");
        store
            .assert_state(|state| assert_eq!(state.0, 1))
            .await
            .expect("move-only assertion");
        store.finish().await.expect("clean finish");
    });
}

#[test]
fn gate_pauses_effect_action_before_production_commit() {
    let runtime = DeterministicRuntime::new();
    let gate = AssertionGate::new();
    let store = Store::builder(State::Idle, Feature, runtime.runtime())
        .assertion_gate(gate.clone())
        .build()
        .expect("valid Store");

    let task = futures_executor::block_on(runtime.run(store.send(Action::Start)))
        .expect("finite send drive")
        .expect("start commit");
    let started = futures_executor::block_on(runtime.run(store.read(|state| *state)))
        .expect("finite read drive")
        .expect("ordered read");
    assert_eq!(started, State::Started);

    runtime.run_until_stalled().expect("gate stall");
    assert!(gate.has_pending());
    assert_eq!(
        gate.inspect_pending(|action| *action),
        Some(Action::Response)
    );
    assert!(gate.release_pending());
    runtime.run_until_stalled().expect("released action");

    let responded = futures_executor::block_on(runtime.run(store.read(|state| *state)))
        .expect("finite read drive")
        .expect("ordered read");
    assert_eq!(responded, State::Responded);
    assert!(task.is_finished());
    futures_executor::block_on(runtime.run(store.shutdown()))
        .expect("finite shutdown drive")
        .expect("clean shutdown");
}

#[test]
fn skipped_effect_action_does_not_reduce() {
    let runtime = DeterministicRuntime::new();
    let gate = AssertionGate::new();
    let store = Store::builder(State::Idle, Feature, runtime.runtime())
        .assertion_gate(gate.clone())
        .build()
        .expect("valid Store");

    let task = futures_executor::block_on(runtime.run(store.send(Action::Start)))
        .expect("finite send drive")
        .expect("start commit");
    runtime.run_until_stalled().expect("gate stall");
    assert!(gate.skip_pending());
    runtime.run_until_stalled().expect("skip cleanup");

    let state = futures_executor::block_on(runtime.run(store.read(|state| *state)))
        .expect("finite read drive")
        .expect("ordered read");
    assert_eq!(state, State::Started);
    assert!(task.is_finished());
    futures_executor::block_on(runtime.run(store.shutdown()))
        .expect("finite shutdown drive")
        .expect("clean shutdown");
}

#[test]
fn final_owner_drop_breaks_pending_gate_wait() {
    let runtime = DeterministicRuntime::new();
    let gate = AssertionGate::new();
    let store = Store::builder(State::Idle, Feature, runtime.runtime())
        .assertion_gate(gate.clone())
        .build()
        .expect("valid Store");
    let task = futures_executor::block_on(runtime.run(store.send(Action::Start)))
        .expect("finite send drive")
        .expect("start commit");
    runtime.run_until_stalled().expect("gate stall");
    assert!(gate.has_pending());

    drop(store);
    runtime.run_until_stalled().expect("abortive cleanup");

    assert!(!gate.has_pending());
    assert!(task.is_cancelled());
    assert!(task.is_finished());
}

#[test]
fn cancellation_before_gate_drive_resolves_receipts_without_inspecting_state() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    for close_store in [false, true] {
        let runtime = DeterministicRuntime::with_step_budget(64);
        let gate = AssertionGate::new();
        let mut store = Some(
            Store::builder(State::Idle, Feature, runtime.runtime())
                .assertion_gate(gate.clone())
                .build()
                .unwrap(),
        );
        let root =
            finish_without_external_wake(runtime.run(store.as_ref().unwrap().send(Action::Start)))
                .unwrap()
                .unwrap();
        runtime.run_until_stalled().unwrap();
        assert_eq!(
            gate.inspect_pending(|action| *action),
            Some(Action::Response)
        );

        let inspections = Arc::new(AtomicUsize::new(0));
        let inspected = Arc::clone(&inspections);
        let inspection = gate.assert_state::<State>(move |_| {
            inspected.fetch_add(1, Ordering::SeqCst);
        });
        let released = gate.release_pending_with_receipt().unwrap();
        // Both requests exist, but neither has reached the actor when authority ends.
        if close_store {
            drop(store.take());
        } else {
            root.cancel();
        }
        runtime.run_until_stalled().unwrap();

        finish_without_external_wake(inspection.finish())
            .expect_err("cancelled queued inspection must resolve without invoking its callback");
        finish_without_external_wake(released.finish())
            .expect_err("cancelled queued release must not report a successful commit");
        assert_eq!(inspections.load(Ordering::SeqCst), 0);
        assert!(!gate.has_pending());
        assert!(root.is_cancelled());
        assert!(root.is_finished());
        finish_without_external_wake(root.finish());

        if let Some(store) = store {
            let fresh = finish_without_external_wake(runtime.run(store.send(Action::External)))
                .unwrap()
                .expect("lineage cancellation must leave the Store usable");
            assert!(fresh.is_finished());
            let state = finish_without_external_wake(runtime.run(store.read(|state| *state)))
                .unwrap()
                .unwrap();
            assert_eq!(state, State::Started, "cancelled response never reduced");
            let snapshot = finish_without_external_wake(runtime.run(store.runtime_snapshot()))
                .unwrap()
                .unwrap();
            assert_eq!(
                snapshot.commit_version, 2,
                "only Start and External committed"
            );
            assert!(snapshot.live_task_ids.is_empty());
            assert_eq!(inspections.load(Ordering::SeqCst), 0);
            finish_without_external_wake(runtime.run(store.shutdown()))
                .unwrap()
                .unwrap();
        }
    }
}

#[test]
fn dropped_inspection_receipt_keeps_the_effect_action_paused() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let runtime = DeterministicRuntime::with_step_budget(64);
    let gate = AssertionGate::new();
    let store = Store::builder(State::Idle, Feature, runtime.runtime())
        .assertion_gate(gate.clone())
        .build()
        .unwrap();
    let root = finish_without_external_wake(runtime.run(store.send(Action::Start)))
        .unwrap()
        .unwrap();
    runtime.run_until_stalled().unwrap();
    let inspections = Arc::new(AtomicUsize::new(0));
    let abandoned_calls = Arc::clone(&inspections);
    let abandoned = gate.assert_state::<State>(move |_| {
        abandoned_calls.fetch_add(1, Ordering::SeqCst);
    });
    drop(abandoned);
    runtime.run_until_stalled().unwrap();
    assert_eq!(inspections.load(Ordering::SeqCst), 0);
    assert_eq!(
        gate.inspect_pending(|action| *action),
        Some(Action::Response)
    );
    assert!(!root.is_finished());

    let retained_calls = Arc::clone(&inspections);
    let retained = gate.assert_state::<State>(move |state| {
        assert_eq!(*state, State::Started);
        retained_calls.fetch_add(1, Ordering::SeqCst);
    });
    runtime.run_until_stalled().unwrap();
    finish_without_external_wake(retained.finish()).unwrap();
    assert_eq!(inspections.load(Ordering::SeqCst), 1);
    assert_eq!(
        gate.inspect_pending(|action| *action),
        Some(Action::Response)
    );
    assert!(!root.is_finished());

    let released = gate.release_pending_with_receipt().unwrap();
    runtime.run_until_stalled().unwrap();
    let committed = finish_without_external_wake(released.finish()).unwrap();
    assert!(committed.is_finished());
    assert!(root.is_finished());
    assert!(!root.is_cancelled());
    assert!(!gate.has_pending());
    let state = finish_without_external_wake(runtime.run(store.read(|state| *state)))
        .unwrap()
        .unwrap();
    assert_eq!(state, State::Responded);
    let snapshot = finish_without_external_wake(runtime.run(store.runtime_snapshot()))
        .unwrap()
        .unwrap();
    assert_eq!(
        snapshot.commit_version, 2,
        "response committed exactly once"
    );
    assert_eq!(inspections.load(Ordering::SeqCst), 1);
    finish_without_external_wake(runtime.run(store.shutdown()))
        .unwrap()
        .unwrap();
}

#[derive(Clone, Copy)]
enum TimerAction {
    Start,
    Tick,
}

#[derive(Clone, Copy, Eq, Hash, PartialEq)]
struct TimerKey;

struct TimerFeature;

impl Reducer for TimerFeature {
    type State = usize;
    type Action = TimerAction;

    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &Self::Action,
        _context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        match action {
            TimerAction::Start => {
                Effect::interval(Duration::from_secs(1), || TimerAction::Tick).cancellable(TimerKey)
            }
            TimerAction::Tick => {
                *state += 1;
                Effect::none()
            }
        }
    }
}

#[test]
fn explicit_timer_skip_preserves_one_shot_and_interval_lifetimes() {
    for repeating in [false, true] {
        finish_without_external_wake(async {
            let runtime = DeterministicRuntime::with_step_budget(64);
            let gate = AssertionGate::new();
            let reducer = rca_core::Reduce::new(
                move |state: &mut usize, action: &TimerAction, _| match action {
                    TimerAction::Start => {
                        if repeating {
                            Effect::interval(Duration::from_secs(1), || TimerAction::Tick)
                        } else {
                            Effect::send_after(Duration::from_secs(1), TimerAction::Tick)
                        }
                    }
                    TimerAction::Tick => {
                        *state += 1;
                        Effect::none()
                    }
                },
            );
            let store = Store::builder(0, reducer, runtime.runtime())
                .assertion_gate(gate.clone())
                .build()
                .unwrap();
            let root = runtime
                .run(store.send(TimerAction::Start))
                .await
                .unwrap()
                .unwrap();
            runtime.advance(Duration::from_secs(1)).unwrap();
            assert_eq!(
                gate.inspect_pending(|action| matches!(action, TimerAction::Tick)),
                Some(true)
            );
            assert!(!root.is_finished());

            assert!(gate.skip_pending());
            runtime.run_until_stalled().unwrap();
            assert!(!gate.has_pending());
            assert!(!root.is_cancelled());
            assert_eq!(
                runtime
                    .run(store.read(|state| *state))
                    .await
                    .unwrap()
                    .unwrap(),
                0
            );
            let snapshot = gate.latest_snapshot().unwrap();
            assert_eq!(snapshot.commit_version, 1);

            if repeating {
                assert!(!root.is_finished());
                assert_eq!(
                    snapshot.timer_deadlines,
                    [rca_core::MonotonicInstant::from_elapsed(
                        Duration::from_secs(2)
                    )]
                );

                runtime.advance(Duration::from_millis(999)).unwrap();
                assert!(!gate.has_pending());
                runtime.advance(Duration::from_millis(1)).unwrap();
                assert_eq!(
                    gate.inspect_pending(|action| matches!(action, TimerAction::Tick)),
                    Some(true)
                );
                assert!(gate.release_pending());
                runtime.run_until_stalled().unwrap();
                assert!(!gate.has_pending());
                assert_eq!(
                    runtime
                        .run(store.read(|state| *state))
                        .await
                        .unwrap()
                        .unwrap(),
                    1
                );
                assert!(!root.is_finished());
                assert!(!root.is_cancelled());
                root.cancel();
                runtime.run_until_stalled().unwrap();
            } else {
                assert!(root.is_finished());
            }

            root.finish().await;
            assert!(root.is_finished());
            assert_eq!(root.is_cancelled(), repeating);
            let snapshot = runtime
                .run(store.runtime_snapshot())
                .await
                .unwrap()
                .unwrap();
            assert!(snapshot.live_task_ids.is_empty());
            assert!(snapshot.timer_deadlines.is_empty());
            assert!(runtime.pending_sleeps().is_empty());
            runtime.run(store.shutdown()).await.unwrap().unwrap();
        });
    }
}

#[test]
fn interval_tick_can_cancel_its_own_identity_without_rescheduling() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    finish_without_external_wake(async {
        let runtime = DeterministicRuntime::with_step_budget(64);
        let gate = AssertionGate::new();
        let calls = Arc::new(AtomicUsize::new(0));
        let callback_calls = Arc::clone(&calls);
        let capture = Arc::new(());
        let capture_lifetime = Arc::downgrade(&capture);
        let mut capture = Some(capture);
        let reducer =
            rca_core::Reduce::new(
                move |state: &mut usize, action: &TimerAction, _| match action {
                    TimerAction::Start => {
                        let capture = capture.take().expect("start interval once");
                        let calls = Arc::clone(&callback_calls);
                        Effect::interval(Duration::from_secs(1), move || {
                            assert_eq!(Arc::strong_count(&capture), 1);
                            calls.fetch_add(1, Ordering::SeqCst);
                            TimerAction::Tick
                        })
                        .cancellable(TimerKey)
                    }
                    TimerAction::Tick => {
                        *state += 1;
                        Effect::cancel(TimerKey)
                    }
                },
            );
        let store = Store::builder(0, reducer, runtime.runtime())
            .assertion_gate(gate.clone())
            .build()
            .expect("valid store");
        let interval = runtime
            .run(store.send(TimerAction::Start))
            .await
            .unwrap()
            .unwrap();
        runtime.advance(Duration::from_secs(1)).unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(capture_lifetime.upgrade().is_some());
        assert!(!interval.is_finished());
        assert_eq!(
            gate.inspect_pending(|action| matches!(action, TimerAction::Tick)),
            Some(true)
        );

        assert!(gate.release_pending());
        runtime.run_until_stalled().unwrap();
        assert_eq!(
            runtime
                .run(store.read(|state| *state))
                .await
                .unwrap()
                .unwrap(),
            1
        );
        assert!(interval.is_finished());
        // Identity cancellation completes this task without cancelling its root lineage.
        assert!(!interval.is_cancelled());
        interval.finish().await;
        assert!(capture_lifetime.upgrade().is_none());
        let snapshot = runtime
            .run(store.runtime_snapshot())
            .await
            .unwrap()
            .unwrap();
        assert!(snapshot.live_task_ids.is_empty());
        assert!(snapshot.timer_deadlines.is_empty());
        assert!(runtime.pending_sleeps().is_empty());

        runtime.advance(Duration::from_secs(3)).unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(!gate.has_pending());
        assert_eq!(
            runtime
                .run(store.read(|state| *state))
                .await
                .unwrap()
                .unwrap(),
            1
        );
        runtime.run(store.shutdown()).await.unwrap().unwrap();
    });
}

#[test]
fn one_advance_exposes_every_fixed_rate_tick_due_by_target() {
    let mut store = TestStore::new(0, TimerFeature);

    futures_executor::block_on(async {
        let interval = store
            .send(TimerAction::Start)
            .await
            .expect("start interval");
        store
            .advance(Duration::from_secs(3))
            .await
            .expect("virtual advance");

        for expected in 1..=3 {
            let _tick = store
                .receive_with(
                    |action| matches!(action, TimerAction::Tick),
                    move |state| *state = expected,
                )
                .await
                .expect("due tick");
            store
                .assert_state(move |state| assert_eq!(*state, expected))
                .await
                .expect("tick state");
        }

        interval.cancel().await.expect("interval cancellation");
        store.finish().await.expect("clean finish");
    });
}

#[test]
fn cancelling_interval_with_gated_tick_finishes_cleanup() {
    let mut store = TestStore::new(0, TimerFeature);

    futures_executor::block_on(async {
        let interval = store
            .send(TimerAction::Start)
            .await
            .expect("start interval");
        store
            .advance(Duration::from_secs(1))
            .await
            .expect("first deadline");

        interval.cancel().await.expect("deterministic cancellation");
        assert!(interval.is_cancelled());
        assert!(interval.is_finished());
        assert!(store.finish().await.unwrap_err().is_incomplete_assertion());
        let boundary = store.pending_boundary().unwrap();
        store
            .receive_terminal(boundary, |error| {
                error.reason() == rca_core::TaskAccessReason::Cancelled
                    && error.progress() == rca_core::TaskProgress::NotStarted
            })
            .await
            .unwrap();
        store.finish().await.expect("clean finish");
    });
}

#[test]
fn task_finish_reports_origin_and_virtual_sleep_without_advancing() {
    let mut store = TestStore::new(0, TimerFeature);

    futures_executor::block_on(async {
        let origin_line = line!() + 2;
        let interval = store
            .send(TimerAction::Start)
            .await
            .expect("start interval");
        let error = interval.finish().await.expect_err("timer remains blocked");

        assert_eq!(error.origin().line(), origin_line);
        assert_eq!(error.current_time(), rca_core::MonotonicInstant::ZERO);
        assert_eq!(error.pending_sleeps().len(), 1);
        assert_eq!(
            error.pending_sleeps()[0].deadline(),
            rca_core::MonotonicInstant::from_elapsed(Duration::from_secs(1))
        );
        let runtime = error.runtime_snapshot().expect("core runtime snapshot");
        assert_eq!(runtime.live_task_ids, [0]);
        assert_eq!(runtime.cancellation_count, 1);
        let identities = error.cancellation_identities();
        assert_eq!(identities.len(), 1);
        assert_eq!(identities[0].diagnostic_id(), 0);
        assert_eq!(identities[0].scope_id(), 0);
        assert_eq!(identities[0].type_name(), std::any::type_name::<TimerKey>());
        assert_eq!(
            runtime.timer_deadlines,
            [rca_core::MonotonicInstant::from_elapsed(
                Duration::from_secs(1)
            )]
        );

        interval.cancel().await.expect("deterministic cancellation");
        interval.finish().await.expect("cancelled task finished");
        store.finish().await.expect("clean finish");
    });
}

#[test]
fn finish_reports_live_core_timer_and_driver_sleep() {
    let mut store = TestStore::new(0, TimerFeature);

    futures_executor::block_on(async {
        let interval = store
            .send(TimerAction::Start)
            .await
            .expect("start interval");
        let error = store.finish().await.expect_err("live interval");
        let diagnostics = error.finish_diagnostics().expect("finish diagnostics");
        assert!(!diagnostics.has_pending_action());
        assert_eq!(diagnostics.runtime().live_task_ids, [0]);
        assert_eq!(diagnostics.runtime().cancellation_count, 1);
        assert_eq!(diagnostics.cancellation_identities().len(), 1);
        let identity = &diagnostics.cancellation_identities()[0];
        assert_eq!(identity.diagnostic_id(), 0);
        assert_eq!(identity.scope_id(), 0);
        assert_eq!(identity.type_name(), std::any::type_name::<TimerKey>());
        assert_eq!(
            diagnostics.runtime().timer_deadlines,
            [rca_core::MonotonicInstant::from_elapsed(
                Duration::from_secs(1)
            )]
        );
        assert_eq!(diagnostics.pending_sleeps().len(), 1);

        interval.cancel().await.expect("deterministic cancellation");
        store.finish().await.expect("clean finish");
    });
}

#[test]
fn dropping_test_store_invalidates_causal_work_and_scheduler_tasks() {
    let mut store = TestStore::new(0, TimerFeature);
    let interval =
        futures_executor::block_on(store.send(TimerAction::Start)).expect("start interval");

    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(store)))
        .expect_err("Drop must report unfinished causal work");
    assert!(panic.downcast_ref::<String>().unwrap().starts_with(
        "TestStore teardown failed: TestStore finished with RCA-owned work pending at "
    ));

    assert!(interval.is_cancelled());
    assert!(interval.is_finished());
}

struct FiniteSelfWakeFeature;

impl Reducer for FiniteSelfWakeFeature {
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

enum GatedWorkAction {
    Start,
    Pending,
}

struct GatedWork {
    gate: Option<rca_test::ControlledGate>,
    polls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

impl Reducer for GatedWork {
    type State = ();
    type Action = GatedWorkAction;

    fn reduce(
        &mut self,
        (): &mut (),
        action: &GatedWorkAction,
        _: &mut ReduceContext<'_>,
    ) -> Effect<GatedWorkAction> {
        if matches!(action, GatedWorkAction::Pending) {
            return Effect::run(|_| std::future::pending());
        }
        let gate = self
            .gate
            .take()
            .expect("only one action starts finite work");
        let polls = std::sync::Arc::clone(&self.polls);
        Effect::run(move |_| async move {
            gate.await.expect("test releases the gate");
            std::future::poll_fn(move |context| {
                // Eight polls require another drive after the seven-poll budget.
                if polls.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1 == 8 {
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

#[test]
fn task_finish_budget_error_preserves_source_and_allows_finite_completion() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    // Losing the drive cause or discarding ready work would break these oracles.
    let (control, gate) = rca_test::controlled_gate();
    let polls = Arc::new(AtomicUsize::new(0));
    let mut store = TestStore::with_step_budget(
        (),
        GatedWork {
            gate: Some(gate),
            polls: Arc::clone(&polls),
        },
        7,
    );
    futures_executor::block_on(async {
        let origin_line = line!() + 2;
        let task = store
            .send(GatedWorkAction::Start)
            .await
            .expect("gated work admits normally");
        control.wait_until_waiting().await.expect("gate was polled");
        assert_eq!(polls.load(Ordering::Relaxed), 0);
        control.release().expect("release finite work");

        let error = task
            .finish()
            .await
            .expect_err("finite work exceeds one drive");
        assert_eq!(error.origin().file(), file!());
        assert_eq!(error.origin().line(), origin_line);
        assert_eq!(error.origin(), task.origin());
        let drive = error.drive_error().expect("retained drive cause");
        assert!(drive.is_step_budget_exhausted());
        assert_eq!(drive.polls(), 7);
        assert_eq!(drive.step_budget(), 7);
        assert_eq!(polls.load(Ordering::Relaxed), 7);
        let display = error.to_string();
        assert!(display.contains(&drive.to_string()), "{display}");
        assert!(!display.contains("remains blocked"), "{display}");
        let debug = format!("{error:?}");
        assert!(debug.contains("StepBudget"), "{debug}");
        let source = std::error::Error::source(&error)
            .and_then(|source| source.downcast_ref::<rca_test::DriveError>())
            .expect("typed drive source");
        assert!(std::ptr::eq(source, drive));
        assert_eq!(error.ready_task_ids(), [1]);
        assert_eq!(error.ready_task_ids(), drive.ready_task_ids());
        assert!(error.pending_sleeps().is_empty());
        assert_eq!(error.pending_sleeps(), drive.pending_sleeps());
        assert_eq!(error.current_time(), rca_core::MonotonicInstant::ZERO);
        assert_eq!(error.current_time(), drive.current_time());
        let runtime = error
            .runtime_snapshot()
            .expect("captured actor diagnostics");
        assert_eq!(runtime.commit_version, 1);
        assert_eq!(runtime.live_task_ids, [0]);
        assert!(error.cancellation_identities().is_empty());
        assert!(!task.is_finished());

        task.finish()
            .await
            .expect("remaining finite work completes");
        assert!(task.is_finished());
        assert_eq!(polls.load(Ordering::Relaxed), 8);
        store.finish().await.expect("clean finish after recovery");
    });
}

#[test]
fn task_cancel_is_lazy_and_preserves_unrelated_work_after_budget_error() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::{Context, Waker};

    let (control, gate) = rca_test::controlled_gate();
    let polls = Arc::new(AtomicUsize::new(0));
    let mut store = TestStore::with_step_budget(
        (),
        GatedWork {
            gate: Some(gate),
            polls: Arc::clone(&polls),
        },
        7,
    );
    futures_executor::block_on(async {
        let selected = store
            .send(GatedWorkAction::Pending)
            .await
            .expect("pending work admits");
        let unrelated = store
            .send(GatedWorkAction::Start)
            .await
            .expect("gated work admits");
        control.wait_until_waiting().await.expect("gate was polled");

        // Requesting cancellation at construction would cancel this live task.
        drop(selected.cancel());
        assert!(!selected.is_cancelled());
        assert!(!selected.is_finished());
        assert_eq!(polls.load(Ordering::Relaxed), 0);
        control.release().expect("release unrelated finite work");

        let error: rca_test::DriveError = {
            let mut cancellation = std::pin::pin!(selected.cancel());
            let mut context = Context::from_waker(Waker::noop());
            let Poll::Ready(Err(error)) = cancellation.as_mut().poll(&mut context) else {
                panic!("first cancellation poll must report the unrelated work budget");
            };
            assert!(selected.is_cancelled());
            error
        };
        assert!(error.is_step_budget_exhausted());
        assert_eq!(error.polls(), 7);
        assert_eq!(error.step_budget(), 7);
        assert!(!error.ready_task_ids().is_empty());
        assert!(error.pending_sleeps().is_empty());
        assert_eq!(error.current_time(), rca_core::MonotonicInstant::ZERO);
        assert!(!unrelated.is_cancelled());
        assert!(!unrelated.is_finished());

        selected
            .cancel()
            .await
            .expect("retry drives finite cleanup");
        assert!(selected.is_finished());
        unrelated
            .finish()
            .await
            .expect("unrelated finite work survives");
        assert!(unrelated.is_finished());
        assert!(!unrelated.is_cancelled());
        assert_eq!(polls.load(Ordering::Relaxed), 8);
        store
            .finish()
            .await
            .expect("clean finish after cancellation retry");
    });
}

#[test]
fn test_store_budget_error_reports_exact_polls_and_caller() {
    let mut store = TestStore::with_step_budget((), FiniteSelfWakeFeature, 7);

    futures_executor::block_on(async {
        let origin_line = line!() + 2;
        let error = store
            .send(())
            .await
            .expect_err("finite effect exceeds drive");
        assert_eq!(error.origin().line(), origin_line);
        let drive = error.drive_error().expect("drive error");
        assert_eq!(drive.polls(), 7);
        assert_eq!(drive.step_budget(), 7);
        assert_eq!(drive.ready_task_ids(), [1]);
        assert_eq!(drive.current_time(), rca_core::MonotonicInstant::ZERO);
        let runtime = error.runtime_snapshot().expect("core runtime snapshot");
        assert_eq!(runtime.commit_version, 1);
        assert_eq!(runtime.live_task_ids, [0]);
        assert!(runtime.timer_deadlines.is_empty());
        assert!(error.cancellation_identities().is_empty());
        assert!(error.finish_diagnostics().is_none());
        let source = std::error::Error::source(&error)
            .and_then(|source| source.downcast_ref::<rca_test::DriveError>())
            .expect("typed scheduler cause");
        assert!(std::ptr::eq(source, drive));

        let error = store
            .finish()
            .await
            .expect_err("admitted assertion remains incomplete");
        assert!(error.is_incomplete_assertion());
    });
    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(store)))
        .expect_err("Drop must enforce the admitted send assertion");
    assert!(
        panic
            .downcast_ref::<String>()
            .unwrap()
            .starts_with("TestStore teardown failed: TestStore incomplete assertion for send at ")
    );
}

#[test]
fn runtime_snapshot_reports_actor_owned_work() {
    let runtime = DeterministicRuntime::new();
    let gate = AssertionGate::new();
    let store = Store::builder(0, TimerFeature, runtime.runtime())
        .assertion_gate(gate)
        .build()
        .expect("valid Store");
    let observation = futures_executor::block_on(runtime.run(store.observe(|state| *state)))
        .expect("finite observation drive")
        .expect("registration");
    let task = futures_executor::block_on(runtime.run(store.send(TimerAction::Start)))
        .expect("finite send drive")
        .expect("timer commit");
    runtime.run_until_stalled().expect("timer stall");

    let snapshot = futures_executor::block_on(runtime.run(store.runtime_snapshot()))
        .expect("finite snapshot drive")
        .expect("snapshot");

    assert_eq!(snapshot.commit_version, 1);
    assert_eq!(snapshot.live_task_ids, [0]);
    assert_eq!(snapshot.cancellation_count, 1);
    assert_eq!(
        snapshot.timer_deadlines,
        [rca_core::MonotonicInstant::from_elapsed(
            Duration::from_secs(1)
        )]
    );
    assert_eq!(snapshot.observation_count, 1);

    task.cancel();
    runtime.run_until_stalled().expect("cancellation cleanup");
    assert!(task.is_finished());
    drop(observation);
    runtime.run_until_stalled().expect("observation cleanup");
    futures_executor::block_on(runtime.run(store.shutdown()))
        .expect("finite shutdown drive")
        .expect("clean shutdown");
}

#[test]
fn async_run_exhausts_its_budget_without_discarding_runnable_work() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::{Context, Waker};

    let runtime = DeterministicRuntime::with_step_budget(3);
    let polls = Arc::new(AtomicUsize::new(0));
    let task_polls = Arc::clone(&polls);
    runtime.spawn_task(Box::pin(std::future::poll_fn(move |context| {
        if task_polls.fetch_add(1, Ordering::Relaxed) + 1 == 5 {
            Poll::Ready(())
        } else {
            context.waker().wake_by_ref();
            Poll::Pending
        }
    })));

    // Ignoring the async budget would leave the third client poll pending.
    let (control, gate) = rca_test::controlled_gate();
    let error = {
        let mut drive = std::pin::pin!(runtime.run(gate));
        let mut context = Context::from_waker(Waker::noop());
        for _ in 0..2 {
            assert!(matches!(drive.as_mut().poll(&mut context), Poll::Pending));
        }
        futures_executor::block_on(control.wait_until_waiting()).unwrap();
        let Poll::Ready(Err(error)) = drive.as_mut().poll(&mut context) else {
            panic!("the third poll must report the exhausted task budget");
        };
        error
    };
    assert!(error.is_step_budget_exhausted());
    assert_eq!(error.polls(), 3);
    assert_eq!(error.step_budget(), 3);
    assert_eq!(error.ready_task_ids(), [0]);
    assert_eq!(error.current_time(), rca_core::MonotonicInstant::ZERO);
    assert_eq!(polls.load(Ordering::Relaxed), 3);
    assert_eq!(control.release(), Err(rca_test::GateDropped));

    let report = runtime
        .run_until_stalled()
        .expect("retained work completes on the next drive");
    assert_eq!(report.polls(), 2);
    assert_eq!(polls.load(Ordering::Relaxed), 5);
    assert_eq!(runtime.run_until_stalled().unwrap().polls(), 0);
}

#[test]
fn advance_rejects_unreceived_action_without_moving_time() {
    let mut store = TestStore::new(State::Idle, Feature);
    futures_executor::block_on(async {
        let start = store
            .send_with(Action::Start, |state| *state = State::Started)
            .await
            .unwrap();
        let error = store
            .advance(Duration::from_secs(10))
            .await
            .expect_err("the response must be explicitly received first");
        assert!(error.is_unreceived_action());
        assert_eq!(store.now(), rca_core::MonotonicInstant::ZERO);
        assert!(!start.is_finished());
        store.assert_state_eq(State::Started).await.unwrap();

        let _response = store
            .receive_with(
                |action| *action == Action::Response,
                |state| *state = State::Responded,
            )
            .await
            .expect("the rejected advance leaves the response pending");
        assert_eq!(store.now(), rca_core::MonotonicInstant::ZERO);
        let report = store.advance(Duration::from_secs(1)).await.unwrap();
        assert_eq!(
            report.current_time(),
            rca_core::MonotonicInstant::from_elapsed(Duration::from_secs(1))
        );
        assert!(start.is_finished());
        store.finish().await.unwrap();
    });
}

#[test]
fn exhausted_advance_discards_its_target_before_the_next_advance() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct GatedWork {
        gate: Option<rca_test::ControlledGate>,
        polls: Arc<AtomicUsize>,
    }

    impl Reducer for GatedWork {
        type State = ();
        type Action = ();

        fn reduce(&mut self, (): &mut (), (): &(), _: &mut ReduceContext<'_>) -> Effect<()> {
            let gate = self.gate.take().expect("only one action starts work");
            let polls = Arc::clone(&self.polls);
            Effect::run(move |_| async move {
                gate.await.expect("test releases the gate");
                std::future::poll_fn(move |context| {
                    if polls.fetch_add(1, Ordering::Relaxed) + 1 == 8 {
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

    let (control, gate) = rca_test::controlled_gate();
    let polls = Arc::new(AtomicUsize::new(0));
    let mut store = TestStore::with_step_budget(
        (),
        GatedWork {
            gate: Some(gate),
            polls: Arc::clone(&polls),
        },
        7,
    );
    futures_executor::block_on(async {
        let task = store.send(()).await.expect("work waits outside the driver");
        control.wait_until_waiting().await.unwrap();
        assert_eq!(polls.load(Ordering::Relaxed), 0);
        control.release().unwrap();
        let error = store
            .advance(Duration::from_secs(10))
            .await
            .expect_err("eight ready polls exceed the seven-poll budget");
        let drive = error.drive_error().expect("typed budget cause");
        assert!(drive.is_step_budget_exhausted());
        assert_eq!(drive.polls(), 7);
        assert_eq!(drive.step_budget(), 7);
        assert_eq!(polls.load(Ordering::Relaxed), 7);
        assert_eq!(store.now(), rca_core::MonotonicInstant::ZERO);
        assert!(!task.is_finished());

        // Retaining the abandoned target would advance to eleven seconds instead.
        let report = store
            .advance(Duration::from_secs(1))
            .await
            .expect("a fresh advance finishes the retained finite work");
        assert_eq!(
            report.current_time(),
            rca_core::MonotonicInstant::from_elapsed(Duration::from_secs(1))
        );
        assert_eq!(store.now(), report.current_time());
        assert_eq!(polls.load(Ordering::Relaxed), 8);
        assert!(task.is_finished());
        store
            .finish()
            .await
            .expect("no abandoned advance or assertion debt");
    });
}

#[test]
fn empty_capture_check_evaluates_expectations_without_driving_ready_work() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let (control, gate) = rca_test::controlled_gate();
    let polls = Arc::new(AtomicUsize::new(0));
    let mut store = TestStore::with_step_budget(
        (),
        GatedWork {
            gate: Some(gate),
            polls: Arc::clone(&polls),
        },
        7,
    );
    futures_executor::block_on(async {
        let task = store.send(GatedWorkAction::Start).await.unwrap();
        control.wait_until_waiting().await.unwrap();
        control.release().unwrap();

        let mut checks = 0;
        store
            .check_captures(|_| {
                checks += 1;
                Ok(())
            })
            .await
            .expect("no declared captures means no capture work");
        assert_eq!(checks, 1);
        assert_eq!(polls.load(Ordering::Relaxed), 0);
        assert_eq!(store.now(), rca_core::MonotonicInstant::ZERO);
        assert!(!task.is_finished());

        let error = task
            .finish()
            .await
            .expect_err("ready work was not discarded");
        assert!(error.drive_error().unwrap().is_step_budget_exhausted());
        assert_eq!(polls.load(Ordering::Relaxed), 7);
        task.finish().await.expect("retained finite work completes");
        assert_eq!(polls.load(Ordering::Relaxed), 8);
        store
            .finish()
            .await
            .expect("empty capture check leaves no assertion debt");
    });
}

fn finish_without_external_wake<T>(future: impl Future<Output = T>) -> T {
    let mut future = std::pin::pin!(future);
    let mut context = std::task::Context::from_waker(std::task::Waker::noop());
    // These fixtures need no external wake; a stranded client must fail, not hang.
    for _ in 0..64 {
        if let std::task::Poll::Ready(output) = future.as_mut().poll(&mut context) {
            return output;
        }
    }
    panic!("finite test operation remained pending after 64 client polls");
}

#[test]
fn issue_budget_failure_preserves_queue_without_running_matcher() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct ReportingWork {
        work: GatedWork,
        issues: Option<[rca_core::Issue; 2]>,
    }

    impl Reducer for ReportingWork {
        type State = ();
        type Action = GatedWorkAction;

        fn reduce(
            &mut self,
            state: &mut (),
            action: &GatedWorkAction,
            context: &mut ReduceContext<'_>,
        ) -> Effect<GatedWorkAction> {
            for issue in self.issues.take().expect("one action starts work") {
                context.report_issue(issue).unwrap();
            }
            self.work.reduce(state, action, context)
        }
    }

    let expected = [
        rca_core::Issue::new("budget-first", "first payload"),
        rca_core::Issue::new("budget-second", "second payload"),
    ];
    let (control, gate) = rca_test::controlled_gate();
    let polls = Arc::new(AtomicUsize::new(0));
    let mut store = TestStore::with_step_budget(
        (),
        ReportingWork {
            work: GatedWork {
                gate: Some(gate),
                polls: Arc::clone(&polls),
            },
            issues: Some(expected.clone()),
        },
        7,
    );
    let task = finish_without_external_wake(store.send(GatedWorkAction::Start)).unwrap();
    finish_without_external_wake(control.wait_until_waiting()).unwrap();
    assert_eq!(polls.load(Ordering::Relaxed), 0);
    control.release().unwrap();

    let mut matches = 0;
    let origin_line = line!() + 1;
    let expectation = store.expect_issue(|_| {
        matches += 1;
        true
    });
    let error = finish_without_external_wake(expectation)
        .expect_err("ready work exceeds this expectation's seven-poll budget");
    assert_eq!(
        matches, 0,
        "failed driving must not inspect or consume an issue"
    );
    assert_eq!(error.origin().file(), file!());
    assert_eq!(error.origin().line(), origin_line);
    let drive = error.drive_error().expect("retained driving cause");
    assert!(drive.is_step_budget_exhausted());
    assert_eq!(drive.polls(), 7);
    assert_eq!(drive.step_budget(), 7);
    assert_eq!(polls.load(Ordering::Relaxed), 7);
    assert_eq!(store.now(), rca_core::MonotonicInstant::ZERO);
    assert!(!task.is_finished());

    for expected_issue in expected {
        let actual = finish_without_external_wake(store.expect_issue(|issue| {
            matches += 1;
            issue.code == expected_issue.code
        }))
        .expect("retry retains the original issue and FIFO order");
        assert_eq!(
            actual, expected_issue,
            "payload and original location survive"
        );
    }
    assert_eq!(matches, 2);
    assert_eq!(polls.load(Ordering::Relaxed), 8);
    assert_eq!(store.now(), rca_core::MonotonicInstant::ZERO);
    assert!(
        finish_without_external_wake(store.expect_issue(|_| panic!("queue must be empty")))
            .unwrap_err()
            .is_no_issue()
    );
    finish_without_external_wake(task.finish()).unwrap();
    finish_without_external_wake(store.finish()).unwrap();
}

#[test]
fn receive_budget_failure_preserves_action_and_defers_matcher_and_update() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct OutputWork(GatedWork);

    impl Reducer for OutputWork {
        type State = u32;
        type Action = GatedWorkAction;

        fn reduce(
            &mut self,
            state: &mut u32,
            action: &GatedWorkAction,
            context: &mut ReduceContext<'_>,
        ) -> Effect<GatedWorkAction> {
            if matches!(action, GatedWorkAction::Pending) {
                *state += 1;
                return Effect::none();
            }
            Effect::merge([
                self.0.reduce(&mut (), action, context),
                Effect::send(GatedWorkAction::Pending),
            ])
        }
    }

    let (control, gate) = rca_test::controlled_gate();
    let polls = Arc::new(AtomicUsize::new(0));
    let mut store = TestStore::with_step_budget(
        0,
        OutputWork(GatedWork {
            gate: Some(gate),
            polls: Arc::clone(&polls),
        }),
        7,
    );
    let task = finish_without_external_wake(store.send(GatedWorkAction::Start)).unwrap();
    finish_without_external_wake(control.wait_until_waiting()).unwrap();
    control.release().unwrap();
    let mut matches = 0;
    let mut updates = 0;
    let origin_line = line!() + 1;
    let receive = store.receive_with(
        |_| {
            matches += 1;
            true
        },
        |state| {
            updates += 1;
            *state += 1;
        },
    );
    let error = finish_without_external_wake(receive).expect_err("ready work exceeds budget");
    assert_eq!((matches, updates), (0, 0));
    assert_eq!(error.origin().file(), file!());
    assert_eq!(error.origin().line(), origin_line);
    let drive = error.drive_error().expect("original typed driving cause");
    assert!(drive.is_step_budget_exhausted());
    assert_eq!((drive.polls(), drive.step_budget()), (7, 7));
    assert_eq!(polls.load(Ordering::Relaxed), 7);
    assert_eq!(store.now(), rca_core::MonotonicInstant::ZERO);
    assert!(!task.is_finished());

    let received = finish_without_external_wake(store.receive_with(
        |action| {
            matches += 1;
            matches!(action, GatedWorkAction::Pending)
        },
        |state| {
            updates += 1;
            *state += 1;
        },
    ))
    .expect("retry receives and reduces the original action exactly once");
    assert_eq!((matches, updates), (1, 1));
    assert_eq!(polls.load(Ordering::Relaxed), 8);
    finish_without_external_wake(store.assert_state_eq(1)).unwrap();
    assert_eq!(store.now(), rca_core::MonotonicInstant::ZERO);
    finish_without_external_wake(received.finish()).unwrap();
    finish_without_external_wake(task.finish()).unwrap();
    finish_without_external_wake(store.finish()).unwrap();
}

#[test]
fn spent_task_budget_still_accepts_external_client_completion() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let runtime = DeterministicRuntime::with_step_budget(1);
    let calls = Arc::new(AtomicUsize::new(0));
    let task_calls = Arc::clone(&calls);
    runtime.spawn_task(Box::pin(async move {
        task_calls.fetch_add(1, Ordering::SeqCst);
    }));
    let (completion, future) = rca_test::controlled_future::<u32>();
    let mut run = Box::pin(runtime.run(future));
    let mut context = std::task::Context::from_waker(std::task::Waker::noop());
    assert!(run.as_mut().poll(&mut context).is_pending());
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    // The sole task has finished: an empty budget is not runnable work or client cancellation.
    assert!(run.as_mut().poll(&mut context).is_pending());
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    completion.complete(17).unwrap();
    assert!(matches!(
        run.as_mut().poll(&mut context),
        Poll::Ready(Ok(Ok(17)))
    ));
    drop(run);
    assert_eq!(runtime.run_until_stalled().unwrap().polls(), 0);
    assert_eq!(runtime.now(), rca_core::MonotonicInstant::ZERO);
}

#[test]
fn received_commit_survives_post_receipt_settle_budget_failure() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let polls = Arc::new(AtomicUsize::new(0));
    let work_polls = Arc::clone(&polls);
    let commits = Arc::new(AtomicUsize::new(0));
    let reductions = Arc::clone(&commits);
    let reducer = rca_core::Reduce::new(
        move |state: &mut u32, apply: &bool, _: &mut ReduceContext<'_>| {
            if !apply {
                return Effect::send(true);
            }
            *state += 1;
            reductions.fetch_add(1, Ordering::SeqCst);
            let work_polls = Arc::clone(&work_polls);
            // This finite effect outlasts both the receipt drive and next settle.
            Effect::future(futures_util::future::poll_fn(move |context| {
                if work_polls.fetch_add(1, Ordering::SeqCst) < 16 {
                    context.waker().wake_by_ref();
                    Poll::Pending
                } else {
                    Poll::Ready(None::<bool>)
                }
            }))
        },
    );
    let mut store = TestStore::with_step_budget(0, reducer, 7);
    let started = finish_without_external_wake(store.send(false)).unwrap();
    let origin_line = line!() + 1;
    let received = store.receive_with(|apply| *apply, |state| *state = 1);
    let error = finish_without_external_wake(received)
        .expect_err("committed receipt still has finite work exceeding the settle budget");
    let drive = error
        .drive_error()
        .expect("retain the typed settle failure");
    assert!(drive.is_step_budget_exhausted());
    assert_eq!((drive.polls(), drive.step_budget()), (7, 7));
    assert_eq!(commits.load(Ordering::SeqCst), 1);
    assert!(polls.load(Ordering::SeqCst) < 17);

    let cleanup = finish_without_external_wake(started.finish())
        .expect_err("remaining finite work exceeds one cleanup budget");
    let drive = cleanup
        .drive_error()
        .expect("cleanup retains its typed budget failure");
    assert!(drive.is_step_budget_exhausted());
    assert_eq!((drive.polls(), drive.step_budget()), (7, 7));
    finish_without_external_wake(started.finish())
        .expect("remaining finite work fits the next cleanup budget");
    assert_eq!(polls.load(Ordering::SeqCst), 17);
    finish_without_external_wake(store.assert_state_eq(1)).unwrap();
    let debt = finish_without_external_wake(store.finish()).unwrap_err();
    assert!(debt.is_incomplete_assertion());
    assert_eq!(debt.origin().file(), file!());
    assert_eq!(debt.origin().line(), origin_line);
    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(store)))
        .expect_err("finishing work must not discharge the failed receive assertion");
    assert!(
        panic.downcast_ref::<String>().unwrap().starts_with(
            "TestStore teardown failed: TestStore incomplete assertion for receive at "
        )
    );
    assert_eq!(commits.load(Ordering::SeqCst), 1);
    assert_eq!(Arc::strong_count(&polls), 1);
    assert_eq!(Arc::strong_count(&commits), 1);
}

#[test]
fn skip_budget_failure_preserves_pending_action_for_retry() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct OutputWork(GatedWork);

    impl Reducer for OutputWork {
        type State = ();
        type Action = GatedWorkAction;

        fn reduce(
            &mut self,
            state: &mut (),
            action: &GatedWorkAction,
            context: &mut ReduceContext<'_>,
        ) -> Effect<GatedWorkAction> {
            assert!(
                matches!(action, GatedWorkAction::Start),
                "skipped effect action must never reduce"
            );
            Effect::merge([
                self.0.reduce(state, action, context),
                Effect::send(GatedWorkAction::Pending),
            ])
        }
    }

    let (control, gate) = rca_test::controlled_gate();
    let polls = Arc::new(AtomicUsize::new(0));
    let mut store = TestStore::builder(
        (),
        OutputWork(GatedWork {
            gate: Some(gate),
            polls: Arc::clone(&polls),
        }),
    )
    .with_exhaustivity(Exhaustivity::NonExhaustive)
    .with_step_budget(7)
    .build();
    assert_eq!(store.exhaustivity(), Exhaustivity::NonExhaustive);
    let task = finish_without_external_wake(store.send(GatedWorkAction::Start)).unwrap();
    finish_without_external_wake(control.wait_until_waiting()).unwrap();
    assert_eq!(polls.load(Ordering::Relaxed), 0);
    assert!(!task.is_finished());
    control.release().unwrap();

    let origin_line = line!() + 1;
    let skip = store.skip_received();
    let error = finish_without_external_wake(skip)
        .expect_err("ready work exhausts the budget before the action can be skipped");
    assert_eq!(error.origin().file(), file!());
    assert_eq!(error.origin().line(), origin_line);
    let drive = error.drive_error().expect("original typed driving cause");
    assert!(drive.is_step_budget_exhausted());
    assert_eq!(drive.polls(), 7);
    assert_eq!(drive.step_budget(), 7);
    assert_eq!(polls.load(Ordering::Relaxed), 7);
    assert_eq!(store.now(), rca_core::MonotonicInstant::ZERO);
    assert!(!task.is_finished());

    // Consuming the pending action before failed driving would make this retry fail.
    finish_without_external_wake(store.skip_received())
        .expect("retry skips the original action without reducing it");
    assert_eq!(polls.load(Ordering::Relaxed), 8);
    assert_eq!(store.now(), rca_core::MonotonicInstant::ZERO);
    assert!(task.is_finished());
    finish_without_external_wake(store.assert_state_eq(())).unwrap();
    finish_without_external_wake(task.finish()).unwrap();
    finish_without_external_wake(store.finish()).unwrap();
}

#[test]
fn skip_settle_budget_failure_retains_admitted_assertion_debt() {
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct CleanupWork(Arc<AtomicUsize>);

    impl Reducer for CleanupWork {
        type State = ();
        type Action = Action;

        fn reduce(
            &mut self,
            (): &mut (),
            action: &Action,
            _: &mut ReduceContext<'_>,
        ) -> Effect<Action> {
            assert_eq!(*action, Action::Start, "a skipped response must not reduce");
            let polls = Arc::clone(&self.0);
            Effect::run(move |send| async move {
                let error = send.send(Action::Response).await.unwrap_err();
                assert!(error.is_cancelled());
                // Cleanup begins only after the actor acknowledges the skip.
                std::future::poll_fn(move |context| {
                    if polls.fetch_add(1, Ordering::SeqCst) + 1 == 128 {
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

    let polls = Arc::new(AtomicUsize::new(0));
    let mut store = TestStore::builder((), CleanupWork(Arc::clone(&polls)))
        .with_exhaustivity(Exhaustivity::NonExhaustive)
        .with_step_budget(32)
        .build();
    let task = finish_without_external_wake(store.send(Action::Start)).unwrap();
    assert_eq!(polls.load(Ordering::SeqCst), 0);
    assert!(!task.is_finished());

    let origin_line = line!() + 1;
    let skipped = store.skip_received();
    let failure = finish_without_external_wake(skipped)
        .expect_err("post-skip cleanup exceeds the finite settle budget");
    assert_eq!(failure.origin().file(), file!());
    assert_eq!(failure.origin().line(), origin_line);
    let drive = failure.drive_error().unwrap();
    assert!(drive.is_step_budget_exhausted());
    assert_eq!((drive.polls(), drive.step_budget()), (32, 32));
    let stopped_at = polls.load(Ordering::SeqCst);
    assert!((1..128).contains(&stopped_at));
    assert!(!task.is_finished());
    assert_eq!(store.now(), rca_core::MonotonicInstant::ZERO);

    // Unlike a pre-admission failure, retry must not erase an admitted obligation.
    for failure in [
        finish_without_external_wake(store.skip_received()).unwrap_err(),
        finish_without_external_wake(store.finish()).unwrap_err(),
    ] {
        assert!(failure.is_incomplete_assertion());
        assert_eq!(failure.origin().line(), origin_line);
    }
    assert_eq!(polls.load(Ordering::SeqCst), stopped_at);
    let panic = catch_unwind(AssertUnwindSafe(|| drop(store)))
        .expect_err("Drop must enforce the admitted skip assertion");
    assert!(panic.downcast_ref::<String>().unwrap().starts_with(
        "TestStore teardown failed: TestStore incomplete assertion for skip_received at "
    ));
    drop(task);
    assert_eq!(polls.load(Ordering::SeqCst), stopped_at);
    assert_eq!(
        Arc::strong_count(&polls),
        1,
        "teardown releases finite cleanup"
    );
}

#[test]
fn finished_store_rejects_owned_actions_without_reopening_assertion_debt() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct OwnedAction(Arc<AtomicUsize>);

    impl Drop for OwnedAction {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    struct NeverReduce;

    impl Reducer for NeverReduce {
        type State = ();
        type Action = OwnedAction;

        fn reduce(
            &mut self,
            (): &mut (),
            _: &OwnedAction,
            _: &mut ReduceContext<'_>,
        ) -> Effect<OwnedAction> {
            panic!("a finished store must not reduce a rejected action");
        }
    }

    let drops = Arc::new(AtomicUsize::new(0));
    let mut store = TestStore::new((), NeverReduce);
    finish_without_external_wake(store.finish()).unwrap();
    let mut updates = 0;
    for expected_drops in 1..=2 {
        let origin_line = line!() + 1;
        let send = store.send_with(OwnedAction(Arc::clone(&drops)), |()| updates += 1);
        let error = finish_without_external_wake(send).expect_err("closed admission");
        assert_eq!(error.origin().file(), file!());
        assert_eq!(error.origin().line(), origin_line);
        assert!(format!("{error:?}").contains("StoreUnavailable"));
        assert!(
            error
                .to_string()
                .starts_with("production Store unavailable during test operation at ")
        );
        assert!(std::error::Error::source(&error).is_none());
        assert!(!error.is_incomplete_assertion());
        assert!(error.drive_error().is_none());
        assert_eq!(drops.load(Ordering::SeqCst), expected_drops);
        assert_eq!(updates, expected_drops);
        finish_without_external_wake(store.finish()).unwrap();
    }
    drop(store);
    assert_eq!(drops.load(Ordering::SeqCst), 2);
}

#[test]
fn resumed_advance_budget_failure_preserves_the_gated_action_for_receive_retry() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct DelayedOutput(GatedWork);

    impl Reducer for DelayedOutput {
        type State = u32;
        type Action = GatedWorkAction;

        fn reduce(
            &mut self,
            state: &mut u32,
            action: &GatedWorkAction,
            context: &mut ReduceContext<'_>,
        ) -> Effect<GatedWorkAction> {
            if matches!(action, GatedWorkAction::Pending) {
                *state += 1;
                return Effect::none();
            }
            Effect::merge([
                self.0.reduce(&mut (), action, context),
                Effect::send_after(Duration::from_secs(1), GatedWorkAction::Pending),
            ])
        }
    }

    let (control, gate) = rca_test::controlled_gate();
    let polls = Arc::new(AtomicUsize::new(0));
    let mut store = TestStore::with_step_budget(
        0,
        DelayedOutput(GatedWork {
            gate: Some(gate),
            polls: Arc::clone(&polls),
        }),
        7,
    );
    let task = finish_without_external_wake(store.send(GatedWorkAction::Start)).unwrap();
    finish_without_external_wake(control.wait_until_waiting()).unwrap();
    finish_without_external_wake(store.advance(Duration::from_secs(10))).unwrap();
    let advanced = rca_core::MonotonicInstant::from_elapsed(Duration::from_secs(10));
    assert_eq!(store.now(), advanced);
    control.release().unwrap();

    let origin_line = line!() + 1;
    let receive = store.receive_with(
        |_| panic!("failed settling must precede matching"),
        |_| panic!("failed settling must precede expected-state updates"),
    );
    let error = finish_without_external_wake(receive).expect_err("resumed advance exceeds budget");
    assert_eq!(error.origin().file(), file!());
    assert_eq!(error.origin().line(), origin_line);
    let drive = error.drive_error().expect("typed scheduler cause");
    assert!(drive.is_step_budget_exhausted());
    assert_eq!((drive.polls(), drive.step_budget()), (7, 7));
    assert_eq!(polls.load(Ordering::Relaxed), 7);
    assert_eq!(store.now(), advanced);
    assert!(!task.is_finished());

    let received = finish_without_external_wake(store.receive_with(
        |action| matches!(action, GatedWorkAction::Pending),
        |state| *state = 1,
    ))
    .expect("retry retains the original output and admits its assertion exactly once");
    assert_eq!(polls.load(Ordering::Relaxed), 8);
    finish_without_external_wake(store.assert_state_eq(1)).unwrap();
    assert_eq!(store.now(), advanced);
    finish_without_external_wake(received.finish()).unwrap();
    finish_without_external_wake(task.finish()).unwrap();
    finish_without_external_wake(store.finish()).unwrap();
}
