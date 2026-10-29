//! Checks finite timer policies against literal virtual-time timelines.

#[cfg(test)]
#[path = "../../mutation.rs"]
mod mutation;

#[cfg(test)]
#[path = "../../common.rs"]
mod common;

#[cfg(test)]
#[path = "../../throttle_candidate.rs"]
mod throttle;

#[cfg(test)]
#[path = "../../leading_candidate.rs"]
mod leading;

#[cfg(test)]
mod tests {
    //! Checks trailing debounce through RCA's exhaustive deterministic test store.

    use super::common::{self, Action, Mode, Step, TIMELINE};
    use rca_core::{Effect, ReduceContext, Reducer};
    use rca_test::TestStore;

    /// Applies the selected trailing-debounce policy to received values.
    struct Feature(Mode);

    impl Reducer for Feature {
        type State = Vec<u8>;
        type Action = Action;

        fn reduce(
            &mut self,
            state: &mut Vec<u8>,
            action: &Action,
            _: &mut ReduceContext<'_>,
        ) -> Effect<Action> {
            match action {
                Action::Input(value) => {
                    let effect = Effect::send(Action::Output(*value)).delay(self.0.delay());
                    if self.0 == Mode::MissingCancellation {
                        effect
                    } else {
                        effect.cancellable_in_flight("debounce")
                    }
                }
                Action::Output(value) => {
                    state.push(*value);
                    Effect::none()
                }
                Action::Check => Effect::none(),
            }
        }
    }

    /// Replays the literal schedule and requires complete domain-state exhaustion.
    fn exercise(mode: Mode) {
        futures_executor::block_on(async {
            let mut store = TestStore::new(Vec::new(), Feature(mode));
            for step in TIMELINE {
                match *step {
                    Step::Input(value) => {
                        let _ = store.send(Action::Input(value)).await.unwrap();
                    }
                    Step::Advance(duration) => {
                        store.advance(duration).await.unwrap();
                    }
                    Step::Check => {
                        let _ = store
                            .send(Action::Check)
                            .await
                            .expect("unexpected timer output");
                    }
                    Step::Receive(value) => {
                        let _ = store
                            .receive_with(
                                move |action| *action == Action::Output(value),
                                move |state| state.push(value),
                            )
                            .await
                            .unwrap();
                    }
                }
            }
            store.assert_state_eq(vec![2, 3]).await.unwrap();
            store.finish().await.unwrap();
        });
    }

    #[test]
    fn trailing_debounce_replaces_and_restarts() {
        exercise(Mode::Native);
    }

    #[test]
    fn missing_cancellation_is_rejected() {
        common::reject_mutation("missing-cancellation", "unexpected timer output");
    }

    #[test]
    fn early_deadline_is_rejected() {
        common::reject_mutation("early-deadline", "unexpected timer output");
    }

    #[test]
    #[ignore = "invoked by bounded negative-control subprocess"]
    fn mutation_child() {
        exercise(common::mutation_mode());
    }
}
