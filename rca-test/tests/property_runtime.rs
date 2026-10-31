//! Generated schedules compare public runtime behaviour with an independent vector model.

use std::collections::HashMap;
use std::time::Duration;

use proptest::prelude::*;
use rca_core::{ActionTask, Effect, ReduceContext, Reducer, Store};
use rca_test::DeterministicRuntime;

/// Distinguishes cancellation values with equal numbers but different Rust types.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum Key {
    Byte(u8),
    Wide(u8),
}

/// One bounded command for both the real runtime and the independent model.
#[derive(Clone, Debug)]
enum Step {
    Add(u8),
    Schedule { key: Key, delay: u8, replace: bool },
    Cancel(Key),
    CancelTask(u8),
    Advance(u8),
    Tree(u8),
    Burst(Vec<u8>),
    Observe,
    Shutdown,
}

/// Separates external commands from delayed and sequential child outputs.
#[derive(Clone, Copy)]
enum Action {
    Add(u8),
    Schedule {
        id: u16,
        key: Key,
        delay: u8,
        replace: bool,
    },
    Cancel(Key),
    Fired(u16),
    Tree(u8),
    Child(u16),
}

/// Records disjoint value ranges for immediate, delayed and sequential actions.
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
        match *action {
            Action::Add(value) => {
                state.push(10_000 + u16::from(value));
                Effect::none()
            }
            Action::Schedule {
                id,
                key,
                delay,
                replace,
            } => {
                let effect =
                    Effect::send_after(Duration::from_millis(u64::from(delay)), Action::Fired(id));
                cancellable(effect, key, replace)
            }
            Action::Cancel(key) => cancel(key),
            Action::Fired(id) | Action::Child(id) => {
                state.push(id);
                Effect::none()
            }
            Action::Tree(value) => {
                let root = 20_000 + u16::from(value) * 2;
                state.push(root);
                Effect::concatenate([
                    Effect::send(Action::Child(root + 1)),
                    Effect::send(Action::Child(root + 2)),
                ])
            }
        }
    }
}

/// Preserves the cancellation key's Rust type and selected replacement policy.
fn cancellable(effect: Effect<Action>, key: Key, replace: bool) -> Effect<Action> {
    match (key, replace) {
        (Key::Byte(value), false) => effect.cancellable(value),
        (Key::Byte(value), true) => effect.cancellable_in_flight(value),
        (Key::Wide(value), false) => effect.cancellable(u16::from(value)),
        (Key::Wide(value), true) => effect.cancellable_in_flight(u16::from(value)),
    }
}

/// Cancels only the selected typed identity.
fn cancel(key: Key) -> Effect<Action> {
    match key {
        Key::Byte(value) => Effect::cancel(value),
        Key::Wide(value) => Effect::cancel(u16::from(value)),
    }
}

/// Distinguishes normal completion from explicit root cancellation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Terminal {
    Live,
    Completed,
    Cancelled,
}

/// Model-only timer data, without production clock or task handles.
#[derive(Clone, Copy, Debug)]
struct Timer {
    /// Absolute expiry in model milliseconds.
    deadline: u64,
    /// Registration order breaks ties between equal deadlines.
    order: u64,
    /// Generated step index used to identify this timer's output.
    id: u16,
    /// Typed cancellation identity represented independently by an enum.
    key: Key,
}

/// Predicts observable state, timer order and task endings using plain collections.
#[derive(Debug, Default)]
struct Model {
    /// Elapsed virtual milliseconds, bounded by the generated schedule length.
    now: u64,
    /// Next registration order, independent of production task identities.
    next_order: u64,
    /// Expected committed action trace.
    state: Vec<u16>,
    /// Outstanding timers sorted only when model time advances.
    timers: Vec<Timer>,
    /// Expected final status of every generated scheduled root.
    terminal: HashMap<u16, Terminal>,
    /// Rejects later commands after the generated shutdown step.
    closed: bool,
}

impl Model {
    /// Registers a timer after applying optional identity replacement.
    fn schedule(&mut self, id: u16, key: Key, delay: u8, replace: bool) {
        if replace {
            self.cancel(key);
        }
        self.timers.push(Timer {
            deadline: self.now + u64::from(delay),
            order: self.next_order,
            id,
            key,
        });
        self.next_order += 1;
        self.terminal.insert(id, Terminal::Live);
    }

    /// Ends identity-owned work without cancelling its root task.
    fn cancel(&mut self, key: Key) {
        for timer in self.timers.iter().filter(|timer| timer.key == key) {
            self.terminal.insert(timer.id, Terminal::Completed);
        }
        self.timers.retain(|timer| timer.key != key);
    }

    /// Cancels a live root while leaving completed roots unchanged.
    fn cancel_task(&mut self, id: u16) {
        if self.terminal.get(&id) == Some(&Terminal::Live) {
            self.timers.retain(|timer| timer.id != id);
            self.terminal.insert(id, Terminal::Cancelled);
        }
    }

    /// Commits due timers in deadline and registration order.
    fn advance(&mut self, duration: u8) {
        self.now += u64::from(duration);
        self.timers
            .sort_by_key(|timer| (timer.deadline, timer.order));
        while self
            .timers
            .first()
            .is_some_and(|timer| timer.deadline <= self.now)
        {
            let timer = self.timers.remove(0);
            self.state.push(timer.id);
            self.terminal.insert(timer.id, Terminal::Completed);
        }
    }

    /// Cancels every remaining root and permanently closes the model.
    fn close(&mut self) {
        for timer in self.timers.drain(..) {
            self.terminal.insert(timer.id, Terminal::Cancelled);
        }
        self.closed = true;
    }
}

/// Generates at most 64 steps with small delays, bursts and typed key spaces.
fn schedule_strategy() -> impl Strategy<Value = Vec<Step>> {
    let key = prop_oneof![(0_u8..4).prop_map(Key::Byte), (0_u8..4).prop_map(Key::Wide),];
    let step = prop_oneof![
        (0_u8..16).prop_map(Step::Add),
        (key.clone(), 1_u8..8, any::<bool>()).prop_map(|(key, delay, replace)| Step::Schedule {
            key,
            delay,
            replace,
        }),
        key.prop_map(Step::Cancel),
        (0_u8..64).prop_map(Step::CancelTask),
        (0_u8..12).prop_map(Step::Advance),
        (0_u8..16).prop_map(Step::Tree),
        prop::collection::vec(0_u8..16, 1..9).prop_map(Step::Burst),
        Just(Step::Observe),
        Just(Step::Shutdown),
    ];
    prop::collection::vec(step, 1..65)
}

/// Uses standard case/shrink settings without writing failure files into the checkout.
fn property_config() -> ProptestConfig {
    ProptestConfig {
        failure_persistence: None,
        ..ProptestConfig::default()
    }
}

/// Checks each live step's state and every scheduled root's final status.
#[expect(
    clippy::too_many_lines,
    reason = "generated schedule interpreter keeps production and reference transitions adjacent"
)]
fn run_schedule(steps: &[Step]) {
    let runtime = DeterministicRuntime::new();
    let store = Store::new(Vec::new(), ScheduleReducer, runtime.runtime());
    let mut model = Model::default();
    let mut tasks = Vec::<(u16, ActionTask)>::new();

    futures_executor::block_on(runtime.run(async {
        for (index, step) in steps.iter().enumerate() {
            if model.closed {
                store
                    .send(Action::Add(0))
                    .await
                    .expect_err("closed Store rejects generated work");
                continue;
            }
            match step {
                Step::Add(value) => {
                    store
                        .send(Action::Add(*value))
                        .await
                        .expect("add commit")
                        .finish()
                        .await;
                    model.state.push(10_000 + u16::from(*value));
                }
                Step::Schedule {
                    key,
                    delay,
                    replace,
                } => {
                    let id = u16::try_from(index).expect("strategy length fits u16");
                    let task = store
                        .send(Action::Schedule {
                            id,
                            key: *key,
                            delay: *delay,
                            replace: *replace,
                        })
                        .await
                        .expect("schedule commit");
                    tasks.push((id, task));
                    model.schedule(id, *key, *delay, *replace);
                }
                Step::Cancel(key) => {
                    store
                        .send(Action::Cancel(*key))
                        .await
                        .expect("cancel commit")
                        .finish()
                        .await;
                    model.cancel(*key);
                }
                Step::CancelTask(slot) if !tasks.is_empty() => {
                    let index = usize::from(*slot) % tasks.len();
                    let (id, task) = &tasks[index];
                    task.cancel();
                    task.finish().await;
                    model.cancel_task(*id);
                }
                Step::CancelTask(_) => {}
                Step::Advance(duration) => {
                    runtime
                        .advance(Duration::from_millis(u64::from(*duration)))
                        .expect("finite virtual advance");
                    model.advance(*duration);
                }
                Step::Tree(value) => {
                    store
                        .send(Action::Tree(*value))
                        .await
                        .expect("tree commit")
                        .finish()
                        .await;
                    let root = 20_000 + u16::from(*value) * 2;
                    model.state.extend([root, root + 1, root + 2]);
                }
                Step::Burst(values) => {
                    let receipts: Vec<_> = values
                        .iter()
                        .map(|value| {
                            store
                                .try_send(Action::Add(*value))
                                .expect("generated burst fits mailbox")
                        })
                        .collect();
                    for receipt in receipts {
                        receipt.await.expect("burst commit").finish().await;
                    }
                    model
                        .state
                        .extend(values.iter().map(|value| 10_000 + u16::from(*value)));
                }
                Step::Observe => {
                    let mut observation = store
                        .observe(Clone::clone)
                        .await
                        .expect("generated observation");
                    let actual = observation.next().await.expect("initial projection");
                    assert_eq!(actual, model.state);
                }
                Step::Shutdown => {
                    let actual = store.read(Clone::clone).await.expect("pre-shutdown state");
                    assert_eq!(actual, model.state);
                    store.shutdown().await.expect("generated shutdown");
                    model.close();
                }
            }

            if !model.closed {
                let actual = store.read(Clone::clone).await.expect("generated state");
                assert_eq!(actual, model.state);
            }
        }

        if !model.closed {
            for value in 0_u8..4 {
                store
                    .send(Action::Cancel(Key::Byte(value)))
                    .await
                    .expect("byte cleanup")
                    .finish()
                    .await;
                model.cancel(Key::Byte(value));
                store
                    .send(Action::Cancel(Key::Wide(value)))
                    .await
                    .expect("wide cleanup")
                    .finish()
                    .await;
                model.cancel(Key::Wide(value));
            }
            let actual = store.read(Clone::clone).await.expect("final state");
            assert_eq!(actual, model.state);
            store.shutdown().await.expect("final shutdown");
            model.close();
        }

        for (id, task) in tasks {
            task.finish().await;
            let terminal = model.terminal.get(&id).expect("model task exists");
            assert!(task.is_finished());
            assert_eq!(task.is_cancelled(), *terminal == Terminal::Cancelled);
        }
        assert!(store.is_closed());
        assert!(model.closed);
    }))
    .expect("generated deterministic schedule");
}

#[test]
fn direct_timer_cancellation_and_replacement_preserve_typed_identity() {
    for (selected, sibling) in [(Key::Byte(1), Key::Wide(1)), (Key::Wide(1), Key::Byte(1))] {
        run_schedule(&[
            Step::Schedule {
                key: selected,
                delay: 2,
                replace: false,
            },
            Step::Schedule {
                key: sibling,
                delay: 2,
                replace: false,
            },
            Step::Cancel(selected),
            Step::Advance(2),
            Step::Schedule {
                key: selected,
                delay: 2,
                replace: false,
            },
            Step::Schedule {
                key: sibling,
                delay: 2,
                replace: false,
            },
            Step::Schedule {
                key: selected,
                delay: 3,
                replace: true,
            },
            Step::Advance(2),
            Step::Advance(1),
        ]);
    }
}

proptest! {
    #![proptest_config(property_config())]

    #[test]
    fn generated_runtime_schedules_match_reference_model(steps in schedule_strategy()) {
        run_schedule(&steps);
    }
}
