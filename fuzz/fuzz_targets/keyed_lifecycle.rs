//! Compares bounded keyed-store histories with an independent collection and lifetime model.

#![no_main]

#[path = "../../rca-core/tests/support/keyed_model.rs"]
#[expect(
    dead_code,
    reason = "the shared model exposes property-only lifetime inspection methods"
)]
mod keyed_model;

use std::collections::HashSet;
use std::future::pending;

use keyed_model::{
    CollectionOperation, GenerationTracker, Lifetime, LogicalGeneration, ModelWorld,
    replacement_values,
};
use libfuzzer_sys::fuzz_target;
use rca_core::{
    ActionTask, Effect, EffectSender, KeyedAction, KeyedActionPath, KeyedState, KeyedStatePath,
    ReduceContext, Reducer, Store, for_each,
};
use rca_test::DeterministicRuntime;

/// Limits histories to 512 operations, matching the 2,048-byte fuzz input cap.
///
/// Each operation consumes at most four bytes. This bound also keeps pending-output
/// slots within `u16` and prevents model generation counters from overflowing.
const MAX_STEPS: usize = 512;

/// Changes the collection or exercises a sender tied to a child lifetime.
#[derive(Clone, Copy, Debug)]
enum FuzzOperation {
    /// Applies the same public operation to the store and independent model.
    Collection(CollectionOperation),
    /// Captures a sender for the currently live child at `id`.
    Start {
        /// Key selected from the eight-key input domain.
        id: u8,
    },
    /// Sends through one of the previously captured child senders.
    Emit {
        /// Index reduced to the number of captured senders at execution time.
        slot: u16,
    },
}

/// Records an output accepted by the parent after child-lifetime checks.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Trace {
    /// Child key attached to the accepted action.
    id: u8,
    /// Captured-sender index that identifies the originating task.
    slot: u16,
}

/// Stores a key copy and value for independent collection consistency checks.
#[derive(Clone)]
struct Row {
    /// Must remain equal to the collection key.
    id: u8,
    /// Small signed value mutated with defined wrapping arithmetic.
    value: i16,
}

/// Holds the real keyed collection, a clone branch and accepted output history.
struct State {
    /// Current collection whose order and lifetimes are under test.
    rows: KeyedState<u8, Row>,
    /// Saved branch used to exercise identity across clone restoration.
    saved: Option<KeyedState<u8, Row>>,
    /// Actions that reached the parent after effect admission.
    trace: Vec<Trace>,
}

/// Starts a persistent effect or records one of its outputs.
enum RowAction {
    /// Exports an effect sender before the effect waits for cancellation.
    Start {
        /// One-slot channel consumed immediately by the fuzz driver.
        capture: async_channel::Sender<EffectSender<RowAction>>,
    },
    /// Identifies the captured sender that produced this action.
    Output {
        /// Stable index in the pending-output list.
        slot: u16,
    },
}

/// Routes child work or changes the entire collection.
enum Action {
    /// Preserves the public child key during effect output.
    Row(KeyedAction<u8, RowAction>),
    /// Changes values, order or insertion lifetimes.
    Collection(CollectionOperation),
}

/// Keeps captured effect senders alive until their child scope is retired.
struct Child;

impl Reducer for Child {
    type State = Row;
    type Action = RowAction;

    fn reduce(
        &mut self,
        _state: &mut Self::State,
        action: &Self::Action,
        _context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        match action {
            RowAction::Start { capture } => {
                let capture = capture.clone();
                Effect::run(move |sender| async move {
                    capture.send(sender).await.expect("capture remains live");
                    pending::<()>().await;
                })
            }
            RowAction::Output { .. } => Effect::none(),
        }
    }
}

/// Records admitted child output and applies public collection operations.
struct Parent;

impl Reducer for Parent {
    type State = State;
    type Action = Action;

    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &Self::Action,
        _context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        match action {
            Action::Row(action) => {
                let RowAction::Output { slot } = action.action() else {
                    return Effect::none();
                };
                state.trace.push(Trace {
                    id: *action.id(),
                    slot: *slot,
                });
            }
            Action::Collection(operation) => apply_collection(state, *operation),
        }
        Effect::none()
    }
}

/// Applies an input operation through public keyed-state APIs.
///
/// Empty moves and missing clones are no-ops. Duplicate outcomes are checked by
/// the separate collection property tests; this target checks state and lifetimes.
fn apply_collection(state: &mut State, operation: CollectionOperation) {
    match operation {
        CollectionOperation::Insert { id, value } => {
            if !state.rows.contains_id(&id) {
                state.rows.insert(id, Row { id, value });
            }
        }
        CollectionOperation::TryInsert { id, value } => {
            if let Err(error) = state.rows.try_insert(id, Row { id, value }) {
                drop(error.into_parts());
            }
        }
        CollectionOperation::Remove { id } => {
            let _removed = state.rows.remove(&id);
        }
        CollectionOperation::SwapRemove { id } => {
            let _removed = state.rows.swap_remove(&id);
        }
        CollectionOperation::Move { from, to } => {
            if !state.rows.is_empty() {
                let len = state.rows.len();
                state
                    .rows
                    .move_index(usize::from(from) % len, usize::from(to) % len);
            }
        }
        CollectionOperation::Retain { selector, delta } => {
            state.rows.retain(|id, row| {
                row.value = row.value.wrapping_add(i16::from(delta));
                (*id ^ selector) & 1 == 0
            });
        }
        CollectionOperation::Clear => state.rows.clear(),
        CollectionOperation::Mutate { id, delta } => {
            if let Some(row) = state.rows.get_mut(&id) {
                row.value = row.value.wrapping_add(i16::from(delta));
            }
        }
        CollectionOperation::Clone => state.saved = Some(state.rows.clone()),
        CollectionOperation::RestoreClone => {
            if let Some(saved) = &state.saved {
                state.rows = saved.clone();
            }
        }
        CollectionOperation::Replace { seed } => {
            state.rows = replacement_values(seed)
                .into_iter()
                .map(|(id, value)| (id, Row { id, value }))
                .collect();
        }
    }
}

/// Composes child-first routing with parent collection changes.
fn reducer() -> impl Reducer<State = State, Action = Action> {
    for_each(
        Parent,
        KeyedStatePath::new(
            |state: &State| &state.rows,
            |state: &mut State| &mut state.rows,
        ),
        KeyedActionPath::new(
            |action: &Action| match action {
                Action::Row(action) => Some(action),
                Action::Collection(_) => None,
            },
            Action::Row,
        ),
        Child,
    )
}

/// Retains a captured sender and the model identity expected to govern it.
struct PendingOutput {
    /// Public key that can later be removed and reused.
    id: u8,
    /// Model insertion identity at capture time.
    lifetime: Lifetime,
    /// Model activation generation at capture time.
    generation: u64,
    /// Stable index used to distinguish senders for the same public key.
    slot: u16,
    /// Real effect sender whose admission is compared with the model.
    sender: EffectSender<RowAction>,
    /// Task that must finish when its child lifetime is retired.
    task: ActionTask,
    /// Whether the model still admits output from this captured identity.
    active: bool,
}

/// Decodes at most 512 four-byte chunks, padding incomplete chunks with zeroes.
///
/// Thirteen tags cover collection changes, sender capture and sender use. Inputs
/// after the first 2,048 bytes are ignored, matching the qualification input cap.
fn decode(data: &[u8]) -> Vec<FuzzOperation> {
    data.chunks(4)
        .take(MAX_STEPS)
        .map(|bytes| {
            let first = bytes.first().copied().unwrap_or(0);
            let second = bytes.get(1).copied().unwrap_or(0);
            let third = bytes.get(2).copied().unwrap_or(0);
            let fourth = bytes.get(3).copied().unwrap_or(0);
            let id = second & 7;
            let value = i16::from_le_bytes([third, fourth]);
            match first % 13 {
                0 => FuzzOperation::Collection(CollectionOperation::Insert { id, value }),
                1 => FuzzOperation::Collection(CollectionOperation::TryInsert { id, value }),
                2 => FuzzOperation::Collection(CollectionOperation::Remove { id }),
                3 => FuzzOperation::Collection(CollectionOperation::SwapRemove { id }),
                4 => FuzzOperation::Collection(CollectionOperation::Move {
                    from: second,
                    to: third,
                }),
                5 => FuzzOperation::Collection(CollectionOperation::Retain {
                    selector: second,
                    delta: i8::from_ne_bytes([third]),
                }),
                6 => FuzzOperation::Collection(CollectionOperation::Clear),
                7 => FuzzOperation::Collection(CollectionOperation::Mutate {
                    id,
                    delta: i8::from_ne_bytes([third]),
                }),
                8 => FuzzOperation::Collection(CollectionOperation::Clone),
                9 => FuzzOperation::Collection(CollectionOperation::RestoreClone),
                10 => FuzzOperation::Collection(CollectionOperation::Replace { seed: second }),
                11 => FuzzOperation::Start { id },
                12 => FuzzOperation::Emit {
                    slot: u16::from_le_bytes([second, third]),
                },
                _ => unreachable!("modulo exhausts operation tags"),
            }
        })
        .collect()
}

/// Runs each operation against the real store and its independent model.
///
/// Every step checks values, task retirement and runtime metadata. Final shutdown
/// must finish every retained task, including those with senders still held here.
fn fuzz_sequence(operations: &[FuzzOperation]) {
    let runtime = DeterministicRuntime::new();
    let store = Store::new(
        State {
            rows: KeyedState::new(),
            saved: None,
            trace: Vec::new(),
        },
        reducer(),
        runtime.runtime(),
    );
    let mut model = ModelWorld::new();
    let mut generations = GenerationTracker::new(&model);
    let mut trace = Vec::new();
    let mut pending: Vec<PendingOutput> = Vec::new();
    let mut version = 0_u64;

    futures_executor::block_on(runtime.run(async {
        for operation in operations.iter().copied() {
            match operation {
                FuzzOperation::Collection(operation) => {
                    store
                        .send(Action::Collection(operation))
                        .await
                        .expect("root collection action commits")
                        .finish()
                        .await;
                    model.apply(operation);
                    generations.reconcile(&model);
                    for output in &mut pending {
                        output.active = generations.get(output.id).is_some_and(|current| {
                            current.lifetime == output.lifetime
                                && current.generation == output.generation
                        });
                    }
                    version += 1;
                }
                FuzzOperation::Start { id } => {
                    if let Some(current) = generations.get(id) {
                        let slot = u16::try_from(pending.len()).expect("bounded pending outputs");
                        let (capture, captured) = async_channel::bounded(1);
                        let task = store
                            .send(Action::Row(KeyedAction::new(
                                id,
                                RowAction::Start { capture },
                            )))
                            .await
                            .expect("current child start commits");
                        let sender = captured.recv().await.expect("start captures sender");
                        pending.push(PendingOutput {
                            id,
                            lifetime: current.lifetime,
                            generation: current.generation,
                            slot,
                            sender,
                            task,
                            active: true,
                        });
                        version += 1;
                    }
                }
                FuzzOperation::Emit { slot } if !pending.is_empty() => {
                    let output = &pending[usize::from(slot) % pending.len()];
                    let current = generations.get(output.id);
                    let expected_active = output.active
                        && current.is_some_and(|entry| {
                            entry
                                == (LogicalGeneration {
                                    lifetime: output.lifetime,
                                    generation: output.generation,
                                })
                        });
                    let result = output
                        .sender
                        .send(RowAction::Output { slot: output.slot })
                        .await;
                    if expected_active {
                        result.expect("current generation output commits");
                        trace.push(Trace {
                            id: output.id,
                            slot: output.slot,
                        });
                        version += 1;
                    } else {
                        let error = result.expect_err("stale generation output is rejected");
                        assert!(error.is_cancelled());
                        assert!(!error.is_store_stopped());
                    }
                }
                FuzzOperation::Emit { .. } => {}
            }

            for output in pending.iter().filter(|output| !output.active) {
                output.task.finish().await;
                assert!(output.task.is_finished());
            }
            assert_state(&store, &model, &trace, &generations, &pending, version).await;
        }

        store.shutdown().await.expect("fuzz Store shutdown");
        for output in &mut pending {
            output.active = false;
            output.task.finish().await;
            assert!(output.task.is_finished());
        }
        assert!(store.is_closed());
    }))
    .expect("deterministic runtime stays within its poll budget");
}

/// Checks ordered values, accepted outputs and task ownership after one operation.
///
/// Expected values come from the model, not the real store. API cross-checks also
/// require unique keys and agreement between iteration, lookup and indexed order.
async fn assert_state(
    store: &Store<State, Action>,
    model: &ModelWorld,
    expected_trace: &[Trace],
    generations: &GenerationTracker,
    pending: &[PendingOutput],
    version: u64,
) {
    let expected_values = model.values();
    let expected_trace = expected_trace.to_vec();
    store
        .read(move |state| {
            assert_eq!(state.rows.len(), state.rows.iter().len());
            let mut unique = HashSet::with_capacity(state.rows.len());
            let actual: Vec<_> = state
                .rows
                .iter()
                .enumerate()
                .map(|(index, (id, row))| {
                    assert!(unique.insert(*id));
                    assert_eq!(*id, row.id);
                    assert_eq!(state.rows.ids().nth(index), Some(id));
                    assert_eq!(
                        state.rows.values().nth(index).map(|value| value.value),
                        Some(row.value)
                    );
                    assert_eq!(state.rows.get(id).map(|value| value.value), Some(row.value));
                    (*id, row.value)
                })
                .collect();
            assert_eq!(actual, expected_values);
            assert_eq!(state.trace, expected_trace);
        })
        .await
        .expect("fuzz state remains readable");

    let active = pending.iter().filter(|output| output.active).count();
    for output in pending {
        assert_eq!(!output.task.is_finished(), output.active);
        if output.active {
            assert_eq!(
                generations.get(output.id),
                Some(LogicalGeneration {
                    lifetime: output.lifetime,
                    generation: output.generation,
                })
            );
        }
    }
    let snapshot = store
        .runtime_snapshot()
        .await
        .expect("fuzz runtime snapshot");
    assert_eq!(snapshot.commit_version, version);
    assert_eq!(snapshot.live_task_ids.len(), active);
    assert_eq!(snapshot.cancellation_count, 0);
    assert!(snapshot.timer_deadlines.is_empty());
    assert_eq!(snapshot.observation_count, 0);
}

fuzz_target!(|data: &[u8]| {
    let operations = decode(data);
    fuzz_sequence(&operations);
});
