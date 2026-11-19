//! Exercises planned trailing throttle through pinned native scheduler helpers.

#[path = "throttle_common.rs"]
mod common;

use common::{Action, Mode, Step};
use composable::{Effects, Interval, Reducer, Task, TestClock, TestStore};
use std::time::Duration;

/// Holds domain output and the native task's planned-deadline history.
#[derive(Debug)]
struct Feature {
    /// Values accepted through native receive assertions.
    outputs: Vec<u8>,
    /// Sole task owner, retained after delivery and omitted from snapshots.
    previous: Option<Task>,
    /// Real policy or deliberate cancellation/delivery mutation.
    mode: Mode,
}

// Assertion snapshots must not own the live cancellation handle.
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
                if self.mode == Mode::MissingCancellation {
                    if let Some(previous) = self.previous.take() {
                        previous.detach();
                    }
                }
                let interval = if self.mode == Mode::LeadingFirst {
                    Interval::Leading(Duration::from_secs(4))
                } else {
                    Interval::Trailing(Duration::from_secs(4))
                };
                effects.throttle(Action::Output(value), &mut self.previous, interval);
            }
            Action::Output(value) => self.outputs.push(value),
            Action::Cancel => {
                if let Some(previous) = self.previous.take() {
                    if self.mode == Mode::MissingResetCancellation {
                        previous.detach();
                    } else {
                        previous.cancel();
                    }
                }
            }
            Action::Check => {}
        }
    }
}

/// Replays an independent schedule, then checks the complete output sequence.
fn exercise(mode: Mode, timeline: &[Step], expected: &[u8]) {
    let mut store = TestStore::with_initial(Feature {
        outputs: Vec::new(),
        previous: None,
        mode,
    });
    for step in timeline {
        match *step {
            Step::Input(value, _deadline) => store.send(Action::Input(value), |_| {}),
            Step::Advance(duration) => store.advance(duration),
            Step::Check => store.send(Action::Check, |_| {}),
            Step::Cancel => store.send(Action::Cancel, |_| {}),
            Step::Receive(value) => {
                store.recv(Action::Output(value), |state| state.outputs.push(value))
            }
        }
    }
    store.wait();
    store.send(Action::Check, |_| {});
    assert_eq!(store.into_inner().outputs, expected);
}

#[test]
fn trailing_throttle_preserves_planned_windows() {
    exercise(Mode::Native, common::TIMELINE, &[2, 4, 8, 5, 7]);
}

#[test]
fn late_wake_keeps_planned_deadline() {
    exercise(Mode::Native, common::LATE_WAKE, &[1, 2]);
}

#[test]
fn missing_cancellation_is_rejected() {
    common::reject_mutation("missing-cancellation", "left: Output(1)");
}

#[test]
fn leading_first_is_rejected() {
    common::reject_mutation("leading-first", "an extra action was received");
}

#[test]
fn missing_reset_cancellation_is_rejected() {
    common::reject_mutation("missing-reset-cancellation", "an extra action was received");
}

#[test]
#[ignore = "invoked by bounded negative-control subprocess"]
fn mutation_child() {
    exercise(common::mutation_mode(), common::TIMELINE, &[2, 4, 8, 5, 7]);
}
