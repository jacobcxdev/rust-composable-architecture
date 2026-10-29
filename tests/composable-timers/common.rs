//! Defines a literal trailing-debounce trace shared by all three engines.

use std::time::Duration;

/// Separates external input, scheduled output and no-output checkpoints.
#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    /// Replaces the value waiting for the end of the quiet period.
    Input(u8),
    /// Records the value that completed its quiet period.
    Output(u8),
    /// Requires the native test store to reject any unreceived output.
    Check,
}

/// Describes one operation in the independent virtual-time oracle.
pub enum Step {
    /// Sends an external value.
    Input(u8),
    /// Moves time forward without accepting output.
    Advance(Duration),
    /// Requires no pending output at the current time.
    Check,
    /// Accepts exactly this value and records its domain-state change.
    Receive(u8),
}

/// Checks replacement, exact deadlines, restart and absence of extra output.
///
/// Inputs start at zero, three and seven seconds. Accepted outputs occur at
/// seven and eleven seconds. The final advance catches extra output.
pub const TIMELINE: &[Step] = &[
    Step::Input(1),
    Step::Advance(Duration::from_secs(3)),
    Step::Check,
    Step::Input(2),
    Step::Advance(Duration::from_secs(1)),
    Step::Check,
    Step::Advance(Duration::new(2, 999_999_999)),
    Step::Check,
    Step::Advance(Duration::from_nanos(1)),
    Step::Receive(2),
    Step::Check,
    Step::Input(3),
    Step::Advance(Duration::new(3, 999_999_999)),
    Step::Check,
    Step::Advance(Duration::from_nanos(1)),
    Step::Receive(3),
    Step::Advance(Duration::from_secs(20)),
    Step::Check,
];

/// Selects the real policy or one deliberately incorrect implementation.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Mode {
    /// Applies the complete four-second trailing-debounce policy.
    Native,
    /// Leaves the replaced value scheduled.
    MissingCancellation,
    /// Schedules output one nanosecond before its deadline.
    EarlyDeadline,
}

impl Mode {
    /// Returns the real period or the early-deadline mutation.
    pub fn delay(self) -> Duration {
        if self == Self::EarlyDeadline {
            Duration::new(3, 999_999_999)
        } else {
            Duration::from_secs(4)
        }
    }
}

/// Requires the selected debounce mutation to fail for its intended reason.
pub fn reject_mutation(mode: &str, diagnostic: &str) {
    crate::mutation::reject(
        "tests::mutation_child",
        "RCA_TIMER_MUTATION",
        mode,
        diagnostic,
    );
}

/// Reads the mutation selected for an isolated child process.
pub fn mutation_mode() -> Mode {
    match std::env::var("RCA_TIMER_MUTATION").unwrap().as_str() {
        "missing-cancellation" => Mode::MissingCancellation,
        "early-deadline" => Mode::EarlyDeadline,
        other => panic!("unknown mutation: {other}"),
    }
}
