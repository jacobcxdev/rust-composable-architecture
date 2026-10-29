//! Compares keyed collections and child lifetimes with an independent ordered model.
//!
//! Each sequence checks visible state, effect acceptance, task completion, and
//! runtime diagnostics. Cases share executor capacity, not state or clock origins.

#![cfg(feature = "test-util")]

#[path = "support/keyed_model.rs"]
mod keyed_model;
mod support;
#[cfg(miri)]
#[path = "support/thread_runtime.rs"]
mod thread_runtime;

use std::collections::HashSet;
#[cfg(not(miri))]
use std::fmt;
use std::future::pending;
#[cfg(not(miri))]
use std::sync::OnceLock;
#[cfg(not(miri))]
use std::task::Poll;
use std::task::{Context, Waker};
use std::time::Duration;

#[cfg(not(miri))]
use futures_executor::ThreadPool;
use keyed_model::{
    CollectionOperation, CollectionOutcome, GenerationTracker, Lifetime, LogicalGeneration,
    ModelWorld, replacement_values,
};
use proptest::prelude::*;
use rca_core::{
    ActionTask, Effect, EffectSender, KeyedAction, KeyedActionPath, KeyedState, KeyedStatePath,
    MonotonicInstant, ReduceContext, Reducer, Runtime, Store, for_each,
};
#[cfg(not(miri))]
use rca_core::{RuntimeDriver, RuntimeFuture};
use support::ManualRuntime;
#[cfg(not(miri))]
use support::immediate_clock::ImmediateClock;

#[test]
fn collection_model_preserves_inserted_order_and_lifetime() {
    let mut model = ModelWorld::new();

    model.apply(CollectionOperation::Insert { id: 2, value: 20 });
    model.apply(CollectionOperation::Insert { id: 1, value: 10 });

    assert_eq!(model.values(), [(2, 20), (1, 10)]);
    assert!(model.lifetime(2).is_some());
}

/// Applies the shared operation inputs to real collections, not to the model.
struct ActualWorld {
    /// Holds the branch whose ordered values are checked after every operation.
    current: KeyedState<u8, i16>,
    /// Saves a cloned branch so later restoration can exercise identity history.
    saved: Option<KeyedState<u8, i16>>,
}

impl ActualWorld {
    /// Starts with an empty current branch and no saved branch.
    fn new() -> Self {
        Self {
            current: KeyedState::new(),
            saved: None,
        }
    }

    /// Calls public collection APIs and reports their observed result.
    fn apply(&mut self, operation: CollectionOperation) -> CollectionOutcome {
        match operation {
            CollectionOperation::Insert { id, value } => {
                if self.current.contains_id(&id) {
                    CollectionOutcome::SkippedDuplicate
                } else {
                    self.current.insert(id, value);
                    CollectionOutcome::Inserted
                }
            }
            CollectionOperation::TryInsert { id, value } => {
                match self.current.try_insert(id, value) {
                    Ok(()) => CollectionOutcome::Inserted,
                    Err(error) => {
                        let (id, value) = error.into_parts();
                        CollectionOutcome::Duplicate { id, value }
                    }
                }
            }
            CollectionOperation::Remove { id } => {
                CollectionOutcome::Removed(self.current.remove(&id))
            }
            CollectionOperation::SwapRemove { id } => {
                CollectionOutcome::Removed(self.current.swap_remove(&id))
            }
            CollectionOperation::Move { from, to } => {
                if self.current.is_empty() {
                    CollectionOutcome::Unchanged
                } else {
                    let len = self.current.len();
                    self.current
                        .move_index(usize::from(from) % len, usize::from(to) % len);
                    CollectionOutcome::Changed
                }
            }
            CollectionOperation::Retain { selector, delta } => {
                self.current.retain(|id, value| {
                    *value = value.wrapping_add(i16::from(delta));
                    (*id ^ selector) & 1 == 0
                });
                CollectionOutcome::Changed
            }
            CollectionOperation::Clear => {
                self.current.clear();
                CollectionOutcome::Changed
            }
            CollectionOperation::Mutate { id, delta } => {
                if let Some(value) = self.current.get_mut(&id) {
                    *value = value.wrapping_add(i16::from(delta));
                    CollectionOutcome::Changed
                } else {
                    CollectionOutcome::Unchanged
                }
            }
            CollectionOperation::Clone => {
                self.saved = Some(self.current.clone());
                CollectionOutcome::Changed
            }
            CollectionOperation::RestoreClone => {
                if let Some(saved) = &self.saved {
                    self.current = saved.clone();
                    CollectionOutcome::Changed
                } else {
                    CollectionOutcome::Unchanged
                }
            }
            CollectionOperation::Replace { seed } => {
                self.current = replacement_values(seed).into_iter().collect();
                CollectionOutcome::Changed
            }
        }
    }
}

/// Generates bounded keys but full signed values, including duplicate and absent keys.
fn collection_operation_strategy() -> impl Strategy<Value = CollectionOperation> {
    prop_oneof![
        (0_u8..8, any::<i16>()).prop_map(|(id, value)| CollectionOperation::Insert { id, value }),
        (0_u8..8, any::<i16>())
            .prop_map(|(id, value)| CollectionOperation::TryInsert { id, value }),
        (0_u8..8).prop_map(|id| CollectionOperation::Remove { id }),
        (0_u8..8).prop_map(|id| CollectionOperation::SwapRemove { id }),
        (any::<u8>(), any::<u8>()).prop_map(|(from, to)| CollectionOperation::Move { from, to }),
        (any::<u8>(), any::<i8>())
            .prop_map(|(selector, delta)| CollectionOperation::Retain { selector, delta }),
        Just(CollectionOperation::Clear),
        (0_u8..8, any::<i8>()).prop_map(|(id, delta)| CollectionOperation::Mutate { id, delta }),
        Just(CollectionOperation::Clone),
        Just(CollectionOperation::RestoreClone),
        any::<u8>().prop_map(|seed| CollectionOperation::Replace { seed }),
    ]
}

/// Keeps caller-selected native case counts and limits interpreter runs to four cases.
fn property_config() -> ProptestConfig {
    let defaults = ProptestConfig::default();
    ProptestConfig {
        cases: if cfg!(miri) { 4 } else { defaults.cases },
        failure_persistence: None,
        ..defaults
    }
}

/// Compares outcomes, order, lookup, and model identity rules after each operation.
fn run_collection_sequence(operations: &[CollectionOperation]) {
    let mut actual = ActualWorld::new();
    let mut model = ModelWorld::new();

    for operation in operations.iter().copied() {
        let before = model.lifetimes();
        let saved = model.saved_lifetimes();
        let actual_outcome = actual.apply(operation);
        let model_outcome = model.apply(operation);

        assert_eq!(actual_outcome, model_outcome);
        assert_eq!(
            actual
                .current
                .iter()
                .map(|(id, value)| (*id, *value))
                .collect::<Vec<_>>(),
            model.values()
        );
        assert_eq!(actual.current.len(), actual.current.iter().len());
        let mut unique = HashSet::with_capacity(actual.current.len());
        for (index, (id, value)) in actual.current.iter().enumerate() {
            assert!(unique.insert(*id));
            assert_eq!(actual.current.ids().nth(index), Some(id));
            assert_eq!(actual.current.values().nth(index), Some(value));
            assert_eq!(actual.current.get(id), Some(value));
            assert_eq!(actual.current[id], *value);
        }
        assert_lifetime_transition(operation, &before, saved.as_deref(), &model.lifetimes());
    }
}

/// Checks the independent model's identity rules without reading RCA's private stamps.
fn assert_lifetime_transition(
    operation: CollectionOperation,
    before: &[(u8, Lifetime)],
    saved: Option<&[(u8, Lifetime)]>,
    after: &[(u8, Lifetime)],
) {
    match operation {
        CollectionOperation::RestoreClone => {
            if let Some(saved) = saved {
                assert_eq!(after, saved);
            } else {
                assert_eq!(after, before);
            }
        }
        CollectionOperation::Replace { .. } => {
            for (id, lifetime) in after {
                assert_ne!(
                    before
                        .iter()
                        .find(|(before_id, _)| before_id == id)
                        .map(|(_, lifetime)| lifetime),
                    Some(lifetime)
                );
            }
        }
        _ => {
            for (id, lifetime) in after {
                if let Some((_, previous)) = before.iter().find(|(before_id, _)| before_id == id) {
                    assert_eq!(lifetime, previous);
                }
            }
        }
    }
    let unique = after
        .iter()
        .map(|(_, lifetime)| *lifetime)
        .collect::<HashSet<_>>();
    assert_eq!(unique.len(), after.len());
}

proptest! {
    #![proptest_config(property_config())]

    #[test]
    fn keyed_state_sequences_match_independent_ordered_model(
        operations in prop::collection::vec(collection_operation_strategy(), 1..65)
    ) {
        run_collection_sequence(&operations);
    }
}

#[cfg(not(miri))]
/// Shares executor capacity while keeping a separate virtual clock for each case.
#[derive(Clone)]
struct PoolDriver {
    pool: ThreadPool,
    clock: ImmediateClock,
}

#[cfg(not(miri))]
impl fmt::Debug for PoolDriver {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PoolDriver").finish_non_exhaustive()
    }
}

#[cfg(not(miri))]
impl RuntimeDriver for PoolDriver {
    fn spawn(&self, task: RuntimeFuture) {
        self.pool.spawn_ok(task);
    }

    fn now(&self) -> MonotonicInstant {
        self.clock.now()
    }

    fn sleep_until(&self, deadline: MonotonicInstant) -> RuntimeFuture {
        self.clock.sleep_until(deadline)
    }

    fn yield_now(&self) -> RuntimeFuture {
        let mut yielded = false;
        Box::pin(futures_util::future::poll_fn(move |context| {
            if yielded {
                Poll::Ready(())
            } else {
                yielded = true;
                context.waker().wake_by_ref();
                Poll::Pending
            }
        }))
    }
}

#[cfg(not(miri))]
#[test]
fn pool_driver_completes_elapsed_waits_and_parks_maximum_deadlines() {
    let driver = PoolDriver {
        pool: ThreadPool::new().expect("property thread pool"),
        clock: ImmediateClock::new(),
    };
    let mut context = Context::from_waker(Waker::noop());
    let mut elapsed = driver.sleep_until(MonotonicInstant::ZERO);
    assert!(elapsed.as_mut().poll(&mut context).is_ready());

    let cloned = driver.clone();
    let deadline = MonotonicInstant::from_elapsed(Duration::from_nanos(1));
    let mut finite = driver.sleep_until(deadline);
    assert_eq!(cloned.now(), MonotonicInstant::ZERO);
    assert!(finite.as_mut().poll(&mut context).is_pending());
    assert_eq!(cloned.now(), deadline);
    assert!(finite.as_mut().poll(&mut context).is_ready());

    let mut parked = driver.sleep_until(MonotonicInstant::MAX);
    assert!(parked.as_mut().poll(&mut context).is_pending());
    assert!(parked.as_mut().poll(&mut context).is_pending());
    assert_eq!(driver.now(), deadline);
}

#[cfg(not(miri))]
/// Reuses worker threads but gives every generated case its own clock origin.
fn property_runtime() -> Runtime {
    // This process-wide pool shares only executor capacity; every case keeps
    // its Store, model, virtual clock, channels, and assertions isolated.
    static POOL: OnceLock<ThreadPool> = OnceLock::new();
    let pool = POOL
        .get_or_init(|| ThreadPool::new().expect("property thread pool"))
        .clone();
    Runtime::new(PoolDriver {
        pool,
        clock: ImmediateClock::new(),
    })
}

#[test]
fn panic_runtime_completes_elapsed_waits_and_parks_maximum_deadlines() {
    let runtime = ManualRuntime::new();
    let clock = runtime.runtime().clock();
    let mut context = Context::from_waker(Waker::noop());
    let mut elapsed = clock.sleep_until(MonotonicInstant::ZERO);
    assert!(elapsed.as_mut().poll(&mut context).is_ready());

    let mut parked = clock.sleep_until(MonotonicInstant::MAX);
    assert!(parked.as_mut().poll(&mut context).is_pending());
    assert!(parked.as_mut().poll(&mut context).is_pending());
}

/// Identifies an output by its public row key and the effect that produced it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct LifecycleTrace {
    id: u8,
    slot: u16,
}

/// Captures only public values and committed output order for comparison.
#[derive(Clone, Debug, Eq, PartialEq)]
struct LifecycleObservable {
    rows: Vec<(u8, i16)>,
    trace: Vec<LifecycleTrace>,
}

/// Keeps a second copy of the public key to detect incorrect row routing.
#[derive(Clone)]
struct LifecycleRow {
    id: u8,
    value: i16,
}

/// Holds the current branch, a saved clone, and every committed child output.
struct LifecycleState {
    rows: KeyedState<u8, LifecycleRow>,
    saved: Option<KeyedState<u8, LifecycleRow>>,
    trace: Vec<LifecycleTrace>,
}

/// Starts captured work or delivers an output from that work.
enum LifecycleRowAction {
    /// Exposes the live effect sender and starts a maximum-deadline timer.
    Start {
        slot: u16,
        capture: async_channel::Sender<EffectSender<LifecycleRowAction>>,
    },
    /// Carries an output that the test sends through a captured effect sender.
    Output { slot: u16 },
    /// Must never commit while its maximum-deadline timer remains parked.
    Timer { slot: u16 },
}

/// Separates child effects from root-level structural collection operations.
enum LifecycleAction {
    Row(KeyedAction<u8, LifecycleRowAction>),
    Collection(CollectionOperation),
}

/// Starts one parked sender task and one parked timer under a shared cancellation ID.
struct LifecycleChild;

impl Reducer for LifecycleChild {
    type State = LifecycleRow;
    type Action = LifecycleRowAction;

    fn reduce(
        &mut self,
        _state: &mut Self::State,
        action: &Self::Action,
        _context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        match action {
            LifecycleRowAction::Start { slot, capture } => {
                let capture = capture.clone();
                Effect::merge([
                    Effect::run(move |sender| async move {
                        capture.send(sender).await.expect("capture remains live");
                        pending::<()>().await;
                    }),
                    Effect::send_after(Duration::MAX, LifecycleRowAction::Timer { slot: *slot }),
                ])
                .cancellable(*slot)
            }
            LifecycleRowAction::Output { .. } | LifecycleRowAction::Timer { .. } => Effect::none(),
        }
    }
}

/// Records accepted outputs and applies collection changes through public APIs.
struct LifecycleParent;

impl Reducer for LifecycleParent {
    type State = LifecycleState;
    type Action = LifecycleAction;

    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &Self::Action,
        _context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        match action {
            LifecycleAction::Row(action) => {
                if let LifecycleRowAction::Output { slot } | LifecycleRowAction::Timer { slot } =
                    action.action()
                {
                    state.trace.push(LifecycleTrace {
                        id: *action.id(),
                        slot: *slot,
                    });
                }
            }
            LifecycleAction::Collection(operation) => {
                apply_lifecycle_collection(state, *operation);
            }
        }
        Effect::none()
    }
}

/// Mutates real child state; the separate vector model predicts the result.
fn apply_lifecycle_collection(state: &mut LifecycleState, operation: CollectionOperation) {
    match operation {
        CollectionOperation::Insert { id, value } => {
            if !state.rows.contains_id(&id) {
                state.rows.insert(id, LifecycleRow { id, value });
            }
        }
        CollectionOperation::TryInsert { id, value } => {
            if let Err(error) = state.rows.try_insert(id, LifecycleRow { id, value }) {
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
                .map(|(id, value)| (id, LifecycleRow { id, value }))
                .collect();
        }
    }
}

/// Builds ordinary lifecycle scenarios with recoverable direct missing-child input.
fn lifecycle_reducer() -> impl Reducer<State = LifecycleState, Action = LifecycleAction> {
    lifecycle_reducer_with_policy(rca_core::MissingChildPolicy::ReportAndBase)
}

/// Keeps the routing identical when the direct-input failure policy changes.
fn lifecycle_reducer_with_policy(
    policy: rca_core::MissingChildPolicy,
) -> impl Reducer<State = LifecycleState, Action = LifecycleAction> {
    for_each(
        LifecycleParent,
        KeyedStatePath::new(
            |state: &LifecycleState| &state.rows,
            |state: &mut LifecycleState| &mut state.rows,
        ),
        KeyedActionPath::new(
            |action: &LifecycleAction| match action {
                LifecycleAction::Row(action) => Some(action),
                LifecycleAction::Collection(_) => None,
            },
            LifecycleAction::Row,
        ),
        LifecycleChild,
    )
    .missing_child_policy(policy)
}

/// Projects public values and output order without exposing internal lifetime tokens.
fn lifecycle_observable(state: &LifecycleState) -> LifecycleObservable {
    LifecycleObservable {
        rows: state
            .rows
            .iter()
            .map(|(id, row)| {
                assert_eq!(*id, row.id);
                (*id, row.value)
            })
            .collect(),
        trace: state.trace.clone(),
    }
}

/// Predicts visible state, active generations, and counts independently of the Store.
#[derive(Debug)]
struct LifecycleModel {
    world: ModelWorld,
    generations: GenerationTracker,
    trace: Vec<LifecycleTrace>,
    version: u64,
    receipts: usize,
    tasks: usize,
    accepted_outputs: usize,
    cancelled_outputs: usize,
}

impl LifecycleModel {
    /// Starts from fixed input shared with the actual fixture, not from actual output.
    fn new() -> Self {
        let mut world = ModelWorld::new();
        let outcome = world.apply(CollectionOperation::Replace { seed: 8 });
        assert_eq!(outcome, CollectionOutcome::Changed);
        let generations = GenerationTracker::new(&world);
        Self {
            world,
            generations,
            trace: Vec::new(),
            version: 0,
            receipts: 0,
            tasks: 0,
            accepted_outputs: 0,
            cancelled_outputs: 0,
        }
    }

    /// Forms the expected view from model state alone.
    fn observable(&self) -> LifecycleObservable {
        LifecycleObservable {
            rows: self.world.values(),
            trace: self.trace.clone(),
        }
    }

    /// Counts the receipt, task handle, and version expected for one root action.
    fn record_root_commit(&mut self) {
        self.receipts = self.receipts.checked_add(1).expect("bounded receipt count");
        self.tasks = self.tasks.checked_add(1).expect("bounded task count");
        self.version = self.version.checked_add(1).expect("bounded commit version");
    }

    /// Retires outputs whose insertion identity or current activation changed.
    fn apply_collection(&mut self, operation: CollectionOperation, pending: &mut [PendingOutput]) {
        self.world.apply(operation);
        self.generations.reconcile(&self.world);
        for output in pending {
            output.active = self.generations.get(output.id).is_some_and(|current| {
                current.lifetime == output.lifetime && current.generation == output.generation
            });
        }
        self.record_root_commit();
    }
}

/// Retains real work handles beside the independent identity expected for that work.
struct PendingOutput {
    id: u8,
    lifetime: Lifetime,
    generation: u64,
    slot: u16,
    sender: EffectSender<LifecycleRowAction>,
    task: ActionTask,
    /// Records whether model reconciliation still admits this sender's outputs.
    active: bool,
}

/// Counts completed API operations separately from the model's predictions.
#[derive(Debug, Default)]
struct LifecycleEvidence {
    receipts: usize,
    tasks: usize,
    accepted_outputs: usize,
    cancelled_outputs: usize,
}

/// Owns the Store whose state and lifecycle are checked by one generated sequence.
struct LifecycleFixture {
    store: Store<LifecycleState, LifecycleAction>,
}

impl LifecycleFixture {
    /// Copies initial model values into a separately constructed real collection.
    fn new(model: &LifecycleModel, runtime: Runtime) -> Self {
        let rows = model
            .world
            .values()
            .into_iter()
            .map(|(id, value)| (id, LifecycleRow { id, value }))
            .collect();
        Self {
            store: Store::new(
                LifecycleState {
                    rows,
                    saved: None,
                    trace: Vec::new(),
                },
                lifecycle_reducer(),
                runtime,
            ),
        }
    }
}

/// Drives a sequence, checks each settled step, then stops all Store-owned work.
fn run_lifecycle_sequence(steps: &[LifecycleStep]) {
    // Miri requires owned executor threads to be joined before process exit.
    #[cfg(miri)]
    let host = thread_runtime::ThreadRuntime::new();
    #[cfg(miri)]
    let runtime = host.runtime();
    #[cfg(not(miri))]
    let runtime = property_runtime();
    let mut model = LifecycleModel::new();
    let fixture = LifecycleFixture::new(&model, runtime);
    let mut pending = Vec::new();
    let mut evidence = LifecycleEvidence::default();

    futures_executor::block_on(async {
        let mut observation = fixture
            .store
            .observe(lifecycle_observable)
            .await
            .expect("lifecycle observation");
        assert_eq!(observation.next().await, Some(model.observable()));
        assert_lifecycle_step(&fixture, &model, &evidence, &pending).await;

        for step in steps.iter().copied() {
            match step {
                LifecycleStep::Collection(operation) => {
                    commit_lifecycle_collection(
                        operation,
                        &fixture,
                        &mut model,
                        &mut evidence,
                        &mut pending,
                        &mut observation,
                    )
                    .await;
                }
                LifecycleStep::Start { id } => {
                    start_lifecycle_output(
                        id,
                        &fixture,
                        &mut model,
                        &mut evidence,
                        &mut pending,
                        &mut observation,
                    )
                    .await;
                }
                LifecycleStep::Emit { slot } => {
                    emit_lifecycle_output(
                        slot,
                        &fixture,
                        &mut model,
                        &mut evidence,
                        &mut pending,
                        &mut observation,
                    )
                    .await;
                }
            }
            settle_inactive_outputs(&pending).await;
            assert_lifecycle_step(&fixture, &model, &evidence, &pending).await;
        }

        fixture
            .store
            .shutdown()
            .await
            .expect("property Store shutdown");
        for output in &mut pending {
            output.active = false;
        }
        settle_inactive_outputs(&pending).await;
        assert!(fixture.store.is_closed());
        assert_eq!(observation.next().await, None);
    });
}

/// Waits for a root change before applying the corresponding independent model step.
async fn commit_lifecycle_collection(
    operation: CollectionOperation,
    fixture: &LifecycleFixture,
    model: &mut LifecycleModel,
    evidence: &mut LifecycleEvidence,
    pending: &mut [PendingOutput],
    observation: &mut rca_core::Observation<LifecycleObservable>,
) {
    let receipt = fixture
        .store
        .try_send(LifecycleAction::Collection(operation))
        .expect("bounded property mailbox admits collection action");
    evidence.receipts += 1;
    let task = receipt.finish().await.expect("collection action commits");
    evidence.tasks += 1;
    task.finish().await;
    assert!(task.is_finished());
    model.apply_collection(operation, pending);
    assert_eq!(observation.next().await, Some(model.observable()));
}

/// Captures the real sender only after its effect starts and retains its task handle.
async fn start_lifecycle_output(
    id: u8,
    fixture: &LifecycleFixture,
    model: &mut LifecycleModel,
    evidence: &mut LifecycleEvidence,
    pending: &mut Vec<PendingOutput>,
    observation: &mut rca_core::Observation<LifecycleObservable>,
) {
    let Some(current) = model.generations.get(id) else {
        return;
    };
    let slot = u16::try_from(pending.len()).expect("bounded pending output count");
    let (capture, captured) = async_channel::bounded(1);
    let receipt = fixture
        .store
        .try_send(LifecycleAction::Row(KeyedAction::new(
            id,
            LifecycleRowAction::Start { slot, capture },
        )))
        .expect("bounded property mailbox admits start action");
    evidence.receipts += 1;
    let task = receipt.finish().await.expect("start action commits");
    evidence.tasks += 1;
    let sender = captured.recv().await.expect("start effect captures sender");
    assert!(!task.is_finished());
    pending.push(PendingOutput {
        id,
        lifetime: current.lifetime,
        generation: current.generation,
        slot,
        sender,
        task,
        active: true,
    });
    model.record_root_commit();
    assert_eq!(observation.next().await, Some(model.observable()));
}

/// Requires a current sender to commit and a stale sender to report cancellation.
async fn emit_lifecycle_output(
    slot: u8,
    fixture: &LifecycleFixture,
    model: &mut LifecycleModel,
    evidence: &mut LifecycleEvidence,
    pending: &mut [PendingOutput],
    observation: &mut rca_core::Observation<LifecycleObservable>,
) {
    if pending.is_empty() {
        return;
    }
    let output = &pending[usize::from(slot) % pending.len()];
    let current = model.generations.get(output.id);
    let expected_active = output.active
        && current.is_some_and(|entry| {
            entry.lifetime == output.lifetime && entry.generation == output.generation
        });
    let result = output
        .sender
        .send(LifecycleRowAction::Output { slot: output.slot })
        .await;
    if expected_active {
        result.expect("current generation output commits");
        evidence.accepted_outputs += 1;
        model.accepted_outputs += 1;
        model.trace.push(LifecycleTrace {
            id: output.id,
            slot: output.slot,
        });
        model.version += 1;
        assert_eq!(observation.next().await, Some(model.observable()));
    } else {
        let error = result.expect_err("stale generation output is rejected");
        assert!(error.is_cancelled());
        assert!(!error.is_store_stopped());
        evidence.cancelled_outputs += 1;
        model.cancelled_outputs += 1;
        assert!(!fixture.store.is_closed());
    }
}

/// Waits for retired effects before checking task and cancellation diagnostics.
async fn settle_inactive_outputs(pending: &[PendingOutput]) {
    for output in pending.iter().filter(|output| !output.active) {
        output.task.finish().await;
        assert!(output.task.is_finished());
    }
}

/// Checks values, completion, and runtime counts against the independent model.
async fn assert_lifecycle_step(
    fixture: &LifecycleFixture,
    model: &LifecycleModel,
    evidence: &LifecycleEvidence,
    pending: &[PendingOutput],
) {
    assert!(!fixture.store.is_closed());
    assert_eq!(evidence.receipts, model.receipts);
    assert_eq!(evidence.tasks, model.tasks);
    assert_eq!(evidence.accepted_outputs, model.accepted_outputs);
    assert_eq!(evidence.cancelled_outputs, model.cancelled_outputs);
    assert_eq!(
        fixture
            .store
            .read(lifecycle_observable)
            .await
            .expect("lifecycle state remains readable"),
        model.observable()
    );

    let active_count = pending.iter().filter(|output| output.active).count();
    for output in pending {
        assert_eq!(!output.task.is_finished(), output.active);
        if output.active {
            assert_eq!(
                model.generations.get(output.id),
                Some(LogicalGeneration {
                    lifetime: output.lifetime,
                    generation: output.generation,
                })
            );
        }
    }
    let lifetimes = model.world.lifetimes();
    assert_eq!(
        lifetimes
            .iter()
            .map(|(_, lifetime)| *lifetime)
            .collect::<HashSet<_>>()
            .len(),
        lifetimes.len()
    );
    assert_eq!(model.generations.len(), lifetimes.len());

    let diagnostics = fixture
        .store
        .runtime_diagnostics()
        .await
        .expect("lifecycle diagnostics");
    let snapshot = diagnostics.runtime();
    assert_eq!(snapshot.commit_version, model.version);
    let expected_work_tasks = active_count
        .checked_mul(2)
        .expect("bounded run and timer task count");
    assert_eq!(snapshot.live_task_ids.len(), expected_work_tasks);
    assert!(snapshot.live_task_ids.windows(2).all(|ids| ids[0] < ids[1]));
    assert_eq!(snapshot.cancellation_count, active_count);
    assert_eq!(snapshot.timer_deadlines.len(), active_count);
    assert!(
        snapshot
            .timer_deadlines
            .iter()
            .all(|deadline| *deadline == MonotonicInstant::MAX)
    );
    assert_eq!(snapshot.observation_count, 1);

    let cancellations = diagnostics.cancellation_identities();
    assert_eq!(cancellations.len(), active_count);
    assert!(
        cancellations
            .iter()
            .all(|identity| identity.type_name == std::any::type_name::<u16>())
    );
    assert_eq!(
        cancellations
            .iter()
            .map(|identity| identity.diagnostic_id)
            .collect::<HashSet<_>>()
            .len(),
        cancellations.len()
    );
    let expected_scopes = pending
        .iter()
        .filter(|output| output.active)
        .map(|output| (output.id, output.generation))
        .collect::<HashSet<_>>()
        .len();
    assert_eq!(
        cancellations
            .iter()
            .map(|identity| identity.scope_id)
            .collect::<HashSet<_>>()
            .len(),
        expected_scopes
    );
}

#[test]
fn stale_provenanced_output_is_cancelled_without_closing_store() {
    run_lifecycle_sequence(&[
        LifecycleStep::Start { id: 0 },
        LifecycleStep::Collection(CollectionOperation::Remove { id: 0 }),
        LifecycleStep::Emit { slot: 0 },
    ]);
}

#[test]
fn clone_assignment_and_reinsertion_preserve_keyed_lifetimes() {
    run_lifecycle_sequence(&[
        LifecycleStep::Start { id: 0 },
        LifecycleStep::Start { id: 3 },
        LifecycleStep::Collection(CollectionOperation::Clone),
        LifecycleStep::Collection(CollectionOperation::RestoreClone),
        LifecycleStep::Emit { slot: 0 },
        LifecycleStep::Emit { slot: 1 },
        LifecycleStep::Collection(CollectionOperation::Clone),
        LifecycleStep::Collection(CollectionOperation::Insert { id: 8, value: 80 }),
        LifecycleStep::Start { id: 8 },
        LifecycleStep::Collection(CollectionOperation::RestoreClone),
        LifecycleStep::Emit { slot: 2 },
        LifecycleStep::Emit { slot: 0 },
        LifecycleStep::Emit { slot: 1 },
        LifecycleStep::Collection(CollectionOperation::Remove { id: 0 }),
        LifecycleStep::Collection(CollectionOperation::Insert { id: 0, value: 90 }),
        LifecycleStep::Start { id: 0 },
        LifecycleStep::Emit { slot: 0 },
        LifecycleStep::Emit { slot: 3 },
        LifecycleStep::Emit { slot: 1 },
    ]);
}

#[test]
fn direct_missing_root_action_panics_and_closes_store() {
    let runtime = ManualRuntime::new();
    let store = Store::new(
        LifecycleState {
            rows: KeyedState::new(),
            saved: None,
            trace: Vec::new(),
        },
        lifecycle_reducer_with_policy(rca_core::MissingChildPolicy::FailFast),
        runtime.runtime(),
    );
    let receipt = store
        .try_send(LifecycleAction::Row(KeyedAction::new(
            0,
            LifecycleRowAction::Output { slot: 0 },
        )))
        .expect("missing root action enters the idle Store mailbox");
    // The same single actor starts after admission, without starting any effects.
    runtime.start_all();
    futures_executor::block_on(receipt.finish()).expect_err("missing root action aborts Store");
    let results = runtime.join_all();
    assert_eq!(results.len(), 1, "only the Store actor was started");
    let panic = results
        .into_iter()
        .next()
        .expect("one actor result exists")
        .expect_err("actor task panics for direct missing root action");
    let message = panic
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| panic.downcast_ref::<&str>().copied())
        .expect("ForEach panic uses a string diagnostic");

    assert!(message.contains("ForEach received keyed child action"));
    assert!(message.contains("was absent from KeyedState"));
    assert!(store.is_closed());
}

/// Interleaves structural changes with starting and sending retained child work.
#[derive(Clone, Copy, Debug)]
enum LifecycleStep {
    Collection(CollectionOperation),
    Start { id: u8 },
    Emit { slot: u8 },
}

/// Favours structural changes while still selecting old senders after retirement.
fn lifecycle_step_strategy() -> impl Strategy<Value = LifecycleStep> {
    prop_oneof![
        8 => collection_operation_strategy().prop_map(LifecycleStep::Collection),
        1 => (0_u8..8).prop_map(|id| LifecycleStep::Start { id }),
        1 => any::<u8>().prop_map(|slot| LifecycleStep::Emit { slot }),
    ]
}

proptest! {
    #![proptest_config(property_config())]

    #[test]
    fn for_each_sequences_match_logical_lifetime_model(
        steps in prop::collection::vec(lifecycle_step_strategy(), 1..17)
    ) {
        run_lifecycle_sequence(&steps);
    }
}
