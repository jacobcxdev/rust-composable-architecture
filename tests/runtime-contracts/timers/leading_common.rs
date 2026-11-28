//! Defines literal fixed-window leading-debounce expectations for all engines.

use std::time::Duration;

/// Selects domain input, delivered output, explicit reset or an output checkpoint.
#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    /// Attempts delivery under the current fixed window.
    Input(u8),
    /// Records an accepted value.
    Output(u8),
    /// Clears completed scheduling history.
    Reset,
    /// Requires the test store to reject any unreceived output.
    Check,
}

/// Describes an operation and its independent expected state.
pub enum Step {
    /// Sends a value and asserts the last accepted time in nanoseconds.
    Input(u8, u64),
    /// Moves virtual time forward without accepting output.
    Advance(Duration),
    /// Accepts exactly this output value.
    Receive(u8),
    /// Clears scheduling history after all prior output is received.
    Reset,
    /// Requires no pending output.
    Check,
}

/// Checks inclusive boundaries, unchanged suppressed history, reset and idle restart.
///
/// Expected times are literal nanoseconds from zero. Suppressed inputs do not
/// move the four-second inclusive window.
pub const TIMELINE: &[Step] = &[
    Step::Input(1, 0),
    Step::Receive(1),
    Step::Input(2, 0),
    Step::Check,
    Step::Advance(Duration::from_secs(3)),
    Step::Input(3, 0),
    Step::Check,
    Step::Advance(Duration::from_secs(1)),
    Step::Input(4, 0),
    Step::Check,
    Step::Advance(Duration::from_nanos(1)),
    Step::Input(5, 4_000_000_001),
    Step::Receive(5),
    Step::Advance(Duration::new(2, 999_999_999)),
    Step::Input(6, 4_000_000_001),
    Step::Check,
    Step::Advance(Duration::new(1, 1)),
    Step::Input(7, 4_000_000_001),
    Step::Check,
    Step::Advance(Duration::from_nanos(1)),
    Step::Input(8, 8_000_000_002),
    Step::Receive(8),
    Step::Advance(Duration::from_nanos(999_999_998)),
    Step::Reset,
    Step::Input(9, 9_000_000_000),
    Step::Receive(9),
    Step::Input(10, 9_000_000_000),
    Step::Check,
    Step::Advance(Duration::from_secs(11)),
    Step::Input(11, 20_000_000_000),
    Step::Receive(11),
    Step::Advance(Duration::from_secs(20)),
    Step::Check,
];

/// Lists the complete accepted output sequence, independently of reducer logic.
pub const EXPECTED: &[u8] = &[1, 5, 8, 9, 11];

/// Selects the real policy or one boundary/history mutation.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Mode {
    /// Suppresses input through the inclusive fixed-window end.
    Native,
    /// Incorrectly accepts input at the end boundary.
    OpenBoundary,
    /// Incorrectly forgets the window when output is received.
    ClearOnOutput,
    /// Incorrectly retains history after an explicit reset.
    MissingReset,
}

/// Reads the leading-debounce mutation selected for a child process.
pub fn mutation_mode() -> Mode {
    match std::env::var("RCA_LEADING_MUTATION").unwrap().as_str() {
        "open-boundary" => Mode::OpenBoundary,
        "clear-on-output" => Mode::ClearOnOutput,
        "missing-reset" => Mode::MissingReset,
        other => panic!("unknown mutation: {other}"),
    }
}

/// Requires the selected leading mutation to fail for its intended reason.
pub fn reject_mutation(mode: &str, diagnostic: &str) {
    crate::mutation::reject(
        "leading::mutation_child",
        "RCA_LEADING_MUTATION",
        mode,
        diagnostic,
    );
}
