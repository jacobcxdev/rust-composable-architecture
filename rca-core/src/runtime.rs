//! Defines runtime-neutral scheduling and monotonic time.

use std::fmt;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

mod timer;

#[cfg(feature = "local")]
pub mod local;

#[doc(inline)]
pub use timer::{ClockTimer, InvalidTimerInterval};

#[cfg(feature = "test-util")]
mod fixtures;

#[cfg(feature = "test-util")]
#[doc(inline)]
pub use fixtures::{ForbiddenClock, ForbiddenClockUsed, ImmediateClock};

/// Boxed task accepted by runtime drivers.
pub type RuntimeFuture = Pin<Box<dyn std::future::Future<Output = ()> + Send + 'static>>;

/// Represents monotonic time from one declared clock origin.
///
/// This unsigned elapsed value does not encode origin identity. Comparing or
/// passing values between independently constructed clock origins is invalid,
/// even when their elapsed values happen to match. Obtain client and actor time
/// from the same runtime, or explicitly arrange a shared origin in your provider.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct MonotonicInstant(Duration);

impl MonotonicInstant {
    /// Zero elapsed monotonic time.
    pub const ZERO: Self = Self(Duration::ZERO);

    /// Maximum representable monotonic time.
    pub const MAX: Self = Self(Duration::MAX);

    /// Creates an instant from elapsed monotonic time.
    #[must_use]
    pub const fn from_elapsed(elapsed: Duration) -> Self {
        Self(elapsed)
    }

    /// Returns elapsed monotonic time.
    #[must_use]
    pub const fn elapsed(self) -> Duration {
        self.0
    }

    /// Adds `duration` without overflowing.
    #[must_use]
    pub const fn saturating_add(self, duration: Duration) -> Self {
        Self(self.0.saturating_add(duration))
    }
}

/// Declares timestamp granularity and supported advisory timer slack.
///
/// Neither field guarantees host wake-up accuracy or an upper bound on scheduling
/// latency. `None` means unknown resolution or unsupported tolerance, respectively.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ClockCapabilities {
    /// Declared timestamp resolution, not observed host scheduling precision.
    pub resolution: Option<Duration>,
    /// Minimum accepted coalescing slack; `None` rejects tolerant waits.
    pub minimum_tolerance: Option<Duration>,
}

/// Reports that a clock does not support explicit timer tolerance.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UnsupportedClockTolerance;

impl fmt::Display for UnsupportedClockTolerance {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("clock does not support explicit timer tolerance")
    }
}

impl std::error::Error for UnsupportedClockTolerance {}

/// Supplies monotonic time and cancellable waits without spawning tasks.
///
/// Implementations share one origin across their handles. Time never decreases;
/// elapsed deadlines are ready on first poll. Dropping a wait cancels it and
/// releases captured resources. A clock handle is not host or spawning authority.
pub trait ClockDriver: fmt::Debug + Send + Sync + 'static {
    /// Returns current monotonic time from this provider's origin.
    fn now(&self) -> MonotonicInstant;

    /// Returns an owned wait completing no earlier than `deadline`.
    ///
    /// `deadline` must use this provider's origin. Elapsed deadlines are ready on
    /// first poll; dropping the returned future cancels and releases the wait.
    fn sleep_until(&self, deadline: MonotonicInstant) -> RuntimeFuture;

    /// Returns declared clock capabilities, defaulting to unknown and unsupported.
    fn capabilities(&self) -> ClockCapabilities {
        ClockCapabilities::default()
    }

    /// Returns a wait permitting advisory coalescing slack after `deadline`.
    ///
    /// Tolerance never permits an early completion and is not a host latency
    /// bound. Providers may ignore slack and wait exactly until the deadline;
    /// this default does so when support is declared. Elapsed deadlines remain
    /// ready on first poll. Callers must use this origin and respect the declared
    /// minimum tolerance; [`Clock`] applies that floor before forwarding.
    ///
    /// # Errors
    ///
    /// Returns [`UnsupportedClockTolerance`] when tolerance is not supported.
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

/// Schedules RCA tasks and supplies monotonic time.
///
/// # Driver contract
///
/// A successfully constructed adapter must accept tasks while its host remains
/// live. Each accepted task must be polled according to the [`Future`] contract
/// until completion, or dropped exactly once during host teardown.
/// Silently discarding accepted work on a live host violates this contract.
///
/// Wake-ups may arrive from any thread and must not require an RCA actor lock.
/// Driver method panics and contract violations are programming faults, not
/// recoverable effect failures. Dropping the host may discard pending tasks;
/// RCA's task drop guards then close affected Stores abortively. Retained
/// [`Runtime`] handles do not guarantee that the host remains alive, and RCA
/// does not migrate tasks to another driver after teardown.
pub trait RuntimeDriver: fmt::Debug + Send + Sync + 'static {
    /// Accepts `task` for later polling.
    ///
    /// Transfers ownership and returns promptly without polling `task` inline.
    /// A host that has already shut down may drop the task without polling it.
    fn spawn(&self, task: RuntimeFuture);

    /// Returns current monotonic time.
    ///
    /// Time never decreases. Clones and handles to the same driver must share
    /// one clock origin; deadlines are meaningful only within that origin.
    fn now(&self) -> MonotonicInstant;

    /// Returns a future completing no earlier than `deadline`.
    ///
    /// An elapsed deadline must be ready on the first poll. Dropping the future
    /// cancels the wait safely and releases its captured resources.
    fn sleep_until(&self, deadline: MonotonicInstant) -> RuntimeFuture;

    /// Returns declared clock capabilities, defaulting to unknown and unsupported.
    fn clock_capabilities(&self) -> ClockCapabilities {
        ClockCapabilities::default()
    }

    /// Returns a wait permitting advisory coalescing slack after `deadline`.
    ///
    /// The same-origin, elapsed-readiness and cancellation contract of
    /// [`Self::sleep_until`] still applies. Slack never permits early completion
    /// or guarantees a host latency bound. The default ignores slack when support
    /// is declared, preserving exact waits. [`Clock`] applies the declared floor.
    ///
    /// # Errors
    ///
    /// Returns [`UnsupportedClockTolerance`] when tolerance is not supported.
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

    /// Returns a future yielding at least one scheduling opportunity.
    ///
    /// Dropping the future cancels the wait safely and releases its captured
    /// resources. A scheduling opportunity does not guarantee another task runs.
    fn yield_now(&self) -> RuntimeFuture;
}

/// Shares access to a runtime without exposing its driver type.
///
/// Cloning this handle shares the driver and clock origin. It does not create
/// another executor. Driver calls propagate provider panics without catching them.
#[derive(Clone)]
pub struct Runtime {
    /// Shares the driver without promising that its external host remains alive.
    driver: Arc<dyn RuntimeDriver>,
}

impl Runtime {
    /// Wraps a runtime driver in a shared handle.
    ///
    /// The caller supplies a driver that satisfies the [`RuntimeDriver`] contract.
    /// Construction does not start or test the executor.
    #[must_use]
    pub fn new(driver: impl RuntimeDriver) -> Self {
        Self {
            driver: Arc::new(driver),
        }
    }

    /// Returns current monotonic time.
    #[must_use]
    pub fn now(&self) -> MonotonicInstant {
        self.driver.now()
    }

    /// Returns a clock sharing this runtime's monotonic origin.
    ///
    /// Construct production clients from this clock and pass this same runtime
    /// handle to their Store. Constructing a separate adapter can create another
    /// origin even when both adapters use the same executor.
    #[must_use]
    pub fn clock(&self) -> Clock {
        Clock {
            source: ClockSource::Runtime(self.clone()),
        }
    }

    /// Transfers an owned task to the driver without polling it here.
    pub(crate) fn spawn(&self, task: RuntimeFuture) {
        self.driver.spawn(task);
    }

    /// Requests an owned wait using this driver's clock origin.
    pub(crate) fn sleep_until(&self, deadline: MonotonicInstant) -> RuntimeFuture {
        self.driver.sleep_until(deadline)
    }

    /// Requests a cancellable scheduling opportunity from the driver.
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

/// Provides cloneable monotonic time and waits without spawning authority.
///
/// Clones share their provider's clock origin, not the Store actor's timer queue
/// ordering or its one-sleep optimisation. Deadlines from another origin are
/// not interchangeable. Retaining a clock does not retain host authority.
/// External futures are not Store-owned merely because they use this clock;
/// use an owned [`crate::Effect::future`] for Store-managed cancellation.
/// Clock operations propagate provider panics without catching them.
#[derive(Clone, Debug)]
pub struct Clock {
    /// Retains either the existing runtime handle or a clock-only provider.
    source: ClockSource,
}

/// Reuses runtime storage or shares a provider that has no spawning interface.
///
/// The runtime variant avoids another allocation when obtaining its clock handle.
#[derive(Clone, Debug)]
enum ClockSource {
    /// Keeps the runtime's existing driver and origin, without exposing task submission.
    Runtime(Runtime),
    /// Shares an independently supplied clock through its own reference-counted allocation.
    Provider(Arc<dyn ClockDriver>),
}

impl Clock {
    /// Wraps a clock-only provider without requiring a spawner.
    ///
    /// The caller supplies a valid [`ClockDriver`]. Independently constructed
    /// providers must not exchange instants unless they declare a shared origin.
    ///
    /// # Examples
    ///
    /// ```
    /// use rca_core::{Clock, MonotonicInstant, RuntimeFuture};
    /// use rca_core::ClockDriver;
    /// use std::time::Duration;
    /// #[derive(Debug)]
    /// struct FixedClock;
    /// impl ClockDriver for FixedClock {
    ///     fn now(&self) -> MonotonicInstant {
    ///         MonotonicInstant::ZERO
    ///     }
    ///
    ///     fn sleep_until(&self, deadline: MonotonicInstant) -> RuntimeFuture {
    ///         if deadline == MonotonicInstant::ZERO {
    ///             Box::pin(std::future::ready(()))
    ///         } else {
    ///             Box::pin(std::future::pending())
    ///         }
    ///     }
    /// }
    /// let clock = Clock::new(FixedClock);
    /// let wait = clock.sleep(Duration::ZERO);
    /// ```
    #[must_use]
    pub fn new(provider: impl ClockDriver) -> Self {
        Self {
            source: ClockSource::Provider(Arc::new(provider)),
        }
    }

    /// Borrows the selected clock interface without another allocation or handle clone.
    fn driver(&self) -> &dyn ClockDriver {
        match &self.source {
            ClockSource::Runtime(runtime) => runtime,
            ClockSource::Provider(provider) => provider.as_ref(),
        }
    }

    /// Returns declared resolution and advisory tolerance support.
    #[must_use]
    pub fn capabilities(&self) -> ClockCapabilities {
        self.driver().capabilities()
    }

    /// Returns current monotonic time from this clock's origin.
    #[must_use]
    pub fn now(&self) -> MonotonicInstant {
        self.driver().now()
    }

    /// Returns an owned wait with its deadline fixed at invocation.
    ///
    /// Adds `duration` to the current time, saturating at [`MonotonicInstant::MAX`].
    /// Delayed first polling does not rebase the deadline. Dropping the future
    /// cancels the wait and releases its captured resources.
    #[must_use = "sleep futures must be polled to wait for their deadline"]
    pub fn sleep(&self, duration: Duration) -> RuntimeFuture {
        self.sleep_until(self.now().saturating_add(duration))
    }

    /// Returns an owned wait completing no earlier than `deadline`.
    ///
    /// The deadline must use this clock's origin. An elapsed deadline is ready
    /// on the first poll. Dropping the future cancels the wait and releases its
    /// captured resources.
    #[must_use = "sleep futures must be polled to wait for their deadline"]
    pub fn sleep_until(&self, deadline: MonotonicInstant) -> RuntimeFuture {
        self.driver().sleep_until(deadline)
    }

    /// Returns a wait with explicit advisory slack, clamped to the declared minimum.
    ///
    /// Slack permits provider coalescing after `deadline`, never before it; it is
    /// not a guarantee about host wake latency. Providers may ignore slack.
    /// The deadline must use this clock's origin. An elapsed deadline is ready
    /// on first poll. Dropping the future cancels the wait.
    ///
    /// # Errors
    ///
    /// Returns [`UnsupportedClockTolerance`] without creating a wait when support
    /// is not declared, including when requested slack is zero.
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

/// Rejects unsupported waits, then distinguishes elapsed deadlines from future waits.
///
/// Unsupported clocks do not read the time. `None` denotes an elapsed deadline;
/// otherwise the returned tolerance respects the provider's declared minimum.
fn prepare_tolerant_wait(
    capabilities: ClockCapabilities,
    now: impl FnOnce() -> MonotonicInstant,
    deadline: MonotonicInstant,
    tolerance: Duration,
) -> Result<Option<Duration>, UnsupportedClockTolerance> {
    let minimum = capabilities
        .minimum_tolerance
        .ok_or(UnsupportedClockTolerance)?;
    Ok((deadline > now()).then_some(tolerance.max(minimum)))
}

impl fmt::Debug for Runtime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Runtime").field(&self.driver).finish()
    }
}
