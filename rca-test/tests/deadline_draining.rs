//! Checks virtual deadline order, separate work budgets and host closure during a drain.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::Poll;
use std::time::Duration;

use futures_executor::block_on;
use rca_core::testing::AssertionGate;
use rca_core::{Effect, MonotonicInstant, ReduceContext, Reducer, Store};
use rca_test::{DeterministicRuntime, controlled_gate};

/// Builds literal expected times without reading the scheduler's clock.
fn instant(seconds: u64) -> MonotonicInstant {
    MonotonicInstant::from_elapsed(Duration::from_secs(seconds))
}

#[test]
fn finite_recurring_and_one_shot_deadlines_drain_in_sequence_at_exact_budgets() {
    let host = DeterministicRuntime::with_now_and_step_budget(instant(10), 6);
    let events = Arc::new(Mutex::new(Vec::new()));
    let clock = host.runtime().clock();
    let output = Arc::clone(&events);
    host.spawn_task(Box::pin(async move {
        for _ in 0..3 {
            clock.sleep(Duration::from_secs(2)).await;
            output.lock().unwrap().push(("recurring", clock.now()));
        }
    }));
    let clock = host.runtime().clock();
    let output = Arc::clone(&events);
    host.spawn_task(Box::pin(async move {
        clock.sleep(Duration::from_secs(3)).await;
        output.lock().unwrap().push(("once", clock.now()));
    }));
    let report = block_on(host.drain_deadlines(4)).unwrap();
    assert_eq!(report.polls(), 6);
    assert_eq!(report.deadline_batches(), 4);
    assert_eq!(report.current_time(), instant(16));
    assert_eq!(
        *events.lock().unwrap(),
        [
            ("recurring", instant(12)),
            ("once", instant(13)),
            ("recurring", instant(14)),
            ("recurring", instant(16)),
        ]
    );
    host.assert_no_pending_time().unwrap();
}

#[test]
fn ready_only_stall_does_not_drain_future_deadlines() {
    let host = DeterministicRuntime::new();
    let clock = host.runtime().clock();
    let finished = Arc::new(AtomicUsize::new(0));
    let output = Arc::clone(&finished);
    host.spawn_task(Box::pin(async move {
        clock.sleep(Duration::from_secs(1)).await;
        output.fetch_add(1, Ordering::Relaxed);
    }));
    let stalled = host.run_until_stalled().unwrap();
    assert_eq!(stalled.deadline_batches(), 0);
    assert_eq!(host.now(), MonotonicInstant::ZERO);
    assert_eq!(finished.load(Ordering::Relaxed), 0);
    assert_eq!(
        block_on(host.drain_deadlines(1))
            .unwrap()
            .deadline_batches(),
        1
    );
    assert_eq!(finished.load(Ordering::Relaxed), 1);
}

#[test]
fn temporal_assertion_checks_only_registered_timers_without_hidden_polling() {
    let host = DeterministicRuntime::new();
    let polls = Arc::new(AtomicUsize::new(0));
    let output = Arc::clone(&polls);
    host.spawn_task(Box::pin(async move {
        output.fetch_add(1, Ordering::Relaxed);
        std::future::pending::<()>().await;
    }));
    host.assert_no_pending_time().unwrap();
    assert_eq!(polls.load(Ordering::Relaxed), 0);
    let mut wait = host.sleep_until(instant(2));
    assert_eq!(
        wait.as_mut()
            .poll(&mut std::task::Context::from_waker(std::task::Waker::noop())),
        Poll::Pending
    );
    let error = host.assert_no_pending_time().unwrap_err();
    assert!(error.is_pending_time());
    assert_eq!(error.pending_sleeps()[0].deadline(), instant(2));
    assert_eq!(error.current_time(), MonotonicInstant::ZERO);
    assert_eq!(polls.load(Ordering::Relaxed), 0);
    drop(wait);
    host.assert_no_pending_time().unwrap();
    assert!(block_on(host.drain_deadlines(0)).unwrap_err().is_blocked());
}

#[test]
fn equal_deadlines_share_one_batch_and_keep_registration_order() {
    let host = DeterministicRuntime::with_step_budget(4);
    let events = Arc::new(Mutex::new(Vec::new()));
    for id in [1, 2] {
        let clock = host.runtime().clock();
        let output = Arc::clone(&events);
        host.spawn_task(Box::pin(async move {
            clock.sleep_until(instant(2)).await;
            output.lock().unwrap().push(id);
        }));
    }
    let report = block_on(host.drain_deadlines(1)).unwrap();
    assert_eq!(report.polls(), 4);
    assert_eq!(report.deadline_batches(), 1);
    assert_eq!(*events.lock().unwrap(), [1, 2]);
}

#[test]
fn infinite_recurrence_exhausts_deadline_budget_without_resetting_poll_accounting() {
    let host = DeterministicRuntime::with_step_budget(20);
    let clock = host.runtime().clock();
    host.spawn_task(Box::pin(async move {
        loop {
            clock.sleep(Duration::from_secs(1)).await;
        }
    }));
    let error = block_on(host.drain_deadlines(3)).unwrap_err();
    assert!(error.is_deadline_budget_exhausted());
    assert!(!error.is_step_budget_exhausted());
    assert_eq!(error.polls(), 4);
    assert_eq!(error.deadline_batches(), 3);
    assert_eq!(error.deadline_budget(), Some(3));
    assert_eq!(error.current_time(), instant(3));
    assert_eq!(error.pending_sleeps()[0].deadline(), instant(4));
    assert_eq!(error.live_task_ids(), [0]);
}

#[test]
fn self_waking_work_exhausts_total_poll_budget_without_advancing_time() {
    let host = DeterministicRuntime::with_step_budget(3);
    host.spawn_task(Box::pin(std::future::poll_fn(|cx| {
        cx.waker().wake_by_ref();
        Poll::Pending
    })));
    let error = block_on(host.drain_deadlines(20)).unwrap_err();
    assert!(error.is_step_budget_exhausted());
    assert_eq!(error.polls(), 3);
    assert_eq!(error.deadline_batches(), 0);
    assert_eq!(error.ready_task_ids(), [0]);
    assert_eq!(host.now(), MonotonicInstant::ZERO);
    let ready_only_error = host.run_until_stalled().unwrap_err();
    assert!(ready_only_error.is_step_budget_exhausted());
    assert_eq!(ready_only_error.deadline_budget(), None);
    assert!(
        ready_only_error
            .to_string()
            .starts_with("deterministic scheduler exhausted 3 polls")
    );
}

#[test]
fn poll_budget_is_not_reset_between_future_deadlines() {
    let host = DeterministicRuntime::with_step_budget(3);
    let clock = host.runtime().clock();
    host.spawn_task(Box::pin(async move {
        for _ in 0..3 {
            clock.sleep(Duration::from_secs(1)).await;
        }
    }));
    let error = block_on(host.drain_deadlines(10)).unwrap_err();
    assert!(error.is_step_budget_exhausted());
    assert_eq!(error.polls(), 3);
    assert_eq!(error.deadline_batches(), 3);
    assert_eq!(error.current_time(), instant(3));
    assert_eq!(error.ready_task_ids(), [0]);
    assert!(error.pending_sleeps().is_empty());
    assert_eq!(block_on(host.drain_deadlines(0)).unwrap().polls(), 1);
}

#[test]
fn blocked_gate_fails_immediately_and_remains_recoverable() {
    let host = DeterministicRuntime::new();
    let (control, gate) = controlled_gate();
    let finished = Arc::new(AtomicUsize::new(0));
    let output = Arc::clone(&finished);
    host.spawn_task(Box::pin(async move {
        gate.await.unwrap();
        output.fetch_add(1, Ordering::Relaxed);
    }));
    let error = block_on(host.drain_deadlines(2)).unwrap_err();
    assert!(error.is_blocked());
    assert!(!error.is_step_budget_exhausted());
    let display = error.to_string();
    assert!(display.starts_with("deterministic scheduler blocked on unfinished tasks"));
    assert!(display.ends_with(&error.backtrace().to_string()));
    assert!(std::error::Error::source(&error).is_none());
    assert_eq!(error.live_task_ids(), [0]);
    assert_eq!(error.polls(), 1);
    assert!(error.pending_sleeps().is_empty());
    assert_eq!(finished.load(Ordering::Relaxed), 0);
    control.release().unwrap();
    block_on(host.drain_deadlines(0)).unwrap();
    assert_eq!(finished.load(Ordering::Relaxed), 1);
    assert_eq!(error.to_string(), display);
}

/// Starts one delayed response used to check assertion-gate blocking.
#[derive(Debug)]
enum Action {
    Start,
    Response,
}

/// Records both commits so a due response cannot pass an unreleased assertion gate.
struct Feature;

impl Reducer for Feature {
    type State = Arc<Mutex<Vec<&'static str>>>;
    type Action = Action;

    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &Action,
        _: &mut ReduceContext<'_>,
    ) -> Effect<Action> {
        match action {
            Action::Start => {
                state.lock().unwrap().push("start");
                Effect::send_after(Duration::from_secs(2), Action::Response)
            }
            Action::Response => {
                state.lock().unwrap().push("response");
                Effect::none()
            }
        }
    }
}

#[test]
fn assertion_parked_actor_is_not_released_or_misreported_as_completed() {
    let host = DeterministicRuntime::new();
    let gate = AssertionGate::new();
    let events = Arc::new(Mutex::new(Vec::new()));
    let store = Store::builder(Arc::clone(&events), Feature, host.runtime())
        .assertion_gate(gate.clone())
        .build()
        .unwrap();
    let receipt = store.try_send(Action::Start).unwrap();
    let task = block_on(host.run(async { receipt.await }))
        .unwrap()
        .unwrap();
    let error = block_on(host.drain_deadlines(2)).unwrap_err();
    assert!(error.is_blocked());
    assert_eq!(error.current_time(), instant(2));
    assert!(gate.has_pending());
    assert_eq!(*events.lock().unwrap(), ["start"]);
    assert!(gate.release_pending());
    host.run_until_stalled().unwrap();
    assert_eq!(*events.lock().unwrap(), ["start", "response"]);
    assert!(task.is_finished());
    block_on(host.run(store.shutdown())).unwrap().unwrap();
}

#[test]
fn dropped_host_does_not_gain_authority_from_an_unpolled_drain() {
    let host = DeterministicRuntime::new();
    let drain = host.drain_deadlines(2);
    drop(host);
    let error = block_on(drain).unwrap_err();
    assert!(error.is_host_closed());
    assert_eq!(error.polls(), 0);
}

#[test]
fn host_closure_during_drain_discards_detached_tasks_without_resurrection() {
    let host = DeterministicRuntime::new();
    let clock = host.runtime().clock();
    let wait = clock.sleep_until(MonotonicInstant::MAX);
    let resource = Arc::new(());
    let weak_resource = Arc::downgrade(&resource);
    let host_slot = Arc::new(Mutex::new(Some(host)));
    let drain = host_slot
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .drain_deadlines(2);
    let slot = Arc::clone(&host_slot);
    host_slot
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .spawn_task(Box::pin(async move {
            drop(slot.lock().unwrap().take());
            wait.await;
            drop(resource);
        }));
    let later_polls = Arc::new(AtomicUsize::new(0));
    let output = Arc::clone(&later_polls);
    host_slot
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .spawn_task(Box::pin(async move {
            output.fetch_add(1, Ordering::Relaxed);
        }));
    let error = block_on(drain).unwrap_err();
    assert!(error.is_host_closed());
    assert_eq!(error.polls(), 1);
    assert!(error.live_task_ids().is_empty());
    assert!(weak_resource.upgrade().is_none());
    assert_eq!(later_polls.load(Ordering::Relaxed), 0);
    drop(clock);
}

#[test]
fn host_closure_after_deadline_progress_preserves_failure_accounting() {
    let host = DeterministicRuntime::new();
    let clock = host.runtime().clock();
    let first_wait = clock.sleep_until(instant(1));
    let closed_wait = clock.sleep_until(MonotonicInstant::MAX);
    let resource = Arc::new(());
    let weak_resource = Arc::downgrade(&resource);
    let host_slot = Arc::new(Mutex::new(Some(host)));
    let drain = host_slot
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .drain_deadlines(2);
    let slot = Arc::clone(&host_slot);
    host_slot
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .spawn_task(Box::pin(async move {
            first_wait.await;
            drop(slot.lock().unwrap().take());
            closed_wait.await;
            drop(resource);
        }));
    let error = block_on(drain).unwrap_err();
    assert!(error.is_host_closed());
    assert_eq!(error.polls(), 2);
    assert_eq!(error.deadline_batches(), 1);
    assert_eq!(error.current_time(), instant(1));
    assert!(weak_resource.upgrade().is_none());
    assert!(error.live_task_ids().is_empty());
    assert!(error.pending_sleeps().is_empty());
}

#[test]
fn dropping_unpolled_drain_does_not_poll_tasks_or_advance_time() {
    let host = DeterministicRuntime::new();
    let polls = Arc::new(AtomicUsize::new(0));
    let output = Arc::clone(&polls);
    host.spawn_task(Box::pin(async move {
        output.fetch_add(1, Ordering::Relaxed);
    }));
    drop(host.drain_deadlines(2));
    assert_eq!(polls.load(Ordering::Relaxed), 0);
    assert_eq!(host.now(), MonotonicInstant::ZERO);
    block_on(host.drain_deadlines(0)).unwrap();
    assert_eq!(polls.load(Ordering::Relaxed), 1);
}

#[test]
fn maximum_deadline_and_zero_budget_have_finite_boundary_behaviour() {
    let host = DeterministicRuntime::with_now_and_step_budget(
        MonotonicInstant::from_elapsed(Duration::new(u64::MAX, 999_999_998)),
        2,
    );
    let clock = host.runtime().clock();
    host.spawn_task(Box::pin(async move {
        clock.sleep(Duration::from_nanos(1)).await;
    }));
    let error = block_on(host.drain_deadlines(0)).unwrap_err();
    assert!(error.is_deadline_budget_exhausted());
    assert_eq!(error.deadline_batches(), 0);
    assert_eq!(
        error.current_time(),
        MonotonicInstant::from_elapsed(Duration::new(u64::MAX, 999_999_998))
    );
    let report = block_on(host.drain_deadlines(1)).unwrap();
    assert_eq!(report.current_time(), MonotonicInstant::MAX);
    assert_eq!(report.deadline_batches(), 1);
    host.assert_no_pending_time().unwrap();
    assert_eq!(block_on(host.drain_deadlines(0)).unwrap().polls(), 0);
}
