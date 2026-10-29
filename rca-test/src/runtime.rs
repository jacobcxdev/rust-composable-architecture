//! Schedules production RCA futures deterministically.

use std::fmt;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use rca_core::{ClockCapabilities, MonotonicInstant, Runtime, RuntimeDriver, RuntimeFuture};

use crate::error::DriveFailure;
use crate::{DriveError, PendingSleep};

#[cfg(all(test, loom))]
use loom::sync::Arc as HostArc;
#[cfg(not(all(test, loom)))]
use std::sync::Arc as HostArc;

/// Default number of deterministic task polls allowed per drive operation.
///
/// This permits the existing 100,000-poll finite workload while bounding a
/// self-waking task. Callers can choose a smaller or larger explicit budget.
pub const DEFAULT_STEP_BUDGET: usize = 100_000;

/// Maximum tasks detached before the client future gets another polling turn.
///
/// The existing 256-task workloads exercise this batching boundary. It bounds
/// temporary ownership and amortises locking; it is not a measured optimal size.
/// Changing it can change when client code runs between scheduler task batches.
const READY_BATCH_LIMIT: usize = 256;

pub(crate) mod scheduler;
use scheduler::{DriveState, Scheduler};

/// Shared scheduler storage accepts futures that can move between threads.
type SchedulerShared = Scheduler<RuntimeFuture>;

/// Reports work performed by one deterministic drive operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DriveReport {
    /// Task polls consumed, excluding polls of the driving client future.
    polls: usize,
    /// Earliest-deadline cohorts removed from the timer queue.
    deadline_batches: usize,
    /// Virtual clock value observed when this report was made.
    current_time: MonotonicInstant,
}

impl DriveReport {
    /// Returns scheduler task polls performed.
    #[must_use]
    pub const fn polls(self) -> usize {
        self.polls
    }

    /// Returns processed deadline cohorts; equal deadlines share one batch.
    #[must_use]
    pub const fn deadline_batches(self) -> usize {
        self.deadline_batches
    }

    /// Returns virtual time after operation.
    #[must_use]
    pub const fn current_time(self) -> MonotonicInstant {
        self.current_time
    }
}

/// Owns deterministic scheduling and virtual monotonic time.
///
/// Dropping the final host clone closes the scheduler. Retained [`rca_core::Runtime`] handles
/// then drop newly submitted tasks without polling them, closing their Stores.
/// Owned sleeps retain virtual time data, not host authority. After host closure,
/// elapsed sleeps remain ready; unelapsed sleeps remain pending until dropped.
#[derive(Clone)]
pub struct DeterministicRuntime {
    /// Scheduler data shared with production runtime handles.
    pub(crate) driver: DeterministicDriver,
    /// Maximum task polls for a single drive operation.
    step_budget: usize,
    /// Only host clones own this authority; its last drop closes the scheduler.
    _host_authority: HostArc<HostAuthority>,
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
        let shared = Arc::new(SchedulerShared::new(now));
        Self {
            driver: DeterministicDriver {
                shared: Arc::clone(&shared),
            },
            step_budget,
            _host_authority: HostArc::new(HostAuthority(shared)),
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
    /// use rca_test::DeterministicRuntime;
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
    ) -> impl Future<Output = Result<DriveReport, DriveError>> + Send + 'static + use<> {
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

    /// Reports whether a `TestStore` advance still owns a target.
    pub(crate) fn has_active_advance(&self) -> bool {
        self.driver.shared.active_target().is_some()
    }

    /// Clears a completed or explicitly abandoned advance target.
    pub(crate) fn finish_advance(&self) {
        self.driver.shared.finish_advance();
    }

    /// Captures ready identities for owned diagnostics, including panic debt.
    pub(crate) fn ready_task_ids(&self) -> Vec<u64> {
        self.driver.shared.ready_task_ids()
    }

    /// Detaches queued tasks for `TestStore` teardown without closing this host.
    pub(crate) fn discard_all_tasks(&self) {
        self.driver.shared.discard_all_tasks();
    }

    /// Runs client future while polling deterministic RCA tasks.
    ///
    /// The client may borrow data and must be Send. Its result need not be Send:
    /// it is returned directly, never placed in scheduler task storage.
    ///
    /// # Errors
    ///
    /// Returns [`DriveError`] after exact configured poll budget is exhausted.
    pub async fn run<F>(&self, future: F) -> Result<F::Output, DriveError>
    where
        F: Future + Send,
    {
        let mut drive = self.drive(future);
        let result = (&mut drive).await;
        // Retain the result locally if client or listener destruction panics.
        drop(drive);
        result
    }

    /// Keeps host authority alive while the client and scheduler are polled.
    pub(crate) fn drive<F>(&self, future: F) -> Drive<F>
    where
        F: Future + Send,
    {
        Drive {
            runtime: self.clone(),
            state: DriveState::new(future),
        }
    }

    // Unlike Drive, this never waits for an external wake. None preserves the
    // caller's unfinished obligation when ready work cannot answer the request.
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

// The last host owns teardown, independently of driver handles retained by tasks.
// Arc invokes this destructor exactly once, including concurrent final host drops.
/// Closes scheduler admission once the final host clone is gone.
struct HostAuthority(
    /// Scheduler closed when this final host-owned authority is destroyed.
    Arc<SchedulerShared>,
);

impl Drop for HostAuthority {
    fn drop(&mut self) {
        self.0.close();
    }
}

#[cfg(all(test, loom))]
mod host_teardown_tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::DeterministicRuntime;

    struct DropProbe(Arc<AtomicUsize>);

    impl Drop for DropProbe {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn concurrent_final_hosts_drop_pending_task_once() {
        loom::model(|| {
            let host = DeterministicRuntime::new();
            let other_host = host.clone();
            let runtime = host.runtime();
            let drops = Arc::new(AtomicUsize::new(0));
            let probe = DropProbe(Arc::clone(&drops));
            host.spawn_task(Box::pin(async move {
                let _probe = probe;
                std::future::pending::<()>().await;
            }));
            host.run_until_stalled().expect("pending task stalls");
            drop(host.clone());
            assert_eq!(drops.load(Ordering::SeqCst), 0);

            let first = loom::thread::spawn(move || drop(host));
            let second = loom::thread::spawn(move || drop(other_host));
            first.join().expect("first host drops");
            second.join().expect("second host drops");

            assert_eq!(drops.load(Ordering::SeqCst), 1);
            drop(runtime);
            assert_eq!(drops.load(Ordering::SeqCst), 1);
        });
    }

    #[test]
    fn admission_racing_final_host_drop_releases_task_once() {
        use rca_core::RuntimeDriver as _;

        loom::model(|| {
            let host = DeterministicRuntime::new();
            let retained_runtime = host.runtime();
            let driver = host.driver.clone();
            let drops = Arc::new(AtomicUsize::new(0));
            let probe = DropProbe(Arc::clone(&drops));

            let teardown = loom::thread::spawn(move || drop(host));
            let admission = loom::thread::spawn(move || {
                driver.spawn(Box::pin(async move {
                    let _probe = probe;
                    std::future::pending::<()>().await;
                }));
            });
            teardown.join().expect("host drops");
            admission.join().expect("admission returns");

            assert_eq!(drops.load(Ordering::SeqCst), 1);
            drop(retained_runtime);
            assert_eq!(drops.load(Ordering::SeqCst), 1);
        });
    }

    #[test]
    fn owned_sleep_drop_racing_deadline_drain_leaves_no_pending_time() {
        use std::task::{Context, Poll, Waker};
        use std::time::Duration;

        use rca_core::MonotonicInstant;

        loom::model(|| {
            let host = DeterministicRuntime::with_step_budget(1);
            let scheduler = Arc::downgrade(&host.driver.shared);
            let deadline = MonotonicInstant::from_elapsed(Duration::from_secs(1));
            let mut context = Context::from_waker(Waker::noop());
            let mut wait = host.sleep_until(deadline);
            assert!(wait.as_mut().poll(&mut context).is_pending());
            assert_eq!(host.pending_sleeps().len(), 1);

            // Keep host authority alive while cancellation races the separate scheduler locks.
            let cancel = loom::thread::spawn(move || drop(wait));
            let mut drain = Box::pin(host.drain_deadlines(1));
            let Poll::Ready(Ok(report)) = drain.as_mut().poll(&mut context) else {
                panic!("one bounded drain must complete");
            };
            cancel.join().expect("owned sleep drops");

            assert_eq!(report.polls(), 0);
            match report.deadline_batches() {
                0 => assert_eq!(report.current_time(), MonotonicInstant::ZERO),
                1 => assert_eq!(report.current_time(), deadline),
                _ => panic!("only one deadline exists"),
            }
            assert_eq!(host.now(), report.current_time());
            assert!(host.pending_sleeps().is_empty());

            drop(drain);
            drop(host);
            assert!(scheduler.upgrade().is_none());
        });
    }
}

/// Movable client driver whose boxed future stays pinned between polls.
pub(crate) struct Drive<F> {
    /// Retains host authority for the full client drive.
    runtime: DeterministicRuntime,
    /// Client pin, accumulated poll count and stalled-drive notification.
    state: DriveState<F>,
}

impl<F> Future for Drive<F>
where
    F: Future + Send,
{
    type Output = Result<F::Output, DriveError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        this.state
            .poll(cx, &this.runtime.driver.shared, this.runtime.step_budget)
    }
}

/// Production adapter that retains scheduler data but not host authority.
#[derive(Clone)]
pub(crate) struct DeterministicDriver {
    /// Closed data can outlive the host through `Runtime` and clock handles.
    shared: Arc<SchedulerShared>,
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

#[cfg(test)]
mod tests {
    use std::pin::Pin;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::task::{Context, Poll, Waker};
    use std::time::Duration;

    use rca_core::{MonotonicInstant, RuntimeDriver};

    use super::{DEFAULT_STEP_BUDGET, DeterministicRuntime, scheduler};

    #[test]
    fn reserved_task_slot_keeps_node_and_batch_entry_size() {
        use std::mem::size_of;

        assert_eq!(
            size_of::<Option<super::scheduler::ScheduledTask<rca_core::RuntimeFuture>>>(),
            size_of::<super::scheduler::ScheduledTask<rca_core::RuntimeFuture>>()
        );
        assert_eq!(
            size_of::<(
                u64,
                Option<super::scheduler::ScheduledTask<rca_core::RuntimeFuture>>
            )>(),
            size_of::<(
                u64,
                super::scheduler::ScheduledTask<rca_core::RuntimeFuture>
            )>()
        );
    }

    #[test]
    fn host_closed_during_poll_drops_detached_pending_task_instead_of_reinserting_it() {
        let host = DeterministicRuntime::new();
        let driver = host.driver.clone();
        let scheduler = Arc::downgrade(&driver.shared);
        let wait = host.sleep_until(MonotonicInstant::MAX);
        let resource = Arc::new(());
        let weak_resource = Arc::downgrade(&resource);
        driver.spawn(Box::pin(async move {
            drop(host);
            wait.await;
            drop(resource);
        }));
        assert_eq!(driver.shared.poll_ready_batch(1), 1);
        assert!(weak_resource.upgrade().is_none());
        assert!(scheduler::lock(&driver.shared.tasks).is_empty());
        drop(driver);
        assert!(scheduler.upgrade().is_none());
    }

    #[test]
    fn reentrant_drive_skips_current_and_reserved_tasks() {
        let runtime = DeterministicRuntime::new();
        let driver = runtime.driver.clone();
        let observations = Arc::new(Mutex::new(Vec::new()));
        let first = Arc::clone(&observations);
        runtime.spawn_task(Box::pin(async move {
            first.lock().unwrap().push(0);
            driver.shared.wake.mark_ready(0);
            driver.shared.wake.mark_ready(1);
            assert!(!driver.shared.has_runnable_task());
            assert_eq!(driver.shared.poll_ready_batch(2), 0);
        }));
        let second = Arc::clone(&observations);
        runtime.spawn_task(Box::pin(async move {
            second.lock().unwrap().push(1);
        }));

        assert_eq!(runtime.driver.shared.poll_ready_batch(2), 2);
        assert_eq!(*observations.lock().unwrap(), [0, 1]);
        assert!(scheduler::lock(&runtime.driver.shared.tasks).is_empty());
    }

    #[test]
    fn discard_during_poll_does_not_repair_live_ids() {
        struct CountDrop(Arc<AtomicUsize>);
        impl Drop for CountDrop {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }

        let runtime = DeterministicRuntime::new();
        let driver = runtime.driver.clone();
        let drops = Arc::new(AtomicUsize::new(0));
        let first = CountDrop(Arc::clone(&drops));
        runtime.spawn_task(Box::pin(std::future::poll_fn(move |_| {
            let _ = &first;
            driver.shared.discard_all_tasks();
            Poll::Pending
        })));
        let second = CountDrop(Arc::clone(&drops));
        runtime.spawn_task(Box::pin(std::future::poll_fn(move |_| {
            let _ = &second;
            Poll::Pending
        })));

        assert_eq!(runtime.driver.shared.poll_ready_batch(2), 2);
        let state = scheduler::lock(&runtime.driver.shared.wake.state);
        assert_eq!(
            scheduler::lock(&runtime.driver.shared.tasks)
                .keys()
                .copied()
                .collect::<Vec<_>>(),
            [0, 1]
        );
        assert!(state.live_tasks.is_empty());
        assert!(state.ready_tasks.is_empty());
        drop(state);
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        runtime.driver.shared.discard_all_tasks();
        assert_eq!(drops.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn escaped_poll_panic_drops_batch_outside_scheduler_lock() {
        use futures_util::FutureExt as _;

        struct DropOutsideLock(Arc<super::SchedulerShared>, Arc<[AtomicUsize; 2]>, usize);
        impl Drop for DropOutsideLock {
            fn drop(&mut self) {
                drop(self.0.wake.state.try_lock().unwrap());
                drop(self.0.tasks.try_lock().unwrap());
                self.1[self.2].fetch_add(1, Ordering::SeqCst);
            }
        }

        for close_host in [false, true] {
            let runtime = DeterministicRuntime::new();
            let scheduler = Arc::clone(&runtime.driver.shared);
            let drops = Arc::new([AtomicUsize::new(0), AtomicUsize::new(0)]);
            let polls = Arc::new([AtomicUsize::new(0), AtomicUsize::new(0)]);
            let mut task_host = Some(runtime.clone());
            for id in 0..2 {
                let guard = DropOutsideLock(Arc::clone(&scheduler), Arc::clone(&drops), id);
                let task_host = task_host.take();
                let polls = Arc::clone(&polls);
                runtime.spawn_task(Box::pin(async move {
                    let _guard = guard;
                    drop(task_host);
                    polls[id].fetch_add(1, Ordering::SeqCst);
                    assert!(id != 0, "poll panic fixture");
                    std::future::pending::<()>().await;
                }));
            }
            // The owned drive retains scheduler data, not host authority.
            let drain = runtime.drain_deadlines(0);
            let retained_host = (!close_host).then_some(runtime);
            let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                drain.now_or_never().expect("one poll reaches the panic")
            }))
            .unwrap_err();
            assert_eq!(panic.downcast_ref::<&str>(), Some(&"poll panic fixture"));
            let state = scheduler::lock(&scheduler.wake.state);
            assert_eq!(state.closed, close_host);
            assert!(scheduler::lock(&scheduler.tasks).is_empty());
            if close_host {
                assert!(state.live_tasks.is_empty());
            } else {
                assert_eq!(state.live_tasks.iter().copied().collect::<Vec<_>>(), [0, 1]);
            }
            assert!(state.ready_tasks.is_empty());
            drop(state);
            assert_eq!(
                polls.each_ref().map(|count| count.load(Ordering::SeqCst)),
                [1, 0]
            );
            assert_eq!(
                drops.each_ref().map(|count| count.load(Ordering::SeqCst)),
                [1, 1]
            );
            drop(retained_host);
        }
    }

    #[test]
    fn deadline_drain_refuses_to_consume_an_existing_partial_advance() {
        let host = DeterministicRuntime::new();
        host.start_advance(Duration::from_secs(2));
        let error = futures_executor::block_on(host.drain_deadlines(1)).unwrap_err();
        assert!(error.is_partial_advance());
        assert!(!error.is_blocked());
        let display = error.to_string();
        assert!(display.starts_with("deterministic scheduler existing partial"));
        assert!(display.ends_with(&error.backtrace().to_string()));
        assert!(std::error::Error::source(&error).is_none());
        assert_eq!(error.current_time(), MonotonicInstant::ZERO);
        assert_eq!(
            error.active_target(),
            Some(MonotonicInstant::from_elapsed(Duration::from_secs(2)))
        );
        assert!(host.has_active_advance());
        assert_eq!(
            host.resume_advance().unwrap().current_time(),
            MonotonicInstant::from_elapsed(Duration::from_secs(2))
        );
        host.finish_advance();
        assert_eq!(error.to_string(), display);
    }

    struct SleepWake {
        host: DeterministicRuntime,
        drops: Arc<AtomicUsize>,
        reentries: Arc<AtomicUsize>,
    }

    #[expect(
        clippy::manual_noop_waker,
        reason = "the destructor probes scheduler re-entry; a noop waker has no owned payload"
    )]
    impl std::task::Wake for SleepWake {
        fn wake(self: Arc<Self>) {}
    }

    impl Drop for SleepWake {
        #[cfg_attr(
            not(loom),
            expect(
                clippy::map_unwrap_or,
                reason = "The lock probe consumes either parking_lot's Option or Loom's Result and drops each guard before re-entry"
            )
        )]
        fn drop(&mut self) {
            self.drops.fetch_add(1, Ordering::SeqCst);
            // Diagnose the old self-deadlock without hanging or poisoning state.
            if self
                .host
                .driver
                .shared
                .wake
                .state
                .try_lock()
                .map(|_| true)
                .unwrap_or(false)
                && self
                    .host
                    .driver
                    .shared
                    .tasks
                    .try_lock()
                    .map(|_| true)
                    .unwrap_or(false)
            {
                let _now = self.host.now();
                self.reentries.fetch_add(1, Ordering::SeqCst);
            }
        }
    }

    #[cfg(not(loom))]
    #[test]
    fn deadline_drain_rechecks_after_last_registered_sleep_is_dropped() {
        struct CancelledSleepWake {
            _drop_probe: SleepWake,
        }
        impl std::task::Wake for CancelledSleepWake {
            fn wake(self: Arc<Self>) {
                panic!("a cancelled sleep must not be woken");
            }
        }

        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<DeterministicRuntime>();

        let host = DeterministicRuntime::new();
        let weak_scheduler = Arc::downgrade(&host.driver.shared);
        let drops = Arc::new(AtomicUsize::new(0));
        let reentries = Arc::new(AtomicUsize::new(0));
        let owner = Arc::new(CancelledSleepWake {
            _drop_probe: SleepWake {
                host: host.clone(),
                drops: Arc::clone(&drops),
                reentries: Arc::clone(&reentries),
            },
        });
        let weak_owner = Arc::downgrade(&owner);
        let waker = Waker::from(owner);
        let mut wait = host.runtime().clock().sleep(Duration::from_secs(1));
        assert!(
            wait.as_mut()
                .poll(&mut Context::from_waker(&waker))
                .is_pending()
        );
        drop(waker);
        assert_eq!(host.pending_sleeps().len(), 1);
        assert_eq!(drops.load(Ordering::SeqCst), 0);

        let hook_calls = Arc::new(AtomicUsize::new(0));
        let calls = Arc::clone(&hook_calls);
        let scheduler = Arc::clone(&host.driver.shared);
        let report = host
            .driver
            .drain_deadlines(
                DEFAULT_STEP_BUDGET,
                1,
                Some(Box::new(move || {
                    // A real concurrent Sleep drop can run at this unlocked boundary.
                    drop(
                        scheduler
                            .wake
                            .state
                            .try_lock()
                            .expect("scheduler is unlocked"),
                    );
                    drop(scheduler.tasks.try_lock().expect("payloads are unlocked"));
                    drop(wait);
                    calls.fetch_add(1, Ordering::SeqCst);
                })),
            )
            .unwrap();
        assert_eq!(hook_calls.load(Ordering::SeqCst), 1);
        assert_eq!(report.polls(), 0);
        assert_eq!(report.deadline_batches(), 0);
        assert_eq!(report.current_time(), MonotonicInstant::ZERO);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert_eq!(reentries.load(Ordering::SeqCst), 1);
        assert!(weak_owner.upgrade().is_none());
        host.assert_no_pending_time().unwrap();
        drop(host);
        assert!(weak_scheduler.upgrade().is_none());
    }

    #[test]
    fn dropping_sleep_releases_waker_outside_scheduler_lock() {
        let host = DeterministicRuntime::new();
        let drops = Arc::new(AtomicUsize::new(0));
        let reentries = Arc::new(AtomicUsize::new(0));
        let owner = Arc::new(SleepWake {
            host: host.clone(),
            drops: Arc::clone(&drops),
            reentries: Arc::clone(&reentries),
        });
        let weak_owner = Arc::downgrade(&owner);
        let waker = Waker::from(owner);
        let clock = host.runtime().clock();
        let mut wait = clock.sleep(Duration::from_secs(1));
        assert!(
            wait.as_mut()
                .poll(&mut Context::from_waker(&waker))
                .is_pending()
        );
        drop(waker);
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        assert_eq!(host.pending_sleeps().len(), 1);

        drop(wait);

        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert_eq!(reentries.load(Ordering::SeqCst), 1);
        assert!(weak_owner.upgrade().is_none());
        assert!(host.pending_sleeps().is_empty());
    }

    #[test]
    fn replacing_sleep_waker_releases_previous_outside_scheduler_lock() {
        let host = DeterministicRuntime::new();
        let drops = Arc::new(AtomicUsize::new(0));
        let reentries = Arc::new(AtomicUsize::new(0));
        let owner = Arc::new(SleepWake {
            host: host.clone(),
            drops: Arc::clone(&drops),
            reentries: Arc::clone(&reentries),
        });
        let weak_owner = Arc::downgrade(&owner);
        let waker = Waker::from(owner);
        let clock = host.runtime().clock();
        let mut wait = clock.sleep(Duration::from_secs(1));
        assert!(
            wait.as_mut()
                .poll(&mut Context::from_waker(&waker))
                .is_pending()
        );
        drop(waker);
        assert_eq!(drops.load(Ordering::SeqCst), 0);

        assert!(
            wait.as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending()
        );

        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert_eq!(reentries.load(Ordering::SeqCst), 1);
        assert!(weak_owner.upgrade().is_none());
        assert_eq!(host.pending_sleeps().len(), 1);
        drop(wait);
        assert!(host.pending_sleeps().is_empty());
    }

    #[test]
    fn owned_sleep_releases_closed_scheduler_data_on_drop() {
        let host = DeterministicRuntime::new();
        let payloads = Arc::downgrade(&host.driver.shared);
        let scheduler = Arc::downgrade(&host.driver.shared.wake);
        let runtime = host.runtime();
        let clock = runtime.clock();
        let mut wait = clock.sleep(Duration::from_secs(1));
        assert_eq!(
            wait.as_mut().poll(&mut Context::from_waker(Waker::noop())),
            Poll::Pending
        );
        drop(clock);
        drop(runtime);
        drop(host);
        assert!(payloads.upgrade().is_none());
        assert!(scheduler.upgrade().is_some());
        drop(wait);
        assert!(scheduler.upgrade().is_none());
    }

    #[test]
    fn spawn_never_polls_inline_and_ready_tasks_use_id_order() {
        let runtime = DeterministicRuntime::new();
        let order = Arc::new(Mutex::new(Vec::new()));

        for value in [0, 1, 2] {
            let order = Arc::clone(&order);
            runtime.driver.spawn(Box::pin(async move {
                order.lock().expect("order lock").push(value);
            }));
        }

        assert!(order.lock().expect("order lock").is_empty());
        let report = runtime.run_until_stalled().expect("finite tasks");
        assert_eq!(*order.lock().expect("order lock"), [0, 1, 2]);
        assert_eq!(report.polls(), 3);
        assert_eq!(runtime.step_budget(), DEFAULT_STEP_BUDGET);
    }

    #[test]
    fn self_waking_task_exhausts_exact_budget() {
        let runtime = DeterministicRuntime::with_step_budget(7);
        let polls = Arc::new(AtomicUsize::new(0));
        runtime.driver.spawn(Box::pin(SelfWaking {
            polls: Arc::clone(&polls),
        }));

        let error = runtime
            .run_until_stalled()
            .expect_err("self-waking task must exhaust budget");

        assert_eq!(polls.load(Ordering::Acquire), 7);
        assert_eq!(error.polls(), 7);
        assert_eq!(error.step_budget(), 7);
        assert_eq!(error.ready_task_ids(), [0]);
        assert_eq!(error.current_time(), MonotonicInstant::ZERO);
    }

    #[test]
    fn repeated_polls_reuse_task_waker_and_deduplicate_retained_wakes() {
        let runtime = DeterministicRuntime::new();
        let order = Arc::new(Mutex::new(Vec::new()));
        let retained = Arc::new(Mutex::new(Vec::new()));
        for id in 0..3 {
            let order = Arc::clone(&order);
            let retained = Arc::clone(&retained);
            let mut first: Option<Waker> = None;
            let mut remaining = 3;
            runtime.spawn_task(Box::pin(std::future::poll_fn(move |context| {
                order.lock().unwrap().push(id);
                if let Some(first) = &first {
                    assert!(first.will_wake(context.waker()));
                    first.wake_by_ref();
                    first.wake_by_ref();
                } else {
                    first = Some(context.waker().clone());
                    retained.lock().unwrap().push(context.waker().clone());
                }
                remaining -= 1;
                if remaining == 0 {
                    Poll::Ready(())
                } else {
                    context.waker().wake_by_ref();
                    Poll::Pending
                }
            })));
        }
        assert_eq!(runtime.run_until_stalled().unwrap().polls(), 9);
        assert_eq!(*order.lock().unwrap(), [0, 1, 2, 0, 1, 2, 0, 1, 2]);
        for waker in retained.lock().unwrap().iter() {
            waker.wake_by_ref();
        }
        assert_eq!(runtime.run_until_stalled().unwrap().polls(), 0);
        let scheduler = Arc::downgrade(&runtime.driver.shared);
        drop(runtime);
        assert!(scheduler.upgrade().is_none());
        for waker in retained.lock().unwrap().iter() {
            waker.wake_by_ref();
        }
    }

    #[test]
    fn poll_context_borrows_cached_task_waker() {
        let runtime = DeterministicRuntime::new();
        let observed_wake = Arc::new(Mutex::new(
            None::<std::sync::Weak<super::scheduler::TaskWake>>,
        ));
        let observed = Arc::clone(&observed_wake);
        runtime.spawn_task(Box::pin(std::future::poll_fn(move |_| {
            if let Some(wake) = observed.lock().unwrap().as_ref() {
                // The detached task owns its cached wake; the poll context borrows it.
                assert_eq!(wake.strong_count(), 1);
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        })));
        assert_eq!(runtime.run_until_stalled().unwrap().polls(), 1);
        *observed_wake.lock().unwrap() = Some(Arc::downgrade(
            scheduler::lock(&runtime.driver.shared.tasks)[&0]
                .as_ref()
                .unwrap()
                .wake
                .as_ref()
                .unwrap(),
        ));
        runtime.driver.shared.wake.mark_ready(0);
        assert_eq!(runtime.run_until_stalled().unwrap().polls(), 1);
        assert_eq!(
            observed_wake
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .strong_count(),
            0
        );
    }

    #[test]
    #[expect(clippy::waker_clone_wake, reason = "exercise the consuming wake path")]
    fn owned_task_wakes_deduplicate_and_remain_harmless_when_stale() {
        let runtime = DeterministicRuntime::new();
        let retained = Arc::new(Mutex::new(None));
        let output = Arc::clone(&retained);
        let mut first_poll = true;
        runtime.spawn_task(Box::pin(std::future::poll_fn(move |context| {
            if first_poll {
                first_poll = false;
                *output.lock().unwrap() = Some(context.waker().clone());
                Poll::Pending
            } else {
                Poll::Ready(())
            }
        })));
        assert_eq!(runtime.run_until_stalled().unwrap().polls(), 1);
        let waker = retained.lock().unwrap().take().unwrap();
        waker.clone().wake();
        waker.clone().wake();
        assert_eq!(runtime.run_until_stalled().unwrap().polls(), 1);
        waker.clone().wake();
        assert_eq!(runtime.run_until_stalled().unwrap().polls(), 0);
        let scheduler = Arc::downgrade(&runtime.driver.shared);
        drop(runtime);
        assert!(scheduler.upgrade().is_none());
        waker.wake();
    }

    #[test]
    fn retained_pending_task_waker_does_not_keep_task_or_scheduler_alive() {
        let runtime = DeterministicRuntime::new();
        let retained = Arc::new(Mutex::new(None));
        let output = Arc::clone(&retained);
        let resource = Arc::new(());
        let weak_resource = Arc::downgrade(&resource);
        let scheduler = Arc::downgrade(&runtime.driver.shared);
        runtime.spawn_task(Box::pin(std::future::poll_fn(move |context| {
            std::hint::black_box(&resource);
            *output.lock().unwrap() = Some(context.waker().clone());
            Poll::Pending
        })));
        assert_eq!(runtime.run_until_stalled().unwrap().polls(), 1);
        drop(runtime);
        assert!(weak_resource.upgrade().is_none());
        assert!(scheduler.upgrade().is_none());
        retained.lock().unwrap().as_ref().unwrap().wake_by_ref();
    }

    #[test]
    fn default_budget_completes_one_hundred_thousand_finite_polls() {
        let runtime = DeterministicRuntime::new();
        let polls = Arc::new(AtomicUsize::new(0));
        runtime.driver.spawn(Box::pin(FiniteSelfWaking {
            polls: Arc::clone(&polls),
            remaining: DEFAULT_STEP_BUDGET,
        }));

        let report = runtime
            .run_until_stalled()
            .expect("default finite poll workload");

        assert_eq!(report.polls(), DEFAULT_STEP_BUDGET);
        assert_eq!(polls.load(Ordering::Acquire), DEFAULT_STEP_BUDGET);
        assert_eq!(runtime.now(), MonotonicInstant::ZERO);
    }

    #[test]
    fn yield_gives_other_ready_task_a_scheduling_turn() {
        let runtime = DeterministicRuntime::new();
        let order = Arc::new(Mutex::new(Vec::new()));
        let first_order = Arc::clone(&order);
        let first_driver = runtime.driver.clone();
        runtime.driver.spawn(Box::pin(async move {
            first_driver.yield_now().await;
            first_order.lock().expect("order lock").push(0);
        }));
        let second_order = Arc::clone(&order);
        runtime.driver.spawn(Box::pin(async move {
            second_order.lock().expect("order lock").push(1);
        }));

        runtime.run_until_stalled().expect("finite tasks");

        assert_eq!(*order.lock().expect("order lock"), [1, 0]);
    }

    #[test]
    fn advance_wakes_equal_deadlines_in_registration_order() {
        let runtime = DeterministicRuntime::new();
        let order = Arc::new(Mutex::new(Vec::new()));
        for value in [0, 1, 2] {
            let driver = runtime.driver.clone();
            let order = Arc::clone(&order);
            runtime.driver.spawn(Box::pin(async move {
                driver
                    .sleep_until(MonotonicInstant::ZERO.saturating_add(Duration::from_secs(1)))
                    .await;
                order.lock().expect("order lock").push(value);
            }));
        }
        runtime.run_until_stalled().expect("sleep registration");

        let report = runtime.advance(Duration::from_secs(1)).expect("advance");

        assert_eq!(*order.lock().expect("order lock"), [0, 1, 2]);
        assert_eq!(
            report.current_time(),
            MonotonicInstant::from_elapsed(Duration::from_secs(1))
        );
    }

    #[test]
    fn dropped_sleep_deregisters_without_waking() {
        let runtime = DeterministicRuntime::new();
        let mut sleep = runtime
            .driver
            .sleep_until(MonotonicInstant::from_elapsed(Duration::from_secs(1)));
        let waker = std::task::Waker::noop();
        let mut context = Context::from_waker(waker);
        assert!(matches!(sleep.as_mut().poll(&mut context), Poll::Pending));
        assert_eq!(runtime.pending_sleeps().len(), 1);

        drop(sleep);

        assert!(runtime.pending_sleeps().is_empty());
        runtime
            .advance(Duration::from_secs(1))
            .expect("empty advance");
    }

    #[test]
    fn advance_drains_new_deadlines_due_by_target() {
        let runtime = DeterministicRuntime::new();
        let driver = runtime.driver.clone();
        let observations = Arc::new(Mutex::new(Vec::new()));
        let task_observations = Arc::clone(&observations);
        runtime.driver.spawn(Box::pin(async move {
            driver
                .sleep_until(MonotonicInstant::from_elapsed(Duration::from_secs(1)))
                .await;
            task_observations
                .lock()
                .expect("observation lock")
                .push(driver.now());
            driver
                .sleep_until(driver.now().saturating_add(Duration::from_secs(1)))
                .await;
            task_observations
                .lock()
                .expect("observation lock")
                .push(driver.now());
        }));

        let report = runtime.advance(Duration::from_secs(2)).expect("advance");

        assert_eq!(
            *observations.lock().expect("observation lock"),
            [
                MonotonicInstant::from_elapsed(Duration::from_secs(1)),
                MonotonicInstant::from_elapsed(Duration::from_secs(2))
            ]
        );
        assert_eq!(
            report.current_time(),
            MonotonicInstant::from_elapsed(Duration::from_secs(2))
        );
    }

    #[test]
    fn advance_never_rewinds_time_after_a_reentrant_deadline_wake() {
        struct AdvanceOnWake {
            host: DeterministicRuntime,
            observed: Arc<Mutex<Vec<MonotonicInstant>>>,
        }

        impl std::task::Wake for AdvanceOnWake {
            fn wake(self: Arc<Self>) {
                let target = MonotonicInstant::from_elapsed(Duration::from_secs(10));
                let report = self.host.advance_to(target).unwrap();
                self.observed.lock().unwrap().push(report.current_time());
            }
        }

        let host = DeterministicRuntime::new();
        let observed = Arc::new(Mutex::new(Vec::new()));
        let waker = Waker::from(Arc::new(AdvanceOnWake {
            host: host.clone(),
            observed: Arc::clone(&observed),
        }));
        let mut wait = host.sleep_until(MonotonicInstant::from_elapsed(Duration::from_secs(1)));
        assert!(
            wait.as_mut()
                .poll(&mut Context::from_waker(&waker))
                .is_pending()
        );

        let report = host
            .advance_to(MonotonicInstant::from_elapsed(Duration::from_secs(2)))
            .unwrap();

        let last_observed = observed.lock().unwrap()[0];
        assert_eq!(
            last_observed,
            MonotonicInstant::from_elapsed(Duration::from_secs(10))
        );
        assert!(report.current_time() >= last_observed);
        assert_eq!(host.now(), report.current_time());
        assert!(host.pending_sleeps().is_empty());
    }

    #[test]
    fn run_drives_client_and_scheduler_without_blocking_thread() {
        let runtime = DeterministicRuntime::new();
        let signal = Arc::new(ClientSignal::default());
        let task_signal = Arc::clone(&signal);
        runtime.driver.spawn(Box::pin(async move {
            task_signal.complete(42);
        }));

        let output = futures_executor::block_on(runtime.run(signal.wait())).expect("finite drive");

        assert_eq!(output, 42);
    }

    #[test]
    fn completed_client_output_is_released_if_client_destruction_panics() {
        struct Output(Arc<AtomicUsize>);

        impl Drop for Output {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::SeqCst);
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
                panic!("completed client destructor");
            }
        }

        let mut observed_drops = Vec::new();
        for synchronous in [true, false] {
            let runtime = DeterministicRuntime::new();
            let drops = Arc::new(AtomicUsize::new(0));
            let client = Client(Some(Output(Arc::clone(&drops))));
            let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                if synchronous {
                    drop(runtime.run_ready(client, &mut 0));
                } else {
                    drop(futures_executor::block_on(runtime.run(client)));
                }
            }))
            .expect_err("client destruction propagates its original panic");

            assert_eq!(
                panic.downcast_ref::<&str>(),
                Some(&"completed client destructor")
            );
            observed_drops.push(drops.load(Ordering::SeqCst));
        }
        assert_eq!(
            observed_drops,
            [1, 1],
            "synchronous and asynchronous clients"
        );
    }

    #[test]
    fn synchronous_ready_driver_crosses_batches_without_external_wake() {
        let runtime = DeterministicRuntime::with_step_budget(1000);
        let signal = Arc::new(ClientSignal::default());
        let task_signal = Arc::clone(&signal);
        runtime.driver.spawn(Box::pin(async move {
            FiniteSelfWaking {
                polls: Arc::new(AtomicUsize::new(0)),
                remaining: 600,
            }
            .await;
            task_signal.complete(42);
        }));
        let mut polls = 0;
        assert_eq!(
            runtime.run_ready(signal.wait(), &mut polls).unwrap(),
            Some(42)
        );
        assert_eq!(polls, 600);
        assert_eq!(
            runtime
                .run_ready(std::future::pending::<()>(), &mut polls)
                .unwrap(),
            None
        );
        assert_eq!(polls, 600);
    }

    #[test]
    fn dropping_client_drive_does_not_cancel_scheduler_tasks() {
        let runtime = DeterministicRuntime::new();
        let driver = runtime.driver.clone();
        let completed = Arc::new(AtomicUsize::new(0));
        let task_completed = Arc::clone(&completed);
        runtime.driver.spawn(Box::pin(async move {
            driver
                .sleep_until(MonotonicInstant::from_elapsed(Duration::from_secs(1)))
                .await;
            task_completed.store(1, Ordering::Release);
        }));
        let mut drive = Box::pin(runtime.drive(std::future::pending::<()>()));
        let waker = Waker::noop();
        let mut context = Context::from_waker(waker);
        assert!(matches!(drive.as_mut().poll(&mut context), Poll::Pending));

        drop(drive);
        runtime
            .advance(Duration::from_secs(1))
            .expect("task survives client cancellation");

        assert_eq!(completed.load(Ordering::Acquire), 1);
    }

    #[test]
    fn public_run_future_is_send() {
        fn assert_send<T: Send>(_: T) {}

        let runtime = DeterministicRuntime::new();
        assert_send(runtime.run(async { 42 }));
    }

    #[test]
    fn drive_is_notified_by_task_wake_after_stall() {
        struct CountWake(AtomicUsize);

        impl std::task::Wake for CountWake {
            fn wake(self: Arc<Self>) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }

            fn wake_by_ref(self: &Arc<Self>) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }

        let runtime = DeterministicRuntime::new();
        let task_waker = Arc::new(Mutex::new(None));
        let recorded = Arc::clone(&task_waker);
        let completed = Arc::new(AtomicUsize::new(0));
        let task_completed = Arc::clone(&completed);
        let mut first_poll = true;
        runtime.spawn_task(Box::pin(std::future::poll_fn(move |cx| {
            if first_poll {
                first_poll = false;
                *recorded.lock().expect("record task waker") = Some(cx.waker().clone());
                Poll::Pending
            } else {
                task_completed.store(1, Ordering::SeqCst);
                Poll::Ready(())
            }
        })));

        // Only the scheduler can notify this drive: the client observes the
        // task's result but deliberately installs no independent wake source.
        let mut drive = Box::pin(runtime.run(std::future::poll_fn(move |_| {
            if completed.load(Ordering::SeqCst) == 1 {
                Poll::Ready(42)
            } else {
                Poll::Pending
            }
        })));
        let outer = Arc::new(CountWake(AtomicUsize::new(0)));
        let outer_waker = Waker::from(Arc::clone(&outer));
        let mut context = Context::from_waker(&outer_waker);
        assert!(drive.as_mut().poll(&mut context).is_pending());
        let before = outer.0.load(Ordering::SeqCst);
        let wake = task_waker.lock().expect("take task waker").take().unwrap();
        std::thread::spawn(move || wake.wake())
            .join()
            .expect("remote waking thread joined");

        assert!(
            outer.0.load(Ordering::SeqCst) > before,
            "a task wake after the drive stalls must notify its outer executor"
        );
        assert!(matches!(
            drive.as_mut().poll(&mut context),
            Poll::Ready(Ok(42))
        ));
    }

    #[derive(Default)]
    struct ClientSignal {
        state: Mutex<ClientSignalState>,
    }

    #[derive(Default)]
    struct ClientSignalState {
        value: Option<usize>,
        waker: Option<Waker>,
    }

    impl ClientSignal {
        fn complete(&self, value: usize) {
            let waker = {
                let mut state = self.state.lock().expect("client signal lock");
                state.value = Some(value);
                state.waker.take()
            };
            if let Some(waker) = waker {
                waker.wake();
            }
        }

        async fn wait(&self) -> usize {
            std::future::poll_fn(|context| {
                let mut state = self.state.lock().expect("client signal lock");
                if let Some(value) = state.value.take() {
                    Poll::Ready(value)
                } else {
                    state.waker = Some(context.waker().clone());
                    Poll::Pending
                }
            })
            .await
        }
    }

    struct SelfWaking {
        polls: Arc<AtomicUsize>,
    }

    impl Future for SelfWaking {
        type Output = ();

        fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
            self.polls.fetch_add(1, Ordering::AcqRel);
            cx.waker().wake_by_ref();
            Poll::Pending
        }
    }

    struct FiniteSelfWaking {
        polls: Arc<AtomicUsize>,
        remaining: usize,
    }

    impl Future for FiniteSelfWaking {
        type Output = ();

        fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
            self.polls.fetch_add(1, Ordering::AcqRel);
            self.remaining -= 1;
            if self.remaining == 0 {
                Poll::Ready(())
            } else {
                cx.waker().wake_by_ref();
                Poll::Pending
            }
        }
    }
}
