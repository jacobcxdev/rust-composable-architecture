//! Exercises fixed-window leading debounce through pinned native scheduler helpers.

#[path = "leading_common.rs"]
mod common;

use common::{Action, Mode, Step};
use composable::{Effects, Interval, Reducer, Task, TestClock, TestStore};
use std::time::Duration;

/// Holds domain output and native task history for the fixed window.
#[derive(Debug)]
struct Feature {
    /// Values accepted through native receive assertions.
    outputs: Vec<u8>,
    /// Sole live task owner, omitted from assertion snapshots.
    previous: Option<Task>,
    /// Real policy or deliberate boundary/history mutation.
    mode: Mode,
}

// Domain snapshots must not own a second live cancellation handle.
impl Clone for Feature {
    fn clone(&self) -> Self {
        Self {
            outputs: self.outputs.clone(),
            previous: None,
            mode: self.mode,
        }
    }
}

impl PartialEq for Feature {
    fn eq(&self, other: &Self) -> bool {
        self.outputs == other.outputs && self.mode == other.mode
    }
}

impl Reducer for Feature {
    type Action = Action;
    type Output = Self;

    fn reduce(&mut self, action: Action, effects: impl Effects<Action>) {
        match action {
            Action::Input(value) => {
                let period = if self.mode == Mode::OpenBoundary {
                    Duration::new(3, 999_999_999)
                } else {
                    Duration::from_secs(4)
                };
                effects.debounce(
                    Action::Output(value),
                    &mut self.previous,
                    Interval::Leading(period),
                );
            }
            Action::Output(value) => {
                self.outputs.push(value);
                if self.mode == Mode::ClearOnOutput {
                    drop(self.previous.take());
                }
            }
            Action::Reset => {
                if self.mode != Mode::MissingReset {
                    drop(self.previous.take());
                }
            }
            Action::Check => {}
        }
    }
}

/// Drives immediate native timers and checks the literal fixed-window timeline.
fn exercise(mode: Mode) {
    let mut store = TestStore::with_initial(Feature {
        outputs: Vec::new(),
        previous: None,
        mode,
    });
    for step in common::TIMELINE {
        match *step {
            Step::Input(value, _previous) => {
                store.send(Action::Input(value), |_| {});
                // at(now) still needs the native executor to poll.
                store.advance(Duration::ZERO);
            }
            Step::Advance(duration) => store.advance(duration),
            Step::Receive(value) => {
                store.recv(Action::Output(value), |state| state.outputs.push(value));
            }
            Step::Reset => store.send(Action::Reset, |_| {}),
            Step::Check => store.send(Action::Check, |_| {}),
        }
    }
    store.wait();
    store.send(Action::Check, |_| {});
    assert_eq!(store.into_inner().outputs, common::EXPECTED);
}

#[test]
fn leading_debounce_uses_inclusive_fixed_windows() {
    exercise(Mode::Native);
}

#[test]
fn open_boundary_is_rejected() {
    common::reject_mutation("open-boundary", "an extra action was received");
}

#[test]
fn clearing_history_on_output_is_rejected() {
    common::reject_mutation("clear-on-output", "an extra action was received");
}

#[test]
fn missing_reset_is_rejected() {
    common::reject_mutation("missing-reset", "no action received");
}

#[test]
#[ignore = "invoked by bounded negative-control subprocess"]
fn mutation_child() {
    exercise(common::mutation_mode());
}
