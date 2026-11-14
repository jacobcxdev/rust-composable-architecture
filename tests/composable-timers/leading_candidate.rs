//! Translates fixed-window leading debounce into explicit RCA state and effects.

#[path = "leading_common.rs"]
mod common;

use common::{Action, Mode, Step};
use rca_core::{Effect, MonotonicInstant, ReduceContext, Reducer};
use rca_test::TestStore;
use std::time::Duration;

/// Retains domain output and the last accepted input time.
#[derive(Clone, Debug, PartialEq)]
struct State {
    /// Values accepted through exhaustive output assertions.
    outputs: Vec<u8>,
    /// Start of the current inclusive suppression window.
    previous: Option<MonotonicInstant>,
}

/// Applies the selected boundary and history policy.
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
                if state.previous.is_some_and(|previous| {
                    let end = previous.saturating_add(Duration::from_secs(4));
                    if self.0 == Mode::OpenBoundary {
                        now < end
                    } else {
                        now <= end
                    }
                }) {
                    return Effect::none();
                }
                state.previous = Some(now);
                Effect::send_at(now, Action::Output(*value))
                    .cancellable_in_flight("leading-debounce")
            }
            Action::Output(value) => {
                state.outputs.push(*value);
                if self.0 == Mode::ClearOnOutput {
                    state.previous = None;
                }
                Effect::none()
            }
            Action::Reset => {
                if self.0 != Mode::MissingReset {
                    state.previous = None;
                }
                Effect::cancel("leading-debounce")
            }
            Action::Check => Effect::none(),
        }
    }
}

/// Replays literal expectations without deriving them from the reducer policy.
fn exercise(mode: Mode) {
    futures_executor::block_on(async {
        let mut store = TestStore::new(
            State {
                outputs: Vec::new(),
                previous: None,
            },
            Feature(mode),
        );
        for step in common::TIMELINE {
            match *step {
                Step::Input(value, previous) => {
                    // Literal mutation states let controls reach output checks
                    // rather than failing first on their known state differences.
                    let previous = match (mode, value) {
                        (Mode::OpenBoundary, 4) => 4_000_000_000,
                        (Mode::MissingReset, 9) => 8_000_000_002,
                        _ => previous,
                    };
                    let _ = store
                        .send_with(Action::Input(value), move |state| {
                            state.previous = Some(
                                MonotonicInstant::ZERO
                                    .saturating_add(Duration::from_nanos(previous)),
                            );
                        })
                        .await
                        .expect("unexpected leading input state or output");
                }
                Step::Advance(duration) => {
                    store.advance(duration).await.unwrap();
                }
                Step::Receive(value) => {
                    let _ = store
                        .receive_with(
                            move |action| *action == Action::Output(value),
                            move |state| {
                                state.outputs.push(value);
                                if mode == Mode::ClearOnOutput {
                                    state.previous = None;
                                }
                            },
                        )
                        .await
                        .expect("unexpected leading output");
                }
                Step::Reset => {
                    let _ = store
                        .send_with(Action::Reset, move |state| {
                            if mode != Mode::MissingReset {
                                state.previous = None;
                            }
                        })
                        .await
                        .unwrap();
                }
                Step::Check => {
                    let _ = store
                        .send(Action::Check)
                        .await
                        .expect("unexpected leading output");
                }
            }
        }
        store
            .assert_state(|state| assert_eq!(state.outputs, common::EXPECTED))
            .await
            .unwrap();
        store.finish().await.unwrap();
    });
}

#[test]
fn leading_debounce_uses_inclusive_fixed_windows() {
    exercise(Mode::Native);
}

#[test]
fn open_boundary_is_rejected() {
    common::reject_mutation("open-boundary", "unexpected leading output");
}

#[test]
fn clearing_history_on_output_is_rejected() {
    common::reject_mutation("clear-on-output", "unexpected leading output");
}

#[test]
fn missing_reset_is_rejected() {
    common::reject_mutation("missing-reset", "unexpected leading output");
}

#[test]
#[ignore = "invoked by bounded negative-control subprocess"]
fn mutation_child() {
    exercise(common::mutation_mode());
}
