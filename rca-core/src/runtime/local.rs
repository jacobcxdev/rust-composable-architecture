//! Provides runtime and clock handles confined to their caller-owned thread.
//!
//! Handles and their futures can own [`Rc`] values. They cannot move to another
//! thread; wake notifications can, but must only notify the owning host. No
//! executor is installed or retained by RCA.
//! Driver callbacks and wait destructors can panic. These handles propagate
//! provider panics without converting them into recoverable effect failures.
//!
//! ```compile_fail
//! use rca_core::local::Runtime;
//! fn require_send<T: Send>() {}
//! require_send::<Runtime>();
//! ```
//!
//! ```compile_fail
//! use rca_core::local::Runtime;
//! fn require_sync<T: Sync>() {}
//! require_sync::<Runtime>();
//! ```
//!
//! ```compile_fail
//! use rca_core::local::Clock;
//! fn require_send<T: Send>() {}
//! require_send::<Clock>();
//! ```
//!
//! ```compile_fail
//! use rca_core::local::Clock;
//! fn require_sync<T: Sync>() {}
//! require_sync::<Clock>();
//! ```
//!
//! ```compile_fail
//! use rca_core::local::RuntimeFuture;
//! fn require_send<T: Send>() {}
//! require_send::<RuntimeFuture>();
//! ```

use std::fmt;
use std::pin::Pin;
use std::rc::Rc;
use std::time::Duration;

use super::{
    ClockCapabilities, MonotonicInstant, UnsupportedClockTolerance, prepare_tolerant_wait,
};

/// Adapts the shared cadence state to owner-local clocks and waits.
mod timer;

#[doc(inline)]
pub use timer::ClockTimer;

/// An owned thread-affine task or wait accepted by a local runtime driver.
pub type RuntimeFuture = Pin<Box<dyn Future<Output = ()> + 'static>>;

/// Supplies monotonic time and cancellable waits on the owning thread.
///
/// Implementations share one origin across their handles. Time never decreases;
/// elapsed deadlines are ready on first poll. Creating, polling and dropping a
/// wait and its captures occur on the owning thread. Dropping a wait cancels it
/// and releases captured resources, including before its first poll.
///
/// A clock is not host or spawning authority. Wake notifications may arrive from
/// any thread; they must not access local payloads or poll or destroy local work.
pub trait ClockDriver: fmt::Debug + 'static {
    /// Returns current monotonic time from this provider's origin.
    fn now(&self) -> MonotonicInstant;

    /// Returns an owned wait completing no earlier than this-origin `deadline`.
    fn sleep_until(&self, deadline: MonotonicInstant) -> RuntimeFuture;

    /// Returns declared resolution and tolerance support.
    fn capabilities(&self) -> ClockCapabilities {
        ClockCapabilities::default()
    }

    /// Returns a wait permitting advisory slack after `deadline`, never before it.
    ///
    /// The default ignores slack when support is declared. [`Clock`] applies the
    /// declared minimum. The same-origin, elapsed-readiness and cancellation
    /// requirements of [`Self::sleep_until`] still apply.
    ///
    /// # Errors
    ///
    /// Returns [`UnsupportedClockTolerance`] when tolerance is unsupported.
    fn sleep_until_with_tolerance(
        &self,
        deadline: MonotonicInstant,
        _tolerance: Duration,
    ) -> Result<RuntimeFuture, UnsupportedClockTolerance> {
        self.capabilities()
            .minimum_tolerance
            .ok_or(UnsupportedClockTolerance)?;
        Ok(self.sleep_until(deadline))
    }
}

/// Schedules local RCA tasks and supplies monotonic time on the owning thread.
///
/// # Driver contract
///
/// The caller constructs and owns the thread-affine host. All driver calls, task
/// polls, callbacks and destruction of local captures occur on that same thread.
/// Accepted tasks remain live until completion or are dropped exactly once when
/// the host shuts down, including tasks never polled. A live host must not
/// silently discard accepted work. Driver handles must not retain host authority.
///
/// Wake-ups can arrive from any thread. The waker may only notify the owner using
/// thread-safe metadata; it must not capture, poll or destroy local task payloads
/// off-thread or require an RCA actor lock. Driver panics and contract violations
/// are programming faults, not recoverable effect failures. Tasks are never
/// migrated to another host after teardown.
pub trait RuntimeDriver: fmt::Debug + 'static {
    /// Accepts a task promptly without polling it inline.
    ///
    /// A host that has already shut down may drop the task without polling it,
    /// on the calling owner thread.
    fn spawn(&self, task: RuntimeFuture);

    /// Returns nondecreasing time from the origin shared by this driver's handles.
    fn now(&self) -> MonotonicInstant;

    /// Returns a wait for a deadline from this origin.
    ///
    /// Elapsed deadlines are ready on first poll. Dropping the future cancels it
    /// and releases captures on the owner thread, even before first polling.
    fn sleep_until(&self, deadline: MonotonicInstant) -> RuntimeFuture;

    /// Returns declared resolution and tolerance support.
    fn clock_capabilities(&self) -> ClockCapabilities {
        ClockCapabilities::default()
    }

    /// Returns a wait permitting advisory slack after `deadline`, never before it.
    ///
    /// The default ignores slack when support is declared. [`Clock`] applies its
    /// declared minimum. The wait and cancellation contract remains unchanged.
    ///
    /// # Errors
    ///
    /// Returns [`UnsupportedClockTolerance`] when tolerance is unsupported.
    fn sleep_until_with_tolerance(
        &self,
        deadline: MonotonicInstant,
        _tolerance: Duration,
    ) -> Result<RuntimeFuture, UnsupportedClockTolerance> {
        self.clock_capabilities()
            .minimum_tolerance
            .ok_or(UnsupportedClockTolerance)?;
        Ok(self.sleep_until(deadline))
    }

    /// Returns a cancellable wait yielding at least one scheduling opportunity.
    ///
    /// Another task need not run. Dropping the future releases its local captures
    /// on the owner thread, even before its first poll.
    fn yield_now(&self) -> RuntimeFuture;
}

/// Provides shallow-cloneable access to a local driver without retaining its host.
///
/// Clones share the driver and its clock origin, not a new executor. Calls and
/// destruction remain on the owning thread. Provider panics propagate unchanged.
#[derive(Clone, Debug)]
pub struct Runtime {
    /// Shares the provider; its contract keeps host ownership outside this handle.
    driver: Rc<dyn RuntimeDriver>,
}

impl Runtime {
    /// Erases a valid local runtime driver on its owning thread.
    ///
    /// The caller supplies a driver satisfying [`RuntimeDriver`]. Construction
    /// does not start the executor or check the provider's behaviour.
    #[must_use]
    pub fn new(driver: impl RuntimeDriver) -> Self {
        Self {
            driver: Rc::new(driver),
        }
    }

    /// Returns current monotonic time from the driver's shared origin.
    #[must_use]
    pub fn now(&self) -> MonotonicInstant {
        self.driver.now()
    }

    /// Returns a clock sharing this runtime's origin, without spawning authority.
    ///
    /// Reuses the existing driver allocation. A separately constructed adapter
    /// can have another origin, even when it uses the same host.
    #[must_use]
    pub fn clock(&self) -> Clock {
        Clock {
            source: ClockSource::Runtime(self.clone()),
        }
    }

    /// Transfers owned work to the provider without polling it in this wrapper.
    pub(crate) fn spawn(&self, task: RuntimeFuture) {
        self.driver.spawn(task);
    }

    /// Requests an owned wait using the provider's clock origin.
    pub(crate) fn sleep_until(&self, deadline: MonotonicInstant) -> RuntimeFuture {
        self.driver.sleep_until(deadline)
    }

    /// Requests a scheduling opportunity without moving local captures off-thread.
    pub(crate) fn yield_now(&self) -> RuntimeFuture {
        self.driver.yield_now()
    }
}

impl ClockDriver for Runtime {
    fn now(&self) -> MonotonicInstant {
        Self::now(self)
    }

    fn sleep_until(&self, deadline: MonotonicInstant) -> RuntimeFuture {
        Self::sleep_until(self, deadline)
    }

    fn capabilities(&self) -> ClockCapabilities {
        self.driver.clock_capabilities()
    }

    fn sleep_until_with_tolerance(
        &self,
        deadline: MonotonicInstant,
        tolerance: Duration,
    ) -> Result<RuntimeFuture, UnsupportedClockTolerance> {
        self.driver.sleep_until_with_tolerance(deadline, tolerance)
    }
}

/// Provides local monotonic time and waits without spawning authority.
///
/// Clones share their provider's origin, not an actor's timer ordering. Instants
/// from independent origins are not interchangeable. Retaining a clock does not
/// retain its host; using it does not make an external future Store-owned.
/// Clock operations propagate provider panics unchanged.
#[derive(Clone, Debug)]
pub struct Clock {
    /// Retains the existing runtime handle or a separately supplied clock provider.
    source: ClockSource,
}

/// Shares a clock interface without adding another allocation for runtime clocks.
#[derive(Clone, Debug)]
enum ClockSource {
    /// Preserves the existing driver and origin without exposing task submission.
    Runtime(Runtime),
    /// Erases an independent provider that has no spawning interface.
    Provider(Rc<dyn ClockDriver>),
}

impl Clock {
    /// Erases a clock-only provider on its owning thread without a spawner.
    ///
    /// The provider must satisfy [`ClockDriver`]. Independent providers cannot
    /// exchange instants unless they share an origin.
    #[must_use]
    pub fn new(provider: impl ClockDriver) -> Self {
        Self {
            source: ClockSource::Provider(Rc::new(provider)),
        }
    }

    /// Borrows the clock interface without allocating or cloning its owner.
    fn driver(&self) -> &dyn ClockDriver {
        match &self.source {
            ClockSource::Runtime(runtime) => runtime,
            ClockSource::Provider(provider) => provider.as_ref(),
        }
    }

    /// Returns declared timestamp resolution and advisory tolerance support.
    #[must_use]
    pub fn capabilities(&self) -> ClockCapabilities {
        self.driver().capabilities()
    }

    /// Returns current monotonic time from this clock's origin.
    #[must_use]
    pub fn now(&self) -> MonotonicInstant {
        self.driver().now()
    }

    /// Returns a wait whose deadline is fixed now, saturating at the maximum.
    ///
    /// Delayed first polling does not rebase the deadline. Dropping the future
    /// cancels the wait and releases its local captures on the owner thread.
    #[must_use = "sleep futures must be polled to wait for their deadline"]
    pub fn sleep(&self, duration: Duration) -> RuntimeFuture {
        self.sleep_until(self.now().saturating_add(duration))
    }

    /// Returns an owned wait for a deadline from this clock's origin.
    ///
    /// Elapsed deadlines are ready on first poll. Dropping the future cancels it
    /// and releases local captures on the owner thread, even before first polling.
    #[must_use = "sleep futures must be polled to wait for their deadline"]
    pub fn sleep_until(&self, deadline: MonotonicInstant) -> RuntimeFuture {
        self.driver().sleep_until(deadline)
    }

    /// Returns a wait with advisory slack clamped to the declared minimum.
    ///
    /// Slack never permits early completion or guarantees host wake latency.
    /// Providers may ignore it. The same-origin, elapsed-readiness and owner-thread
    /// cancellation requirements of [`Self::sleep_until`] still apply.
    ///
    /// # Errors
    ///
    /// Returns [`UnsupportedClockTolerance`] without creating a wait when support
    /// is not declared, even for elapsed deadlines or zero slack.
    pub fn sleep_until_with_tolerance(
        &self,
        deadline: MonotonicInstant,
        tolerance: Duration,
    ) -> Result<RuntimeFuture, UnsupportedClockTolerance> {
        match prepare_tolerant_wait(self.capabilities(), || self.now(), deadline, tolerance)? {
            // A zero-capture future needs no heap storage for this completed wait.
            None => Ok(Box::pin(std::future::poll_fn(|_| {
                std::task::Poll::Ready(())
            }))),
            Some(tolerance) => self
                .driver()
                .sleep_until_with_tolerance(deadline, tolerance),
        }
    }
}

#[cfg(test)]
ordinary_test_items! {
mod tests {
    use std::cell::{Cell, RefCell};
    use std::future::poll_fn;
    use std::task::{Context, Poll, Waker};
    use std::thread::{self, ThreadId};

    use futures_executor::LocalSpawner;
    use futures_util::task::LocalSpawnExt;

    use super::*;

    #[derive(Debug)]
    struct Driver {
        spawner: LocalSpawner,
        yields: Rc<Cell<usize>>,
    }

    impl RuntimeDriver for Driver {
        fn spawn(&self, task: RuntimeFuture) {
            self.spawner.spawn_local(task).expect("live test host");
        }

        fn now(&self) -> MonotonicInstant {
            MonotonicInstant::ZERO
        }

        fn sleep_until(&self, _: MonotonicInstant) -> RuntimeFuture {
            panic!("task fixture must not sleep")
        }

        fn yield_now(&self) -> RuntimeFuture {
            self.yields.set(self.yields.get() + 1);
            let capture = std::rc::Rc::clone(&self.yields);
            let owner = thread::current().id();
            let mut yielded = false;
            Box::pin(poll_fn(move |context| {
                assert_eq!(thread::current().id(), owner);
                assert!(capture.get() > 0);
                if yielded {
                    Poll::Ready(())
                } else {
                    yielded = true;
                    context.waker().wake_by_ref();
                    Poll::Pending
                }
            }))
        }
    }

    struct Records {
        owner: ThreadId,
        polls: Cell<usize>,
        drops: Cell<usize>,
        ready: Cell<bool>,
        waker: RefCell<Option<Waker>>,
    }

    impl Records {
        fn new() -> Rc<Self> {
            Rc::new(Self {
                owner: thread::current().id(),
                polls: Cell::new(0),
                drops: Cell::new(0),
                ready: Cell::new(false),
                waker: RefCell::new(None),
            })
        }
    }

    struct Task(Rc<Records>);

    impl Future for Task {
        type Output = ();

        fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
            assert_eq!(thread::current().id(), self.0.owner);
            self.0.polls.set(self.0.polls.get() + 1);
            if self.0.ready.get() {
                Poll::Ready(())
            } else {
                *self.0.waker.borrow_mut() = Some(cx.waker().clone());
                Poll::Pending
            }
        }
    }

    impl Drop for Task {
        fn drop(&mut self) {
            assert_eq!(thread::current().id(), self.0.owner);
            self.0.drops.set(self.0.drops.get() + 1);
            self.0.waker.borrow_mut().take();
        }
    }

    #[test]
    fn host_teardown_drops_unpolled_and_polled_rc_tasks_on_owner() {
        for poll_before_teardown in [false, true] {
            let mut host = futures_executor::LocalPool::new();
            let runtime = Runtime::new(Driver {
                spawner: host.spawner(),
                yields: Rc::default(),
            });
            let records = Records::new();
            runtime.spawn(Box::pin(Task(std::rc::Rc::clone(&records))));
            assert_eq!(records.polls.get(), 0, "spawn must not poll inline");
            if poll_before_teardown {
                host.run_until_stalled();
                assert_eq!(records.polls.get(), 1);
            }
            drop(host);
            assert_eq!(records.drops.get(), 1);
            assert_eq!(Rc::strong_count(&records), 1);
            assert_eq!(runtime.now(), MonotonicInstant::ZERO);
        }
    }

    #[test]
    fn remote_task_waker_leaves_poll_and_destruction_on_owner() {
        let mut host = futures_executor::LocalPool::new();
        let runtime = Runtime::new(Driver {
            spawner: host.spawner(),
            yields: Rc::default(),
        });
        let records = Records::new();
        runtime.clone().spawn(Box::pin(Task(std::rc::Rc::clone(&records))));
        host.run_until_stalled();
        let remote_waker = records.waker.borrow_mut().take().unwrap();
        thread::spawn(move || remote_waker.wake()).join().unwrap();
        assert_eq!(records.polls.get(), 1);
        assert_eq!(records.drops.get(), 0);
        records.ready.set(true);
        host.run_until_stalled();
        assert_eq!(records.polls.get(), 2);
        assert_eq!(records.drops.get(), 1);
        assert_eq!(Rc::strong_count(&records), 1);
    }

    #[test]
    fn local_yield_forwards_and_releases_rc_wait_captures() {
        let host = futures_executor::LocalPool::new();
        let yields = Rc::new(Cell::new(0));
        let runtime = Runtime::new(Driver {
            spawner: host.spawner(),
            yields: std::rc::Rc::clone(&yields),
        });
        drop(runtime.yield_now());
        assert_eq!(Rc::strong_count(&yields), 2);
        let mut wait = runtime.yield_now();
        let mut context = Context::from_waker(Waker::noop());
        assert!(wait.as_mut().poll(&mut context).is_pending());
        assert!(wait.as_mut().poll(&mut context).is_ready());
        drop(wait);
        assert_eq!(yields.get(), 2);
        assert_eq!(Rc::strong_count(&yields), 2);
    }
}
}
