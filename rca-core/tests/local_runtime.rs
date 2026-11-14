//! Checks owner-thread clock lifetimes through direct and runtime-erased drivers.
//!
//! Tests poll waits explicitly; remote wakers only notify and never drive local work.

#![cfg(feature = "local")]

use std::cell::{Cell, RefCell};
use std::pin::Pin;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll, Wake, Waker};
use std::thread::{self, ThreadId};
use std::time::Duration;

use rca_core::local::{Clock, ClockDriver, Runtime, RuntimeDriver, RuntimeFuture};
use rca_core::{ClockCapabilities, MonotonicInstant, UnsupportedClockTolerance};

/// Retains observable driver activity outside each wait's lifetime.
#[derive(Debug)]
struct Records {
    /// Must own every wait poll and destructor.
    owner: ThreadId,
    /// Counts real polls, including the poll that completes a wait.
    polls: Cell<usize>,
    /// Counts wait destruction independently of completion.
    drops: Cell<usize>,
    /// Advances only when the test changes it.
    now: Cell<MonotonicInstant>,
    /// Records deadlines passed to the underlying driver.
    deadlines: RefCell<Vec<MonotonicInstant>>,
    /// Records normalised tolerance requests passed to the driver.
    tolerances: RefCell<Vec<Duration>>,
    /// Exposes the latest pending notification without transferring the wait.
    waker: RefCell<Option<Waker>>,
}

impl Records {
    /// Starts an unpolled clock at zero on the current thread.
    fn new() -> Rc<Self> {
        Rc::new(Self {
            owner: thread::current().id(),
            polls: Cell::new(0),
            drops: Cell::new(0),
            now: Cell::new(MonotonicInstant::ZERO),
            deadlines: RefCell::new(Vec::new()),
            tolerances: RefCell::new(Vec::new()),
            waker: RefCell::new(None),
        })
    }
}

/// Keeps the driver's owner-local state alive until this exact deadline is reached or dropped.
struct LocalWait(Rc<Records>, MonotonicInstant);

impl Future for LocalWait {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        assert_eq!(thread::current().id(), self.0.owner);
        self.0.polls.set(self.0.polls.get() + 1);
        if self.0.now.get() >= self.1 {
            Poll::Ready(())
        } else {
            *self.0.waker.borrow_mut() = Some(cx.waker().clone());
            Poll::Pending
        }
    }
}

impl Drop for LocalWait {
    fn drop(&mut self) {
        assert_eq!(thread::current().id(), self.0.owner);
        self.0.drops.set(self.0.drops.get() + 1);
        self.0.waker.borrow_mut().take();
    }
}

/// Implements exact waits and records supported tolerance requests without coalescing.
#[derive(Debug)]
struct Driver {
    records: Rc<Records>,
    minimum_tolerance: Option<Duration>,
}

impl ClockDriver for Driver {
    fn now(&self) -> MonotonicInstant {
        self.records.now.get()
    }

    fn sleep_until(&self, deadline: MonotonicInstant) -> RuntimeFuture {
        self.records.deadlines.borrow_mut().push(deadline);
        Box::pin(LocalWait(std::rc::Rc::clone(&self.records), deadline))
    }

    fn capabilities(&self) -> ClockCapabilities {
        ClockCapabilities {
            resolution: Some(Duration::from_millis(1)),
            minimum_tolerance: self.minimum_tolerance,
        }
    }

    fn sleep_until_with_tolerance(
        &self,
        deadline: MonotonicInstant,
        tolerance: Duration,
    ) -> Result<RuntimeFuture, UnsupportedClockTolerance> {
        self.records.tolerances.borrow_mut().push(tolerance);
        Ok(ClockDriver::sleep_until(self, deadline))
    }
}

impl RuntimeDriver for Driver {
    fn spawn(&self, _: RuntimeFuture) {
        panic!("clock-only fixture must not spawn")
    }

    fn now(&self) -> MonotonicInstant {
        ClockDriver::now(self)
    }

    fn sleep_until(&self, deadline: MonotonicInstant) -> RuntimeFuture {
        ClockDriver::sleep_until(self, deadline)
    }

    fn clock_capabilities(&self) -> ClockCapabilities {
        self.capabilities()
    }

    fn sleep_until_with_tolerance(
        &self,
        deadline: MonotonicInstant,
        tolerance: Duration,
    ) -> Result<RuntimeFuture, UnsupportedClockTolerance> {
        ClockDriver::sleep_until_with_tolerance(self, deadline, tolerance)
    }

    fn yield_now(&self) -> RuntimeFuture {
        panic!("clock-only fixture must not yield")
    }
}

/// Keeps capability metadata but deliberately uses the traits' default tolerance methods.
#[derive(Debug)]
struct DefaultToleranceDriver(Driver);

impl ClockDriver for DefaultToleranceDriver {
    fn now(&self) -> MonotonicInstant {
        ClockDriver::now(&self.0)
    }

    fn sleep_until(&self, deadline: MonotonicInstant) -> RuntimeFuture {
        ClockDriver::sleep_until(&self.0, deadline)
    }

    fn capabilities(&self) -> ClockCapabilities {
        ClockDriver::capabilities(&self.0)
    }
}

impl RuntimeDriver for DefaultToleranceDriver {
    fn spawn(&self, task: RuntimeFuture) {
        RuntimeDriver::spawn(&self.0, task);
    }

    fn now(&self) -> MonotonicInstant {
        ClockDriver::now(&self.0)
    }

    fn sleep_until(&self, deadline: MonotonicInstant) -> RuntimeFuture {
        ClockDriver::sleep_until(&self.0, deadline)
    }

    fn clock_capabilities(&self) -> ClockCapabilities {
        ClockDriver::capabilities(&self.0)
    }

    fn yield_now(&self) -> RuntimeFuture {
        RuntimeDriver::yield_now(&self.0)
    }
}

/// Deliberately omits both capability metadata and tolerance overrides.
#[derive(Debug)]
struct UnknownMetadataDriver(Driver);

impl ClockDriver for UnknownMetadataDriver {
    fn now(&self) -> MonotonicInstant {
        ClockDriver::now(&self.0)
    }

    fn sleep_until(&self, deadline: MonotonicInstant) -> RuntimeFuture {
        ClockDriver::sleep_until(&self.0, deadline)
    }
}

impl RuntimeDriver for UnknownMetadataDriver {
    fn spawn(&self, task: RuntimeFuture) {
        RuntimeDriver::spawn(&self.0, task);
    }

    fn now(&self) -> MonotonicInstant {
        ClockDriver::now(&self.0)
    }

    fn sleep_until(&self, deadline: MonotonicInstant) -> RuntimeFuture {
        ClockDriver::sleep_until(&self.0, deadline)
    }

    fn yield_now(&self) -> RuntimeFuture {
        RuntimeDriver::yield_now(&self.0)
    }
}

/// Exercises the same driver directly and through the runtime's erased clock interface.
fn clock(records: &Rc<Records>, runtime: bool, minimum_tolerance: Option<Duration>) -> Clock {
    let driver = Driver {
        records: std::rc::Rc::clone(records),
        minimum_tolerance,
    };
    if runtime {
        Runtime::new(driver).clock()
    } else {
        Clock::new(driver)
    }
}

#[test]
fn local_clocks_release_rc_captures_when_cancelled_without_polling() {
    for runtime in [false, true] {
        let records = Records::new();
        let clock = clock(&records, runtime, None);
        let wait = clock.sleep(Duration::from_secs(1));
        drop(clock);
        assert_eq!(Rc::strong_count(&records), 2);
        drop(wait);
        assert_eq!(records.polls.get(), 0);
        assert_eq!(records.drops.get(), 1);
        assert_eq!(Rc::strong_count(&records), 1);
    }
}

#[test]
fn local_clocks_cancel_polled_waits_on_the_owner_thread() {
    for runtime in [false, true] {
        let records = Records::new();
        let clock = clock(&records, runtime, None);
        let mut wait = clock.sleep(Duration::from_secs(1));
        let mut context = Context::from_waker(Waker::noop());
        assert!(wait.as_mut().poll(&mut context).is_pending());
        drop(wait);
        assert_eq!(records.polls.get(), 1);
        assert_eq!(records.drops.get(), 1);
    }
}

#[test]
fn local_clock_preserves_origin_saturation_and_elapsed_deadlines() {
    for runtime in [false, true] {
        let records = Records::new();
        records.now.set(MonotonicInstant::MAX);
        let clock = clock(&records, runtime, None);
        assert_eq!(clock.clone().now(), MonotonicInstant::MAX);
        let mut wait = clock.sleep(Duration::from_secs(1));
        assert_eq!(*records.deadlines.borrow(), [MonotonicInstant::MAX]);
        assert!(
            wait.as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_ready()
        );
    }
}

#[test]
fn local_clock_tolerance_matches_portable_policy() {
    for runtime in [false, true] {
        let records = Records::new();
        let deadline = MonotonicInstant::from_elapsed(Duration::from_secs(1));
        let unsupported = clock(&records, runtime, None);
        assert!(
            unsupported
                .sleep_until_with_tolerance(MonotonicInstant::ZERO, Duration::ZERO)
                .is_err()
        );
        assert!(records.deadlines.borrow().is_empty());
        let supported = clock(&records, runtime, Some(Duration::from_millis(5)));
        assert_eq!(
            supported.capabilities().resolution,
            Some(Duration::from_millis(1))
        );
        drop(
            supported
                .sleep_until_with_tolerance(deadline, Duration::ZERO)
                .unwrap(),
        );
        assert_eq!(*records.tolerances.borrow(), [Duration::from_millis(5)]);
        records.now.set(deadline);
        let mut elapsed = supported
            .sleep_until_with_tolerance(deadline, Duration::ZERO)
            .unwrap();
        assert!(
            elapsed
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_ready()
        );
        assert_eq!(records.deadlines.borrow().len(), 1);
    }
}

#[test]
fn local_tolerance_defaults_reject_unsupported_without_creating_waits() {
    let records = Records::new();
    let driver = DefaultToleranceDriver(Driver {
        records: Rc::clone(&records),
        minimum_tolerance: None,
    });
    for deadline in [
        MonotonicInstant::ZERO,
        MonotonicInstant::from_elapsed(Duration::from_secs(1)),
    ] {
        for tolerance in [Duration::ZERO, Duration::MAX] {
            for result in [
                ClockDriver::sleep_until_with_tolerance(&driver, deadline, tolerance),
                RuntimeDriver::sleep_until_with_tolerance(&driver, deadline, tolerance),
            ] {
                let error = result.err().expect("undeclared tolerance is unsupported");
                assert_eq!(error, UnsupportedClockTolerance);
                assert_eq!(
                    error.to_string(),
                    "clock does not support explicit timer tolerance"
                );
            }
        }
    }
    assert!(records.deadlines.borrow().is_empty());
    assert_eq!(records.polls.get(), 0);
    assert_eq!(records.drops.get(), 0);
    drop(driver);
    assert_eq!(Rc::strong_count(&records), 1);
}

#[test]
fn unknown_local_clock_metadata_rejects_tolerance_without_disabling_exact_waits() {
    for runtime in [false, true] {
        let records = Records::new();
        let driver = UnknownMetadataDriver(Driver {
            records: Rc::clone(&records),
            minimum_tolerance: None,
        });
        let clock = if runtime {
            Runtime::new(driver).clock()
        } else {
            Clock::new(driver)
        };
        assert_eq!(clock.capabilities(), ClockCapabilities::default());

        let deadline = MonotonicInstant::from_elapsed(Duration::from_secs(1));
        for tolerance in [Duration::ZERO, Duration::MAX] {
            let error = clock
                .sleep_until_with_tolerance(deadline, tolerance)
                .err()
                .expect("unknown capabilities cannot admit advisory tolerance");
            assert_eq!(error, UnsupportedClockTolerance);
        }
        assert!(records.deadlines.borrow().is_empty());
        assert_eq!((records.polls.get(), records.drops.get()), (0, 0));

        let mut exact = clock.sleep_until(deadline);
        assert_eq!(*records.deadlines.borrow(), [deadline]);
        let mut context = Context::from_waker(Waker::noop());
        assert!(exact.as_mut().poll(&mut context).is_pending());
        drop(clock);
        assert_eq!(Rc::strong_count(&records), 2);
        drop(exact);
        assert_eq!((records.polls.get(), records.drops.get()), (1, 1));
        assert!(records.waker.borrow().is_none());
        assert_eq!(Rc::strong_count(&records), 1);
    }
}

#[test]
fn local_tolerance_defaults_wait_at_original_deadline_without_coalescing() {
    for runtime in [false, true] {
        let records = Records::new();
        let driver = DefaultToleranceDriver(Driver {
            records: Rc::clone(&records),
            minimum_tolerance: Some(Duration::from_millis(5)),
        });
        let deadline = MonotonicInstant::from_elapsed(Duration::from_secs(1));
        let mut wait = if runtime {
            RuntimeDriver::sleep_until_with_tolerance(&driver, deadline, Duration::MAX)
        } else {
            ClockDriver::sleep_until_with_tolerance(&driver, deadline, Duration::MAX)
        }
        .expect("declared advisory tolerance may be ignored");
        assert_eq!(*records.deadlines.borrow(), [deadline]);
        assert_eq!(records.polls.get(), 0);
        drop(driver);
        assert_eq!(Rc::strong_count(&records), 2);
        let mut context = Context::from_waker(Waker::noop());
        assert!(wait.as_mut().poll(&mut context).is_pending());
        records
            .now
            .set(MonotonicInstant::from_elapsed(Duration::from_millis(999)));
        assert!(wait.as_mut().poll(&mut context).is_pending());
        records.now.set(deadline);
        assert!(wait.as_mut().poll(&mut context).is_ready());
        drop(wait);
        assert_eq!(records.polls.get(), 3);
        assert_eq!(records.drops.get(), 1);
        assert_eq!(Rc::strong_count(&records), 1);
    }
}

#[test]
fn erased_local_default_tolerant_waits_release_owner_captures_on_cancellation() {
    for runtime in [false, true] {
        for poll_before_cancel in [false, true] {
            let records = Records::new();
            let driver = DefaultToleranceDriver(Driver {
                records: Rc::clone(&records),
                minimum_tolerance: Some(Duration::from_millis(5)),
            });
            let clock = if runtime {
                Runtime::new(driver).clock()
            } else {
                Clock::new(driver)
            };
            let deadline = MonotonicInstant::from_elapsed(Duration::from_secs(1));
            let mut wait = clock
                .sleep_until_with_tolerance(deadline, Duration::ZERO)
                .expect("clock applies the declared minimum before the default");
            assert_eq!(*records.deadlines.borrow(), [deadline]);
            assert_eq!(records.polls.get(), 0);
            drop(clock);
            assert_eq!(Rc::strong_count(&records), 2);
            if poll_before_cancel {
                let mut context = Context::from_waker(Waker::noop());
                assert!(wait.as_mut().poll(&mut context).is_pending());
                records
                    .now
                    .set(MonotonicInstant::from_elapsed(Duration::from_millis(999)));
                assert!(wait.as_mut().poll(&mut context).is_pending());
            }
            drop(wait);
            assert_eq!(records.polls.get(), if poll_before_cancel { 2 } else { 0 });
            assert_eq!(records.drops.get(), 1);
            assert!(records.waker.borrow().is_none());
            assert_eq!(Rc::strong_count(&records), 1);
        }
    }
}

#[derive(Default)]
/// Lets another thread signal readiness without owning any local payload.
struct Notification(AtomicBool);

impl Wake for Notification {
    fn wake(self: Arc<Self>) {
        self.0.store(true, Ordering::Release);
    }
}

#[test]
fn remote_wake_notifies_without_polling_or_dropping_local_payloads() {
    let records = Records::new();
    let clock = clock(&records, true, None);
    let mut wait = clock.sleep(Duration::from_secs(1));
    let notification = Arc::new(Notification::default());
    let waker = Waker::from(std::sync::Arc::clone(&notification));
    assert!(
        wait.as_mut()
            .poll(&mut Context::from_waker(&waker))
            .is_pending()
    );
    let remote_waker = records.waker.borrow_mut().take().unwrap();
    thread::spawn(move || remote_waker.wake()).join().unwrap();
    assert!(notification.0.load(Ordering::Acquire));
    assert_eq!(records.polls.get(), 1);
    assert_eq!(records.drops.get(), 0);
    records
        .now
        .set(MonotonicInstant::from_elapsed(Duration::from_secs(1)));
    assert!(
        wait.as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_ready()
    );
    drop(wait);
    assert_eq!(records.drops.get(), 1);
}
