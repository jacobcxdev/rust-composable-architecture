//! Checks clock fixtures through explicit polls and issue reports, without wall-clock delays.

#![cfg(feature = "test-util")]

#[expect(dead_code, reason = "manual actor polling needs only captured tasks")]
mod support;

use rca_core::{
    Clock, DependencyKey, DependencyValues, Effect, Issue, IssueReporter, MonotonicInstant,
    ReduceContext, Reducer, RuntimeFuture, Store, UnsupportedClockTolerance,
    testing::IssueRecorder,
};
use rca_core::{ClockDriver, ForbiddenClock, ForbiddenClockUsed, ImmediateClock};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Wake, Waker};
use std::time::Duration;

fn instant(seconds: u64) -> MonotonicInstant {
    MonotonicInstant::from_elapsed(Duration::from_secs(seconds))
}

/// Polls once without scheduling another poll after a wake.
fn poll(wait: &mut RuntimeFuture) -> Poll<()> {
    wait.as_mut().poll(&mut Context::from_waker(Waker::noop()))
}

/// Counts the wake that lets a wait yield after it advances fixture time.
#[derive(Default)]
struct WakeCount(AtomicUsize);
impl Wake for WakeCount {
    fn wake(self: Arc<Self>) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
    fn wake_by_ref(self: &Arc<Self>) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}

#[test]
fn immediate_debug_writer_panic_poisoning_remains_visible_to_clock_operations() {
    use std::fmt::Write as _;

    struct PanicOnInstant;
    impl std::fmt::Write for PanicOnInstant {
        fn write_str(&mut self, s: &str) -> std::fmt::Result {
            assert!(!s.contains("MonotonicInstant"), "diagnostic writer failed");
            Ok(())
        }
    }

    let fixture = ImmediateClock::new(instant(3));
    let formatting = std::panic::catch_unwind(|| write!(PanicOnInstant, "{fixture:?}"));
    formatting.expect_err("the diagnostic writer must panic while formatting the instant");
    std::panic::catch_unwind(|| fixture.now())
        .expect_err("reading time must propagate the poisoned lock");
    let mut wait = fixture.sleep_until(instant(4));
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| poll(&mut wait)))
        .expect_err("polling a wait must propagate the poisoned lock");
}

#[test]
fn diagnostics_timer_formatting_observes_state_without_using_the_clock() {
    use futures_util::FutureExt as _;

    let recorder = IssueRecorder::new();
    let fixture = ForbiddenClock::new("diagnostic timer", instant(5), recorder.reporter());
    let clock = Clock::new(fixture.clone());
    let error = rca_core::ClockTimer::new(clock.clone(), Duration::ZERO).unwrap_err();
    assert_eq!(error.to_string(), "timer interval must exceed zero");
    let mut timer = rca_core::ClockTimer::new(clock, Duration::from_secs(2)).unwrap();
    drop(timer.next());
    let idle = format!("{timer:?}");
    assert!(idle.starts_with("ClockTimer {"));
    assert!(idle.contains("interval: 2s"));
    assert!(idle.contains("deadline: None"));
    assert!(idle.contains("waiting: false"));
    assert!(idle.contains("finished: false"));
    fixture.assert_unused().unwrap();
    assert!(recorder.snapshot().is_empty());

    let mut next = Box::pin(timer.next());
    assert!(
        next.as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_pending()
    );
    drop(next);
    let before = recorder.snapshot();
    assert_eq!(before.len(), 2); // The demand poll reads time and creates one wait.
    let pending = format!("{timer:?}");
    assert!(pending.contains("deadline: Some("));
    assert!(pending.contains("waiting: true"));
    assert!(pending.contains("finished: false"));
    assert_eq!(recorder.snapshot(), before);
    assert_eq!(
        fixture.assert_unused().unwrap_err().to_string(),
        "forbidden clock was used"
    );

    timer.cancel();
    let finished = format!("{timer:?}");
    assert!(finished.contains("deadline: None"));
    assert!(finished.contains("waiting: false"));
    assert!(finished.contains("finished: true"));
    assert_eq!(
        timer
            .next()
            .now_or_never()
            .expect("cancelled timer must resolve without clock progress"),
        None,
    );
    assert_eq!(recorder.snapshot(), before);
}

#[test]
fn shared_timer_resumes_abandoned_wait_and_reanchors_after_observed_tick() {
    let fixture = ImmediateClock::new(instant(3));
    let mut timer =
        rca_core::ClockTimer::new(Clock::new(fixture.clone()), Duration::from_secs(2)).unwrap();

    let mut abandoned = Box::pin(timer.next());
    assert!(
        abandoned
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_pending()
    );
    assert_eq!(fixture.now(), instant(5));
    drop(abandoned);

    let mut resumed = Box::pin(timer.next());
    assert_eq!(
        resumed
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop())),
        Poll::Ready(Some(instant(5)))
    );
    drop(resumed);

    let mut subsequent = Box::pin(timer.next());
    assert!(
        subsequent
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_pending()
    );
    assert_eq!(fixture.now(), instant(7));
    assert_eq!(
        subsequent
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop())),
        Poll::Ready(Some(instant(7)))
    );
}

#[test]
fn shared_timer_maximum_has_one_final_tick_or_none_on_first_demand() {
    let at_max = ImmediateClock::new(MonotonicInstant::MAX);
    let mut initially_max =
        rca_core::ClockTimer::new(Clock::new(at_max.clone()), Duration::from_secs(1)).unwrap();
    assert_eq!(
        Box::pin(initially_max.next())
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop())),
        Poll::Ready(None)
    );
    assert_eq!(
        Box::pin(initially_max.next())
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop())),
        Poll::Ready(None)
    );
    assert_eq!(at_max.now(), MonotonicInstant::MAX);

    let fixture = ImmediateClock::new(instant(1));
    let mut saturating =
        rca_core::ClockTimer::new(Clock::new(fixture.clone()), Duration::MAX).unwrap();
    let mut final_tick = Box::pin(saturating.next());
    assert!(
        final_tick
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_pending()
    );
    assert_eq!(fixture.now(), MonotonicInstant::MAX);
    assert_eq!(
        final_tick
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop())),
        Poll::Ready(Some(MonotonicInstant::MAX))
    );
    drop(final_tick);
    assert_eq!(
        Box::pin(saturating.next())
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop())),
        Poll::Ready(None)
    );
}

#[test]
fn immediate_wait_advances_only_on_poll_and_yields_after_committing_time() {
    let fixture = ImmediateClock::new(instant(3));
    let clock = Clock::new(fixture.clone());
    let unpolled = clock.sleep_until(instant(100));
    assert_eq!(clock.now(), instant(3));
    drop(unpolled);
    assert_eq!(clock.now(), instant(3));

    let mut wait = clock.sleep_until(instant(9));
    let counter = Arc::new(WakeCount::default());
    let waker = Waker::from(Arc::clone(&counter));
    assert_eq!(
        wait.as_mut().poll(&mut Context::from_waker(&waker)),
        Poll::Pending
    );
    assert_eq!(fixture.now(), instant(9));
    assert_eq!(counter.0.load(Ordering::Relaxed), 1);
    drop(wait);
    assert_eq!(clock.now(), instant(9));
}

#[test]
fn immediate_elapsed_zero_and_out_of_order_deadlines_never_rewind() {
    let fixture = ImmediateClock::new(instant(7));
    let clock = Clock::new(fixture.clone());
    let mut earlier = clock.sleep_until(instant(10));
    let mut later = clock.sleep_until(instant(20));
    assert_eq!(poll(&mut later), Poll::Pending);
    assert_eq!(poll(&mut later), Poll::Ready(()));
    assert_eq!(poll(&mut earlier), Poll::Ready(()));
    for deadline in [MonotonicInstant::ZERO, instant(19), instant(20)] {
        assert_eq!(poll(&mut clock.sleep_until(deadline)), Poll::Ready(()));
    }
    assert_eq!(poll(&mut clock.sleep(Duration::ZERO)), Poll::Ready(()));
    assert_eq!(fixture.now(), instant(20));
}

#[test]
fn immediate_relative_deadline_is_fixed_at_invocation_and_saturates() {
    let fixture = ImmediateClock::new(instant(1));
    let clock = Clock::new(fixture.clone());
    let mut relative = clock.sleep(Duration::from_secs(3));
    let mut advance = clock.sleep_until(instant(9));
    assert_eq!(poll(&mut advance), Poll::Pending);
    assert_eq!(poll(&mut relative), Poll::Ready(()));
    assert_eq!(fixture.now(), instant(9));
    let mut saturated = clock.sleep(Duration::MAX);
    assert_eq!(poll(&mut saturated), Poll::Pending);
    assert_eq!(fixture.now(), MonotonicInstant::MAX);
    assert_eq!(poll(&mut saturated), Poll::Ready(()));
    assert_eq!(poll(&mut clock.sleep(Duration::MAX)), Poll::Ready(()));
}

#[test]
fn immediate_advisory_tolerance_is_supported_without_early_completion() {
    let fixture = ImmediateClock::new(instant(2));
    let clock = Clock::new(fixture.clone());
    let capabilities = clock.capabilities();
    assert_eq!(capabilities.resolution, Some(Duration::ZERO));
    assert_eq!(capabilities.minimum_tolerance, Some(Duration::ZERO));
    let mut wait = clock
        .sleep_until_with_tolerance(instant(8), Duration::MAX)
        .unwrap();
    assert_eq!(clock.now(), instant(2));
    assert_eq!(poll(&mut wait), Poll::Pending);
    assert_eq!(clock.now(), instant(8));
    assert_eq!(poll(&mut wait), Poll::Ready(()));
    assert_eq!(
        poll(
            &mut clock
                .sleep_until_with_tolerance(instant(7), Duration::ZERO)
                .unwrap()
        ),
        Poll::Ready(())
    );
}

struct ClientClock;
impl DependencyKey for ClientClock {
    type Value = Clock;
    fn live_value() -> Clock {
        panic!("test must inject the client clock")
    }
}

#[test]
fn immediate_injected_client_runs_finite_recurrence_cooperatively() {
    let fixture = ImmediateClock::new(instant(2));
    let independent = ImmediateClock::new(instant(2));
    let mut dependencies = DependencyValues::new();
    dependencies.set::<ClientClock>(Clock::new(fixture.clone()));
    let client = dependencies.get_or_insert::<ClientClock>().clone();
    let results = Arc::new(Mutex::new(Vec::new()));
    let observed = Arc::clone(&results);
    let mut task: RuntimeFuture = Box::pin(async move {
        for _ in 0..3 {
            client.sleep(Duration::from_secs(4)).await;
            observed.lock().unwrap().push(client.now());
        }
    });
    assert!(results.lock().unwrap().is_empty());
    assert_eq!(fixture.now(), instant(2));
    for (time, completed) in [(6, 0), (10, 1), (14, 2)] {
        assert_eq!(poll(&mut task), Poll::Pending);
        assert_eq!(fixture.now(), instant(time));
        assert_eq!(results.lock().unwrap().len(), completed);
    }
    assert_eq!(poll(&mut task), Poll::Ready(()));
    assert_eq!(
        *results.lock().unwrap(),
        [instant(6), instant(10), instant(14)]
    );
    assert_eq!(independent.now(), instant(2));
}

struct Capture;
impl Reducer for Capture {
    type State = usize;
    type Action = ();
    fn reduce(
        &mut self,
        state: &mut usize,
        (): &(),
        context: &mut ReduceContext<'_>,
    ) -> Effect<()> {
        *state += 1;
        let _ = context.dependency::<ClientClock>().now();
        Effect::none()
    }
}

#[test]
fn clock_injection_does_not_poll_the_store_actor_inline() {
    let runtime = support::ManualRuntime::new();
    let recorder = IssueRecorder::new();
    let fixture = ForbiddenClock::new("actor client", instant(5), recorder.reporter());
    let store = Store::builder(0, Capture, runtime.runtime())
        .with_dependency::<ClientClock>(Clock::new(fixture.clone()))
        .build()
        .unwrap();
    assert_eq!(runtime.spawn_count(), 1);
    fixture.assert_unused().unwrap();
    let mut send = Box::pin(store.send(()));
    assert!(
        send.as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_pending()
    );
    fixture.assert_unused().unwrap();
    assert!(recorder.snapshot().is_empty());
    let mut actor = runtime.take_only_queued_task();
    assert_eq!(poll(&mut actor), Poll::Pending);
    assert_eq!(fixture.assert_unused(), Err(ForbiddenClockUsed));
    assert_eq!(recorder.snapshot().len(), 1);
    drop(send);
    drop(actor);
    drop(store);
}

#[test]
fn forbidden_operations_report_at_construction_site_before_named_fallbacks() {
    let recorder = IssueRecorder::new();
    let construction_line = line!() + 1;
    let fixture = ForbiddenClock::new("billing", instant(5), recorder.reporter());
    fixture.assert_unused().unwrap();
    assert_eq!(fixture.now(), instant(5));
    assert_eq!(fixture.capabilities().minimum_tolerance, None);
    let mut future = fixture.sleep_until(instant(6));
    assert_eq!(recorder.snapshot().len(), 3); // Reports precede even first polling.
    assert_eq!(poll(&mut future), Poll::Pending);
    assert_eq!(poll(&mut future), Poll::Pending);
    drop(future);
    assert_eq!(poll(&mut fixture.sleep_until(instant(5))), Poll::Ready(()));
    assert_eq!(
        poll(&mut fixture.sleep_until(MonotonicInstant::ZERO)),
        Poll::Ready(())
    );
    assert_eq!(
        fixture
            .sleep_until_with_tolerance(instant(5), Duration::ZERO)
            .err()
            .unwrap(),
        UnsupportedClockTolerance
    );
    let issues = recorder.snapshot();
    assert_eq!(
        issues.iter().map(|issue| issue.code).collect::<Vec<_>>(),
        [
            "clock.forbidden.now",
            "clock.forbidden.capabilities",
            "clock.forbidden.sleep_until",
            "clock.forbidden.sleep_until",
            "clock.forbidden.sleep_until",
            "clock.forbidden.tolerance",
        ]
    );
    assert!(issues.iter().all(|issue| issue.message.contains("billing")));
    assert!(
        issues
            .iter()
            .all(|issue| issue.location.line() == construction_line)
    );
    assert_eq!(fixture.assert_unused(), Err(ForbiddenClockUsed));
}

#[test]
fn forbidden_required_assertion_survives_disabled_closed_and_consumed_delivery() {
    let runtime = support::ManualRuntime::new();
    let recorder = IssueRecorder::new();
    let store = Store::builder(0, Capture, runtime.runtime())
        .issue_recorder(recorder.clone())
        .build()
        .unwrap();
    drop(runtime.take_only_queued_task());
    assert!(
        recorder
            .reporter()
            .report(Issue::new("probe", "closed"))
            .unwrap_err()
            .is_closed()
    );
    for reporter in [IssueReporter::default(), recorder.reporter()] {
        let fixture = ForbiddenClock::new("unused", instant(3), reporter);
        let clone = fixture.clone();
        let independent = ForbiddenClock::new("unused", instant(3), IssueReporter::default());
        assert_eq!(Clock::new(clone).now(), instant(3));
        assert_eq!(fixture.assert_unused(), Err(ForbiddenClockUsed));
        assert_eq!(fixture.assert_unused(), Err(ForbiddenClockUsed));
        independent.assert_unused().unwrap();
    }
    let mut recorder = IssueRecorder::new();
    let fixture = ForbiddenClock::new("consumed", instant(3), recorder.reporter());
    let _ = fixture.now();
    assert!(recorder.consume_oldest().is_some());
    assert_eq!(fixture.assert_unused(), Err(ForbiddenClockUsed));
    drop(store);
}

#[test]
fn forbidden_flag_is_visible_inside_reentrant_callback_before_fallback() {
    let slot = Arc::new(Mutex::new(None::<ForbiddenClock>));
    let lifetime = Arc::downgrade(&slot);
    let callback_slot = Arc::downgrade(&slot);
    let callbacks = Arc::new(AtomicUsize::new(0));
    let callback_count = Arc::clone(&callbacks);
    let reporter = IssueReporter::new(move |_| {
        let callback_slot = callback_slot
            .upgrade()
            .expect("the test owns the clock during its synchronous callback");
        let fixture = callback_slot.lock().unwrap().as_ref().unwrap().clone();
        assert_eq!(fixture.assert_unused(), Err(ForbiddenClockUsed));
        callback_count.fetch_add(1, Ordering::Relaxed);
    });
    let fixture = ForbiddenClock::new("callback", instant(5), reporter);
    *slot.lock().unwrap() = Some(fixture.clone());
    assert_eq!(fixture.now(), instant(5));
    assert_eq!(callbacks.load(Ordering::Relaxed), 1);
    drop(fixture);
    drop(slot);
    assert!(
        lifetime.upgrade().is_none(),
        "the callback must not retain its owner"
    );
}

#[test]
fn forbidden_erased_tolerance_reports_and_rejects_even_elapsed_deadlines() {
    let recorder = IssueRecorder::new();
    let fixture = ForbiddenClock::new("tolerance", instant(5), recorder.reporter());
    let clock = Clock::new(fixture.clone());
    assert_eq!(
        clock
            .sleep_until_with_tolerance(instant(2), Duration::ZERO)
            .err()
            .unwrap(),
        UnsupportedClockTolerance
    );
    assert_eq!(recorder.snapshot()[0].code, "clock.forbidden.capabilities");
    assert_eq!(recorder.snapshot().len(), 1);
    assert_eq!(fixture.assert_unused(), Err(ForbiddenClockUsed));
}

#[test]
fn every_forbidden_operation_independently_marks_required_use_before_polling() {
    for operation in 0..4 {
        let fixture = ForbiddenClock::new("client", instant(3), IssueReporter::default());
        fixture.assert_unused().unwrap();
        match operation {
            0 => {
                let _ = fixture.now();
            }
            1 => {
                let _ = fixture.capabilities();
            }
            2 => {
                drop(fixture.sleep_until(instant(9)));
            }
            _ => {
                assert!(
                    fixture
                        .sleep_until_with_tolerance(instant(9), Duration::ZERO)
                        .is_err()
                );
            }
        }
        assert_eq!(fixture.assert_unused(), Err(ForbiddenClockUsed));
    }
}
