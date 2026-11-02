//! Exercises trailing debounce through either pinned Composable implementation.

#[path = "common.rs"]
mod common;

use common::{Action, Mode, Step, TIMELINE};
use composable::{Effects, Interval, Reducer, Task, TestClock, TestStore};

/// Holds domain output and the native task that owns debounce cancellation.
#[derive(Debug)]
struct Feature {
    /// Values accepted through native receive assertions.
    outputs: Vec<u8>,
    /// Sole live cancellation owner, omitted from assertion snapshots.
    previous: Option<Task>,
    /// Real policy or deliberate negative-control mutation.
    mode: Mode,
}

// Match the pinned TestClock example: assertion snapshots contain domain data,
// never a second owner of the live cancellation handle.
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
                    effects
                        .after(self.mode.delay(), Action::Output(value))
                        .detach();
                } else {
                    effects.debounce(
                        Action::Output(value),
                        &mut self.previous,
                        Interval::Trailing(self.mode.delay()),
                    );
                }
            }
            Action::Output(value) => self.outputs.push(value),
            Action::Check => {}
        }
    }
}

/// Replays the independent timeline, then exhausts the native test executor.
fn exercise(mode: Mode) {
    let mut store = TestStore::with_initial(Feature {
        outputs: Vec::new(),
        previous: None,
        mode,
    });
    for step in TIMELINE {
        match *step {
            Step::Input(value) => store.send(Action::Input(value), |_| {}),
            Step::Advance(duration) => store.advance(duration),
            // send checks the native received-action queue before reduction.
            Step::Check => store.send(Action::Check, |_| {}),
            Step::Receive(value) => {
                store.recv(Action::Output(value), |state| state.outputs.push(value));
            }
        }
    }
    // All deadlines have passed. This checks task exhaustion, not live-host joining.
    // The outer test command must have a process deadline in case wait regresses.
    store.wait();
    store.send(Action::Check, |_| {});
    assert_eq!(store.into_inner().outputs, [2, 3]);
}

#[test]
fn trailing_debounce_replaces_and_restarts() {
    exercise(Mode::Native);
}

#[test]
fn missing_cancellation_is_rejected() {
    common::reject_mutation("missing-cancellation", "an extra action was received");
}

#[test]
fn early_deadline_is_rejected() {
    common::reject_mutation("early-deadline", "an extra action was received");
}

#[test]
#[ignore = "invoked by bounded negative-control subprocess"]
fn mutation_child() {
    exercise(common::mutation_mode());
}
