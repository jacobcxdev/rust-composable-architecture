//! Clock-only providers, seeded time and owned waits keep their distinct lifetimes.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Wake, Waker};
use std::time::Duration;

use futures_executor::block_on;
use rca_core::{
    Clock, ClockCapabilities, ClockDriver, Effect, MonotonicInstant, ReduceContext, Reducer,
    RuntimeFuture, Store, UnsupportedClockTolerance,
};
use rca_test::DeterministicRuntime;

/// Expresses literal expected timestamps in seconds from the shared origin.
fn instant(seconds: u64) -> MonotonicInstant {
    MonotonicInstant::from_elapsed(Duration::from_secs(seconds))
}

/// Polls manually after each clock change without relying on wake delivery.
fn poll(wait: &mut RuntimeFuture) -> Poll<()> {
    wait.as_mut().poll(&mut Context::from_waker(Waker::noop()))
}

/// A manually polled clock that waits until the end of the requested slack.
///
/// The shared value is its current time. It does not register wakers, spawn tasks
/// or yield; only the explicit polling tests below may use this fixture.
#[derive(Debug)]
struct CoalescingClock(Arc<Mutex<MonotonicInstant>>);

impl ClockDriver for CoalescingClock {
    fn now(&self) -> MonotonicInstant {
        *self.0.lock().unwrap()
    }

    fn capabilities(&self) -> ClockCapabilities {
        ClockCapabilities {
            resolution: Some(Duration::from_millis(1)),
            minimum_tolerance: Some(Duration::from_secs(2)),
        }
    }

    fn sleep_until(&self, deadline: MonotonicInstant) -> RuntimeFuture {
        let now = Arc::clone(&self.0);
        Box::pin(std::future::poll_fn(move |_| {
            if *now.lock().unwrap() >= deadline {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        }))
    }

    fn sleep_until_with_tolerance(
        &self,
        deadline: MonotonicInstant,
        tolerance: Duration,
    ) -> Result<RuntimeFuture, UnsupportedClockTolerance> {
        Ok(self.sleep_until(deadline.saturating_add(tolerance)))
    }
}

#[test]
fn standalone_clock_waits_without_a_spawner_and_applies_declared_tolerance_floor() {
    let now = Arc::new(Mutex::new(instant(10)));
    let clock = Clock::new(CoalescingClock(Arc::clone(&now)));
    assert_eq!(
        clock.capabilities().resolution,
        Some(Duration::from_millis(1))
    );
    let mut exact = clock.sleep(Duration::from_secs(3));
    let mut tolerant = clock
        .sleep_until_with_tolerance(instant(13), Duration::from_secs(1))
        .unwrap();
    *now.lock().unwrap() = instant(13);
    assert_eq!(poll(&mut exact), Poll::Ready(()));
    assert_eq!(poll(&mut tolerant), Poll::Pending);
    *now.lock().unwrap() = instant(14);
    assert_eq!(poll(&mut tolerant), Poll::Pending);
    *now.lock().unwrap() = instant(15);
    assert_eq!(poll(&mut tolerant), Poll::Ready(()));
    let mut larger = clock
        .sleep_until_with_tolerance(instant(16), Duration::from_secs(4))
        .unwrap();
    *now.lock().unwrap() = instant(19);
    assert_eq!(poll(&mut larger), Poll::Pending);
    *now.lock().unwrap() = instant(20);
    assert_eq!(poll(&mut larger), Poll::Ready(()));
    let mut elapsed = clock
        .sleep_until_with_tolerance(instant(16), Duration::from_secs(100))
        .unwrap();
    assert_eq!(poll(&mut elapsed), Poll::Ready(()));
}

/// Exposes default metadata and rejects any attempted future-deadline wait.
///
/// Its tests use elapsed waits only; this is not a general-purpose clock.
#[derive(Debug)]
struct ExactClock;

impl ClockDriver for ExactClock {
    fn now(&self) -> MonotonicInstant {
        instant(10)
    }

    fn sleep_until(&self, deadline: MonotonicInstant) -> RuntimeFuture {
        Box::pin(async move {
            assert!(deadline <= instant(10));
        })
    }
}

#[test]
fn undeclared_metadata_is_unknown_and_tolerance_is_explicitly_unsupported() {
    let clock = Clock::new(ExactClock);
    assert_eq!(clock.capabilities().resolution, None);
    assert_eq!(clock.capabilities().minimum_tolerance, None);
    assert!(
        clock
            .sleep_until_with_tolerance(instant(12), Duration::ZERO)
            .is_err()
    );
    assert_eq!(poll(&mut clock.sleep_until(instant(9))), Poll::Ready(()));
}

#[test]
fn runtime_adapter_forwards_deterministic_metadata_and_tolerance_without_early_wake() {
    let host = DeterministicRuntime::with_now(instant(10));
    let clock = host.runtime().clock();
    assert_eq!(
        clock.capabilities().resolution,
        Some(Duration::from_nanos(1))
    );
    assert_eq!(clock.capabilities().minimum_tolerance, Some(Duration::ZERO));
    let mut wait = clock
        .sleep_until_with_tolerance(instant(12), Duration::from_secs(10))
        .unwrap();
    assert_eq!(poll(&mut wait), Poll::Pending);
    host.advance_to(instant(11)).unwrap();
    assert_eq!(poll(&mut wait), Poll::Pending);
    host.advance_to(instant(12)).unwrap();
    assert_eq!(poll(&mut wait), Poll::Ready(()));
}

#[test]
fn absolute_advance_visits_newly_registered_intermediate_deadlines_from_a_seed() {
    let host = DeterministicRuntime::with_now_and_step_budget(instant(10), 30);
    let clock = host.runtime().clock();
    let events = Arc::new(Mutex::new(Vec::new()));
    let output = Arc::clone(&events);
    host.spawn_task(Box::pin(async move {
        clock.sleep(Duration::from_secs(2)).await;
        output.lock().unwrap().push(clock.now());
        clock.sleep(Duration::from_secs(1)).await;
        output.lock().unwrap().push(clock.now());
    }));
    assert!(events.lock().unwrap().is_empty());
    let report = host.advance_to(instant(15)).unwrap();
    assert_eq!(*events.lock().unwrap(), [instant(12), instant(13)]);
    assert_eq!(report.current_time(), instant(15));
    assert_eq!(host.step_budget(), 30);
}

#[test]
fn past_absolute_target_drains_ready_work_but_never_moves_time_backwards() {
    let host = DeterministicRuntime::with_now(instant(10));
    let events = Arc::new(Mutex::new(Vec::new()));
    let output = Arc::clone(&events);
    let clock = host.runtime().clock();
    host.spawn_task(Box::pin(async move {
        clock.sleep_until(instant(9)).await;
        output.lock().unwrap().push(clock.now());
    }));
    let report = host.advance_to(instant(5)).unwrap();
    assert_eq!(report.current_time(), instant(10));
    assert_eq!(*events.lock().unwrap(), [instant(10)]);
    assert_eq!(
        host.advance(Duration::from_secs(1)).unwrap().current_time(),
        instant(11)
    );
}

#[test]
fn seeded_wait_is_invocation_fixed_and_saturates_without_signed_wrap() {
    let host = DeterministicRuntime::with_now(instant(10));
    let clock = host.runtime().clock();
    let mut wait = clock.sleep(Duration::from_secs(2));
    host.advance_to(instant(11)).unwrap();
    assert_eq!(poll(&mut wait), Poll::Pending);
    assert_eq!(host.pending_sleeps()[0].deadline(), instant(12));
    host.advance_to(instant(12)).unwrap();
    assert_eq!(poll(&mut wait), Poll::Ready(()));
    let mut saturated = clock.sleep(Duration::MAX);
    assert_eq!(poll(&mut saturated), Poll::Pending);
    assert_eq!(host.pending_sleeps()[0].deadline(), MonotonicInstant::MAX);
    host.advance_to(MonotonicInstant::MAX).unwrap();
    assert_eq!(poll(&mut saturated), Poll::Ready(()));
    assert_eq!(
        host.advance(Duration::MAX).unwrap().current_time(),
        MonotonicInstant::MAX
    );
}

#[test]
fn seeded_elapsed_wait_is_ready_and_drop_cancels_registered_wait() {
    let host = DeterministicRuntime::with_now(instant(10));
    let clock = host.runtime().clock();
    assert_eq!(poll(&mut clock.sleep_until(instant(9))), Poll::Ready(()));
    let mut wait = clock.sleep(Duration::from_secs(2));
    assert_eq!(poll(&mut wait), Poll::Pending);
    assert_eq!(host.pending_sleeps().len(), 1);
    drop(wait);
    assert!(host.pending_sleeps().is_empty());
}

#[test]
fn owned_unpolled_wait_does_not_complete_when_all_handles_are_dropped() {
    let host = DeterministicRuntime::with_now(instant(10));
    let runtime = host.runtime();
    let clock = runtime.clock();
    let mut wait = clock.sleep_until(instant(12));
    drop(clock);
    drop(runtime);
    drop(host);
    assert_eq!(poll(&mut wait), Poll::Pending);
    drop(wait);
}

#[test]
fn owned_registered_wait_does_not_complete_when_all_handles_are_dropped() {
    let host = DeterministicRuntime::with_now(instant(10));
    let runtime = host.runtime();
    let clock = runtime.clock();
    let mut wait = clock.sleep_until(instant(12));
    assert_eq!(poll(&mut wait), Poll::Pending);
    drop(clock);
    drop(runtime);
    drop(host);
    assert_eq!(poll(&mut wait), Poll::Pending);
    drop(wait);
}

#[test]
fn owned_elapsed_and_zero_waits_complete_after_all_handles_are_dropped() {
    let host = DeterministicRuntime::with_now(instant(10));
    let runtime = host.runtime();
    let clock = runtime.clock();
    let mut past = clock.sleep_until(instant(9));
    let mut zero = clock.sleep(Duration::ZERO);
    let mut advanced = clock.sleep_until(instant(12));
    assert_eq!(poll(&mut advanced), Poll::Pending);
    host.advance_to(instant(12)).unwrap();
    drop(clock);
    drop(runtime);
    drop(host);
    assert_eq!(poll(&mut past), Poll::Ready(()));
    assert_eq!(poll(&mut zero), Poll::Ready(()));
    assert_eq!(poll(&mut advanced), Poll::Ready(()));
}

/// Owns a wake counter so weak references can detect retained wakers.
struct ProbeWake(AtomicUsize);

impl Wake for ProbeWake {
    fn wake(self: Arc<Self>) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}

#[test]
fn owned_wait_releases_wakers_and_host_tasks_without_retaining_authority() {
    let host = DeterministicRuntime::with_now(instant(10));
    let runtime = host.runtime();
    let clock = runtime.clock();
    let mut wait = clock.sleep_until(instant(12));
    let wake = Arc::new(ProbeWake(AtomicUsize::new(0)));
    let weak_wake = Arc::downgrade(&wake);
    let waker = Waker::from(wake);
    assert_eq!(
        wait.as_mut().poll(&mut Context::from_waker(&waker)),
        Poll::Pending
    );
    drop(waker);
    assert!(weak_wake.upgrade().is_some());
    let task_resource = Arc::new(());
    let weak_task_resource = Arc::downgrade(&task_resource);
    let task_wait = clock.sleep_until(instant(13));
    host.spawn_task(Box::pin(async move {
        task_wait.await;
        drop(task_resource);
    }));
    host.run_until_stalled().unwrap();
    drop(clock);
    drop(runtime);
    drop(host);
    assert!(weak_task_resource.upgrade().is_none());
    assert!(weak_wake.upgrade().is_none());
    let closed_wake = Arc::new(ProbeWake(AtomicUsize::new(0)));
    let weak_closed_wake = Arc::downgrade(&closed_wake);
    let closed_waker = Waker::from(closed_wake);
    assert_eq!(
        wait.as_mut().poll(&mut Context::from_waker(&closed_waker)),
        Poll::Pending
    );
    drop(closed_waker);
    assert!(weak_closed_wake.upgrade().is_none());
    drop(wait);
}

/// Identifies the start, clock-driven response and actor-driven response.
#[derive(Debug)]
enum Action {
    Start,
    Client,
    Actor,
}

/// Records each action's observed time using the supplied clock.
struct Feature(Clock);

impl Reducer for Feature {
    type State = Arc<Mutex<Vec<(&'static str, MonotonicInstant)>>>;
    type Action = Action;

    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &Action,
        _: &mut ReduceContext<'_>,
    ) -> Effect<Action> {
        match action {
            Action::Start => {
                state.lock().unwrap().push(("start", self.0.now()));
                let wait = self.0.sleep(Duration::from_secs(2));
                Effect::merge([
                    Effect::future(async move {
                        wait.await;
                        Some(Action::Client)
                    }),
                    Effect::send_after(Duration::from_secs(3), Action::Actor),
                ])
            }
            Action::Client | Action::Actor => {
                let label = if matches!(action, Action::Client) {
                    "client"
                } else {
                    "actor"
                };
                state.lock().unwrap().push((label, self.0.now()));
                Effect::none()
            }
        }
    }
}

#[test]
fn seeded_actor_and_clock_client_share_origin_without_inline_polling() {
    let host = DeterministicRuntime::with_now(instant(10));
    let runtime = host.runtime();
    let events = Arc::new(Mutex::new(Vec::new()));
    let store = Store::new(Arc::clone(&events), Feature(runtime.clock()), runtime);
    let receipt = store.try_send(Action::Start).unwrap();
    assert!(events.lock().unwrap().is_empty());
    let task = block_on(host.run(async { receipt.await }))
        .unwrap()
        .unwrap();
    host.advance_to(instant(15)).unwrap();
    assert_eq!(
        *events.lock().unwrap(),
        [
            ("start", instant(10)),
            ("client", instant(12)),
            ("actor", instant(13))
        ]
    );
    assert!(task.is_finished());
    block_on(host.run(store.shutdown())).unwrap().unwrap();
}

#[test]
fn retained_seeded_clock_does_not_retain_host_spawning_authority() {
    let host = DeterministicRuntime::with_now(instant(10));
    let runtime = host.runtime();
    let clock = runtime.clock();
    drop(host);
    let events = Arc::new(Mutex::new(Vec::new()));
    let store = Store::new(Arc::clone(&events), Feature(clock.clone()), runtime);
    assert!(store.is_closed());
    assert!(events.lock().unwrap().is_empty());
    assert_eq!(clock.now(), instant(10));
}
