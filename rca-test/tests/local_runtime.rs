//! Checks owner-thread polling, remote wake notifications and virtual-time policy.

#![cfg(feature = "local")]

use std::cell::Cell;
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
use std::task::{Context, Poll, Waker};
use std::thread::ThreadId;
use std::time::Duration;

use rca_core::MonotonicInstant;
use rca_core::ReduceContext;
use rca_core::local::{Effect, Reducer, Store};
use rca_test::local::DeterministicRuntime;

#[test]
fn local_task_retains_rc_across_sleep_and_returns_rc_result() {
    let runtime = DeterministicRuntime::new();
    let completed = Rc::new(Cell::new(false));
    let captured = Rc::clone(&completed);
    let sleep = runtime.sleep_until(MonotonicInstant::from_elapsed(Duration::from_secs(1)));
    runtime.spawn_task(Box::pin(async move {
        sleep.await;
        captured.set(true);
    }));
    runtime.run_until_stalled().expect("register local sleep");
    assert!(!completed.get());
    runtime
        .advance(Duration::from_secs(1))
        .expect("wake local task");
    let result = futures_executor::block_on(runtime.run(async move { completed }))
        .expect("return owner-local result");
    assert!(result.get());
}

/// Records destruction and rejects polling or dropping an `Rc` capture off its owner.
struct OwnerProbe {
    owner: ThreadId,
    drops: Rc<Cell<usize>>,
}

impl OwnerProbe {
    /// Records the creating thread without making the captured counter `Send`.
    fn new(drops: &Rc<Cell<usize>>) -> Self {
        Self {
            owner: std::thread::current().id(),
            drops: Rc::clone(drops),
        }
    }

    /// Checks the current thread before touching owner-local state.
    fn check(&self) {
        assert_eq!(self.owner, std::thread::current().id());
    }
}

impl Drop for OwnerProbe {
    fn drop(&mut self) {
        self.check();
        self.drops.set(self.drops.get() + 1);
    }
}

/// Counts wake notifications without polling any local future.
#[derive(Default)]
struct CountWake(AtomicUsize);

impl std::task::Wake for CountWake {
    fn wake(self: Arc<Self>) {
        self.0.fetch_add(1, SeqCst);
    }
    fn wake_by_ref(self: &Arc<Self>) {
        self.0.fetch_add(1, SeqCst);
    }
}

#[test]
fn remote_wake_after_stall_only_notifies_owner() {
    let host = DeterministicRuntime::new();
    let owner = std::thread::current().id();
    let drops = Rc::new(Cell::new(0));
    let probe = OwnerProbe::new(&drops);
    let recorded = Rc::new(RefCell::new(None));
    let output = Rc::clone(&recorded);
    let polls = Rc::new(Cell::new(0));
    let task_polls = Rc::clone(&polls);
    host.spawn_task(Box::pin(std::future::poll_fn(move |cx| {
        probe.check();
        task_polls.set(task_polls.get() + 1);
        if task_polls.get() == 1 {
            *output.borrow_mut() = Some(cx.waker().clone());
            Poll::Pending
        } else {
            Poll::Ready(())
        }
    })));
    assert_eq!(polls.get(), 0);
    let mut drive = Box::pin(host.run(std::future::poll_fn(|_| {
        if polls.get() == 2 {
            Poll::Ready(Rc::new(42))
        } else {
            Poll::Pending
        }
    })));
    let notified = Arc::new(CountWake::default());
    let waker = Waker::from(Arc::clone(&notified));
    let mut cx = Context::from_waker(&waker);
    assert!(drive.as_mut().poll(&mut cx).is_pending());
    assert_eq!(polls.get(), 1);
    let wake = recorded.borrow_mut().take().unwrap();
    let before_remote_wake = notified.0.load(SeqCst);
    std::thread::spawn(move || {
        assert_ne!(owner, std::thread::current().id());
        wake.wake();
    })
    .join()
    .unwrap();
    assert!(notified.0.load(SeqCst) > before_remote_wake);
    assert_eq!(polls.get(), 1);
    assert_eq!(drops.get(), 0);
    let Poll::Ready(Ok(result)) = drive.as_mut().poll(&mut cx) else {
        panic!("owner finishes task")
    };
    assert_eq!(*result, 42);
    assert_eq!(drops.get(), 1);
}

#[test]
fn joined_remote_wake_and_exact_budget_keep_local_polls_on_owner() {
    let host = DeterministicRuntime::with_step_budget(7);
    assert_eq!(host.step_budget(), 7);
    let drops = Rc::new(Cell::new(0));
    let probe = OwnerProbe::new(&drops);
    let polls = Rc::new(Cell::new(0));
    let count = Rc::clone(&polls);
    host.spawn_task(Box::pin(std::future::poll_fn(move |cx| {
        probe.check();
        count.set(count.get() + 1);
        let wake = cx.waker().clone();
        std::thread::spawn(move || wake.wake()).join().unwrap();
        Poll::Pending
    })));
    let failure = host.run_until_stalled().unwrap_err();
    assert_eq!(failure.polls(), 7);
    assert_eq!(polls.get(), 7);
    assert_eq!(failure.ready_task_ids(), &[0]);
    drop(host);
    assert_eq!(drops.get(), 1);
}

#[test]
fn local_seeded_clock_reports_capabilities_and_waits_until_its_deadline() {
    let seed = MonotonicInstant::from_elapsed(Duration::from_secs(10));
    let host = DeterministicRuntime::with_now(seed);
    let clock = host.runtime().clock();
    assert_eq!(host.now(), seed);
    assert_eq!(clock.now(), seed);
    assert_eq!(
        clock.capabilities().resolution,
        Some(Duration::from_nanos(1))
    );
    assert_eq!(clock.capabilities().minimum_tolerance, Some(Duration::ZERO));

    let mut wait = clock.sleep(Duration::from_secs(2));
    let mut cx = Context::from_waker(Waker::noop());
    assert!(wait.as_mut().poll(&mut cx).is_pending());
    let pending = host.assert_no_pending_time().unwrap_err();
    assert!(pending.is_pending_time());
    assert_eq!(pending.current_time(), seed);
    assert_eq!(pending.pending_sleeps().len(), 1);
    let deadline = MonotonicInstant::from_elapsed(Duration::from_secs(12));
    assert_eq!(pending.pending_sleeps()[0].deadline(), deadline);
    host.advance(Duration::from_secs(1)).unwrap();
    assert!(wait.as_mut().poll(&mut cx).is_pending());
    assert_eq!(host.pending_sleeps()[0].deadline(), deadline);
    host.advance(Duration::from_secs(1)).unwrap();
    assert!(wait.as_mut().poll(&mut cx).is_ready());
    host.assert_no_pending_time().unwrap();
}

#[test]
fn local_default_diagnostics_observe_work_without_driving_it() {
    let host = DeterministicRuntime::default();
    assert_eq!(host.now(), MonotonicInstant::ZERO);
    assert_eq!(host.step_budget(), rca_test::DEFAULT_STEP_BUDGET);
    assert_eq!(
        format!("{:?}", host.runtime()),
        "Runtime { driver: DeterministicDriver { .. } }"
    );

    let completed = Rc::new(Cell::new(false));
    let output = Rc::clone(&completed);
    let sleep = host.runtime().clock().sleep(Duration::from_secs(1));
    host.spawn_task(Box::pin(async move {
        sleep.await;
        output.set(true);
    }));
    let ready = format!("{host:?}");
    assert!(ready.contains("ready_task_ids: [0]"));
    assert!(ready.contains("pending_sleeps: []"));
    assert!(!completed.get());

    host.run_until_stalled().unwrap();
    let waiting = format!("{host:?}");
    assert!(waiting.contains("ready_task_ids: []"));
    assert!(waiting.contains("pending_sleeps: [PendingSleep"));
    assert!(!completed.get());
    host.advance(Duration::from_secs(1)).unwrap();
    assert!(completed.get());
}

#[test]
fn local_actor_yields_after_a_full_command_batch() {
    /// Exposes the number of real reducer calls to a competing local task.
    struct Counter(Rc<Cell<usize>>);

    impl Reducer for Counter {
        type State = ();
        type Action = ();

        fn reduce(&mut self, (): &mut (), (): &(), _: &mut ReduceContext<'_>) -> Effect<()> {
            self.0.set(self.0.get() + 1);
            Effect::none()
        }
    }

    let host = DeterministicRuntime::new();
    let processed = Rc::new(Cell::new(0));
    let store = Store::new((), Counter(Rc::clone(&processed)), host.runtime());
    let observed = Rc::new(Cell::new(None));
    let output = Rc::clone(&observed);
    let count = Rc::clone(&processed);
    host.spawn_task(Box::pin(async move { output.set(Some(count.get())) }));
    for _ in 0..65 {
        drop(
            store
                .try_send(())
                .expect("admit command within default capacity"),
        );
    }

    host.run_until_stalled().unwrap();
    assert_eq!(observed.get(), Some(64));
    assert_eq!(processed.get(), 65);
}

#[test]
fn cancellation_before_first_poll_and_unpolled_teardown_drop_on_owner() {
    let drops = Rc::new(Cell::new(0));
    let polls = Rc::new(Cell::new(0));
    let host = DeterministicRuntime::new();
    let probe = OwnerProbe::new(&drops);
    let count = Rc::clone(&polls);
    let task = std::future::poll_fn(move |_| {
        probe.check();
        count.set(count.get() + 1);
        Poll::Pending::<()>
    });
    let (abort, registration) = futures_util::future::AbortHandle::new_pair();
    host.spawn_task(Box::pin(async move {
        let _ = futures_util::future::Abortable::new(task, registration).await;
    }));
    abort.abort();
    host.run_until_stalled().unwrap();
    assert_eq!(polls.get(), 0);
    assert_eq!(drops.get(), 1);
    let probe = OwnerProbe::new(&drops);
    host.spawn_task(Box::pin(async move {
        probe.check();
        std::future::pending::<()>().await;
    }));
    drop(host);
    assert_eq!(drops.get(), 2);
}

#[test]
fn stale_waker_clone_wake_and_drop_after_teardown_never_touch_local_captures() {
    for retain_clock in [false, true] {
        let host = DeterministicRuntime::new();
        let clock = retain_clock.then(|| host.runtime().clock());
        let drops = Rc::new(Cell::new(0));
        let probe = OwnerProbe::new(&drops);
        let payload = Rc::new(());
        let weak = Rc::downgrade(&payload);
        let recorded = Rc::new(RefCell::new(None));
        let output = Rc::clone(&recorded);
        host.spawn_task(Box::pin(std::future::poll_fn(move |cx| {
            probe.check();
            let _ = &payload;
            *output.borrow_mut() = Some(cx.waker().clone());
            Poll::Pending
        })));
        host.run_until_stalled().unwrap();
        let wake = recorded.borrow_mut().take().unwrap();
        drop(host);
        assert_eq!(drops.get(), 1);
        assert!(weak.upgrade().is_none());
        std::thread::spawn(move || {
            let clone = wake.clone();
            wake.wake_by_ref();
            clone.wake();
            drop(wake);
        })
        .join()
        .unwrap();
        assert_eq!(drops.get(), 1);
        drop(clock);
    }
}

#[test]
fn local_deadline_order_cancellation_timer_and_saturation_share_policy() {
    let host = DeterministicRuntime::new();
    let seen = Rc::new(RefCell::new(Vec::new()));
    for id in 0..3 {
        let clock = host.runtime().clock();
        let output = Rc::clone(&seen);
        host.spawn_task(Box::pin(async move {
            clock.sleep(Duration::from_secs(1)).await;
            output.borrow_mut().push(id);
            clock.sleep(Duration::from_secs(1)).await;
            output.borrow_mut().push(id + 3);
        }));
    }
    host.run_until_stalled().unwrap();
    host.advance(Duration::from_secs(2)).unwrap();
    assert_eq!(*seen.borrow(), [0, 1, 2, 3, 4, 5]);
    host.assert_no_pending_time().unwrap();
    let clock = host.runtime().clock();
    let mut wait = clock.sleep(Duration::from_secs(1));
    assert!(
        wait.as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_pending()
    );
    assert_eq!(host.pending_sleeps().len(), 1);
    drop(wait);
    assert_eq!(host.pending_sleeps(), [] as [rca_test::PendingSleep; 0]);
    drop(clock.sleep(Duration::from_secs(1)));
    assert_eq!(host.pending_sleeps(), [] as [rca_test::PendingSleep; 0]);
    let mut timer =
        rca_core::local::ClockTimer::new(clock.clone(), Duration::from_secs(1)).unwrap();
    assert!(
        Box::pin(timer.next())
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_pending()
    );
    timer.cancel();
    assert_eq!(host.pending_sleeps(), [] as [rca_test::PendingSleep; 0]);
    host.advance_to(MonotonicInstant::MAX).unwrap();
    assert_eq!(host.now(), MonotonicInstant::MAX);
    assert!(
        clock
            .sleep(Duration::MAX)
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_ready()
    );
}

#[test]
fn run_accepts_borrowed_non_clone_non_debug_result_and_drain_does_not_keep_host_alive() {
    /// Deliberately has neither `Clone` nor `Debug` and retains owner-local data.
    struct ResultValue(Rc<usize>);

    let host = DeterministicRuntime::new();
    let value = Rc::new(42);
    let output =
        futures_executor::block_on(host.run(async { ResultValue(Rc::clone(&value)) })).unwrap();
    assert_eq!(*output.0, 42);
    let drain = host.drain_deadlines(0);
    drop(host);
    assert!(
        futures_executor::block_on(drain)
            .unwrap_err()
            .is_host_closed()
    );
}
