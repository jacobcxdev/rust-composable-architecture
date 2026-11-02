//! Provides explicit client-clock policies for deterministic tests.
//!
//! These fixtures have no spawning or host authority. Immediate waits advance
//! time when polled; forbidden operations report use and follow fixed fallbacks.
//! Client recurrence remains explicitly owned by the caller's future.

use super::{
    ClockCapabilities, ClockDriver, MonotonicInstant, RuntimeFuture, UnsupportedClockTolerance,
};
use crate::{Issue, IssueReporter};
use std::fmt;
use std::panic::Location;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::Poll;
use std::time::Duration;

/// Advances virtual time when a client first polls a future wait.
///
/// Clones share one explicit origin and nondecreasing time. Independent constructors
/// create independent origins; equal elapsed values do not make them interchangeable.
/// A future deadline advances time, then yields one cooperative pending opportunity
/// before completing. Elapsed deadlines are ready on first poll. Dropping an
/// unpolled wait does not move time; dropping after first poll retains its advance.
/// Resolution and minimum advisory tolerance are zero. Slack is ignored: time
/// advances exactly to the deadline, not before it. Relative waits through
/// [`super::Clock`] use unsigned duration arithmetic saturating at the maximum.
///
/// # Examples
///
/// ```
/// use rca_core::{Clock, MonotonicInstant};
/// use rca_core::ImmediateClock;
/// use std::time::Duration;
/// let fixture = ImmediateClock::new(MonotonicInstant::ZERO);
/// let clock = Clock::new(fixture.clone());
/// let wait = clock.sleep(Duration::from_secs(10));
/// drop(wait); // Cancellation before polling does not advance virtual time.
/// assert_eq!(clock.now(), MonotonicInstant::ZERO);
/// ```
///
/// # Panics
///
/// Clock operations propagate poisoning of the shared time mutex.
#[derive(Clone, Debug)]
pub struct ImmediateClock {
    /// Shares the current elapsed time; every update keeps the larger instant.
    now: Arc<Mutex<MonotonicInstant>>,
}

impl ImmediateClock {
    /// Creates an independent clock at the explicit origin-relative `now`.
    #[must_use]
    pub fn new(now: MonotonicInstant) -> Self {
        Self {
            now: Arc::new(Mutex::new(now)),
        }
    }
}

impl ClockDriver for ImmediateClock {
    fn now(&self) -> MonotonicInstant {
        *self
            .now
            .lock()
            .expect("immediate clock lock must not be poisoned")
    }

    fn sleep_until(&self, deadline: MonotonicInstant) -> RuntimeFuture {
        let now = Arc::clone(&self.now);
        let mut advanced = false;
        Box::pin(std::future::poll_fn(move |context| {
            if advanced {
                return Poll::Ready(());
            }
            let elapsed = {
                let mut now = now
                    .lock()
                    .expect("immediate clock lock must not be poisoned");
                let elapsed = deadline <= *now;
                *now = (*now).max(deadline);
                elapsed
            };
            if elapsed {
                return Poll::Ready(());
            }
            advanced = true;
            // User wake code runs after releasing the time lock.
            context.waker().wake_by_ref();
            Poll::Pending
        }))
    }

    fn capabilities(&self) -> ClockCapabilities {
        ClockCapabilities {
            resolution: Some(Duration::ZERO),
            minimum_tolerance: Some(Duration::ZERO),
        }
    }
}

/// Records forbidden dependency use independently of diagnostic delivery.
///
/// Every clock operation marks this fixture used before reporting an issue. Now
/// falls back to the fixed baseline; elapsed waits are ready and future waits stay
/// pending forever without advancing time. Tolerant waits always return unsupported.
/// Reports name the operation and retain the fixture construction location.
/// Disabled or closed delivery is ignored only for this documented fallback;
/// it never clears required-use evidence. Call [`Self::assert_unused`] after all
/// client work has finished or been cancelled. Merely recording an issue does not
/// fail a test, and dropping this fixture does not implicitly assert anything.
/// Clones share use evidence; independent constructions do not. No ambient
/// registry, executor or test detector is consulted.
/// Reports occur synchronously at operation invocation, even for an unpolled
/// wait; dropping that wait does not erase use evidence. The erased
/// [`super::Clock`] may query capabilities or now before forwarding a wait, so
/// each of those actual operations reports separately.
///
/// # Examples
///
/// ```
/// use rca_core::{Clock, IssueReporter, MonotonicInstant};
/// use rca_core::ForbiddenClock;
/// let fixture = ForbiddenClock::new("client", MonotonicInstant::ZERO, IssueReporter::default());
/// fixture.assert_unused().unwrap();
/// let clock = Clock::new(fixture.clone());
/// assert_eq!(clock.now(), MonotonicInstant::ZERO); // Documented fallback.
/// assert!(fixture.assert_unused().is_err()); // Required assertion survives disabled reporting.
/// ```
///
/// # Panics
///
/// Clock operations propagate report-handler panics after marking this fixture used.
#[derive(Clone, Debug)]
pub struct ForbiddenClock {
    /// Shares immutable fallback settings and monotone use evidence across clones.
    shared: Arc<ForbiddenState>,
}

/// Retains one fixture's diagnostic identity, fallback and required-use evidence.
#[derive(Debug)]
struct ForbiddenState {
    /// Identifies the injected dependency in each report.
    name: String,
    /// Supplies fixed time and distinguishes elapsed waits from future waits.
    baseline: MonotonicInstant,
    /// Receives diagnostics without owning the required-unused assertion.
    reporter: IssueReporter,
    /// Points to fixture construction rather than its internal reporting helper.
    location: &'static Location<'static>,
    /// Records any use permanently, independently of reporting success.
    used: AtomicBool,
}

/// Reports that a forbidden clock was used during a required-unused test.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ForbiddenClockUsed;

impl fmt::Display for ForbiddenClockUsed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("forbidden clock was used")
    }
}
impl std::error::Error for ForbiddenClockUsed {}

impl ForbiddenClock {
    /// Creates an independent named fixture with a fixed fallback baseline.
    #[track_caller]
    #[must_use]
    pub fn new(
        name: impl Into<String>,
        baseline: MonotonicInstant,
        reporter: IssueReporter,
    ) -> Self {
        Self {
            shared: Arc::new(ForbiddenState {
                name: name.into(),
                baseline,
                reporter,
                location: Location::caller(),
                used: AtomicBool::new(false),
            }),
        }
    }

    /// Checks required-unused evidence without consuming or clearing it.
    ///
    /// Run after joining or cancelling all client work; a concurrent future use
    /// can occur after this snapshot. This check is independent of issue delivery.
    ///
    /// # Errors
    ///
    /// Returns [`ForbiddenClockUsed`] if any clone has performed a clock operation.
    pub fn assert_unused(&self) -> Result<(), ForbiddenClockUsed> {
        if self.shared.used.load(Ordering::Acquire) {
            Err(ForbiddenClockUsed)
        } else {
            Ok(())
        }
    }

    /// Marks use before formatting or invoking a handler, then reports the operation.
    fn report_use(&self, operation: &'static str) {
        self.shared.used.store(true, Ordering::Release);
        let mut issue = Issue::new(
            operation,
            format!("Forbidden clock '{}': {operation}", self.shared.name),
        );
        issue.location = self.shared.location;
        let _ = self.shared.reporter.report(issue);
    }
}

impl ClockDriver for ForbiddenClock {
    fn now(&self) -> MonotonicInstant {
        self.report_use("clock.forbidden.now");
        self.shared.baseline
    }

    fn sleep_until(&self, deadline: MonotonicInstant) -> RuntimeFuture {
        self.report_use("clock.forbidden.sleep_until");
        if deadline <= self.shared.baseline {
            // Reporting owns its diagnostic; the completed wait needs no storage.
            Box::pin(std::future::poll_fn(|_| Poll::Ready(())))
        } else {
            Box::pin(std::future::pending())
        }
    }

    fn capabilities(&self) -> ClockCapabilities {
        self.report_use("clock.forbidden.capabilities");
        ClockCapabilities::default()
    }

    fn sleep_until_with_tolerance(
        &self,
        _deadline: MonotonicInstant,
        _tolerance: Duration,
    ) -> Result<RuntimeFuture, UnsupportedClockTolerance> {
        self.report_use("clock.forbidden.tolerance");
        Err(UnsupportedClockTolerance)
    }
}
