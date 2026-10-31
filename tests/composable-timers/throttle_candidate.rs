//! Translates trailing throttle into explicit planned deadlines and scoped effects.

#[path = "throttle_common.rs"]
mod common;

use common::{Action, Mode, Step};
use rca_core::{Effect, MonotonicInstant, ReduceContext, Reducer};
use rca_test::TestStore;
use std::time::Duration;

/// Retains domain output and the previous planned deadline.
#[derive(Clone, Debug, PartialEq)]
struct State {
    /// Values accepted through exhaustive output assertions.
    outputs: Vec<u8>,
    /// Planned deadline retained after delivery and cleared by cancellation.
    previous: Option<MonotonicInstant>,
}

/// Applies the selected scheduling and cancellation policy.
struct Feature(Mode);

impl Reducer for Feature {
    type State = State;
    type Action = Action;

    fn reduce(
        &mut self,
        state: &mut State,
        action: &Action,
        context: &mut ReduceContext<'_>,
    ) -> Effect<Action> {
        match action {
            Action::Input(value) => {
                let now = context.now();
                let period = Duration::from_secs(4);
                let when = match state.previous {
                    Some(when) if when > now => when,
                    Some(when) if when.saturating_add(period) > now => when.saturating_add(period),
                    _ if self.0 == Mode::LeadingFirst => now,
                    _ => now.saturating_add(period),
                };
                state.previous = Some(when);
                let effect = Effect::send_at(when, Action::Output(*value));
                if self.0 == Mode::MissingCancellation {
                    effect
                } else {
                    effect.cancellable_in_flight("throttle")
                }
            }
            Action::Output(value) => {
                state.outputs.push(*value);
                Effect::none()
            }
            Action::Cancel => {
                state.previous = None;
                if self.0 == Mode::MissingResetCancellation {
                    Effect::none()
                } else {
                    Effect::cancel("throttle")
                }
            }
            Action::Check => Effect::none(),
        }
    }
}

/// Replays the timeline and checks its complete independent output sequence.
fn exercise(mode: Mode, timeline: &[Step], expected: &'static [u8]) {
    futures_executor::block_on(async {
        let mut store = TestStore::new(
            State {
                outputs: Vec::new(),
                previous: None,
            },
            Feature(mode),
        );
        for step in timeline {
            match *step {
                Step::Input(value, deadline) => {
                    // Let the mutation reach the observable no-output checkpoint.
                    let deadline = if mode == Mode::LeadingFirst && value == 1 {
                        0
                    } else {
                        deadline
                    };
                    let _ = store
                        .send_with(Action::Input(value), move |state| {
                            state.previous = Some(
                                MonotonicInstant::ZERO
                                    .saturating_add(Duration::from_secs(deadline)),
                            );
                        })
                        .await
                        .expect("unexpected throttle input state or output");
                }
                Step::Advance(duration) => {
                    store.advance(duration).await.unwrap();
                }
                Step::Check => {
                    let _ = store
                        .send(Action::Check)
                        .await
                        .expect("unexpected throttle output");
                }
                Step::Cancel => {
                    let _ = store
                        .send_with(Action::Cancel, |state| state.previous = None)
                        .await
                        .unwrap();
                }
                Step::Receive(value) => {
                    let _ = store
                        .receive_with(
                            move |action| *action == Action::Output(value),
                            move |state| state.outputs.push(value),
                        )
                        .await
                        .expect("unexpected throttle output");
                }
            }
        }
        store
            .assert_state(move |state| assert_eq!(state.outputs, expected))
            .await
            .unwrap();
        store.finish().await.unwrap();
    });
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
    common::reject_mutation("missing-cancellation", "unexpected throttle output");
}

#[test]
fn leading_first_is_rejected() {
    common::reject_mutation("leading-first", "unexpected throttle output");
}

#[test]
fn missing_reset_cancellation_is_rejected() {
    common::reject_mutation("missing-reset-cancellation", "unexpected throttle output");
}

#[test]
#[ignore = "invoked by bounded negative-control subprocess"]
fn mutation_child() {
    exercise(common::mutation_mode(), common::TIMELINE, &[2, 4, 8, 5, 7]);
}
