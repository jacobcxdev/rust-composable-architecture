//! Defines literal trailing-throttle schedules and planned-deadline expectations.

use std::time::Duration;

/// Separates external input, scheduled output, cancellation and checkpoints.
#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    /// Replaces the value due at the selected planned deadline.
    Input(u8),
    /// Records the value delivered at the deadline.
    Output(u8),
    /// Cancels pending output and clears retained deadline history.
    Cancel,
    /// Requires the test store to reject any unreceived output.
    Check,
}

/// Describes a timeline operation and its independent deadline expectation.
pub enum Step {
    /// Sends a value and asserts its planned deadline in seconds.
    Input(u8, u64),
    /// Moves virtual time forward without accepting output.
    Advance(Duration),
    /// Requires no pending output.
    Check,
    /// Accepts exactly this output value.
    Receive(u8),
    /// Cancels before another input can replace the pending value.
    Cancel,
}

/// Checks replacement, planned windows, cancellation and fresh restart.
///
/// Expected deadlines are literal seconds from zero. They do not use the
/// reducer's deadline-selection calculation.
pub const TIMELINE: &[Step] = &[
    Step::Input(1, 4),
    Step::Check,
    Step::Advance(Duration::from_secs(3)),
    Step::Input(2, 4),
    Step::Advance(Duration::from_nanos(999_999_999)),
    Step::Check,
    Step::Advance(Duration::from_nanos(1)),
    Step::Receive(2),
    Step::Check,
    Step::Input(3, 8),
    Step::Advance(Duration::from_secs(1)),
    Step::Input(4, 8),
    Step::Advance(Duration::new(2, 999_999_999)),
    Step::Check,
    Step::Advance(Duration::from_nanos(1)),
    Step::Receive(4),
    Step::Advance(Duration::from_secs(1)),
    Step::Input(8, 12),
    Step::Advance(Duration::new(2, 999_999_999)),
    Step::Check,
    Step::Advance(Duration::from_nanos(1)),
    Step::Receive(8),
    Step::Advance(Duration::from_secs(4)),
    // At exactly previous deadline + period, trailing mode opens a fresh window.
    Step::Input(5, 20),
    Step::Check,
    Step::Advance(Duration::new(3, 999_999_999)),
    Step::Check,
    Step::Advance(Duration::from_nanos(1)),
    Step::Receive(5),
    Step::Advance(Duration::from_secs(10)),
    Step::Input(6, 34),
    Step::Advance(Duration::from_secs(1)),
    Step::Cancel,
    Step::Advance(Duration::from_secs(3)),
    Step::Check,
    Step::Advance(Duration::from_secs(1)),
    Step::Input(7, 39),
    Step::Advance(Duration::from_secs(3)),
    Step::Check,
    Step::Advance(Duration::from_nanos(999_999_999)),
    Step::Check,
    Step::Advance(Duration::from_nanos(1)),
    Step::Receive(7),
    Step::Advance(Duration::from_secs(20)),
    Step::Check,
];

/// Keeps planned deadlines independent of the time when output is observed.
///
/// The first output is observed at six seconds. The next deadline remains eight,
/// based on the previous planned deadline of four, not observation at six.
pub const LATE_WAKE: &[Step] = &[
    Step::Input(1, 4),
    Step::Advance(Duration::from_secs(6)),
    Step::Receive(1),
    Step::Input(2, 8),
    Step::Advance(Duration::new(1, 999_999_999)),
    Step::Check,
    Step::Advance(Duration::from_nanos(1)),
    Step::Receive(2),
    Step::Advance(Duration::from_secs(20)),
    Step::Check,
];

/// Selects the real policy or one cancellation/delivery mutation.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Mode {
    /// Uses trailing windows and both cancellation boundaries.
    Native,
    /// Leaves a replaced value scheduled.
    MissingCancellation,
    /// Leaves a cancelled value scheduled after clearing history.
    MissingResetCancellation,
    /// Incorrectly delivers the first value immediately.
    LeadingFirst,
}

/// Reads the throttle mutation selected for a child process.
pub fn mutation_mode() -> Mode {
    match std::env::var("RCA_THROTTLE_MUTATION").unwrap().as_str() {
        "missing-cancellation" => Mode::MissingCancellation,
        "missing-reset-cancellation" => Mode::MissingResetCancellation,
        "leading-first" => Mode::LeadingFirst,
        other => panic!("unknown mutation: {other}"),
    }
}

/// Requires the selected throttle mutation to fail for its intended reason.
pub fn reject_mutation(mode: &str, diagnostic: &str) {
    crate::mutation::reject(
        "throttle::mutation_child",
        "RCA_THROTTLE_MUTATION",
        mode,
        diagnostic,
    );
}
