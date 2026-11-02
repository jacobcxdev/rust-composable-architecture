//! Provides demand-driven, owned timers over an explicit clock.
//!
//! Shared and owner-local wrappers use the same cadence and cancellation state.
//! Each wrapper supplies its own clock and owned wait type. The state adds no
//! allocation or spawning interface beyond the wait supplied by that clock.

use super::{Clock, MonotonicInstant, RuntimeFuture};
use std::fmt;
use std::time::Duration;

/// Reports a zero timer interval.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InvalidTimerInterval;

impl fmt::Display for InvalidTimerInterval {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("timer interval must exceed zero")
    }
}

impl std::error::Error for InvalidTimerInterval {}

/// Owns a demand-driven sequence of observed clock instants.
///
/// The first poll of [`Self::next`] anchors the first deadline one interval after
/// that poll's clock time. Construction and unpolled `next` futures do not read
/// time or create a wait. Independent timers have independent anchors and state,
/// even when their captured clocks share an origin.
///
/// # Missed ticks and demand
///
/// A completed wait produces observed clock time `n`, not its scheduled deadline
/// `d`. When `n >= d`, the `1 + floor((n - d) / interval)` elapsed cadence positions
/// coalesce into that one result. The next deadline is `n + interval`. This
/// deliberately skips missed ticks and resets phase instead of replaying them.
/// Arithmetic saturates at [`MonotonicInstant::MAX`]. One final tick at `MAX`
/// terminates the timer. A first poll already at `MAX` terminates without a tick.
///
/// The timer spawns no task, maintains no tick queue and owns at most one wait.
/// After a tick, no sleep is created until the next demand poll. A stalled
/// consumer receives at most one overdue tick before waiting another interval.
/// This is not lossless buffering or a guarantee of host scheduling precision.
///
/// # Cancellation and ownership
///
/// Dropping a pending `next` future preserves the timer's deadline and sleep.
/// The sleep can retain the abandoned poll's waker until repolled, completed or
/// dropped, according to its clock driver. [`Self::cancel`] or dropping the timer
/// retires that sleep. Cancellation is terminal and future calls return `None`.
/// Capturing a clock does not make this timer Store-owned. For Store cancellation,
/// move it into an owned [`crate::Effect::future`] or [`crate::Effect::run`].
/// Clock callbacks and wait destructors can panic. Timer operations propagate
/// these panics and do not promise to resume a tick interrupted by a panic.
///
/// # Examples
///
/// ```
/// use rca_core::{Clock, ClockTimer};
/// use std::time::Duration;
///
/// async fn consume(clock: Clock) {
///     let mut timer = ClockTimer::new(clock, Duration::from_secs(1)).unwrap();
///     let _first = timer.next().await;
///     timer.cancel();
///     assert_eq!(timer.next().await, None);
/// }
/// ```
#[must_use = "timers do nothing until next is polled"]
pub struct ClockTimer {
    /// Retains the explicit provider used for time reads and wait creation.
    clock: Clock,
    /// Keeps cadence and wait ownership across successive `next` calls.
    state: TimerState<RuntimeFuture>,
}

impl ClockTimer {
    /// Creates an inactive timer capturing `clock` without reading it.
    ///
    /// # Errors
    ///
    /// If `interval` is zero, returns [`InvalidTimerInterval`] without querying the clock.
    pub fn new(clock: Clock, interval: Duration) -> Result<Self, InvalidTimerInterval> {
        Ok(Self {
            clock,
            state: TimerState::new(interval)?,
        })
    }

    /// Waits for one observed instant, or returns `None` after termination.
    ///
    /// First-poll anchoring, missed ticks and cancellation follow [`ClockTimer`].
    /// Dropping this future retains any registered sleep in the timer. Waiting
    /// again resumes the same deadline. Use [`Self::cancel`] to retire the stream.
    ///
    /// # Panics
    ///
    /// Propagates clock, wait-poll and wait-destructor panics.
    /// Resuming the interrupted tick after a panic is not supported.
    pub async fn next(&mut self) -> Option<MonotonicInstant> {
        let clock = &self.clock;
        self.state
            .next(|| clock.now(), |deadline| clock.sleep_until(deadline))
            .await
    }

    /// Terminates the timer and drops any owned wait immediately.
    ///
    /// Repeated calls do nothing. Later [`Self::next`] calls return `None` without
    /// consulting the clock, even if the timer was cancelled before its first poll.
    ///
    /// # Panics
    ///
    /// Propagates a panic from the owned wait's destructor. The timer is already
    /// terminal, so later `next` calls do not query the clock or recreate the wait.
    pub fn cancel(&mut self) {
        self.state.cancel();
    }
}

impl fmt::Debug for ClockTimer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.state.fmt(&self.clock, f)
    }
}

/// Shares cadence and single-wait ownership between shared and local timers.
///
/// `W` owns the registered wait. An abandoned demand future only borrows it.
/// At suspension points, an owned wait has a deadline and the timer is unfinished.
/// A completed tick keeps its next deadline but creates no wait until demand resumes.
pub(super) struct TimerState<W> {
    /// Positive spacing between the observed tick and its next deadline.
    interval: Duration,
    /// The pending or next deadline, absent before first demand and after termination.
    deadline: Option<MonotonicInstant>,
    /// The sole wait, retained across abandoned `next` futures.
    wait: Option<W>,
    /// Prevents all later clock access after cancellation or the maximum instant.
    finished: bool,
}

impl<W> TimerState<W> {
    /// Rejects zero spacing and creates idle state without a wait or time read.
    pub(super) fn new(interval: Duration) -> Result<Self, InvalidTimerInterval> {
        if interval.is_zero() {
            return Err(InvalidTimerInterval);
        }
        Ok(Self {
            interval,
            deadline: None,
            wait: None,
            finished: false,
        })
    }

    /// Commits terminal state before dropping the wait, including during unwinding.
    pub(super) fn cancel(&mut self) {
        self.finished = true;
        self.deadline = None;
        self.wait = None;
    }

    /// Formats cached progress and the supplied clock description.
    pub(super) fn fmt(&self, clock: &dyn fmt::Debug, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClockTimer")
            .field("clock", clock)
            .field("interval", &self.interval)
            .field("deadline", &self.deadline)
            .field("waiting", &self.wait.is_some())
            .field("finished", &self.finished)
            .finish()
    }
}

impl<W: Future<Output = ()> + Unpin> TimerState<W> {
    /// Anchors on first demand, resumes the owned wait and returns observed time.
    ///
    /// `Unpin` applies to the wait handle, not the future behind a pinned box.
    /// Both clock callbacks must share one monotonic origin. The wait must not
    /// complete before its deadline. The helper relies on these driver contracts.
    pub(super) async fn next(
        &mut self,
        now: impl Fn() -> MonotonicInstant,
        sleep_until: impl FnOnce(MonotonicInstant) -> W,
    ) -> Option<MonotonicInstant> {
        if self.finished {
            return None;
        }
        let deadline = if let Some(deadline) = self.deadline {
            deadline
        } else {
            let now = now();
            if now == MonotonicInstant::MAX {
                self.cancel();
                return None;
            }
            let deadline = now.saturating_add(self.interval);
            self.deadline = Some(deadline);
            deadline
        };
        self.wait.get_or_insert_with(|| sleep_until(deadline)).await;
        self.wait = None;
        let now = now();
        if now == MonotonicInstant::MAX {
            self.cancel();
        } else {
            self.deadline = Some(now.saturating_add(self.interval));
        }
        Some(now)
    }
}
