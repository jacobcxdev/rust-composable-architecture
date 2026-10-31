//! Deterministic host for owner-local tasks and client futures.
//!
//! This host does not migrate local work. Only task wake notifications cross
//! threads; polls and destruction remain on the constructing owner. Runtime
//! handles retain scheduler data, not host authority.
//!
//! A stored task owns its captures for `'static`; a client passed to `run`
//! may borrow data and return non-Send values. Application-owned mutable Rc
//! aliases are not transactions or independent historical snapshots.
//!
//! ```
//! use std::rc::Rc;
//! use rca_test::local::DeterministicRuntime;
//! let host = DeterministicRuntime::new();
//! let value = Rc::new(42);
//! let result = futures_executor::block_on(host.run(async { Rc::clone(&value) })).unwrap();
//! assert_eq!(*result, 42);
//! fn send<T: Send>(_: T) {}
//! fn send_sync<T: Send + Sync>() {}
//! send_sync::<rca_test::DeterministicRuntime>();
//! let shared = rca_test::DeterministicRuntime::new();
//! send(shared.run(async { 42 }));
//! send(std::task::Waker::noop().clone());
//! ```
//!
//! ```compile_fail
//! fn send<T: Send>() {}
//! send::<rca_test::local::DeterministicRuntime>();
//! ```
//!
//! ```compile_fail
//! fn sync<T: Sync>() {}
//! sync::<rca_test::local::DeterministicRuntime>();
//! ```
//!
//! ```compile_fail
//! fn send<T: Send>(_: T) {}
//! let host = rca_test::local::DeterministicRuntime::new();
//! send(host.run(async { 42 }));
//! ```
//!
//! ```compile_fail
//! fn send<T: Send>(_: T) {}
//! let host = rca_test::local::DeterministicRuntime::new();
//! send(host.drain_deadlines(1));
//! ```
//!
//! ```compile_fail
//! let value = std::rc::Rc::new(42);
//! rca_test::DeterministicRuntime::new().spawn_task(Box::pin(async move { drop(value); }));
//! ```
//!
//! A Shared client must be Send, but a result created by its final poll need not
//! be. The host returns that value directly rather than transporting it.
//!
//! ```
//! let host = rca_test::DeterministicRuntime::new();
//! let value = futures_executor::block_on(host.run(async { std::rc::Rc::new(42) })).unwrap();
//! assert_eq!(*value, 42);
//! ```
//!
//! ```compile_fail
//! let host = rca_test::local::DeterministicRuntime::new();
//! let value = String::from("borrowed");
//! host.spawn_task(Box::pin(async { drop(&value); }));
//! ```

use crate::error::DriveFailure;
#[doc(inline)]
pub use crate::error::local::TestStoreError;
use crate::runtime::scheduler::{self, DriveState, Scheduler};
#[doc(inline)]
pub use crate::store::local::{TestStore, TestStoreBuilder};
#[doc(inline)]
pub use crate::store::local_captures::{CaptureExpectations, CaptureKey};
#[doc(inline)]
pub use crate::task::local::TestActionTask;
use crate::{DEFAULT_STEP_BUDGET, DriveError, DriveReport, PendingSleep};
use rca_core::local::{Runtime, RuntimeDriver, RuntimeFuture};
use rca_core::{ClockCapabilities, MonotonicInstant};
use std::fmt;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll};
use std::time::Duration;

/// The common policy with owner-local, potentially non-Send task payloads.
type SchedulerLocal = Scheduler<RuntimeFuture>;

/// Owns deterministic scheduling and virtual time on one thread.
///
/// Dropping the final host clone closes the scheduler. Retained [`rca_core::local::Runtime`] handles
/// then drop newly submitted tasks without polling them.
/// Owned sleeps retain virtual time data, not host authority. After host closure,
/// elapsed sleeps remain ready; unelapsed sleeps remain pending until dropped.
#[derive(Clone)]
pub struct DeterministicRuntime {
    /// Owner-local scheduler data shared with production runtime handles.
    driver: DeterministicDriver,
    /// Maximum task polls for one drive operation.
    step_budget: usize,
    /// Only host clones own authority; the final drop closes on this owner.
    _host_authority: Rc<HostAuthority>,
}

impl DeterministicRuntime {
    /// Creates runtime using [`DEFAULT_STEP_BUDGET`].
    #[must_use]
    pub fn new() -> Self {
        Self::with_step_budget(DEFAULT_STEP_BUDGET)
    }

    /// Creates runtime with positive task-poll budget per drive operation.
    ///
    /// # Panics
    ///
    /// Panics when `step_budget` is zero.
    #[must_use]
    pub fn with_step_budget(step_budget: usize) -> Self {
        Self::with_now_and_step_budget(MonotonicInstant::ZERO, step_budget)
    }

    /// Creates an independently owned runtime seeded at `now`.
    ///
    /// The seed declares elapsed time in this host's origin, not a global epoch.
    /// Pass handles from this host to both actors and clock clients. Identical
    /// seeds in different hosts do not make their origins interchangeable.
    #[must_use]
    pub fn with_now(now: MonotonicInstant) -> Self {
        Self::with_now_and_step_budget(now, DEFAULT_STEP_BUDGET)
    }

    /// Creates a seeded runtime with a positive task-poll budget.
    ///
    /// `now` declares elapsed time in this host's independent monotonic origin.
    ///
    /// # Panics
    ///
    /// Panics when `step_budget` is zero.
    #[must_use]
    pub fn with_now_and_step_budget(now: MonotonicInstant, step_budget: usize) -> Self {
        assert!(
            step_budget > 0,
            "deterministic step budget must exceed zero"
        );
        let shared = Rc::new(SchedulerLocal::new(now));
        Self {
            driver: DeterministicDriver {
                shared: Rc::clone(&shared),
            },
            step_budget,
            _host_authority: Rc::new(HostAuthority(shared)),
        }
    }

    /// Returns production runtime handle backed by deterministic driver.
    #[must_use]
    pub fn runtime(&self) -> Runtime {
        Runtime::new(self.driver.clone())
    }

    /// Returns task-poll budget used by each drive operation.
    #[must_use]
    pub const fn step_budget(&self) -> usize {
        self.step_budget
    }

    /// Returns current virtual monotonic time.
    #[must_use]
    pub fn now(&self) -> MonotonicInstant {
        self.driver.now()
    }

    /// Returns registered virtual sleeps in deadline order.
    #[must_use]
    pub fn pending_sleeps(&self) -> Vec<PendingSleep> {
        self.driver.shared.pending_sleeps()
    }

    /// Spawns one task for deterministic conformance and performance testing.
    #[doc(hidden)]
    pub fn spawn_task(&self, task: RuntimeFuture) {
        self.driver.spawn(task);
    }

    /// Creates one virtual sleep for deterministic conformance and performance testing.
    #[doc(hidden)]
    #[must_use]
    pub fn sleep_until(&self, deadline: MonotonicInstant) -> RuntimeFuture {
        self.driver.sleep_until(deadline)
    }

    /// Polls deterministic tasks until no task remains ready.
    ///
    /// # Errors
    ///
    /// Returns [`DriveError`] after exact configured poll budget is exhausted.
    pub fn run_until_stalled(&self) -> Result<DriveReport, DriveError> {
        let polls = self.drive_ready(0)?;
        Ok(self.report(polls))
    }

    /// Drains future deadlines within explicit task-poll and deadline-batch budgets.
    ///
    /// Driving starts only when the returned owned future is polled. It retains
    /// scheduler data, not host authority. Dropping it before polling does nothing.
    /// Each earliest-deadline cohort consumes one batch from `max_deadline_batches`;
    /// all task polls share this runtime's configured step budget. A zero deadline
    /// budget still permits ready work. No wall time or external wakes are awaited.
    /// Each task poll must return; budgets cannot preempt a blocking user future.
    /// Use only one host drive operation at a time.
    ///
    /// Success requires no live scheduler tasks or registered timers. An idle actor
    /// or assertion-parked actor is blocked, not complete. This never releases
    /// assertion gates, acknowledges actions, or consumes a partial `TestStore` advance.
    ///
    /// # Examples
    ///
    /// ```
    /// # futures_executor::block_on(async {
    /// use rca_test::local::DeterministicRuntime;
    /// let host = DeterministicRuntime::new();
    /// let report = host.drain_deadlines(4).await.unwrap();
    /// assert_eq!(report.deadline_batches(), 0);
    /// host.assert_no_pending_time().unwrap();
    /// # });
    /// ```
    ///
    /// # Errors
    ///
    /// Returns [`DriveError`] for task-poll or deadline-batch exhaustion, blocked
    /// tasks, host closure, or an existing partial virtual-time advance. Failures
    /// report progress. While the host remains live, failure retains unfinished
    /// work rather than cancelling or finishing it. Host closure instead tears
    /// down scheduler tasks.
    pub fn drain_deadlines(
        &self,
        max_deadline_batches: usize,
    ) -> impl Future<Output = Result<DriveReport, DriveError>> + 'static + use<> {
        let driver = self.driver.clone();
        let step_budget = self.step_budget;
        async move {
            driver.drain_deadlines(
                step_budget,
                max_deadline_batches,
                #[cfg(test)]
                None,
            )
        }
    }

    /// Verifies no registered virtual timers remain, without driving any work.
    ///
    /// This inspects only the registration queue. Unpolled waits, blocked tasks,
    /// pending assertions, and task completion are not verified. It never advances
    /// time or polls tasks and does not replace [`crate::TestStore::finish`].
    ///
    /// # Errors
    ///
    /// Returns [`DriveError`] with pending timer identities and deadlines.
    pub fn assert_no_pending_time(&self) -> Result<(), DriveError> {
        let snapshot = self.driver.shared.snapshot();
        if snapshot.pending_sleeps.is_empty() {
            return Ok(());
        }
        Err(snapshot.temporal_error(DriveFailure::PendingTime, 0, self.step_budget, 0, None))
    }

    /// Advances virtual time and drains every deadline due by target.
    ///
    /// # Errors
    ///
    /// Returns [`DriveError`] after exact configured poll budget is exhausted.
    pub fn advance(&self, duration: Duration) -> Result<DriveReport, DriveError> {
        self.start_advance(duration);
        let result = self.resume_advance();
        self.finish_advance();
        result
    }

    /// Advances to an absolute target, draining newly registered intermediate deadlines.
    ///
    /// `target` must use this host's origin. A past target is clamped to current
    /// time and still drains ready tasks; time never moves backwards. Like
    /// [`Self::advance`], this explicitly drives tasks rather than granting a
    /// clock handle host control. Budget exhaustion leaves time at the last
    /// processed deadline, and a later call can continue towards its own target.
    ///
    /// # Errors
    ///
    /// Returns [`DriveError`] after the configured task-poll budget is exhausted.
    pub fn advance_to(&self, target: MonotonicInstant) -> Result<DriveReport, DriveError> {
        self.driver.shared.start_advance_to(target);
        let result = self.resume_advance();
        self.finish_advance();
        result
    }

    /// Starts or extends the partial advance retained by `TestStore` assertions.
    pub(crate) fn start_advance(&self, duration: Duration) {
        self.driver.shared.start_advance(duration);
    }

    /// Drives the retained target without clearing it at an assertion boundary.
    pub(crate) fn resume_advance(&self) -> Result<DriveReport, DriveError> {
        self.driver.shared.resume_advance(self.step_budget)
    }

    /// Clears a completed or explicitly abandoned advance target.
    pub(crate) fn finish_advance(&self) {
        self.driver.shared.finish_advance();
    }

    /// Captures ready identities for owned diagnostics, including panic debt.
    pub(crate) fn ready_task_ids(&self) -> Vec<u64> {
        self.driver.shared.ready_task_ids()
    }

    /// Reports whether a `TestStore` advance still owns a target.
    pub(crate) fn has_active_advance(&self) -> bool {
        self.driver.shared.active_target().is_some()
    }

    /// Detaches queued tasks for `TestStore` teardown without closing this host.
    pub(crate) fn discard_all_tasks(&self) {
        self.driver.shared.discard_all_tasks();
    }

    /// Polls a client and ready tasks without waiting for external notification.
    ///
    /// `polls` carries a shared operation budget across repeated teardown calls.
    pub(crate) fn run_ready<F: Future>(
        &self,
        future: F,
        polls: &mut usize,
    ) -> Result<Option<F::Output>, DriveError> {
        self.driver
            .shared
            .run_ready(future, polls, self.step_budget)
    }

    /// Runs client future while polling deterministic RCA tasks.
    ///
    /// # Errors
    ///
    /// Returns [`DriveError`] after exact configured poll budget is exhausted.
    pub async fn run<F>(&self, future: F) -> Result<F::Output, DriveError>
    where
        F: Future,
    {
        let mut drive = self.drive(future);
        let result = (&mut drive).await;
        // Retain the owner-local result if client or listener destruction panics.
        drop(drive);
        result
    }

    /// Keeps owner-local host authority alive while driving a borrowing client.
    pub(crate) fn drive<F>(&self, future: F) -> Drive<F>
    where
        F: Future,
    {
        Drive {
            runtime: self.clone(),
            state: DriveState::new(future),
        }
    }

    /// Drains ready batches using the caller's accumulated poll count.
    fn drive_ready(&self, polls: usize) -> Result<usize, DriveError> {
        self.driver.shared.drive_ready(polls, self.step_budget)
    }

    /// Reports a ready-only operation with no consumed deadline cohorts.
    fn report(&self, polls: usize) -> DriveReport {
        self.driver.shared.report(polls)
    }
}

impl Default for DeterministicRuntime {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Debug for DeterministicRuntime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let snapshot = self.driver.shared.snapshot();
        f.debug_struct("DeterministicRuntime")
            .field("step_budget", &self.step_budget)
            .field("ready_task_ids", &snapshot.ready_task_ids)
            .field("pending_sleeps", &snapshot.pending_sleeps)
            .field("current_time", &snapshot.current_time)
            .finish_non_exhaustive()
    }
}

/// Closes scheduler admission on the owner when its final host clone drops.
struct HostAuthority(
    /// Scheduler closed on its owner when this final host authority is destroyed.
    Rc<SchedulerLocal>,
);

impl Drop for HostAuthority {
    fn drop(&mut self) {
        self.0.close();
    }
}

/// Owner-local client driver; the boxed client stays pinned between polls.
pub(crate) struct Drive<F> {
    /// Retains owner-local host authority for the full client drive.
    runtime: DeterministicRuntime,
    /// Client pin, accumulated poll count and stalled-drive notification.
    state: DriveState<F>,
}

impl<F> Future for Drive<F>
where
    F: Future,
{
    type Output = Result<F::Output, DriveError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        this.state
            .poll(cx, &this.runtime.driver.shared, this.runtime.step_budget)
    }
}

/// Production adapter that retains owner-local data, not host authority.
#[derive(Clone)]
pub(crate) struct DeterministicDriver {
    /// `Rc` ownership prevents polling or dropping Local tasks on another thread.
    shared: Rc<SchedulerLocal>,
}

impl DeterministicDriver {
    /// Runs the common bounded deadline drain; the hook is test-only.
    fn drain_deadlines(
        &self,
        step_budget: usize,
        max_deadline_batches: usize,
        #[cfg(test)] before_take_deadline: Option<Box<dyn FnOnce() + Send>>,
    ) -> Result<DriveReport, DriveError> {
        self.shared.drain_deadlines(
            step_budget,
            max_deadline_batches,
            #[cfg(test)]
            before_take_deadline,
        )
    }
}

impl RuntimeDriver for DeterministicDriver {
    fn spawn(&self, task: RuntimeFuture) {
        self.shared.spawn(task);
    }

    fn now(&self) -> MonotonicInstant {
        self.shared.now()
    }

    fn sleep_until(&self, deadline: MonotonicInstant) -> RuntimeFuture {
        Box::pin(self.shared.sleep_until(deadline))
    }

    fn clock_capabilities(&self) -> ClockCapabilities {
        // Duration represents virtual timestamps to one nanosecond. This is
        // not a statement about host precision or task scheduling latency.
        // Tolerance is supported as advisory slack and ignored by exact waits.
        ClockCapabilities {
            resolution: Some(Duration::from_nanos(1)),
            minimum_tolerance: Some(Duration::ZERO),
        }
    }

    fn yield_now(&self) -> RuntimeFuture {
        Box::pin(scheduler::yield_now())
    }
}

impl fmt::Debug for DeterministicDriver {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DeterministicDriver")
            .finish_non_exhaustive()
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::task::Waker;

    struct OnDrop(Option<Box<dyn FnOnce()>>);
    impl Drop for OnDrop {
        fn drop(&mut self) {
            if let Some(callback) = self.0.take() {
                callback();
            }
        }
    }

    #[test]
    fn completed_local_client_output_is_released_if_client_destruction_panics() {
        struct Output {
            owner: std::thread::ThreadId,
            drops: Rc<Cell<usize>>,
        }

        impl Drop for Output {
            fn drop(&mut self) {
                assert_eq!(self.owner, std::thread::current().id());
                self.drops.set(self.drops.get() + 1);
            }
        }

        struct Client(Option<Output>);

        impl Future for Client {
            type Output = Output;

            fn poll(mut self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Output> {
                Poll::Ready(self.0.take().expect("client is polled once"))
            }
        }

        impl Drop for Client {
            fn drop(&mut self) {
                panic!("completed local client destructor");
            }
        }

        let runtime = DeterministicRuntime::new();
        let drops = Rc::new(Cell::new(0));
        let client = Client(Some(Output {
            owner: std::thread::current().id(),
            drops: Rc::clone(&drops),
        }));
        let panic = catch_unwind(AssertUnwindSafe(|| {
            drop(futures_executor::block_on(runtime.run(client)));
        }))
        .expect_err("client destruction propagates its original panic");

        assert_eq!(
            panic.downcast_ref::<&str>(),
            Some(&"completed local client destructor")
        );
        assert_eq!(drops.get(), 1);
    }

    #[test]
    fn retained_driver_drops_closed_host_submissions_unpolled_on_owner() {
        let owner = std::thread::current().id();
        let host = DeterministicRuntime::new();
        let driver = host.driver.clone();
        let runtime = host.runtime();
        let weak = Rc::downgrade(&driver.shared);
        let drops = Rc::new(Cell::new(0));
        let nested_drops = Rc::clone(&drops);
        let reentrant = driver.clone();
        let guard = OnDrop(Some(Box::new(move || {
            assert_eq!(owner, std::thread::current().id());
            drop(
                reentrant
                    .shared
                    .wake
                    .state
                    .try_lock()
                    .expect("metadata unlocked"),
            );
            drop(
                reentrant
                    .shared
                    .tasks
                    .try_lock()
                    .expect("payloads unlocked"),
            );
            nested_drops.set(nested_drops.get() + 1);
            let output = Rc::clone(&nested_drops);
            let nested = OnDrop(Some(Box::new(move || {
                assert_eq!(owner, std::thread::current().id());
                output.set(output.get() + 1);
            })));
            reentrant.spawn(Box::pin(async move {
                let _guard = nested;
                panic!("closed nested task is never polled");
            }));
        })));
        drop(host);
        driver.spawn(Box::pin(async move {
            let _guard = guard;
            panic!("closed task is never polled");
        }));
        assert_eq!(drops.get(), 2);
        assert_eq!(driver.shared.snapshot().live_task_ids, [] as [u64; 0]);
        drop(driver);
        assert!(
            weak.upgrade().is_some(),
            "retained Runtime owns closed data, not host authority"
        );
        drop(runtime);
        assert!(weak.upgrade().is_none());
    }

    #[test]
    fn reentrant_completed_task_drop_can_cancel_spawn_and_drive() {
        let owner = std::thread::current().id();
        let host = DeterministicRuntime::new();
        let reentrant = host.clone();
        let count = Rc::new(Cell::new(0));
        let output = Rc::clone(&count);
        let mut sleep = host.sleep_until(MonotonicInstant::MAX);
        assert!(
            sleep
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending()
        );
        let guard = OnDrop(Some(Box::new(move || {
            assert_eq!(owner, std::thread::current().id());
            drop(
                reentrant
                    .driver
                    .shared
                    .wake
                    .state
                    .try_lock()
                    .expect("metadata unlocked"),
            );
            drop(
                reentrant
                    .driver
                    .shared
                    .tasks
                    .try_lock()
                    .expect("payloads unlocked"),
            );
            assert_eq!(reentrant.now(), MonotonicInstant::ZERO);
            drop(sleep);
            assert_eq!(reentrant.pending_sleeps(), [] as [crate::PendingSleep; 0]);
            reentrant.spawn_task(Box::pin(async move {
                output.set(1);
            }));
            assert_eq!(reentrant.run_until_stalled().unwrap().polls(), 1);
        })));
        host.spawn_task(Box::pin(async move {
            drop(guard);
        }));
        assert_eq!(host.run_until_stalled().unwrap().polls(), 1);
        assert_eq!(count.get(), 1);
    }

    #[test]
    fn poll_panic_preserves_diagnostic_debt_and_owner_drop_in_both_host_orders() {
        for close_host in [false, true] {
            let owner = std::thread::current().id();
            let host = DeterministicRuntime::new();
            let scheduler = Rc::clone(&host.driver.shared);
            let drops = Rc::new(Cell::new(0));
            let recorded = Rc::new(RefCell::new(None));
            let mut last_host = close_host.then(|| host.clone());
            for id in 0..2 {
                let data = Rc::clone(&scheduler);
                let output = Rc::clone(&drops);
                let guard = OnDrop(Some(Box::new(move || {
                    assert_eq!(owner, std::thread::current().id());
                    drop(data.wake.state.try_lock().expect("metadata unlocked"));
                    drop(data.tasks.try_lock().expect("payloads unlocked"));
                    output.set(output.get() + 1);
                })));
                let mut task_host = last_host.take();
                let captured = Rc::clone(&recorded);
                host.spawn_task(Box::pin(std::future::poll_fn(move |cx| -> Poll<()> {
                    let _ = &guard;
                    assert_eq!(owner, std::thread::current().id());
                    assert_eq!(id, 0, "second detached task never polled");
                    *captured.borrow_mut() = Some(cx.waker().clone());
                    drop(task_host.take());
                    panic!("original local poll panic");
                })));
            }
            let retained = (!close_host).then_some(host);
            let panic =
                catch_unwind(AssertUnwindSafe(|| scheduler.poll_ready_batch(2))).unwrap_err();
            assert_eq!(
                panic.downcast_ref::<&str>(),
                Some(&"original local poll panic")
            );
            assert_eq!(drops.get(), 2);
            assert!(scheduler.tasks.lock().is_empty());
            let before = scheduler.snapshot();
            assert_eq!(
                before.live_task_ids,
                if close_host { vec![] } else { vec![0, 1] }
            );
            let wake = recorded.borrow_mut().take().unwrap();
            std::thread::spawn(move || {
                for _ in 0..10 {
                    wake.wake_by_ref();
                }
            })
            .join()
            .unwrap();
            let after = scheduler.snapshot();
            assert_eq!(after.live_task_ids, before.live_task_ids);
            assert_eq!(
                after.ready_task_ids,
                if close_host { vec![] } else { vec![0] }
            );
            assert_eq!(scheduler.poll_ready_batch(10), 0);
            assert_eq!(drops.get(), 2);
            drop(retained);
        }
    }

    #[test]
    fn one_normal_close_destructor_panic_still_releases_non_panicking_siblings() {
        let owner = std::thread::current().id();
        let host = DeterministicRuntime::new();
        let driver = host.driver.clone();
        let drops = Rc::new(Cell::new(0));
        for id in 0..3 {
            let data = Rc::clone(&driver.shared);
            let output = Rc::clone(&drops);
            let guard = OnDrop(Some(Box::new(move || {
                assert_eq!(owner, std::thread::current().id());
                drop(data.wake.state.try_lock().expect("metadata unlocked"));
                drop(data.tasks.try_lock().expect("payloads unlocked"));
                assert_eq!(data.snapshot().live_task_ids, [] as [u64; 0]);
                output.set(output.get() + 1);
                assert_ne!(id, 0, "one destructor panic");
            })));
            host.spawn_task(Box::pin(async move {
                let _guard = guard;
                std::future::pending::<()>().await;
            }));
        }
        assert!(catch_unwind(AssertUnwindSafe(|| drop(host))).is_err());
        assert_eq!(drops.get(), 3);
        assert!(driver.shared.tasks.lock().is_empty());
        assert_eq!(driver.shared.snapshot().live_task_ids, [] as [u64; 0]);
        driver.drain_deadlines(10, 1, None).unwrap_err();
    }
}
