//! One scheduling policy; only the stored future and outer owner differ.

use std::collections::{BTreeMap, BTreeSet};
use std::pin::Pin;
use std::sync::{Arc, Weak};
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use event_listener::{Event, EventListener};
use futures_util::task::{ArcWake, waker_ref};
use rca_core::MonotonicInstant;

use super::{DriveReport, READY_BATCH_LIMIT};
use crate::error::DriveFailure;
use crate::{DriveError, PendingSleep};

#[cfg(all(test, loom))]
use loom::sync::atomic::{AtomicBool, Ordering};
#[cfg(all(test, loom))]
use loom::sync::{Mutex, MutexGuard};
#[cfg(not(all(test, loom)))]
use parking_lot::{Mutex, MutexGuard};
#[cfg(not(all(test, loom)))]
use std::sync::atomic::{AtomicBool, Ordering};

/// Uses instrumented locks in Loom models and non-poisoning locks otherwise.
pub(super) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    #[cfg(all(test, loom))]
    {
        mutex
            .lock()
            .expect("model scheduler lock must not be poisoned")
    }
    #[cfg(not(all(test, loom)))]
    {
        mutex.lock()
    }
}

/// Monotonic admission identity; also fixes ready-task polling order.
type TaskId = u64;
/// Monotonic construction identity; orders equal-deadline wake delivery.
type SleepId = u64;

/// Common scheduling policy for Send and owner-local boxed task futures.
///
/// Always acquire wake state before task storage when both are needed. User
/// polls, wakes and destructors run after those guards have been released.
pub(crate) struct Scheduler<W> {
    /// Thread-safe metadata retained weakly by task wakers.
    pub(crate) wake: Arc<WakeState>,
    /// `None` reserves a detached task's node while its future is being polled.
    pub(crate) tasks: Mutex<BTreeMap<TaskId, Option<ScheduledTask<W>>>>,
}

// Preserve the former std Mutex unwind boundary without relying on poisoning:
// no user poll, Drop, waker or event callback runs under either guard. Reserved
// state cleanup preserves logical consistency on unwind; checked ID exhaustion
// occurs before any state mutation. This does not assert unwind safety of W.
impl<W> std::panic::UnwindSafe for Scheduler<W> {}
impl<W> std::panic::RefUnwindSafe for Scheduler<W> {}

/// Thread-safe wake and timer data, separate from potentially non-Send tasks.
pub(crate) struct WakeState {
    /// Durable readiness and virtual time inspected by every host operation.
    pub(crate) state: Mutex<SchedulerState>,
    /// Ends admission under the state guard and stops polls without taking it.
    pub(super) closed: AtomicBool,
    /// Broadcasts changes to stalled drives; readiness remains authoritative.
    changed: Event,
}

impl<W: Future<Output = ()> + Unpin> Scheduler<W> {
    /// Creates independent data with no tasks, timers or host authority.
    pub(crate) fn new(now: MonotonicInstant) -> Self {
        Self {
            wake: Arc::new(WakeState {
                state: Mutex::new(SchedulerState {
                    live_tasks: BTreeSet::new(),
                    ready_tasks: BTreeSet::new(),
                    sleeps: BTreeMap::new(),
                    next_task_id: 0,
                    next_sleep_id: 0,
                    now,
                    active_target: None,
                }),
                closed: AtomicBool::new(false),
                changed: Event::new(),
            }),
            tasks: Mutex::new(BTreeMap::new()),
        }
    }

    /// Admits without polling, or drops outside the locks after host closure.
    pub(crate) fn spawn(&self, future: W) {
        let mut state = lock(&self.wake.state);
        if self.wake.closed.load(Ordering::Relaxed) {
            drop(state);
            drop(future);
            return;
        }
        let mut tasks = lock(&self.tasks);
        let id = state.next_task_id;
        state.next_task_id = id
            .checked_add(1)
            .expect("deterministic task ID space exhausted");
        let previous = tasks.insert(id, Some(ScheduledTask { future, wake: None }));
        debug_assert!(previous.is_none(), "task ID must be unique");
        state.live_tasks.insert(id);
        state.ready_tasks.insert(id);
        drop(tasks);
        drop(state);
        self.wake.notify();
    }

    /// Detaches an ID-ordered batch and polls each admitted payload at most once.
    ///
    /// Internal calls with larger limits still detach at most `READY_BATCH_LIMIT`
    /// tasks. Public drivers already apply this cap before calling.
    /// Re-entry skips reserved nodes. Pending work returns to its existing node;
    /// completed work loses wake authority before its destructor runs. Unwinding
    /// drops detached payloads but preserves unfinished IDs for diagnostics.
    pub(crate) fn poll_ready_batch(&self, limit: usize) -> usize {
        if limit == 0 {
            return 0;
        }
        let mut batch = self.detach_ready_batch(limit);
        let mut count = 0;
        while let Some((id, mut task)) = batch.first.take().or_else(|| batch.remaining.next()) {
            batch.current_id = Some(id);
            if self.wake.closed.load(Ordering::Acquire) {
                {
                    // Closure can still own both guards. Wait for its cleanup,
                    // then remove this reserved slot before dropping the future.
                    let _state = lock(&self.wake.state);
                    lock(&self.tasks).remove(&id)
                };
                drop(task);
                batch.current_id = None;
                continue;
            }
            let result = {
                let wake = task.wake.get_or_insert_with(|| {
                    Arc::new(TaskWake {
                        task_id: id,
                        scheduler: Arc::downgrade(&self.wake),
                    })
                });
                let waker = waker_ref(wake);
                Pin::new(&mut task.future).poll(&mut Context::from_waker(&waker))
            };
            count += 1;
            let mut state = lock(&self.wake.state);
            let mut tasks = lock(&self.tasks);
            match result {
                Poll::Pending if !self.wake.closed.load(Ordering::Relaxed) => {
                    let ready = state.live_tasks.contains(&id) && state.ready_tasks.contains(&id);
                    let replaced = tasks.insert(id, Some(task));
                    debug_assert!(replaced.is_none_or(|value| value.is_none()));
                    drop(tasks);
                    drop(state);
                    // A second drive may have consumed the earlier wake while
                    // this payload was reserved. Publish its now-pollable slot.
                    if ready {
                        self.wake.notify();
                    }
                }
                Poll::Pending | Poll::Ready(()) => {
                    tasks.remove(&id);
                    state.live_tasks.remove(&id);
                    state.ready_tasks.remove(&id);
                    drop(tasks);
                    drop(state);
                    drop(task);
                }
            }
            batch.current_id = None;
        }
        count
    }

    /// Detaches one bounded batch without keeping its ID buffer across user polls.
    // End this stack frame before a future can poll or re-enter the scheduler.
    #[inline(never)]
    fn detach_ready_batch(&self, limit: usize) -> PollBatch<'_, W> {
        let mut ids = [0; READY_BATCH_LIMIT];
        let mut state = lock(&self.wake.state);
        let mut tasks = lock(&self.tasks);
        let mut count = 0;
        for (slot, id) in ids.iter_mut().zip(
            state
                .ready_tasks
                .iter()
                .filter(|id| tasks.get(id).is_some_and(Option::is_some))
                .take(limit),
        ) {
            *slot = *id;
            count += 1;
        }

        let mut detached = ids[..count].iter().map(|&id| {
            state.ready_tasks.remove(&id);
            let task = tasks
                .get_mut(&id)
                .and_then(Option::take)
                .expect("ready task must exist");
            (id, task)
        });
        let first = detached.next();
        let remaining = detached.collect::<Vec<_>>().into_iter();
        PollBatch {
            scheduler: self,
            first,
            remaining,
            current_id: None,
        }
    }

    /// Excludes reserved and abandoned diagnostic IDs from runnable work.
    pub(crate) fn has_runnable_task(&self) -> bool {
        let state = lock(&self.wake.state);
        let tasks = lock(&self.tasks);
        state
            .ready_tasks
            .iter()
            .any(|id| tasks.get(id).is_some_and(Option::is_some))
    }

    /// Ends admission and detaches all ownership before notification and drops.
    pub(crate) fn close(&self) {
        let (tasks, sleeps) = {
            let mut state = lock(&self.wake.state);
            let mut tasks = lock(&self.tasks);
            // Locked readers share this guard. Detached polls use an acquire
            // load, then take the guards before any closed-path cleanup.
            self.wake.closed.store(true, Ordering::Release);
            state.live_tasks.clear();
            state.ready_tasks.clear();
            state.active_target = None;
            (
                std::mem::take(&mut *tasks),
                std::mem::take(&mut state.sleeps),
            )
        };
        // Notify before a user destructor can panic; collections are already
        // detached so unwinding still owns and drops their other members.
        self.wake.notify();
        drop(tasks);
        drop(sleeps);
    }

    /// Removes queued payloads and diagnostic debt without ending admission.
    ///
    /// Intended for idle `TestStore` teardown, not cancellation of a detached poll.
    /// Externally owned sleeps remain registered until their own cancellation.
    pub(crate) fn discard_all_tasks(&self) {
        let tasks = {
            let mut state = lock(&self.wake.state);
            let mut tasks = lock(&self.tasks);
            state.live_tasks.clear();
            state.ready_tasks.clear();
            state.active_target = None;
            std::mem::take(&mut *tasks)
        };
        drop(tasks);
    }

    /// Reserves an identity now; the sleep registers only when first polled.
    pub(crate) fn sleep_until(&self, deadline: MonotonicInstant) -> DeterministicSleep {
        DeterministicSleep {
            scheduler: Arc::clone(&self.wake),
            deadline,
            id: self.wake.next_sleep_id(),
            registered: false,
        }
    }

    /// Reads the current virtual instant without driving work.
    pub(crate) fn now(&self) -> MonotonicInstant {
        self.wake.now()
    }

    /// Captures deadline-then-identity order for diagnostics.
    pub(crate) fn pending_sleeps(&self) -> Vec<PendingSleep> {
        self.wake.pending_sleeps()
    }

    /// Extends an existing partial target, or starts from current time.
    pub(crate) fn start_advance(&self, duration: Duration) {
        self.wake.start_advance(duration);
    }

    /// Replaces the target, clamped to the current virtual instant.
    pub(crate) fn start_advance_to(&self, target: MonotonicInstant) {
        self.wake.start_advance_to(target);
    }

    /// Reads the target retained across a `TestStore` assertion boundary.
    pub(crate) fn active_target(&self) -> Option<MonotonicInstant> {
        self.wake.active_target()
    }

    /// Clears the target after completion or explicit abandonment.
    pub(crate) fn finish_advance(&self) {
        self.wake.finish_advance();
    }

    /// Captures ready IDs, including reserved or abandoned diagnostic debt.
    pub(crate) fn ready_task_ids(&self) -> Vec<TaskId> {
        self.wake.ready_task_ids()
    }

    /// Copies metadata without retaining any task or waker payload.
    pub(crate) fn snapshot(&self) -> SchedulerSnapshot {
        self.wake.snapshot()
    }

    /// Captures owned failure evidence for an exhausted task-poll budget.
    pub(crate) fn exhausted(&self, polls: usize, step_budget: usize) -> DriveError {
        let snapshot = self.snapshot();
        DriveError::exhausted(
            polls,
            step_budget,
            snapshot.live_task_ids,
            snapshot.ready_task_ids,
            snapshot.pending_sleeps,
            snapshot.current_time,
        )
    }

    /// Reports ready-only progress with no deadline cohorts.
    pub(crate) fn report(&self, polls: usize) -> DriveReport {
        DriveReport {
            polls,
            deadline_batches: 0,
            current_time: self.now(),
        }
    }

    /// Drains runnable work while preserving exact cumulative budget accounting.
    pub(crate) fn drive_ready(
        &self,
        mut polls: usize,
        step_budget: usize,
    ) -> Result<usize, DriveError> {
        loop {
            if polls == step_budget {
                return if self.has_runnable_task() {
                    Err(self.exhausted(polls, step_budget))
                } else {
                    Ok(polls)
                };
            }
            let count = self.poll_ready_batch(READY_BATCH_LIMIT.min(step_budget - polls));
            if count == 0 {
                return Ok(polls);
            }
            polls += count;
        }
    }

    /// Drives an owned client synchronously until ready, stalled or out of budget.
    ///
    /// The caller owns the cumulative poll count. Pending clients are discarded;
    /// callers needing external wakes use `DriveState` instead.
    pub(crate) fn run_ready<F: Future>(
        &self,
        future: F,
        polls: &mut usize,
        step_budget: usize,
    ) -> Result<Option<F::Output>, DriveError> {
        // Keep the owned result in this scope while the pinned client is dropped.
        // A client destructor panic must not strand it in the return slot.
        let result;
        {
            let mut future = std::pin::pin!(future);
            let mut context = Context::from_waker(Waker::noop());
            result = loop {
                if let Poll::Ready(output) = future.as_mut().poll(&mut context) {
                    break Ok(Some(output));
                }
                if !self.has_runnable_task() {
                    break Ok(None);
                }
                if *polls == step_budget {
                    break Err(self.exhausted(*polls, step_budget));
                }
                *polls += self.poll_ready_batch(READY_BATCH_LIMIT.min(step_budget - *polls));
            }
        };
        result
    }

    /// Drains ready work and successive due cohorts towards the retained target.
    ///
    /// The owner clears the target; partial assertion-driven advances retain it.
    pub(crate) fn resume_advance(&self, step_budget: usize) -> Result<DriveReport, DriveError> {
        let target = self
            .active_target()
            .expect("active virtual-time target must exist");
        let mut polls = 0;
        let mut deadline_batches = 0;
        loop {
            polls = self.drive_ready(polls, step_budget)?;
            let Some(wakers) = self.wake.take_next_deadline(target) else {
                self.wake.set_now(target);
                return Ok(DriveReport {
                    deadline_batches,
                    ..self.report(polls)
                });
            };
            deadline_batches += 1;
            for waker in wakers {
                waker.wake();
            }
        }
    }

    /// Finishes bounded timer work without consuming partial advances or waiting.
    pub(crate) fn drain_deadlines(
        &self,
        step_budget: usize,
        max_deadline_batches: usize,
        #[cfg(test)] mut before_take_deadline: Option<Box<dyn FnOnce() + Send>>,
    ) -> Result<DriveReport, DriveError> {
        let mut polls = 0;
        let mut deadline_batches = 0;
        loop {
            let state = lock(&self.wake.state);
            let tasks = lock(&self.tasks);
            let closed = self.wake.closed.load(Ordering::Relaxed);
            let runnable = !closed
                && state.active_target.is_none()
                && state
                    .ready_tasks
                    .iter()
                    .any(|id| tasks.get(id).is_some_and(Option::is_some));
            let failure = if closed {
                Some(DriveFailure::Closed)
            } else if state.active_target.is_some() {
                Some(DriveFailure::PartialAdvance)
            } else if runnable {
                (polls == step_budget).then_some(DriveFailure::StepBudget)
            } else if state.sleeps.is_empty() {
                if state.live_tasks.is_empty() {
                    return Ok(DriveReport {
                        polls,
                        deadline_batches,
                        current_time: state.now,
                    });
                }
                Some(DriveFailure::Blocked)
            } else if deadline_batches == max_deadline_batches {
                Some(DriveFailure::DeadlineBudget)
            } else {
                None
            };
            if let Some(failure) = failure {
                let snapshot = SchedulerSnapshot::from_state(&state);
                drop(tasks);
                drop(state);
                return Err(snapshot.temporal_error(
                    failure,
                    polls,
                    step_budget,
                    deadline_batches,
                    Some(max_deadline_batches),
                ));
            }
            drop(tasks);
            drop(state);
            if runnable {
                polls += self.poll_ready_batch(READY_BATCH_LIMIT.min(step_budget - polls));
                continue;
            }
            #[cfg(test)]
            if let Some(hook) = before_take_deadline.take() {
                hook();
            }
            if let Some(wakers) = self.wake.take_next_deadline(MonotonicInstant::MAX) {
                deadline_batches += 1;
                for waker in wakers {
                    waker.wake();
                }
            }
        }
    }
}

/// Client polling state shared by the Shared and Local host wrappers.
pub(crate) struct DriveState<F> {
    /// Stable pin for a client that may borrow data and need not be `Unpin`.
    future: Pin<Box<F>>,
    /// Cumulative scheduler polls, excluding polls of the client itself.
    polls: usize,
    /// Allocated lazily only when both client and scheduler are stalled.
    listener: Option<EventListener>,
}

impl<F: Future> DriveState<F> {
    /// Pins the client without registering a wake listener or polling any work.
    pub(crate) fn new(future: F) -> Self {
        Self {
            future: Box::pin(future),
            polls: 0,
            listener: None,
        }
    }

    /// Checks the client around one ready batch, then listens only if stalled.
    pub(crate) fn poll<W: Future<Output = ()> + Unpin>(
        &mut self,
        cx: &mut Context<'_>,
        scheduler: &Scheduler<W>,
        step_budget: usize,
    ) -> Poll<Result<F::Output, DriveError>> {
        self.poll_inner(
            cx,
            scheduler,
            step_budget,
            #[cfg(test)]
            &mut |_| {},
        )
    }

    /// Keeps the readiness recheck after registration to close every wake gap.
    fn poll_inner<W: Future<Output = ()> + Unpin>(
        &mut self,
        cx: &mut Context<'_>,
        scheduler: &Scheduler<W>,
        step_budget: usize,
        #[cfg(test)] hook: &mut impl FnMut(WaitStage),
    ) -> Poll<Result<F::Output, DriveError>> {
        if let Poll::Ready(value) = self.future.as_mut().poll(cx) {
            return Poll::Ready(Ok(value));
        }
        self.polls += scheduler.poll_ready_batch(READY_BATCH_LIMIT.min(step_budget - self.polls));
        if let Poll::Ready(value) = self.future.as_mut().poll(cx) {
            return Poll::Ready(Ok(value));
        }
        if scheduler.has_runnable_task() {
            if self.polls == step_budget {
                return Poll::Ready(Err(scheduler.exhausted(self.polls, step_budget)));
            }
            cx.waker().wake_by_ref();
            return Poll::Pending;
        }
        // Register only when stalled. Readiness is durable: registration must
        // precede this final client/runnable recheck, not the first task batch.
        #[cfg(test)]
        hook(WaitStage::BeforeListen);
        self.listener
            .get_or_insert_with(|| scheduler.wake.changed.listen());
        #[cfg(test)]
        hook(WaitStage::AfterListen);
        if let Poll::Ready(value) = self.future.as_mut().poll(cx) {
            return Poll::Ready(Ok(value));
        }
        if scheduler.has_runnable_task() {
            if self.polls == step_budget {
                return Poll::Ready(Err(scheduler.exhausted(self.polls, step_budget)));
            }
            cx.waker().wake_by_ref();
            return Poll::Pending;
        }
        #[cfg(test)]
        hook(WaitStage::BeforeListenerPoll);
        if Pin::new(self.listener.as_mut().expect("registered listener"))
            .poll(cx)
            .is_ready()
        {
            self.listener = None;
            cx.waker().wake_by_ref();
        }
        Poll::Pending
    }
}

#[cfg(test)]
#[derive(Clone, Copy, PartialEq)]
enum WaitStage {
    BeforeListen,
    AfterListen,
    BeforeListenerPoll,
}

/// Requests one later poll without changing virtual time.
pub(crate) fn yield_now() -> impl Future<Output = ()> + Send + 'static {
    let mut yielded = false;
    std::future::poll_fn(move |context| {
        if yielded {
            Poll::Ready(())
        } else {
            yielded = true;
            context.waker().wake_by_ref();
            Poll::Pending
        }
    })
}

/// Payload detached while polling and returned to its reserved node if pending.
pub(crate) struct ScheduledTask<W> {
    /// Boxed pinned task supplied by the runtime's ownership family.
    pub(super) future: W,
    /// Lazily allocated once; poll contexts borrow it and tasks may clone it.
    pub(super) wake: Option<Arc<TaskWake>>,
}

/// Removes reserved storage on unwind before unpolled payloads are destroyed.
struct PollBatch<'a, W> {
    /// Metadata and reserved task nodes, never held locked across user code.
    scheduler: &'a Scheduler<W>,
    /// First detached task, stored inline so singleton batches need no allocation.
    first: Option<(TaskId, ScheduledTask<W>)>,
    /// Unvisited tasks after the first in the finite detached cohort.
    remaining: std::vec::IntoIter<(TaskId, ScheduledTask<W>)>,
    /// Current reservation, set before polling or destroying its payload.
    current_id: Option<TaskId>,
}

impl<W> Drop for PollBatch<'_, W> {
    fn drop(&mut self) {
        // At drop entry, unvisited work has either its first task or a current ID.
        // Moving to the next task cannot panic, so no other state needs cleanup.
        if self.current_id.is_none() && self.first.is_none() {
            return;
        }
        let state = lock(&self.scheduler.wake.state);
        let mut tasks = lock(&self.scheduler.tasks);
        for id in self
            .current_id
            .iter()
            .copied()
            .chain(self.first.iter().map(|(id, _)| *id))
            .chain(self.remaining.as_slice().iter().map(|(id, _)| *id))
        {
            if tasks.get(&id).is_some_and(Option::is_none) {
                tasks.remove(&id);
            }
        }
        drop(tasks);
        drop(state);
    }
}

/// Cross-thread notification without strong ownership of task or scheduler data.
pub(super) struct TaskWake {
    /// Admission identity checked against the live diagnostic set before waking.
    task_id: TaskId,
    /// Weak metadata only, so retained wakers cannot retain Local task payloads.
    scheduler: Weak<WakeState>,
}

impl ArcWake for TaskWake {
    fn wake_by_ref(arc_self: &Arc<Self>) {
        if let Some(scheduler) = arc_self.scheduler.upgrade() {
            scheduler.mark_ready(arc_self.task_id);
        }
    }
}

impl WakeState {
    /// Copies ready identities in their deterministic polling order.
    fn ready_task_ids(&self) -> Vec<TaskId> {
        lock(&self.state).ready_tasks.iter().copied().collect()
    }

    /// Reads the current virtual instant under the metadata guard.
    fn now(&self) -> MonotonicInstant {
        lock(&self.state).now
    }

    /// Commits a completed target without undoing a farther re-entrant advance.
    fn set_now(&self, now: MonotonicInstant) {
        let mut state = lock(&self.state);
        // A task poll or deadline wake can re-enter the host and advance farther.
        state.now = state.now.max(now);
    }

    /// Adds to a partial target, or current time when no advance is pending.
    fn start_advance(&self, duration: Duration) {
        let mut state = lock(&self.state);
        let base = state.active_target.unwrap_or(state.now);
        state.active_target = Some(base.saturating_add(duration));
    }

    /// Replaces the requested target without allowing a past virtual instant.
    fn start_advance_to(&self, target: MonotonicInstant) {
        let mut state = lock(&self.state);
        state.active_target = Some(target.max(state.now));
    }

    /// Reads the target retained across assertion-driven partial progress.
    fn active_target(&self) -> Option<MonotonicInstant> {
        lock(&self.state).active_target
    }

    /// Ends target ownership without cancelling any queued task or timer.
    fn finish_advance(&self) {
        lock(&self.state).active_target = None;
    }

    /// Reserves a never-reused sleep identity before any timer registration.
    fn next_sleep_id(&self) -> SleepId {
        let mut state = lock(&self.state);
        let id = state.next_sleep_id;
        state.next_sleep_id = state
            .next_sleep_id
            .checked_add(1)
            .expect("deterministic sleep ID space exhausted");
        id
    }

    /// Replaces a pending registration, or reports elapsed/closed clock state.
    ///
    /// Waker clone and drop can re-enter. Both run outside the guard, and state
    /// is checked after cloning so any clock movement during clone is observed.
    fn register_sleep(
        &self,
        deadline: MonotonicInstant,
        id: SleepId,
        waker: &Waker,
    ) -> SleepRegistration {
        // Waker cloning and destruction can re-enter the scheduler.
        let waker = waker.clone();
        let (registration, removed) = {
            let mut state = lock(&self.state);
            if state.now >= deadline {
                (
                    SleepRegistration::Elapsed,
                    state.sleeps.remove(&(deadline, id)),
                )
            } else if self.closed.load(Ordering::Relaxed) {
                (SleepRegistration::Closed, None)
            } else {
                (
                    SleepRegistration::Registered,
                    state.sleeps.insert((deadline, id), waker),
                )
            }
        };
        drop(removed);
        registration
    }

    /// Detaches a registration before its waker can re-enter during destruction.
    fn remove_sleep(&self, deadline: MonotonicInstant, id: SleepId) {
        let removed = lock(&self.state).sleeps.remove(&(deadline, id));
        drop(removed);
    }

    /// Detaches one due cohort in sleep-ID order before invoking any waker.
    fn take_next_deadline(&self, target: MonotonicInstant) -> Option<Vec<Waker>> {
        let mut state = lock(&self.state);
        let deadline = state
            .sleeps
            .first_key_value()
            .map(|((deadline, _), _)| *deadline)
            .filter(|deadline| *deadline <= target)?;
        state.now = deadline;
        Some(
            state
                .sleeps
                .extract_if(
                    (deadline, SleepId::MIN)..=(deadline, SleepId::MAX),
                    |_, _| true,
                )
                .map(|(_, waker)| waker)
                .collect(),
        )
    }

    /// Copies registered deadlines and IDs, never their owned wakers.
    fn pending_sleeps(&self) -> Vec<PendingSleep> {
        lock(&self.state)
            .sleeps
            .keys()
            .map(|(deadline, id)| PendingSleep::new(*deadline, *id))
            .collect()
    }

    /// Captures an owned metadata view under one guard.
    fn snapshot(&self) -> SchedulerSnapshot {
        let state = lock(&self.state);
        SchedulerSnapshot::from_state(&state)
    }
}

/// Ordered metadata shared by task notification, clocks and host driving.
pub(crate) struct SchedulerState {
    /// Admitted unfinished tasks, including debt from a caught task panic.
    pub(super) live_tasks: BTreeSet<TaskId>,
    /// Coalesced readiness; a live ID need not currently have a runnable payload.
    pub(super) ready_tasks: BTreeSet<TaskId>,
    /// Pending waits ordered by deadline and then construction identity.
    pub(super) sleeps: BTreeMap<(MonotonicInstant, SleepId), Waker>,
    /// Next task admission identity; checked before mutation to avoid reuse.
    pub(super) next_task_id: TaskId,
    /// Next sleep construction identity; checked before mutation to avoid reuse.
    pub(super) next_sleep_id: SleepId,
    /// Monotonic virtual time in this host's independent clock origin.
    pub(super) now: MonotonicInstant,
    /// Target retained across partial `TestStore` advances.
    pub(super) active_target: Option<MonotonicInstant>,
}

/// Owned diagnostic data that cannot extend task or waker lifetimes.
pub(crate) struct SchedulerSnapshot {
    /// Admitted unfinished IDs, including abandoned panic debt.
    pub(crate) live_task_ids: Vec<TaskId>,
    /// Recorded ready IDs in deterministic order.
    pub(crate) ready_task_ids: Vec<TaskId>,
    /// Registered waits in deadline-then-ID order.
    pub(crate) pending_sleeps: Vec<PendingSleep>,
    /// Virtual instant at capture.
    pub(crate) current_time: MonotonicInstant,
    /// Partial advance target at capture, if any.
    pub(crate) active_target: Option<MonotonicInstant>,
}

impl SchedulerSnapshot {
    /// Copies metadata while its owner holds the state guard.
    fn from_state(state: &SchedulerState) -> Self {
        Self {
            live_task_ids: state.live_tasks.iter().copied().collect(),
            ready_task_ids: state.ready_tasks.iter().copied().collect(),
            pending_sleeps: state
                .sleeps
                .keys()
                .map(|(deadline, id)| PendingSleep::new(*deadline, *id))
                .collect(),
            current_time: state.now,
            active_target: state.active_target,
        }
    }

    /// Adds owned task/time evidence to a bounded-drive failure.
    pub(crate) fn temporal_error(
        self,
        failure: DriveFailure,
        polls: usize,
        step_budget: usize,
        deadline_batches: usize,
        deadline_budget: Option<usize>,
    ) -> DriveError {
        DriveError::exhausted(
            polls,
            step_budget,
            self.live_task_ids,
            self.ready_task_ids,
            self.pending_sleeps,
            self.current_time,
        )
        .with_temporal_context(
            failure,
            deadline_batches,
            deadline_budget,
            self.active_target,
        )
    }
}

/// Clock state observed while attempting to register the current poll's waker.
enum SleepRegistration {
    /// Deadline has passed; any previous registration was removed.
    Elapsed,
    /// The current waker owns a pending registration.
    Registered,
    /// Host is closed and this unelapsed wait cannot make progress.
    Closed,
}

/// Owned wait that retains clock data but cannot keep a host or task alive.
pub(crate) struct DeterministicSleep {
    /// Clock metadata can outlive the host without retaining host authority.
    scheduler: Arc<WakeState>,
    /// Absolute target in this scheduler's clock origin.
    deadline: MonotonicInstant,
    /// Construction identity shared by every poll of this wait.
    id: SleepId,
    /// A pending poll registered a waker that `Drop` may need to remove.
    registered: bool,
}

impl Future for DeterministicSleep {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match self
            .scheduler
            .register_sleep(self.deadline, self.id, cx.waker())
        {
            SleepRegistration::Elapsed => {
                self.registered = false;
                Poll::Ready(())
            }
            SleepRegistration::Registered => {
                self.registered = true;
                Poll::Pending
            }
            SleepRegistration::Closed => {
                self.registered = false;
                Poll::Pending
            }
        }
    }
}

impl Drop for DeterministicSleep {
    fn drop(&mut self) {
        if self.registered {
            self.scheduler.remove_sleep(self.deadline, self.id);
        }
    }
}

impl WakeState {
    /// Broadcasts a change only when stalled drives are listening.
    fn notify(&self) {
        self.notify_inner(
            #[cfg(test)]
            |_| {},
        );
    }

    /// Skips event work without listeners; durable readiness closes the race.
    fn notify_inner(&self, #[cfg(test)] hook: impl FnOnce(bool)) {
        let listening = self.changed.total_listeners() != 0;
        #[cfg(test)]
        hook(listening);
        if listening {
            self.changed.notify(usize::MAX);
        }
    }

    /// Coalesces one live task's wake and notifies outside the metadata guard.
    pub(super) fn mark_ready(&self, id: TaskId) {
        let inserted = {
            let mut state = lock(&self.state);
            state.live_tasks.contains(&id) && state.ready_tasks.insert(id)
        };
        if inserted {
            self.notify();
        }
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};

    type Host = Scheduler<rca_core::RuntimeFuture>;
    type Client = Pin<Box<dyn Future<Output = usize> + Send>>;

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

    fn client(done: &Arc<AtomicUsize>) -> DriveState<Client> {
        let done = Arc::clone(done);
        DriveState::new(Box::pin(std::future::poll_fn(move |_| {
            let value = done.load(SeqCst);
            if value == 0 {
                Poll::Pending
            } else {
                Poll::Ready(value)
            }
        })) as Client)
    }

    fn counter() -> (Arc<CountWake>, Waker) {
        let count = Arc::new(CountWake::default());
        let waker = Waker::from(Arc::clone(&count));
        (count, waker)
    }

    #[test]
    fn bounded_ready_batches_preserve_order_and_remaining_tasks() {
        let expected_order: Vec<TaskId> = (0..258).collect();
        for (limit, [first, second, third]) in [
            (0, [0, 256, 2]),
            (1, [1, 256, 1]),
            (255, [255, 3, 0]),
            (256, [256, 2, 0]),
            (257, [256, 2, 0]),
            (usize::MAX, [256, 2, 0]),
        ] {
            let scheduler = Host::new(MonotonicInstant::ZERO);
            let observed = Arc::new(Mutex::new(Vec::new()));
            for &id in &expected_order {
                let output = Arc::clone(&observed);
                scheduler.spawn(Box::pin(async move {
                    output.lock().unwrap().push(id);
                }));
            }
            assert!(observed.lock().unwrap().is_empty());

            let mut completed = 0;
            for (batch_limit, expected_polls) in [(limit, first), (256, second), (256, third)] {
                assert_eq!(
                    scheduler.poll_ready_batch(batch_limit),
                    expected_polls,
                    "first limit {limit}, current limit {batch_limit}"
                );
                completed += expected_polls;
                assert_eq!(
                    observed.lock().unwrap().as_slice(),
                    &expected_order[..completed]
                );
                let snapshot = scheduler.snapshot();
                assert_eq!(snapshot.ready_task_ids, expected_order[completed..]);
                assert_eq!(snapshot.live_task_ids, expected_order[completed..]);
            }

            assert_eq!(scheduler.poll_ready_batch(usize::MAX), 0);
            assert_eq!(*observed.lock().unwrap(), expected_order);
            assert!(lock(&scheduler.tasks).is_empty());
        }
    }

    #[test]
    fn unconsumed_ready_batches_drop_unlocked_and_preserve_diagnostic_debt() {
        struct DropOutsideLock {
            scheduler: Arc<Host>,
            drops: Arc<Mutex<Vec<usize>>>,
            id: usize,
            panic_id: Option<usize>,
        }

        impl Drop for DropOutsideLock {
            fn drop(&mut self) {
                drop(
                    self.scheduler
                        .wake
                        .state
                        .try_lock()
                        .expect("metadata unlocked"),
                );
                assert!(
                    self.scheduler
                        .tasks
                        .try_lock()
                        .expect("payloads unlocked")
                        .is_empty(),
                    "all reservations must be removed before any payload is dropped"
                );
                assert_eq!(self.scheduler.poll_ready_batch(256), 0);
                self.drops.lock().unwrap().push(self.id);
                assert!(
                    Some(self.id) != self.panic_id,
                    "unconsumed batch destructor panic"
                );
            }
        }

        for (count, panic_id) in [
            (0, None),
            (1, None),
            (2, None),
            (256, None),
            (3, Some(0)),
            (3, Some(1)),
        ] {
            let scheduler = Arc::new(Host::new(MonotonicInstant::ZERO));
            let drops = Arc::new(Mutex::new(Vec::new()));
            let polls = Arc::new(AtomicUsize::new(0));
            for id in 0..count {
                let guard = DropOutsideLock {
                    scheduler: Arc::clone(&scheduler),
                    drops: Arc::clone(&drops),
                    id,
                    panic_id,
                };
                let polls = Arc::clone(&polls);
                scheduler.spawn(Box::pin(async move {
                    let _guard = guard;
                    polls.fetch_add(1, SeqCst);
                    std::future::pending::<()>().await;
                }));
            }

            let expected_ids = (0..u64::try_from(count).unwrap()).collect::<Vec<_>>();
            let batch = scheduler.detach_ready_batch(256);
            assert!(drops.lock().unwrap().is_empty());
            assert_eq!(polls.load(SeqCst), 0);
            assert_eq!(scheduler.snapshot().ready_task_ids, [] as [u64; 0]);
            assert_eq!(scheduler.snapshot().live_task_ids, expected_ids);
            {
                let tasks = lock(&scheduler.tasks);
                assert_eq!(tasks.len(), count);
                assert!(tasks.values().all(Option::is_none));
            }

            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(batch)));
            if panic_id.is_some() {
                let panic = result.expect_err("one destructor must panic");
                assert_eq!(
                    panic.downcast_ref::<&str>(),
                    Some(&"unconsumed batch destructor panic")
                );
            } else {
                result.expect("normal payload destruction must not panic");
            }

            assert_eq!(*drops.lock().unwrap(), (0..count).collect::<Vec<_>>());
            assert_eq!(polls.load(SeqCst), 0);
            assert!(lock(&scheduler.tasks).is_empty());
            assert_eq!(scheduler.snapshot().ready_task_ids, [] as [u64; 0]);
            assert_eq!(scheduler.snapshot().live_task_ids, expected_ids);
            assert_eq!(scheduler.poll_ready_batch(256), 0);
            scheduler.close();
            assert_eq!(scheduler.snapshot().live_task_ids, [] as [u64; 0]);
            assert_eq!(drops.lock().unwrap().len(), count);
        }
    }

    #[test]
    fn exhausted_identity_spaces_leave_scheduler_state_unchanged() {
        struct DropProbe(Arc<AtomicUsize>);

        impl Drop for DropProbe {
            fn drop(&mut self) {
                self.0.fetch_add(1, SeqCst);
            }
        }

        let scheduler = Host::new(MonotonicInstant::ZERO);
        lock(&scheduler.wake.state).next_task_id = TaskId::MAX;
        let drops = Arc::new(AtomicUsize::new(0));
        let probe = DropProbe(Arc::clone(&drops));
        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            scheduler.spawn(Box::pin(async move {
                let _probe = probe;
                std::future::pending::<()>().await;
            }));
        }));
        assert!(panic.is_err());
        assert_eq!(drops.load(SeqCst), 1);
        assert!(lock(&scheduler.tasks).is_empty());
        assert_eq!(scheduler.snapshot().live_task_ids, [] as [u64; 0]);
        assert_eq!(lock(&scheduler.wake.state).next_task_id, TaskId::MAX);

        lock(&scheduler.wake.state).next_sleep_id = SleepId::MAX;
        let panic = std::panic::catch_unwind(|| scheduler.sleep_until(MonotonicInstant::MAX));
        assert!(panic.is_err());
        assert_eq!(scheduler.pending_sleeps(), [] as [crate::PendingSleep; 0]);
        assert_eq!(lock(&scheduler.wake.state).next_sleep_id, SleepId::MAX);
    }

    #[test]
    fn every_registration_gap_observes_durable_readiness() {
        for stage in [
            WaitStage::BeforeListen,
            WaitStage::AfterListen,
            WaitStage::BeforeListenerPoll,
        ] {
            let scheduler = Host::new(MonotonicInstant::ZERO);
            let done = Arc::new(AtomicUsize::new(0));
            let mut drive = client(&done);
            let (count, waker) = counter();
            let mut context = Context::from_waker(&waker);
            let mut fired = false;
            assert!(
                drive
                    .poll_inner(&mut context, &scheduler, 10, &mut |at| {
                        if at == stage && !fired {
                            fired = true;
                            let done = Arc::clone(&done);
                            scheduler.spawn(Box::pin(async move {
                                done.store(42, SeqCst);
                            }));
                        }
                    })
                    .is_pending()
            );
            assert!(fired);
            assert!(
                count.0.load(SeqCst) > 0,
                "each gap must request another outer turn"
            );
            assert!(matches!(
                drive.poll(&mut context, &scheduler, 10),
                Poll::Ready(Ok(42))
            ));
        }
    }

    #[test]
    fn client_completion_during_registration_returns_without_another_turn() {
        for stage in [WaitStage::BeforeListen, WaitStage::AfterListen] {
            let scheduler = Host::new(MonotonicInstant::ZERO);
            let done = Arc::new(AtomicUsize::new(0));
            let mut drive = client(&done);
            let mut fired = false;
            let result = drive.poll_inner(
                &mut Context::from_waker(Waker::noop()),
                &scheduler,
                1,
                &mut |at| {
                    if at == stage {
                        fired = true;
                        done.store(42, SeqCst);
                    }
                },
            );
            assert!(fired);
            assert!(matches!(result, Poll::Ready(Ok(42))));
            assert_eq!(drive.polls, 0);
        }
    }

    #[test]
    fn work_published_during_registration_cannot_exceed_the_poll_budget() {
        let scheduler = Host::new(MonotonicInstant::ZERO);
        let done = Arc::new(AtomicUsize::new(0));
        let mut drive = client(&done);
        scheduler.spawn(Box::pin(async {}));
        let result = drive.poll_inner(
            &mut Context::from_waker(Waker::noop()),
            &scheduler,
            1,
            &mut |at| {
                if at == WaitStage::AfterListen {
                    let done = Arc::clone(&done);
                    scheduler.spawn(Box::pin(async move {
                        done.store(42, SeqCst);
                    }));
                }
            },
        );
        let Poll::Ready(Err(error)) = result else {
            panic!("newly published work must respect the exhausted poll budget");
        };
        assert!(error.is_step_budget_exhausted());
        assert_eq!(error.polls(), 1);
        assert_eq!(done.load(SeqCst), 0);
        assert_eq!(scheduler.poll_ready_batch(1), 1);
        assert_eq!(done.load(SeqCst), 42);
    }

    #[test]
    fn published_work_before_first_registration_needs_no_event_allocation() {
        let scheduler = Host::new(MonotonicInstant::ZERO);
        let done = Arc::new(AtomicUsize::new(0));
        let mut drive = client(&done);
        let output = Arc::clone(&done);
        scheduler.spawn(Box::pin(async move {
            output.store(42, SeqCst);
        }));
        assert_eq!(scheduler.wake.changed.total_listeners(), 0);
        // The guard has observed zero; registration/drive races after that
        // observation. Durable work still completes without a notification permit.
        scheduler.wake.notify_inner(|listening| {
            assert!(!listening);
            assert!(matches!(
                drive.poll(&mut Context::from_waker(Waker::noop()), &scheduler, 10),
                Poll::Ready(Ok(42))
            ));
        });
        assert!(drive.listener.is_none());
    }

    #[test]
    fn publication_broadcasts_to_two_pending_drives() {
        let scheduler = Host::new(MonotonicInstant::ZERO);
        let done = Arc::new(AtomicUsize::new(0));
        let mut first = client(&done);
        let mut second = client(&done);
        let (first_count, first_wake) = counter();
        let (second_count, second_wake) = counter();
        assert!(
            first
                .poll(&mut Context::from_waker(&first_wake), &scheduler, 10)
                .is_pending()
        );
        assert!(
            second
                .poll(&mut Context::from_waker(&second_wake), &scheduler, 10)
                .is_pending()
        );
        let output = Arc::clone(&done);
        scheduler.spawn(Box::pin(async move {
            output.store(42, SeqCst);
        }));
        assert_eq!(first_count.0.load(SeqCst), 1);
        assert_eq!(second_count.0.load(SeqCst), 1);
        assert!(matches!(
            first.poll(&mut Context::from_waker(&first_wake), &scheduler, 10),
            Poll::Ready(Ok(42))
        ));
        drop(first);
        assert!(matches!(
            second.poll(&mut Context::from_waker(&second_wake), &scheduler, 10),
            Poll::Ready(Ok(42))
        ));
    }

    #[test]
    fn consumed_notification_rearms_with_replacement_outer_waker() {
        let scheduler = Host::new(MonotonicInstant::ZERO);
        let done = Arc::new(AtomicUsize::new(0));
        let mut drive = client(&done);
        let (old_count, old_wake) = counter();
        let (new_count, new_wake) = counter();
        assert!(
            drive
                .poll(&mut Context::from_waker(&old_wake), &scheduler, 10)
                .is_pending()
        );
        scheduler.spawn(Box::pin(async {}));
        assert_eq!(old_count.0.load(SeqCst), 1);
        assert!(
            drive
                .poll(&mut Context::from_waker(&new_wake), &scheduler, 10)
                .is_pending()
        );
        assert!(drive.listener.is_none());
        assert!(
            drive
                .poll(&mut Context::from_waker(&new_wake), &scheduler, 10)
                .is_pending()
        );
        assert!(drive.listener.is_some());
        let before = new_count.0.load(SeqCst);
        scheduler.spawn(Box::pin(async {}));
        assert_eq!(new_count.0.load(SeqCst), before + 1);
        assert_eq!(old_count.0.load(SeqCst), 1);
    }

    #[test]
    fn completed_and_closed_task_wakes_do_not_notify_stalled_drives() {
        for close in [false, true] {
            let scheduler = Host::new(MonotonicInstant::ZERO);
            let recorded = Arc::new(Mutex::new(None));
            let captured = Arc::clone(&recorded);
            scheduler.spawn(Box::pin(std::future::poll_fn(move |cx| {
                *captured.lock().unwrap() = Some(cx.waker().clone());
                if close {
                    Poll::Pending
                } else {
                    Poll::Ready(())
                }
            })));
            assert_eq!(scheduler.poll_ready_batch(1), 1);
            if close {
                scheduler.close();
            }
            let mut drive = client(&Arc::new(AtomicUsize::new(0)));
            let (count, waker) = counter();
            assert!(
                drive
                    .poll(&mut Context::from_waker(&waker), &scheduler, 10)
                    .is_pending()
            );
            let before = count.0.load(SeqCst);
            let stale = recorded.lock().unwrap().take().unwrap();
            std::thread::spawn(move || {
                for _ in 0..32 {
                    stale.wake_by_ref();
                }
            })
            .join()
            .unwrap();
            assert_eq!(count.0.load(SeqCst), before);
            assert_eq!(scheduler.snapshot().ready_task_ids, [] as [u64; 0]);
        }
    }

    #[test]
    fn abandoned_diagnostic_id_wakes_notify_once_without_resurrecting_payload() {
        let scheduler = Host::new(MonotonicInstant::ZERO);
        let recorded = Arc::new(Mutex::new(None));
        let captured = Arc::clone(&recorded);
        scheduler.spawn(Box::pin(std::future::poll_fn(move |cx| -> Poll<()> {
            *captured.lock().unwrap() = Some(cx.waker().clone());
            panic!("original task panic");
        })));
        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            scheduler.poll_ready_batch(1)
        }))
        .unwrap_err();
        assert_eq!(panic.downcast_ref::<&str>(), Some(&"original task panic"));
        assert!(scheduler.tasks.lock().is_empty());
        assert_eq!(scheduler.snapshot().live_task_ids, [0]);
        let mut drive = client(&Arc::new(AtomicUsize::new(0)));
        let (count, waker) = counter();
        assert!(
            drive
                .poll(&mut Context::from_waker(&waker), &scheduler, 10)
                .is_pending()
        );
        let stale = recorded.lock().unwrap().take().unwrap();
        std::thread::spawn(move || {
            for _ in 0..32 {
                stale.wake_by_ref();
            }
        })
        .join()
        .unwrap();
        assert_eq!(count.0.load(SeqCst), 1);
        assert_eq!(scheduler.snapshot().ready_task_ids, [0]);
        assert_eq!(scheduler.poll_ready_batch(10), 0);
        assert!(scheduler.tasks.lock().is_empty());
    }

    #[test]
    fn close_notifies_before_one_payload_destructor_panics() {
        struct PanicOnDrop;
        impl Drop for PanicOnDrop {
            fn drop(&mut self) {
                panic!("original destructor panic");
            }
        }
        let scheduler = Host::new(MonotonicInstant::ZERO);
        let probe = PanicOnDrop;
        scheduler.spawn(Box::pin(async move {
            let _probe = probe;
            std::future::pending::<()>().await;
        }));
        let mut drive = client(&Arc::new(AtomicUsize::new(0)));
        let (count, waker) = counter();
        assert!(
            drive
                .poll(&mut Context::from_waker(&waker), &scheduler, 10)
                .is_pending()
        );
        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| scheduler.close()))
            .unwrap_err();
        assert_eq!(
            panic.downcast_ref::<&str>(),
            Some(&"original destructor panic")
        );
        assert_eq!(count.0.load(SeqCst), 1);
        assert_eq!(scheduler.snapshot().live_task_ids, [] as [u64; 0]);
        assert!(scheduler.tasks.lock().is_empty());
    }

    #[test]
    fn ready_reserved_payload_republishes_after_other_drive_rearms() {
        let scheduler = Arc::new(Host::new(MonotonicInstant::ZERO));
        let done_a = Arc::new(AtomicUsize::new(0));
        let done_b = Arc::new(AtomicUsize::new(0));
        let mut drive_a = client(&done_a);
        let drive_b = Arc::new(Mutex::new(client(&done_b)));
        let (b_count, b_wake) = counter();
        let recorded = Arc::new(Mutex::new(None));
        let observed = Arc::clone(&recorded);
        let polls = Arc::new(AtomicUsize::new(0));
        let task_polls = Arc::clone(&polls);
        let reserved_count = Arc::new(AtomicUsize::new(0));
        let task_count = Arc::clone(&reserved_count);
        let task_scheduler = Arc::clone(&scheduler);
        let task_b = Arc::clone(&drive_b);
        let task_b_wake = b_wake.clone();
        let task_b_count = Arc::clone(&b_count);
        scheduler.spawn(Box::pin(std::future::poll_fn(move |cx| {
            match task_polls.fetch_add(1, SeqCst) {
                0 => {
                    *observed.lock().unwrap() = Some(cx.waker().clone());
                    Poll::Pending
                }
                1 => {
                    cx.waker().wake_by_ref();
                    let mut other = task_b.lock().unwrap();
                    let mut context = Context::from_waker(&task_b_wake);
                    assert!(other.poll(&mut context, &task_scheduler, 10).is_pending());
                    assert!(other.poll(&mut context, &task_scheduler, 10).is_pending());
                    assert!(other.listener.is_some());
                    task_count.store(task_b_count.0.load(SeqCst), SeqCst);
                    done_a.store(1, SeqCst);
                    Poll::Pending
                }
                2 => {
                    done_b.store(2, SeqCst);
                    Poll::Ready(())
                }
                _ => panic!("reserved task must not be polled recursively"),
            }
        })));
        assert_eq!(scheduler.poll_ready_batch(1), 1);
        assert!(
            drive_b
                .lock()
                .unwrap()
                .poll(&mut Context::from_waker(&b_wake), &scheduler, 10)
                .is_pending()
        );
        recorded.lock().unwrap().take().unwrap().wake();
        assert!(matches!(
            drive_a.poll(&mut Context::from_waker(Waker::noop()), &scheduler, 10),
            Poll::Ready(Ok(1))
        ));
        assert_eq!(polls.load(SeqCst), 2);
        assert!(b_count.0.load(SeqCst) > reserved_count.load(SeqCst));
        assert!(matches!(
            drive_b
                .lock()
                .unwrap()
                .poll(&mut Context::from_waker(&b_wake), &scheduler, 10),
            Poll::Ready(Ok(2))
        ));
        assert_eq!(polls.load(SeqCst), 3);
    }
}
