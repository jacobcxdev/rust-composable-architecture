//! Checks demand, cancellation and owner-thread rules for local clock timers.

#![cfg(feature = "local")]

use std::cell::{Cell, RefCell};
use std::pin::{Pin, pin};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll, Wake, Waker};
use std::thread::{self, ThreadId};
use std::time::Duration;

use rca_core::local::{Clock, ClockDriver, ClockTimer, RuntimeFuture};
use rca_core::{InvalidTimerInterval, MonotonicInstant};

/// Keeps cadence values small and readable without using elapsed wall time.
const INTERVAL: Duration = Duration::from_millis(10);

/// Converts a test's millisecond value to the shared monotonic time type.
fn instant(millis: u64) -> MonotonicInstant {
    MonotonicInstant::from_elapsed(Duration::from_millis(millis))
}

/// Records provider calls while tests control time and poll timers explicitly.
#[derive(Debug)]
struct Records {
    /// Requires provider calls and wait destruction on the creating thread.
    owner: ThreadId,
    /// Changes only when the test sets it; no background clock runs.
    now: Cell<MonotonicInstant>,
    /// Counts clock queries independently of wait polls.
    reads: Cell<usize>,
    /// Counts every poll of a provider wait.
    polls: Cell<usize>,
    /// Counts each provider wait's destruction once.
    drops: Cell<usize>,
    /// Triggers one deliberate destructor panic, then clears before unwinding.
    panic_on_drop: Cell<bool>,
    /// Records every requested deadline, including already-elapsed ones.
    deadlines: RefCell<Vec<MonotonicInstant>>,
    /// Holds the latest waker for the single-wait remote-notification test.
    waker: RefCell<Option<Waker>>,
}

impl Records {
    /// Creates independent counters and fixes their owner thread.
    fn new() -> Rc<Self> {
        Rc::new(Self {
            owner: thread::current().id(),
            now: Cell::new(MonotonicInstant::ZERO),
            reads: Cell::new(0),
            polls: Cell::new(0),
            drops: Cell::new(0),
            panic_on_drop: Cell::new(false),
            deadlines: RefCell::new(Vec::new()),
            waker: RefCell::new(None),
        })
    }

    /// Rejects a provider call or destruction on another thread.
    fn assert_owner(&self) {
        assert_eq!(self.owner, thread::current().id());
    }

    /// Creates another local provider handle without copying its observations.
    fn clock(self: &Rc<Self>) -> Clock {
        self.assert_owner();
        Clock::new(Driver(std::rc::Rc::clone(self)))
    }

    /// Counts constructed waits whose destructors have not run.
    fn active_waits(&self) -> usize {
        self.deadlines.borrow().len() - self.drops.get()
    }
}

/// Supplies the manually controlled time without any executor authority.
#[derive(Debug)]
struct Driver(Rc<Records>);

impl ClockDriver for Driver {
    fn now(&self) -> MonotonicInstant {
        self.0.assert_owner();
        self.0.reads.set(self.0.reads.get() + 1);
        self.0.now.get()
    }

    fn sleep_until(&self, deadline: MonotonicInstant) -> RuntimeFuture {
        self.0.assert_owner();
        self.0.deadlines.borrow_mut().push(deadline);
        Box::pin(Wait(std::rc::Rc::clone(&self.0), deadline))
    }
}

impl Drop for Driver {
    fn drop(&mut self) {
        self.0.assert_owner();
    }
}

/// Retains its local provider and requested deadline until completion or disposal.
struct Wait(Rc<Records>, MonotonicInstant);

impl Future for Wait {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        self.0.assert_owner();
        self.0.polls.set(self.0.polls.get() + 1);
        if self.0.now.get() >= self.1 {
            Poll::Ready(())
        } else {
            let previous = self.0.waker.replace(Some(cx.waker().clone()));
            drop(previous);
            Poll::Pending
        }
    }
}

impl Drop for Wait {
    fn drop(&mut self) {
        self.0.assert_owner();
        self.0.drops.set(self.0.drops.get() + 1);
        let previous = self.0.waker.take();
        drop(previous);
        assert!(
            !self.0.panic_on_drop.replace(false),
            "wait destructor panic"
        );
    }
}

/// Checks local waker destruction without requiring its non-`Send` records in `Wake`.
fn assert_wait_waker_dropped_unborrowed(replace: bool) {
    thread_local! {
        /// Gives the owner-thread destructor access without moving local records.
        static ACTIVE_RECORDS: RefCell<Option<Rc<Records>>> = const { RefCell::new(None) };
    }

    /// Records whether final waker destruction can borrow the provider's slot.
    struct DropNotification(Arc<AtomicUsize>);

    #[expect(
        clippy::manual_noop_waker,
        reason = "the waker's destructor records whether its slot is still borrowed"
    )]
    impl Wake for DropNotification {
        fn wake(self: Arc<Self>) {}
    }

    impl Drop for DropNotification {
        fn drop(&mut self) {
            let available = ACTIVE_RECORDS.with(|active| {
                active
                    .borrow()
                    .as_ref()
                    .is_some_and(|records| records.waker.try_borrow_mut().is_ok())
            });
            self.0
                .store(if available { 1 } else { 2 }, Ordering::SeqCst);
        }
    }

    let records = Records::new();
    ACTIVE_RECORDS.with(|active| *active.borrow_mut() = Some(Rc::clone(&records)));
    let result = Arc::new(AtomicUsize::new(0));
    let waker = Waker::from(Arc::new(DropNotification(Arc::clone(&result))));
    let mut wait = records.clock().sleep_until(instant(10));
    assert!(
        wait.as_mut()
            .poll(&mut Context::from_waker(&waker))
            .is_pending()
    );
    drop(waker);
    if replace {
        assert!(
            wait.as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending()
        );
    }
    drop(wait);
    ACTIVE_RECORDS.with(|active| active.borrow_mut().take());
    assert_eq!(
        result.load(Ordering::SeqCst),
        1,
        "stored waker must be dropped outside its RefCell borrow"
    );
    assert_eq!(records.active_waits(), 0);
    assert_eq!(Rc::strong_count(&records), 1);
}

/// Repolling must release the previous waker after the slot's mutable borrow ends.
#[test]
fn local_wait_replacement_drops_waker_outside_borrow() {
    assert_wait_waker_dropped_unborrowed(true);
}

/// Cancelling a wait must release its waker after the slot's mutable borrow ends.
#[test]
fn local_wait_cancellation_drops_waker_outside_borrow() {
    assert_wait_waker_dropped_unborrowed(false);
}

/// Polls and abandons one demand; the timer must retain any pending provider wait.
fn poll_next(timer: &mut ClockTimer) -> Poll<Option<MonotonicInstant>> {
    pin!(timer.next()).poll(&mut Context::from_waker(Waker::noop()))
}

/// First polling anchors cadence, and abandoned demand does not replace its wait.
#[test]
fn first_demand_and_abandoned_next_preserve_one_wait() {
    let records = Records::new();
    let mut timer = ClockTimer::new(records.clock(), INTERVAL).unwrap();
    let next = timer.next();
    records.now.set(instant(30));
    drop(next);
    assert_eq!(records.reads.get(), 0);
    assert_eq!(records.active_waits(), 0);
    assert_eq!(poll_next(&mut timer), Poll::Pending);
    assert_eq!(*records.deadlines.borrow(), [instant(40)]);
    records.now.set(instant(35));
    assert_eq!(poll_next(&mut timer), Poll::Pending);
    assert_eq!(*records.deadlines.borrow(), [instant(40)]);
    assert_eq!(records.drops.get(), 0);
    records.now.set(instant(40));
    assert_eq!(poll_next(&mut timer), Poll::Ready(Some(instant(40))));
    assert_eq!(records.drops.get(), 1);
}

/// Late demand emits one observed tick and creates no successor wait before demand.
#[test]
fn late_demand_coalesces_and_never_prearms() {
    let records = Records::new();
    let mut timer = ClockTimer::new(records.clock(), INTERVAL).unwrap();
    assert_eq!(poll_next(&mut timer), Poll::Pending);
    records.now.set(instant(105));
    assert_eq!(poll_next(&mut timer), Poll::Ready(Some(instant(105))));
    assert_eq!(records.active_waits(), 0);
    assert_eq!(records.deadlines.borrow().len(), 1);
    assert_eq!(poll_next(&mut timer), Poll::Pending);
    assert_eq!(records.deadlines.borrow()[1], instant(115));
    records.now.set(instant(115));
    assert_eq!(poll_next(&mut timer), Poll::Ready(Some(instant(115))));
    assert_eq!(records.active_waits(), 0);
    records.now.set(instant(1010));
    assert_eq!(records.deadlines.borrow().len(), 2);
    assert_eq!(poll_next(&mut timer), Poll::Ready(Some(instant(1010))));
    assert_eq!(poll_next(&mut timer), Poll::Pending);
    assert_eq!(records.deadlines.borrow()[3], instant(1020));
}

/// Cancelling one timer leaves another timer's deadline and completion intact.
#[test]
fn independent_timers_do_not_share_cadence_or_cancellation() {
    let records = Records::new();
    let clock = records.clock();
    let mut first = ClockTimer::new(clock.clone(), INTERVAL).unwrap();
    let mut second = ClockTimer::new(clock, INTERVAL).unwrap();
    assert_eq!(poll_next(&mut first), Poll::Pending);
    records.now.set(instant(3));
    assert_eq!(poll_next(&mut second), Poll::Pending);
    assert_eq!(*records.deadlines.borrow(), [instant(10), instant(13)]);
    first.cancel();
    assert_eq!(records.active_waits(), 1);
    records.now.set(instant(13));
    assert_eq!(poll_next(&mut second), Poll::Ready(Some(instant(13))));
    assert_eq!(poll_next(&mut first), Poll::Ready(None));
    assert_eq!(records.active_waits(), 0);
}

/// Invalid construction and terminal polling require no clock reads or waits.
#[test]
fn invalid_or_cancelled_timer_does_not_consult_clock() {
    let records = Records::new();
    assert_eq!(
        ClockTimer::new(records.clock(), Duration::ZERO).unwrap_err(),
        InvalidTimerInterval
    );
    let mut timer = ClockTimer::new(records.clock(), INTERVAL).unwrap();
    let idle = format!("{timer:?}");
    assert!(idle.starts_with("ClockTimer {"));
    for field in [
        "interval: 10ms",
        "deadline: None",
        "waiting: false",
        "finished: false",
    ] {
        assert!(idle.contains(field));
    }
    timer.cancel();
    timer.cancel();
    assert_eq!(poll_next(&mut timer), Poll::Ready(None));
    assert_eq!(poll_next(&mut timer), Poll::Ready(None));
    assert!(format!("{timer:?}").contains("finished: true"));
    assert_eq!(records.reads.get(), 0);
    assert!(records.deadlines.borrow().is_empty());
}

/// Cancellation and owner disposal release exactly one wait on the original thread.
#[test]
fn cancel_and_drop_release_local_wait_on_owner() {
    for cancel in [false, true] {
        let records = Records::new();
        let mut timer = ClockTimer::new(records.clock(), INTERVAL).unwrap();
        assert_eq!(poll_next(&mut timer), Poll::Pending);
        assert_eq!(Rc::strong_count(&records), 3);
        assert_eq!(records.active_waits(), 1);
        let reads = records.reads.get();
        let pending = format!("{timer:?}");
        assert!(pending.contains("waiting: true"));
        assert_eq!(records.reads.get(), reads);
        if cancel {
            timer.cancel();
            assert_eq!(Rc::strong_count(&records), 2);
            timer.cancel();
            assert_eq!(poll_next(&mut timer), Poll::Ready(None));
            assert_eq!(records.reads.get(), reads);
        }
        drop(timer);
        assert_eq!(records.drops.get(), 1);
        assert_eq!(records.active_waits(), 0);
        assert_eq!(Rc::strong_count(&records), 1);
    }
}

/// Cancellation stays terminal even when the provider wait's destructor panics.
#[test]
fn wait_destructor_panic_does_not_revive_a_cancelled_timer() {
    let records = Records::new();
    let mut timer = ClockTimer::new(records.clock(), INTERVAL).unwrap();
    assert_eq!(poll_next(&mut timer), Poll::Pending);
    let reads = records.reads.get();
    records.panic_on_drop.set(true);
    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| timer.cancel()))
        .expect_err("the provider wait must panic during destruction");
    assert_eq!(panic.downcast_ref::<&str>(), Some(&"wait destructor panic"));
    assert_eq!(records.drops.get(), 1);
    assert_eq!(records.active_waits(), 0);
    assert!(records.waker.borrow().is_none());
    assert_eq!(Rc::strong_count(&records), 2);

    timer.cancel();
    assert_eq!(poll_next(&mut timer), Poll::Ready(None));
    assert_eq!(records.reads.get(), reads);
    assert_eq!(records.deadlines.borrow().len(), 1);
    drop(timer);
    assert_eq!(records.drops.get(), 1);
    assert_eq!(Rc::strong_count(&records), 1);
}

/// Saturation emits at most one final tick, then remains terminal without new waits.
#[test]
fn maximum_is_terminal_not_a_ready_loop() {
    let records = Records::new();
    records.now.set(instant(1));
    let mut timer = ClockTimer::new(records.clock(), Duration::MAX).unwrap();
    assert_eq!(poll_next(&mut timer), Poll::Pending);
    assert_eq!(*records.deadlines.borrow(), [MonotonicInstant::MAX]);
    records.now.set(MonotonicInstant::MAX);
    assert_eq!(
        poll_next(&mut timer),
        Poll::Ready(Some(MonotonicInstant::MAX))
    );
    assert_eq!(poll_next(&mut timer), Poll::Ready(None));
    assert_eq!(records.active_waits(), 0);
    let reads = records.reads.get();
    let mut at_max = ClockTimer::new(records.clock(), INTERVAL).unwrap();
    assert_eq!(poll_next(&mut at_max), Poll::Ready(None));
    assert_eq!(poll_next(&mut at_max), Poll::Ready(None));
    assert_eq!(records.reads.get(), reads + 1);
    assert_eq!(records.deadlines.borrow().len(), 1);
}

/// Counts remote notifications without touching local timer state.
#[derive(Default)]
struct Notification(AtomicUsize);

impl Wake for Notification {
    fn wake(self: Arc<Self>) {
        self.0.fetch_add(1, Ordering::Release);
    }
}

/// A remote wake notifies only; the owner thread still polls and destroys the wait.
#[test]
fn remote_wait_waker_never_runs_local_timer_work() {
    let records = Records::new();
    let mut timer = ClockTimer::new(records.clock(), INTERVAL).unwrap();
    let notification = Arc::new(Notification::default());
    let waker = Waker::from(std::sync::Arc::clone(&notification));
    assert!(
        pin!(timer.next())
            .poll(&mut Context::from_waker(&waker))
            .is_pending()
    );
    let remote = records.waker.borrow_mut().take().unwrap();
    thread::spawn(move || remote.wake()).join().unwrap();
    assert_eq!(notification.0.load(Ordering::Acquire), 1);
    assert_eq!(records.polls.get(), 1);
    assert_eq!(records.drops.get(), 0);
    records.now.set(instant(10));
    assert_eq!(poll_next(&mut timer), Poll::Ready(Some(instant(10))));
    assert_eq!(records.polls.get(), 2);
    assert_eq!(records.drops.get(), 1);
}

/// Shared timers and their demand futures retain `Send` despite the local variant.
#[test]
fn portable_timer_and_next_still_send() {
    /// Requires a type-level `Send` implementation without constructing a value.
    fn require_send<T: Send>() {}
    /// Requires the concrete demand future to implement `Send` without polling it.
    fn require_send_value(_: impl Send) {}
    /// Compiles the shared timer's borrowing demand against the `Send` requirement.
    fn check_next(timer: &mut rca_core::ClockTimer) {
        require_send_value(timer.next());
    }
    require_send::<rca_core::ClockTimer>();
    let _ = check_next;
}
