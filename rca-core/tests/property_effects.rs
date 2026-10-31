//! Compares keyed effect cancellation with an independent, step-driven model.
//!
//! Each completion releases one gate and waits for that action's descendants.
//! This makes output order explicit without sleeps or assumptions about thread scheduling.

#[path = "support/thread_runtime.rs"]
mod support;

use std::collections::HashMap;

use proptest::prelude::*;
use rca_core::{ActionTask, Effect, ReduceContext, Reducer, Store};

use support::ThreadRuntime;

/// Describes caller operations without inspecting the Store's state or task status.
#[derive(Clone, Copy, Debug)]
enum Step {
    /// Starts another task, optionally cancelling older tasks with the same key.
    Start { key: u8, replace: bool },
    /// Cancels all unfinished tasks with this key.
    Cancel { key: u8 },
    /// Selects a previous start by position, wrapping within the number of starts.
    Complete { slot: u8 },
}

/// Drives the real reducer with the same operations used by the reference model.
enum Action {
    /// Holds output behind a caller-owned gate until completion is requested.
    Start {
        /// Identifies this start by its position in the generated schedule.
        id: u16,
        /// Groups tasks for keyed cancellation.
        key: u8,
        /// Cancels older tasks with the same key before this task can produce output.
        replace: bool,
        /// Waits for the caller's single completion signal.
        gate: async_channel::Receiver<()>,
    },
    /// Requests keyed cancellation through the public effect API.
    Cancel(u8),
    /// Records the start that survived cancellation and completed its gate wait.
    Output(u16),
}

/// Records effect outputs without copying the reference model's cancellation logic.
struct ScheduleReducer;

impl Reducer for ScheduleReducer {
    type State = Vec<u16>;
    type Action = Action;

    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &Self::Action,
        _context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        match action {
            Action::Start {
                id,
                key,
                replace,
                gate,
            } => {
                let id = *id;
                let key = *key;
                let gate = gate.clone();
                let effect = Effect::run(move |sender| async move {
                    if gate.recv().await.is_ok() {
                        let _ = sender.send(Action::Output(id)).await;
                    }
                });
                if *replace {
                    effect.cancellable_in_flight(key)
                } else {
                    effect.cancellable(key)
                }
            }
            Action::Cancel(key) => Effect::cancel(*key),
            Action::Output(id) => {
                state.push(*id);
                Effect::none()
            }
        }
    }
}

/// Keeps reference-model state beside the real task's completion controls.
struct LiveTask {
    /// Selects tasks affected by a generated cancellation or replacement.
    key: u8,
    /// Comes only from generated steps, never from the real task's reported state.
    live: bool,
    /// Permits one completion signal, including for a task that was already cancelled.
    release: Option<async_channel::Sender<()>>,
    /// Waits for the real action and its descendants without determining expected output.
    task: ActionTask,
}

/// Generates up to 32 steps across four keys, including repeated and early completions.
fn schedule_strategy() -> impl Strategy<Value = Vec<Step>> {
    let step = prop_oneof![
        (0_u8..4, any::<bool>()).prop_map(|(key, replace)| Step::Start { key, replace }),
        (0_u8..4).prop_map(|key| Step::Cancel { key }),
        (0_u8..32).prop_map(|slot| Step::Complete { slot }),
    ];
    prop::collection::vec(step, 1..33)
}

/// Retains the normal case budget and limits interpreter runs to four cases.
fn property_config() -> ProptestConfig {
    ProptestConfig {
        // Miri interprets every runtime operation; stable runs retain the full case budget.
        cases: if cfg!(miri) {
            4
        } else {
            ProptestConfig::default().cases
        },
        failure_persistence: None,
        ..ProptestConfig::default()
    }
}

/// Returns real outputs and independent expected outputs after all tasks stop.
fn run_schedule(steps: &[Step]) -> (Vec<u16>, Vec<u16>) {
    let runtime = ThreadRuntime::new();
    let store = Store::new(Vec::new(), ScheduleReducer, runtime.runtime());
    let mut tasks = HashMap::<u16, LiveTask>::new();
    let mut starts = Vec::new();
    let mut expected = Vec::new();

    futures_executor::block_on(async {
        for (index, step) in steps.iter().copied().enumerate() {
            match step {
                Step::Start { key, replace } => {
                    if replace {
                        for task in tasks.values_mut().filter(|task| task.key == key) {
                            task.live = false;
                        }
                    }
                    let id = u16::try_from(index).expect("strategy length fits u16");
                    let (release, gate) = async_channel::bounded(1);
                    let task = store
                        .send(Action::Start {
                            id,
                            key,
                            replace,
                            gate,
                        })
                        .await
                        .expect("start commit");
                    tasks.insert(
                        id,
                        LiveTask {
                            key,
                            live: true,
                            release: Some(release),
                            task,
                        },
                    );
                    starts.push(id);
                }
                Step::Cancel { key } => {
                    store
                        .send(Action::Cancel(key))
                        .await
                        .expect("cancel commit")
                        .finish()
                        .await;
                    for task in tasks.values_mut().filter(|task| task.key == key) {
                        task.live = false;
                    }
                }
                Step::Complete { slot } if !starts.is_empty() => {
                    let id = starts[usize::from(slot) % starts.len()];
                    let task = tasks.get_mut(&id).expect("started task exists");
                    if let Some(release) = task.release.take() {
                        // Cancellation can close the receiver before this completion signal.
                        let _ = release.send(()).await;
                        task.task.finish().await;
                        if task.live {
                            expected.push(id);
                            task.live = false;
                        }
                    }
                }
                Step::Complete { .. } => {}
            }
        }

        // Cancel every task before waiting, so no pending gate keeps another task alive.
        for task in tasks.values() {
            task.task.cancel();
        }
        for task in tasks.values() {
            task.task.finish().await;
        }
        let actual = store.read(Clone::clone).await.expect("final state");
        store.shutdown().await.expect("clean shutdown");
        (actual, expected)
    })
}

#[test]
fn fixed_schedules_check_the_reference_model_against_literal_outputs() {
    let cases: &[(&[Step], &[u16])] = &[
        (&[Step::Complete { slot: 31 }, Step::Cancel { key: 3 }], &[]),
        (
            &[
                Step::Start {
                    key: 0,
                    replace: false,
                },
                Step::Start {
                    key: 0,
                    replace: false,
                },
                Step::Complete { slot: 1 },
                Step::Complete { slot: 0 },
                Step::Complete { slot: 1 },
            ],
            &[1, 0],
        ),
        (
            &[
                Step::Start {
                    key: 0,
                    replace: false,
                },
                Step::Start {
                    key: 0,
                    replace: false,
                },
                Step::Start {
                    key: 1,
                    replace: false,
                },
                Step::Cancel { key: 0 },
                Step::Complete { slot: 0 },
                Step::Complete { slot: 1 },
                Step::Complete { slot: 2 },
            ],
            &[2],
        ),
        (
            &[
                Step::Start {
                    key: 0,
                    replace: false,
                },
                Step::Start {
                    key: 1,
                    replace: false,
                },
                Step::Start {
                    key: 0,
                    replace: true,
                },
                Step::Complete { slot: 0 },
                Step::Complete { slot: 1 },
                Step::Complete { slot: 2 },
            ],
            &[1, 2],
        ),
        (
            &[
                Step::Start {
                    key: 0,
                    replace: false,
                },
                Step::Complete { slot: 0 },
                Step::Cancel { key: 0 },
                Step::Complete { slot: 0 },
                Step::Start {
                    key: 0,
                    replace: false,
                },
                Step::Complete { slot: 1 },
            ],
            &[0, 4],
        ),
    ];

    for &(steps, expected) in cases {
        let (actual, model) = run_schedule(steps);
        assert_eq!(model.as_slice(), expected, "reference model for {steps:?}");
        assert_eq!(actual.as_slice(), expected, "real outputs for {steps:?}");
    }
}

proptest! {
    #![proptest_config(property_config())]

    #[test]
    fn generated_cancellation_schedules_match_reference_model(steps in schedule_strategy()) {
        let (actual, expected) = run_schedule(&steps);
        prop_assert_eq!(actual, expected);
    }
}
