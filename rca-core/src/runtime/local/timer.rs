//! Provides demand-driven timers retaining local clocks and wait captures.

use std::fmt;
use std::time::Duration;

use super::super::timer::{InvalidTimerInterval, TimerState};
use super::{Clock, MonotonicInstant, RuntimeFuture};

/// Owns a demand-driven sequence of observed instants on the clock's owner thread.
///
/// Timing, missed ticks and cancellation follow [`super::super::ClockTimer`]: the
/// first demand poll anchors the cadence; completed waits return observed time,
/// skip missed ticks and reset the next deadline one interval after that time.
/// Construction and unpolled `next` calls never read the clock or create a wait.
/// One final tick at [`MonotonicInstant::MAX`] terminates the timer; a first poll
/// already at the maximum terminates without a tick.
///
/// At most one wait is retained. Dropping a pending [`Self::next`] future preserves
/// its wait and deadline; [`Self::cancel`] or timer destruction releases it on the
/// owner thread. No task is spawned and no tick queue or background wait is added.
/// Holding a clock does not retain its host or confer Store mutation authority.
/// A driver may receive wake notifications from any thread but must only poll
/// and destroy local payloads on the owner thread.
/// An abandoned demand can leave its waker in the retained wait until repolled
/// or dropped. Clock callbacks and wait destructors can panic; an interrupted
/// tick is not resumable after a panic.
///
/// # Examples
///
/// ```
/// use rca_core::local::{Clock, ClockTimer};
/// use std::time::Duration;
///
/// async fn consume(clock: Clock) {
///     let mut timer = ClockTimer::new(clock, Duration::from_secs(1)).unwrap();
///     let _first = timer.next().await;
///     timer.cancel();
///     assert_eq!(timer.next().await, None);
/// }
/// ```
///
/// A local timer and its pending demand cannot cross threads:
///
/// ```compile_fail
/// use rca_core::local::ClockTimer;
/// fn require_send<T: Send>() {}
/// require_send::<ClockTimer>();
/// ```
///
/// ```compile_fail
/// use rca_core::local::ClockTimer;
/// fn require_sync<T: Sync>() {}
/// require_sync::<ClockTimer>();
/// ```
///
/// ```compile_fail
/// use rca_core::local::ClockTimer;
/// fn require_send(_: impl Send) {}
/// fn transfer_next(timer: &mut ClockTimer) {
///     require_send(timer.next());
/// }
/// ```
#[must_use = "timers do nothing until next is polled"]
pub struct ClockTimer {
    /// Retains the explicit owner-local provider without retaining its host.
    clock: Clock,
    /// Keeps one wait and its cadence across successive demand futures.
    state: TimerState<RuntimeFuture>,
}

impl ClockTimer {
    /// Creates an inactive timer without reading its local clock.
    ///
    /// # Errors
    ///
    /// Returns [`InvalidTimerInterval`] for a zero interval, without querying the clock.
    pub fn new(clock: Clock, interval: Duration) -> Result<Self, InvalidTimerInterval> {
        Ok(Self {
            clock,
            state: TimerState::new(interval)?,
        })
    }

    /// Waits for one observed instant, or returns `None` after termination.
    ///
    /// Dropping this future preserves any registered wait and its deadline.
    /// Waiting again resumes it; use [`Self::cancel`] to retire the timer.
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

    /// Terminates the timer and drops its wait immediately on the owner thread.
    ///
    /// Repeated calls do nothing. Future [`Self::next`] calls return `None` without
    /// consulting the clock, even when cancelled before its first demand poll.
    ///
    /// # Panics
    ///
    /// Propagates a panic from the owned wait's destructor. The timer is already
    /// terminal, so later `next` calls do not read time or recreate the wait.
    pub fn cancel(&mut self) {
        self.state.cancel();
    }
}

impl fmt::Debug for ClockTimer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.state.fmt(&self.clock, f)
    }
}
