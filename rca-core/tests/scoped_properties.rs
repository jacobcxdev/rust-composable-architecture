//! Independent scoped-handle models and dependency-precedence controls.
//!
//! The pool retains a real clock origin, but these properties do not use finite
//! timers. Elapsed waits complete; MAX stays pending; other future waits fail
//! explicitly rather than silently turning a future property into a hang.

use std::fmt;
use std::future::pending;
use std::pin::Pin;
use std::sync::Arc;
#[cfg(not(miri))]
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll};
use std::time::Instant;

use futures_executor::ThreadPool;
use proptest::prelude::*;
#[cfg(feature = "test-util")]
use rca_core::testing::AssertionGate;
use rca_core::{
    ActionPath, DependencyKey, Effect, MonotonicInstant, Observation, OptionalStatePath,
    ReduceContext, Reducer, Runtime, RuntimeDriver, RuntimeFuture, ScopedStore, Store, combine,
    with_dependency,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
/// Drives both the public handle and a separate state-machine model.
enum Operation {
    Register,
    Send(i8),
    TrySend(i8),
    Read,
    Observe,
    Take,
    Remove,
    Replace(u8),
    Reinsert(u8),
    Drop,
    Shutdown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
/// Separates stable child identity from the value changed by actions.
struct Child {
    id: u8,
    value: i16,
}

#[derive(Clone, Debug, Eq, PartialEq)]
/// Records routed reductions independently from handle and observation lifetimes.
struct AppState {
    child: Option<Child>,
    reducer_calls: Vec<usize>,
}

/// Carries a value change and its position in the generated operation sequence.
enum ChildAction {
    Add { amount: i8, position: usize },
}

/// Distinguishes routed mutations from parent-controlled lifetime transitions.
enum AppAction {
    Child(ChildAction),
    Remove,
    Replace(u8),
    Reinsert(u8),
    #[cfg(feature = "test-util")]
    Start(RaceTransition),
}

#[cfg(feature = "test-util")]
#[derive(Clone, Copy, Debug)]
/// Changes the child while previously admitted actions remain queued.
enum RaceTransition {
    Remove,
    Replace(u8),
    Reinsert(u8),
}

#[cfg(feature = "test-util")]
impl RaceTransition {
    /// Converts the generated transition into the parent's real action.
    fn action(self) -> AppAction {
        match self {
            Self::Remove => AppAction::Remove,
            Self::Replace(id) => AppAction::Replace(id),
            Self::Reinsert(id) => AppAction::Reinsert(id),
        }
    }
}

/// Applies domain mutations without modelling handle validity itself.
struct AppReducer;

impl Reducer for AppReducer {
    type State = AppState;
    type Action = AppAction;

    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &Self::Action,
        _context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        match action {
            AppAction::Child(ChildAction::Add { amount, position }) => {
                let child = state
                    .child
                    .as_mut()
                    .expect("registered child action must retain its lifetime");
                child.value = child.value.wrapping_add(i16::from(*amount));
                state.reducer_calls.push(*position);
            }
            AppAction::Remove => state.child = None,
            AppAction::Replace(id) => {
                state.child = Some(Child { id: *id, value: 0 });
            }
            AppAction::Reinsert(id) => {
                if state.child.is_none() {
                    state.child = Some(Child { id: *id, value: 0 });
                }
            }
            #[cfg(feature = "test-util")]
            AppAction::Start(transition) => return Effect::send(transition.action()),
        }
        Effect::none()
    }
}

#[expect(
    clippy::type_complexity,
    reason = "the path API exposes both higher-ranked closure types"
)]
/// Borrows optional child state without copying it into the handle.
fn child_state_path() -> OptionalStatePath<
    AppState,
    Child,
    impl for<'a> Fn(&'a AppState) -> Option<&'a Child> + Clone + Send + Sync,
    impl for<'a> Fn(&'a mut AppState) -> Option<&'a mut Child> + Clone + Send + Sync,
> {
    OptionalStatePath::new(
        |state: &AppState| state.child.as_ref(),
        |state: &mut AppState| state.child.as_mut(),
    )
}

/// Routes child inputs and maps their outputs back to the parent action.
fn child_action_path() -> ActionPath<
    AppAction,
    ChildAction,
    impl for<'a> Fn(&'a AppAction) -> Option<&'a ChildAction> + Clone + Send + Sync,
    impl Fn(ChildAction) -> AppAction + Clone + Send + Sync,
> {
    ActionPath::new(
        |action: &AppAction| match action {
            AppAction::Child(action) => Some(action),
            AppAction::Remove | AppAction::Replace(_) | AppAction::Reinsert(_) => None,
            #[cfg(feature = "test-util")]
            AppAction::Start(_) => None,
        },
        AppAction::Child,
    )
}

#[derive(Clone)]
/// Runs properties on a shared pool without providing a finite timer scheduler.
struct PoolDriver {
    /// Keeps accepted tasks alive independently from individual Store handles.
    pool: ThreadPool,
    /// Remains identical across clones of one runtime.
    origin: Instant,
}

impl fmt::Debug for PoolDriver {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PoolDriver").finish_non_exhaustive()
    }
}

impl RuntimeDriver for PoolDriver {
    fn spawn(&self, task: RuntimeFuture) {
        self.pool.spawn_ok(task);
    }

    fn now(&self) -> MonotonicInstant {
        MonotonicInstant::from_elapsed(self.origin.elapsed())
    }

    fn sleep_until(&self, deadline: MonotonicInstant) -> RuntimeFuture {
        if deadline <= self.now() {
            return Box::pin(std::future::ready(()));
        }
        assert_eq!(
            deadline,
            MonotonicInstant::MAX,
            "property runtime does not support finite future deadlines"
        );
        Box::pin(pending())
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

/// Shares native workers, but gives each property case its own clock origin.
fn property_runtime() -> Runtime {
    #[cfg(miri)]
    let pool = ThreadPool::new().expect("Miri property thread pool");
    #[cfg(not(miri))]
    let pool = {
        static POOL: OnceLock<ThreadPool> = OnceLock::new();
        POOL.get_or_init(|| ThreadPool::new().expect("property thread pool"))
            .clone()
    };
    Runtime::new(PoolDriver {
        pool,
        origin: Instant::now(),
    })
}

#[cfg(miri)]
/// Allows asynchronous pool shutdown after the final per-case Miri handle drops.
fn wait_for_runtime_threads() {
    // ThreadPool closes workers asynchronously after its final handle drops.
    std::thread::sleep(std::time::Duration::from_millis(500));
}

#[test]
fn pool_driver_clock_completes_elapsed_deadlines_and_keeps_max_pending() {
    let driver = PoolDriver {
        pool: ThreadPool::new().expect("clock control thread pool"),
        origin: Instant::now(),
    };
    let mut context = Context::from_waker(std::task::Waker::noop());
    let elapsed = driver.now();
    let mut wait = driver.sleep_until(elapsed);
    assert!(
        wait.as_mut().poll(&mut context).is_ready(),
        "elapsed deadline must be ready on its first poll"
    );
    assert!(driver.now() >= elapsed);

    let mut distant = driver.sleep_until(MonotonicInstant::MAX);
    assert!(distant.as_mut().poll(&mut context).is_pending());
    assert!(distant.as_mut().poll(&mut context).is_pending());
    drop((wait, distant, driver));
    #[cfg(miri)]
    wait_for_runtime_threads();
}

#[test]
fn pool_driver_rejects_unsupported_finite_future_deadlines() {
    let driver = PoolDriver {
        pool: ThreadPool::new().expect("clock control thread pool"),
        origin: Instant::now(),
    };
    let deadline = driver
        .now()
        .saturating_add(std::time::Duration::from_secs(3_600));
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        drop(driver.sleep_until(deadline));
    }));
    drop(driver);
    #[cfg(miri)]
    wait_for_runtime_threads();

    let panic = result.expect_err("unsupported finite wait must panic");
    let message = panic
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| panic.downcast_ref::<&str>().copied())
        .expect("clock restriction has a text panic payload");
    assert!(message.contains("property runtime does not support finite future deadlines"));
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
/// Keeps child invalidation distinct from shutdown of an otherwise valid handle.
enum ExpectedError {
    ScopeEnded,
    StoreStopped,
}

#[derive(Clone, Copy, Debug)]
/// Records registration ownership separately from the selected child's lifetime.
struct ModelHandle {
    /// Changes whenever the caller acquires a new handle, even for the same child.
    registration_generation: u64,
    /// Changes when the selected identity changes or disappears.
    child_generation: u64,
}

#[derive(Clone, Copy, Debug)]
/// Models a retained initial value followed by one replaceable publication slot.
struct ModelObservation {
    /// Identifies the handle that owns this stream.
    registration_generation: u64,
    /// Prevents later children from publishing through an older stream.
    child_generation: u64,
    /// Survives closure until consumed once.
    initial: Option<i16>,
    /// Is replaced by later publications and discarded at closure.
    latest: Option<i16>,
    closed: bool,
}

#[derive(Debug)]
/// Predicts outcomes without calling RCA's routing or observation implementation.
struct ReferenceModel {
    child: Option<Child>,
    /// Advances on identity transitions, not ordinary child value changes.
    child_generation: u64,
    /// Gives each explicit registration a separate observation owner.
    next_registration_generation: u64,
    handle: Option<ModelHandle>,
    observation: Option<ModelObservation>,
    /// Records operation positions accepted for actual child reduction.
    reducer_calls: Vec<usize>,
    /// Counts parent commits independently from diagnostic snapshots.
    commit_version: u64,
    /// Tracks input progress even when no public handle exists for an operation.
    mailbox_position: usize,
    stopped: bool,
}

impl ReferenceModel {
    /// Starts with one child and no registered public handle.
    fn new() -> Self {
        Self {
            child: Some(Child { id: 0, value: 0 }),
            child_generation: 1,
            next_registration_generation: 1,
            handle: None,
            observation: None,
            reducer_calls: Vec::new(),
            commit_version: 0,
            mailbox_position: 0,
            stopped: false,
        }
    }

    /// Rejects a handle when its child generation has ended.
    fn current_handle(&self) -> Option<ModelHandle> {
        self.handle.filter(|handle| {
            handle.child_generation == self.child_generation && self.child.is_some()
        })
    }

    /// Chooses the error only for an operation already known to be invalid.
    fn operation_error(&self) -> ExpectedError {
        if self.stopped && self.current_handle().is_some() {
            ExpectedError::StoreStopped
        } else if self.current_handle().is_none() {
            ExpectedError::ScopeEnded
        } else {
            unreachable!("live handle has no expected error")
        }
    }

    /// Discards later publications while retaining an unread initial value.
    fn close_observation(&mut self) {
        if let Some(observation) = &mut self.observation {
            observation.closed = true;
            observation.latest = None;
        }
    }

    /// Ends this registration and its observation together.
    fn drop_handle(&mut self) {
        self.handle = None;
        self.close_observation();
    }

    /// Replaces the owned registration only when the Store and child are live.
    fn register(&mut self) -> Result<(), ExpectedError> {
        self.drop_handle();
        if self.stopped {
            return Err(ExpectedError::StoreStopped);
        }
        if self.child.is_none() {
            return Err(ExpectedError::ScopeEnded);
        }
        let generation = self.next_registration_generation;
        self.next_registration_generation += 1;
        self.handle = Some(ModelHandle {
            registration_generation: generation,
            child_generation: self.child_generation,
        });
        Ok(())
    }

    /// Ends changed identities or replaces the current observation's latest value.
    fn publish_or_invalidate(&mut self, before: Option<u8>) {
        let after = self.child.map(|child| child.id);
        if before != after {
            self.child_generation += 1;
            self.close_observation();
            return;
        }
        let handle = self.current_handle();
        if let (Some(handle), Some(child), Some(observation)) =
            (handle, self.child, &mut self.observation)
            && observation.registration_generation == handle.registration_generation
            && observation.child_generation == handle.child_generation
            && !observation.closed
        {
            observation.latest = Some(child.value);
        }
    }

    /// Applies a domain action and advances the independent publication model.
    fn commit_root(&mut self, action: &AppAction) {
        let before = self.child.map(|child| child.id);
        match action {
            AppAction::Child(ChildAction::Add { amount, position }) => {
                let child = self.child.as_mut().expect("model child must exist");
                child.value = child.value.wrapping_add(i16::from(*amount));
                self.reducer_calls.push(*position);
            }
            AppAction::Remove => self.child = None,
            AppAction::Replace(id) => self.child = Some(Child { id: *id, value: 0 }),
            AppAction::Reinsert(id) => {
                if self.child.is_none() {
                    self.child = Some(Child { id: *id, value: 0 });
                }
            }
            #[cfg(feature = "test-util")]
            AppAction::Start(_) => unreachable!("model commits generated operations directly"),
        }
        self.commit_version += 1;
        self.publish_or_invalidate(before);
    }

    /// Starts a stream with the current value and no later publication.
    fn observe(&mut self) -> Result<(), ExpectedError> {
        if self.stopped || self.current_handle().is_none() {
            return Err(self.operation_error());
        }
        let handle = self.current_handle().expect("checked current handle");
        self.observation = Some(ModelObservation {
            registration_generation: handle.registration_generation,
            child_generation: handle.child_generation,
            initial: self.child.map(|child| child.value),
            latest: None,
            closed: false,
        });
        Ok(())
    }

    /// Predicts one poll, including unread initial values after invalidation.
    fn take(&mut self) -> ModelTake {
        let Some(observation) = &mut self.observation else {
            return ModelTake::Absent;
        };
        if let Some(initial) = observation.initial.take() {
            return ModelTake::Value(initial);
        }
        if observation.closed {
            return ModelTake::Ended;
        }
        if let Some(latest) = observation.latest.take() {
            return ModelTake::Value(latest);
        }
        ModelTake::Pending
    }

    #[cfg(feature = "test-util")]
    /// Counts only the actor's live registration, not retained expired handles.
    fn active_handle_count(&self) -> usize {
        usize::from(!self.stopped && self.current_handle().is_some())
    }

    #[cfg(feature = "test-util")]
    /// Excludes closed streams even if the caller still retains their value.
    fn active_observation_count(&self) -> usize {
        usize::from(
            !self.stopped
                && self
                    .observation
                    .is_some_and(|observation| !observation.closed),
        )
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
/// Distinguishes no stream, a value, a pending stream and an ended stream.
enum ModelTake {
    Absent,
    Value(i16),
    Pending,
    Ended,
}

/// Checks the precise failure category instead of accepting any rejection.
fn assert_scoped_error(error: &rca_core::ScopedStoreEnded, expected: ExpectedError) {
    match expected {
        ExpectedError::ScopeEnded => assert!(error.is_scope_ended()),
        ExpectedError::StoreStopped => assert!(error.is_store_stopped()),
    }
}

/// Requires lifetime rejection, not capacity failure, and recovers the typed action.
fn assert_try_error(error: rca_core::TryScopedSendError<ChildAction>, expected: ExpectedError) {
    match expected {
        ExpectedError::ScopeEnded => assert!(error.is_scope_ended()),
        ExpectedError::StoreStopped => assert!(error.is_store_stopped()),
    }
    assert!(!error.is_full());
    let ChildAction::Add { .. } = error.into_action();
}

/// Proves no publication is available without waiting on a wall-clock timeout.
fn assert_next_pending(values: &mut Observation<i16>) {
    let mut next = Box::pin(values.next());
    let waker = futures_util::task::noop_waker();
    let mut context = Context::from_waker(&waker);
    assert!(Pin::as_mut(&mut next).poll(&mut context).is_pending());
}

/// Compares public state and, when enabled, actor-owned registration counts.
async fn assert_actual_model(store: &Store<AppState, AppAction>, model: &ReferenceModel) {
    if model.stopped {
        assert!(store.is_closed());
        return;
    }
    let actual = store
        .read(Clone::clone)
        .await
        .expect("live model Store read");
    assert_eq!(actual.child, model.child);
    assert_eq!(actual.reducer_calls, model.reducer_calls);

    #[cfg(feature = "test-util")]
    {
        let diagnostics = store
            .runtime_diagnostics()
            .await
            .expect("live model diagnostics");
        assert_eq!(diagnostics.runtime().commit_version, model.commit_version);
        assert_eq!(
            diagnostics.handle_registration_count(),
            model.active_handle_count()
        );
        assert_eq!(
            diagnostics.runtime().observation_count,
            model.active_observation_count()
        );
    }
}

#[expect(
    clippy::too_many_lines,
    reason = "one match keeps each generated operation and model transition adjacent"
)]
/// Executes one generated operation against both the public API and the model.
async fn apply_operation(
    operation: Operation,
    position: usize,
    store: &Store<AppState, AppAction>,
    handle: &mut Option<ScopedStore<Child, ChildAction>>,
    observation: &mut Option<Observation<i16>>,
    model: &mut ReferenceModel,
) {
    model.mailbox_position = position + 1;
    match operation {
        Operation::Register => {
            *handle = None;
            let expected = model.register();
            let actual = store
                .scope_some_identified(child_state_path(), child_action_path(), |child| &child.id)
                .await;
            match (actual, expected) {
                (Ok(actual), Ok(())) => *handle = Some(actual),
                (Err(actual), Err(expected)) => assert_scoped_error(&actual, expected),
                _ => panic!("registration outcome diverged from model"),
            }
        }
        Operation::Send(amount) => {
            let action = ChildAction::Add { amount, position };
            let expected = if model.stopped || model.current_handle().is_none() {
                Err(model.operation_error())
            } else {
                model.commit_root(&AppAction::Child(ChildAction::Add { amount, position }));
                Ok(())
            };
            let actual = match handle {
                Some(handle) => handle.send(action).await.map(|_task| ()),
                None => return,
            };
            match (actual, expected) {
                (Ok(()), Ok(())) => {}
                (Err(actual), Err(expected)) => assert_scoped_error(&actual, expected),
                _ => panic!("scoped send outcome diverged from model"),
            }
        }
        Operation::TrySend(amount) => {
            let action = ChildAction::Add { amount, position };
            let expected = if model.stopped || model.current_handle().is_none() {
                Err(model.operation_error())
            } else {
                model.commit_root(&AppAction::Child(ChildAction::Add { amount, position }));
                Ok(())
            };
            let Some(handle) = handle else { return };
            match (handle.try_send(action), expected) {
                (Ok(receipt), Ok(())) => {
                    let _task = receipt
                        .finish()
                        .await
                        .expect("modelled admitted scoped action must commit");
                }
                (Err(actual), Err(expected)) => assert_try_error(actual, expected),
                _ => panic!("scoped try_send outcome diverged from model"),
            }
        }
        Operation::Read => {
            let expected = if model.stopped || model.current_handle().is_none() {
                Err(model.operation_error())
            } else {
                Ok(model.child.expect("current child").value)
            };
            let Some(handle) = handle else { return };
            let actual = handle.read(|child| child.value).await;
            match (actual, expected) {
                (Ok(actual), Ok(expected)) => assert_eq!(actual, expected),
                (Err(actual), Err(expected)) => assert_scoped_error(&actual, expected),
                _ => panic!("scoped read outcome diverged from model"),
            }
        }
        Operation::Observe => {
            *observation = None;
            model.observation = None;
            let expected = model.observe();
            let Some(handle) = handle else { return };
            let actual = handle.observe(|child| child.value).await;
            match (actual, expected) {
                (Ok(actual), Ok(())) => *observation = Some(actual),
                (Err(actual), Err(expected)) => assert_scoped_error(&actual, expected),
                _ => panic!("scoped observation outcome diverged from model"),
            }
        }
        Operation::Take => {
            let expected = model.take();
            match (observation, expected) {
                (None, ModelTake::Absent) => {}
                (Some(actual), ModelTake::Value(expected)) => {
                    assert_eq!(actual.next().await, Some(expected));
                }
                (Some(actual), ModelTake::Pending) => assert_next_pending(actual),
                (Some(actual), ModelTake::Ended) => assert_eq!(actual.next().await, None),
                _ => panic!("observation take outcome diverged from model"),
            }
        }
        Operation::Remove => {
            if model.stopped {
                store.send(AppAction::Remove).await.unwrap_err();
            } else {
                let _task = store.send(AppAction::Remove).await.expect("remove commit");
                model.commit_root(&AppAction::Remove);
            }
        }
        Operation::Replace(id) => {
            if model.stopped {
                store.send(AppAction::Replace(id)).await.unwrap_err();
            } else {
                let _task = store
                    .send(AppAction::Replace(id))
                    .await
                    .expect("replace commit");
                model.commit_root(&AppAction::Replace(id));
            }
        }
        Operation::Reinsert(id) => {
            if model.stopped {
                store.send(AppAction::Reinsert(id)).await.unwrap_err();
            } else {
                let _task = store
                    .send(AppAction::Reinsert(id))
                    .await
                    .expect("reinsert commit");
                model.commit_root(&AppAction::Reinsert(id));
            }
        }
        Operation::Drop => {
            *handle = None;
            model.drop_handle();
        }
        Operation::Shutdown => {
            if model.stopped {
                store
                    .shutdown()
                    .await
                    .expect("graceful shutdown is idempotent");
            } else {
                store.shutdown().await.expect("model shutdown");
                model.stopped = true;
                model.close_observation();
            }
        }
    }
}

/// Checks every generated transition before shutting down the case's Store.
fn run_scoped_sequence(operations: &[Operation]) {
    futures_executor::block_on(async {
        let store = Store::new(
            AppState {
                child: Some(Child { id: 0, value: 0 }),
                reducer_calls: Vec::new(),
            },
            AppReducer,
            property_runtime(),
        );
        let mut handle = None;
        let mut observation = None;
        let mut model = ReferenceModel::new();

        for (position, operation) in operations.iter().copied().enumerate() {
            apply_operation(
                operation,
                position,
                &store,
                &mut handle,
                &mut observation,
                &mut model,
            )
            .await;
            assert_eq!(model.mailbox_position, position + 1);
            assert_actual_model(&store, &model).await;
        }

        if !model.stopped {
            store.shutdown().await.expect("property Store shutdown");
        }
    });
    #[cfg(miri)]
    wait_for_runtime_threads();
}

/// Favours routed actions while retaining every lifetime and observation operation.
fn operation_strategy() -> impl Strategy<Value = Operation> {
    prop_oneof![
        3 => Just(Operation::Register),
        5 => any::<i8>().prop_map(Operation::Send),
        5 => any::<i8>().prop_map(Operation::TrySend),
        2 => Just(Operation::Read),
        2 => Just(Operation::Observe),
        3 => Just(Operation::Take),
        2 => Just(Operation::Remove),
        3 => (0_u8..4).prop_map(Operation::Replace),
        2 => (0_u8..4).prop_map(Operation::Reinsert),
        2 => Just(Operation::Drop),
        1 => Just(Operation::Shutdown),
    ]
}

/// Keeps standard native case counts and a smaller interpreter-friendly Miri set.
fn property_config() -> ProptestConfig {
    let defaults = ProptestConfig::default();
    ProptestConfig {
        cases: if cfg!(miri) { 4 } else { defaults.cases },
        failure_persistence: None,
        ..defaults
    }
}

#[test]
fn fixed_sequence_covers_every_scoped_operation() {
    run_scoped_sequence(&[
        Operation::Read,
        Operation::Register,
        Operation::Read,
        Operation::Observe,
        Operation::Take,
        Operation::Send(2),
        Operation::Take,
        Operation::TrySend(-1),
        Operation::Replace(0),
        Operation::Read,
        Operation::Replace(1),
        Operation::Send(1),
        Operation::Take,
        Operation::Register,
        Operation::Observe,
        Operation::Remove,
        Operation::Take,
        Operation::Reinsert(2),
        Operation::Drop,
        Operation::Register,
        Operation::Shutdown,
        Operation::Read,
        Operation::Observe,
        Operation::TrySend(4),
        Operation::Drop,
        Operation::Shutdown,
    ]);
}

proptest! {
    #![proptest_config(property_config())]

    #[test]
    fn scoped_sequences_match_actor_owned_reference_model(
        operations in prop::collection::vec(operation_strategy(), 1..65)
    ) {
        run_scoped_sequence(&operations);
    }
}

#[cfg(feature = "test-util")]
/// Includes identity-preserving replacement as well as removal and reinsertion.
fn race_transition_strategy() -> impl Strategy<Value = RaceTransition> {
    prop_oneof![
        Just(RaceTransition::Remove),
        (0_u8..4).prop_map(RaceTransition::Replace),
        (0_u8..4).prop_map(RaceTransition::Reinsert),
    ]
}

#[cfg(feature = "test-util")]
#[expect(
    clippy::too_many_lines,
    reason = "race setup and assertions form one mailbox-order transaction"
)]
/// Places child receipts behind a held transition, then checks commit-time validity.
fn run_queued_generation_race(initial_id: u8, transition: RaceTransition, amounts: &[i8]) {
    futures_executor::block_on(async {
        let gate = AssertionGate::new();
        let store = Store::builder(
            AppState {
                child: Some(Child {
                    id: initial_id,
                    value: 0,
                }),
                reducer_calls: Vec::new(),
            },
            AppReducer,
            property_runtime(),
        )
        .mailbox_capacity(amounts.len() + 1)
        .assertion_gate(gate.clone())
        .build()
        .expect("generated mailbox capacity is non-zero");
        let handle = store
            .scope_some_identified(child_state_path(), child_action_path(), |child| &child.id)
            .await
            .expect("initial child registers");
        let mut observation = handle
            .observe(|child| child.value)
            .await
            .expect("initial scoped observation registers");
        assert_eq!(observation.next().await, Some(0));

        let start = store
            .send(AppAction::Start(transition))
            .await
            .expect("race transition starts");
        gate.wait_for_pending().await;
        assert_eq!(
            gate.pending_snapshot()
                .expect("effect transition is paused")
                .commit_version,
            1,
        );

        let mut receipts = Vec::with_capacity(amounts.len());
        for (offset, amount) in amounts.iter().copied().enumerate() {
            receipts.push(
                handle
                    .try_send(ChildAction::Add {
                        amount,
                        position: offset + 2,
                    })
                    .expect("pre-commit handle admits queued action"),
            );
        }
        let pending = gate
            .pending_diagnostics()
            .expect("paused transition retains diagnostics");
        assert_eq!(pending.handle_registration_count(), 1);
        assert_eq!(pending.runtime().observation_count, 1);

        let transition_receipt = gate
            .release_pending_with_receipt()
            .expect("paused transition releases");
        transition_receipt
            .finish()
            .await
            .expect("transition commits")
            .finish()
            .await;

        let generation_survives = match transition {
            RaceTransition::Remove => false,
            RaceTransition::Replace(id) => id == initial_id,
            RaceTransition::Reinsert(_) => true,
        };
        for receipt in receipts {
            match receipt.finish().await {
                Ok(task) if generation_survives => task.finish().await,
                Err(_ended) if !generation_survives => {}
                Ok(_) => panic!("old-generation action committed after transition"),
                Err(error) => panic!("live-generation action was rejected: {error}"),
            }
        }
        start.finish().await;

        let expected_calls = if generation_survives {
            (2..amounts.len() + 2).collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        let expected_value = amounts
            .iter()
            .copied()
            .fold(0_i16, |value, amount| value.wrapping_add(i16::from(amount)));
        let actual = store.read(Clone::clone).await.expect("root remains live");
        assert_eq!(actual.reducer_calls, expected_calls);
        if generation_survives {
            assert_eq!(actual.child.expect("surviving child").value, expected_value);
            assert_eq!(
                handle.read(|child| child.value).await.unwrap(),
                expected_value
            );
            assert_eq!(observation.next().await, Some(expected_value));
        } else {
            assert!(
                handle
                    .read(|child| child.value)
                    .await
                    .unwrap_err()
                    .is_scope_ended()
            );
            assert_eq!(observation.next().await, None);
        }

        let diagnostics = store
            .runtime_diagnostics()
            .await
            .expect("final diagnostics");
        assert_eq!(
            diagnostics.runtime().commit_version,
            2 + u64::try_from(expected_calls.len()).expect("bounded generated sequence"),
        );
        assert_eq!(
            diagnostics.handle_registration_count(),
            usize::from(generation_survives)
        );
        assert_eq!(
            diagnostics.runtime().observation_count,
            usize::from(generation_survives)
        );
        store.shutdown().await.expect("race Store shutdown");
    });
    #[cfg(miri)]
    wait_for_runtime_threads();
}

#[cfg(feature = "test-util")]
proptest! {
    #![proptest_config(property_config())]

    #[test]
    fn queued_mailbox_positions_obey_handle_generation(
        initial_id in 0_u8..4,
        transition in race_transition_strategy(),
        amounts in prop::collection::vec(any::<i8>(), 1..17),
    ) {
        run_queued_generation_race(initial_id, transition, &amounts);
    }
}

#[derive(Clone)]
/// Counts use of the selected service rather than only comparing copied IDs.
struct ServiceHandle {
    /// Eleven wrapping consecutive IDs remain distinct within each generated case.
    id: u8,
    /// Counts deferred use of this exact shared handle.
    uses: Arc<AtomicUsize>,
}

/// Supplies a distinctive fallback value when no tested override applies.
struct ServiceKey;

impl DependencyKey for ServiceKey {
    type Value = ServiceHandle;

    fn live_value() -> Self::Value {
        ServiceHandle {
            id: u8::MAX,
            uses: Arc::new(AtomicUsize::new(0)),
        }
    }
}

#[derive(Default)]
/// Records sibling, nested, captured and completed dependency selections separately.
struct DependencyState {
    sibling: Vec<u8>,
    nested: Vec<u8>,
    captured: Vec<u8>,
    completed: Vec<u8>,
}

/// Splits synchronous lookup from deferred use of an owned dependency handle.
enum DependencyAction {
    Probe,
    Capture,
    Completed(ServiceHandle),
}

#[derive(Clone, Copy)]
/// Selects either the unaffected sibling or the overridden nested reducer.
enum ProbeRole {
    Sibling,
    Nested,
}

/// Captures a chosen dependency before a later action and an explicit release.
struct DependencyProbe {
    role: ProbeRole,
    /// Overrides normal scope selection, but not the test override when enabled.
    explicit: Option<ServiceHandle>,
    /// Keeps deferred use pending until another probe has already committed.
    release: async_channel::Receiver<()>,
}

impl DependencyProbe {
    /// Applies a local override without leaking it into sibling reductions.
    fn selected(&self, context: &mut ReduceContext<'_>) -> ServiceHandle {
        if let Some(explicit) = &self.explicit {
            context.with_dependency_override::<ServiceKey, _>(explicit.clone(), |context| {
                context.dependency::<ServiceKey>().clone()
            })
        } else {
            context.dependency::<ServiceKey>().clone()
        }
    }
}

impl Reducer for DependencyProbe {
    type State = DependencyState;
    type Action = DependencyAction;

    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &Self::Action,
        context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        match action {
            DependencyAction::Probe => {
                let id = self.selected(context).id;
                match self.role {
                    ProbeRole::Sibling => state.sibling.push(id),
                    ProbeRole::Nested => state.nested.push(id),
                }
                Effect::none()
            }
            DependencyAction::Capture if matches!(self.role, ProbeRole::Nested) => {
                let selected = self.selected(context);
                state.captured.push(selected.id);
                let release = self.release.clone();
                Effect::run(move |sender| async move {
                    release
                        .recv()
                        .await
                        .expect("dependency release remains live");
                    selected.uses.fetch_add(1, Ordering::SeqCst);
                    sender
                        .send(DependencyAction::Completed(selected))
                        .await
                        .expect("captured dependency output commits");
                })
            }
            DependencyAction::Completed(handle) if matches!(self.role, ProbeRole::Nested) => {
                state.completed.push(handle.id);
                Effect::none()
            }
            DependencyAction::Capture | DependencyAction::Completed(_) => Effect::none(),
        }
    }
}

/// Gives one expected ID a distinct shared usage counter.
fn service_handle(id: u8) -> ServiceHandle {
    ServiceHandle {
        id,
        uses: Arc::new(AtomicUsize::new(0)),
    }
}

/// Checks lookup precedence and ownership after the captured effect resumes.
fn run_dependency_reducer<R>(
    reducer: R,
    store_handle: ServiceHandle,
    test_handle: ServiceHandle,
    handles: &[ServiceHandle],
    expected_sibling: u8,
    expected_nested: u8,
    release: async_channel::Sender<()>,
) where
    R: Reducer<State = DependencyState, Action = DependencyAction> + Send + 'static,
{
    futures_executor::block_on(async move {
        let (_unused_release, unused_receive) = async_channel::bounded(1);
        let reducer = combine((
            DependencyProbe {
                role: ProbeRole::Sibling,
                explicit: None,
                release: unused_receive,
            },
            reducer,
        ));
        let builder = Store::builder(DependencyState::default(), reducer, property_runtime())
            .with_dependency::<ServiceKey>(store_handle);
        #[cfg(feature = "test-util")]
        let builder = builder.with_test_dependency::<ServiceKey>(test_handle);
        #[cfg(not(feature = "test-util"))]
        drop(test_handle);
        let store = builder.build().expect("valid dependency Store");

        let _task = store
            .send(DependencyAction::Probe)
            .await
            .expect("initial dependency probe");
        let task = store
            .send(DependencyAction::Capture)
            .await
            .expect("dependency capture");
        let _task = store
            .send(DependencyAction::Probe)
            .await
            .expect("later dependency probe");
        release
            .send(())
            .await
            .expect("release receiver remains live");
        task.finish().await;

        store
            .read(move |state| {
                assert_eq!(state.sibling, [expected_sibling; 2]);
                assert_eq!(state.nested, [expected_nested; 2]);
                assert_eq!(state.captured, [expected_nested]);
                assert_eq!(state.completed, [expected_nested]);
            })
            .await
            .expect("dependency state read");

        for handle in handles {
            assert_eq!(
                handle.uses.load(Ordering::SeqCst),
                usize::from(handle.id == expected_nested),
                "only the selected captured handle may be used"
            );
        }
        store.shutdown().await.expect("dependency Store shutdown");
    });
    #[cfg(miri)]
    wait_for_runtime_threads();
}

#[expect(
    clippy::too_many_lines,
    reason = "literal overlay shapes independently encode all supported depths"
)]
/// Builds literal typed overlay depths and computes their expected winning IDs.
fn run_dependency_case(depth: usize, base: u8, explicit_enabled: bool) {
    let handles = (0_u8..11)
        .map(|offset| service_handle(base.wrapping_add(offset)))
        .collect::<Vec<_>>();
    let store_handle = handles[0].clone();
    let explicit_handle = handles[1].clone();
    let test_handle = handles[2].clone();
    let features = &handles[3..11];
    let explicit = explicit_enabled.then(|| explicit_handle.clone());
    let (release, receive) = async_channel::bounded(1);

    let test_wins = cfg!(feature = "test-util");
    let expected_sibling = if test_wins {
        test_handle.id
    } else {
        store_handle.id
    };
    let expected_nested = if test_wins {
        test_handle.id
    } else if explicit_enabled {
        explicit_handle.id
    } else if depth == 0 {
        store_handle.id
    } else {
        features[depth - 1].id
    };
    let probe = || DependencyProbe {
        role: ProbeRole::Nested,
        explicit: explicit.clone(),
        release: receive.clone(),
    };

    macro_rules! run {
        ($reducer:expr) => {
            run_dependency_reducer(
                $reducer,
                store_handle,
                test_handle,
                &handles,
                expected_sibling,
                expected_nested,
                release,
            )
        };
    }

    match depth {
        0 => run!(probe()),
        1 => run!(with_dependency::<ServiceKey, _>(
            features[0].clone(),
            probe()
        )),
        2 => run!(with_dependency::<ServiceKey, _>(
            features[0].clone(),
            with_dependency::<ServiceKey, _>(features[1].clone(), probe())
        )),
        3 => run!(with_dependency::<ServiceKey, _>(
            features[0].clone(),
            with_dependency::<ServiceKey, _>(
                features[1].clone(),
                with_dependency::<ServiceKey, _>(features[2].clone(), probe())
            )
        )),
        4 => run!(with_dependency::<ServiceKey, _>(
            features[0].clone(),
            with_dependency::<ServiceKey, _>(
                features[1].clone(),
                with_dependency::<ServiceKey, _>(
                    features[2].clone(),
                    with_dependency::<ServiceKey, _>(features[3].clone(), probe())
                )
            )
        )),
        5 => run!(with_dependency::<ServiceKey, _>(
            features[0].clone(),
            with_dependency::<ServiceKey, _>(
                features[1].clone(),
                with_dependency::<ServiceKey, _>(
                    features[2].clone(),
                    with_dependency::<ServiceKey, _>(
                        features[3].clone(),
                        with_dependency::<ServiceKey, _>(features[4].clone(), probe())
                    )
                )
            )
        )),
        6 => run!(with_dependency::<ServiceKey, _>(
            features[0].clone(),
            with_dependency::<ServiceKey, _>(
                features[1].clone(),
                with_dependency::<ServiceKey, _>(
                    features[2].clone(),
                    with_dependency::<ServiceKey, _>(
                        features[3].clone(),
                        with_dependency::<ServiceKey, _>(
                            features[4].clone(),
                            with_dependency::<ServiceKey, _>(features[5].clone(), probe())
                        )
                    )
                )
            )
        )),
        7 => run!(with_dependency::<ServiceKey, _>(
            features[0].clone(),
            with_dependency::<ServiceKey, _>(
                features[1].clone(),
                with_dependency::<ServiceKey, _>(
                    features[2].clone(),
                    with_dependency::<ServiceKey, _>(
                        features[3].clone(),
                        with_dependency::<ServiceKey, _>(
                            features[4].clone(),
                            with_dependency::<ServiceKey, _>(
                                features[5].clone(),
                                with_dependency::<ServiceKey, _>(features[6].clone(), probe())
                            )
                        )
                    )
                )
            )
        )),
        8 => run!(with_dependency::<ServiceKey, _>(
            features[0].clone(),
            with_dependency::<ServiceKey, _>(
                features[1].clone(),
                with_dependency::<ServiceKey, _>(
                    features[2].clone(),
                    with_dependency::<ServiceKey, _>(
                        features[3].clone(),
                        with_dependency::<ServiceKey, _>(
                            features[4].clone(),
                            with_dependency::<ServiceKey, _>(
                                features[5].clone(),
                                with_dependency::<ServiceKey, _>(
                                    features[6].clone(),
                                    with_dependency::<ServiceKey, _>(features[7].clone(), probe())
                                )
                            )
                        )
                    )
                )
            )
        )),
        _ => unreachable!("strategy limits dependency depth to eight"),
    }
}

proptest! {
    #![proptest_config(property_config())]

    #[test]
    fn dependency_layers_preserve_precedence_and_effect_handle_identity(
        depth in 0_usize..=8,
        base in any::<u8>(),
        explicit_enabled in any::<bool>(),
    ) {
        run_dependency_case(depth, base, explicit_enabled);
    }
}
