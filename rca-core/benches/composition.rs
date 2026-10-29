//! Composition, scoped Store and collection workloads.
//!
//! Persistent Store rows exclude construction and shutdown but include admission,
//! causal completion and worker scheduling. Generated-input rows exclude fixture
//! construction and returned-owner destruction; their mutation and assertions are
//! timed. Collection controls compare public operations, not equal metadata or
//! capacity contracts. Allocation mode retains the returned output at its snapshot
//! and counts all threads in the process; retained bytes are not a leak verdict.

use std::cell::Cell;
use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::future::pending;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier, Mutex, OnceLock};
use std::task::Poll;
use std::time::Instant;

use futures_executor::{ThreadPool, ThreadPoolBuilder};
use indexmap::IndexMap;
use rca_core::{
    ActionPath, ActionReceipt, ActionTask, CaseStatePath, DependencyKey, Effect, KeyedAction,
    KeyedActionPath, KeyedState, KeyedStatePath, MonotonicInstant, Observation, OptionalStatePath,
    ReduceContext, Reducer, Runtime, RuntimeDriver, RuntimeFuture, ScopedStore, StatePath, Store,
    combine, for_each, if_case, if_some, scope, with_dependency,
};

#[cfg(feature = "allocation-measurement")]
#[global_allocator]
static ALLOCATOR: dhat::Alloc = dhat::Alloc;

fn main() {
    #[cfg(feature = "allocation-measurement")]
    if let Ok(scenario) = std::env::var("RCA_COMPOSITION_ALLOCATION_SCENARIO")
        .or_else(|_| std::env::var("RCA_SCOPED_ALLOCATION_SCENARIO"))
    {
        let samples = std::env::var("RCA_COMPOSITION_ALLOCATION_SAMPLES")
            .or_else(|_| std::env::var("RCA_SCOPED_ALLOCATION_SAMPLES"))
            .map_or(Ok(1), |value| value.parse::<usize>())
            .expect("allocation sample count must be a positive integer");
        assert!(samples > 0, "allocation sample count must be positive");
        run_allocation_scenario(&scenario, samples);
        return;
    }
    divan::main();
}

#[divan::bench]
fn map_none(bencher: divan::Bencher<'_, '_>) {
    bencher.bench(|| {
        divan::black_box(Effect::<u8>::none().map(u16::from));
    });
}

/// Records both work cardinality and a checksum sensitive to mapping depth.
struct EffectMapState {
    /// Counts actual output reductions, not emitted effect descriptions.
    outputs: usize,
    /// Wraps deliberately so long sampling runs preserve the same oracle.
    checksum: u64,
}

/// Separates batch admission from each mapped output reduction.
enum EffectMapAction {
    Start,
    Output(u64),
}

/// Creates a finite output batch and applies the configured mapping chain.
struct EffectMapReducer {
    /// Includes the final conversion into the Store's action type.
    depth: usize,
    /// Number of outputs emitted by each start action.
    outputs: usize,
}

impl Reducer for EffectMapReducer {
    type State = EffectMapState;
    type Action = EffectMapAction;

    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &Self::Action,
        _context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        match action {
            EffectMapAction::Start => {
                let mut effect =
                    Effect::merge((0..self.outputs).map(|output| {
                        Effect::send(u64::try_from(output).expect("output fits u64"))
                    }));
                for level in 0..self.depth.saturating_sub(1) {
                    let increment = u64::try_from(level + 1).expect("mapping depth fits u64");
                    effect = effect.map(move |output| output.wrapping_add(increment));
                }
                let final_increment = u64::try_from(self.depth).expect("mapping depth fits u64");
                effect.map(move |output| {
                    EffectMapAction::Output(output.wrapping_add(final_increment))
                })
            }
            EffectMapAction::Output(output) => {
                state.outputs = state.outputs.wrapping_add(1);
                state.checksum = state.checksum.wrapping_add(*output);
                Effect::none()
            }
        }
    }
}

/// Computes the wrapping output oracle independently of the effect interpreter.
fn effect_map_batch_checksum(depth: usize, outputs: usize) -> u64 {
    let map_increment = (1..=depth).fold(0_u64, |checksum, level| {
        checksum.wrapping_add(u64::try_from(level).expect("mapping depth fits u64"))
    });
    (0..outputs).fold(0_u64, |checksum, output| {
        checksum
            .wrapping_add(u64::try_from(output).expect("output count fits u64"))
            .wrapping_add(map_increment)
    })
}

/// Warms one batch, then validates output count and mapping depth after timing.
fn effect_map_bench(bencher: divan::Bencher<'_, '_>, depth: usize, outputs: usize) {
    let fixture = Fixture::new(
        EffectMapState {
            outputs: 0,
            checksum: 0,
        },
        EffectMapReducer { depth, outputs },
    );
    finish_task(&fixture.commit(EffectMapAction::Start));
    let before =
        futures_executor::block_on(fixture.store.read(|state| (state.outputs, state.checksum)))
            .expect("effect-map warm-up state remains readable");
    assert_eq!(before.0, outputs, "warm-up emits every mapped output");
    assert_eq!(
        before.1,
        effect_map_batch_checksum(depth, outputs),
        "warm-up applies the configured mapping depth"
    );

    bencher.counter(outputs).bench_local(|| {
        finish_task(&fixture.commit(divan::black_box(EffectMapAction::Start)));
    });

    let after =
        futures_executor::block_on(fixture.store.read(|state| (state.outputs, state.checksum)))
            .expect("effect-map measured state remains readable");
    assert!(after.0 > before.0, "measurement emits mapped outputs");
    assert_eq!(
        (after.0 - before.0) % outputs,
        0,
        "each measured batch emits the configured output count"
    );
    let measured_batches = (after.0 - before.0) / outputs;
    assert_eq!(
        after.1.wrapping_sub(before.1),
        effect_map_batch_checksum(depth, outputs)
            .wrapping_mul(u64::try_from(measured_batches).expect("sample count fits u64")),
        "every measured output traverses the configured map chain"
    );
}

#[divan::bench(args = [1_usize, 4, 16], sample_count = 50, max_time = 0.5)]
fn effect_map_depth(bencher: divan::Bencher<'_, '_>, depth: usize) {
    effect_map_bench(bencher, depth, 1);
}

#[divan::bench(args = [1_usize, 16, 256], sample_count = 50, max_time = 0.5)]
fn effect_map_output_rate(bencher: divan::Bencher<'_, '_>, outputs: usize) {
    effect_map_bench(bencher, 4, outputs);
}

/// Counts leaf reductions through the configured composition graph.
struct State {
    /// Includes the explicit warm-up action and all timed actions.
    calls: usize,
}

/// Selects how many nested scope filters accept this input.
struct Action {
    /// Each scope accepts depths at least as large as its own level.
    depth: usize,
}

/// Supplies one observable state mutation without effects or dependency lookup.
struct Leaf;

impl Reducer for Leaf {
    type State = State;
    type Action = Action;

    fn reduce(
        &mut self,
        state: &mut Self::State,
        _action: &Self::Action,
        _context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        state.calls = state.calls.wrapping_add(1);
        Effect::none()
    }
}

/// Adds a borrowed identity-state scope with one depth-based action filter.
fn level<Child>(required_depth: usize, child: Child) -> impl Reducer<State = State, Action = Action>
where
    Child: Reducer<State = State, Action = Action>,
{
    scope(
        StatePath::new(|state: &State| state, |state: &mut State| state),
        ActionPath::new(
            move |action: &Action| (action.depth >= required_depth).then_some(action),
            |action| action,
        ),
        child,
    )
}

fn depth_1() -> impl Reducer<State = State, Action = Action> {
    level(1, Leaf)
}

fn depth_4() -> impl Reducer<State = State, Action = Action> {
    level(1, level(2, level(3, level(4, Leaf))))
}

fn depth_16() -> impl Reducer<State = State, Action = Action> {
    level(
        1,
        level(
            2,
            level(
                3,
                level(
                    4,
                    level(
                        5,
                        level(
                            6,
                            level(
                                7,
                                level(
                                    8,
                                    level(
                                        9,
                                        level(
                                            10,
                                            level(
                                                11,
                                                level(
                                                    12,
                                                    level(
                                                        13,
                                                        level(14, level(15, level(16, Leaf))),
                                                    ),
                                                ),
                                            ),
                                        ),
                                    ),
                                ),
                            ),
                        ),
                    ),
                ),
            ),
        ),
    )
}

/// Retains a four-worker pool for one fixture, outside persistent sample timing.
struct BenchRuntime {
    /// Shares pool ownership with the Store runtime facade.
    driver: ThreadDriver,
}

#[derive(Clone)]
/// Submits fixture tasks to the shared pool without a timer-thread implementation.
/// Elapsed waits complete and MAX stays pending; finite future waits are rejected.
struct ThreadDriver {
    /// Executes actor and effect futures; these workloads do not request timers.
    pool: ThreadPool,
    /// Common clock origin retained by every driver clone.
    origin: Instant,
}

impl RuntimeDriver for ThreadDriver {
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
            "composition runtime does not support finite future deadlines"
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

impl fmt::Debug for ThreadDriver {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ThreadDriver").finish_non_exhaustive()
    }
}

impl BenchRuntime {
    /// Starts the pool before persistent benchmark timing begins.
    fn new() -> Self {
        let pool = ThreadPoolBuilder::new()
            .pool_size(4)
            .name_prefix("rca-composition-bench-")
            .create()
            .expect("benchmark thread pool must start");
        Self {
            driver: ThreadDriver {
                pool,
                origin: Instant::now(),
            },
        }
    }

    /// Shares the existing pool and clock origin with a Store.
    fn runtime(&self) -> Runtime {
        Runtime::new(self.driver.clone())
    }
}

/// Owns an actor and awaits shutdown before releasing its shared worker pool.
struct Fixture<State: Send + 'static, Action: Send + 'static> {
    /// Carries actual public admission, projection and causal-completion operations.
    store: Store<State, Action>,
    /// Keeps the pool alive until the fixture's Drop has shut down its Store.
    _runtime: BenchRuntime,
}

impl<State, Action> Fixture<State, Action>
where
    State: Send + 'static,
    Action: Send + 'static,
{
    /// Creates the actor and orders a read after startup before returning it.
    fn new(
        state: State,
        reducer: impl Reducer<State = State, Action = Action> + Send + 'static,
    ) -> Self {
        let runtime = BenchRuntime::new();
        let store = Store::new(state, reducer, runtime.runtime());
        futures_executor::block_on(store.read(|_| ())).expect("actor must start");
        Self {
            store,
            _runtime: runtime,
        }
    }

    /// Waits for reduction, leaving causal-effect completion to the caller.
    fn commit(&self, action: Action) -> ActionTask {
        futures_executor::block_on(async {
            self.store.send(action).await.expect("action must commit")
        })
    }
}

impl<State, Action> Drop for Fixture<State, Action>
where
    State: Send + 'static,
    Action: Send + 'static,
{
    fn drop(&mut self) {
        futures_executor::block_on(self.store.shutdown()).expect("clean shutdown");
    }
}

/// Waits for causal completion without changing the action's cancellation policy.
fn finish_task(task: &ActionTask) {
    futures_executor::block_on(task.finish());
}

/// Measures committed actions and checks exact leaf work after the timed loop.
fn bench_store(
    bencher: divan::Bencher<'_, '_>,
    reducer: impl Reducer<State = State, Action = Action> + Send + 'static,
    action_depth: usize,
    calls_per_action: usize,
) {
    let fixture = Fixture::new(State { calls: 0 }, reducer);
    finish_task(&fixture.commit(Action {
        depth: action_depth,
    }));
    let mut iterations = 1;
    bencher.bench_local(|| {
        finish_task(&fixture.commit(Action {
            depth: divan::black_box(action_depth),
        }));
        iterations += 1;
    });
    let calls = futures_executor::block_on(fixture.store.read(|state| state.calls))
        .expect("final reducer count");
    assert_eq!(calls, iterations * calls_per_action);
}

#[divan::bench(args = [1_usize, 4, 16], sample_count = 50, items_count = 1_u64)]
fn direct(bencher: divan::Bencher<'_, '_>, depth: usize) {
    bench_store(bencher, Leaf, depth, 1);
}

#[divan::bench(
    args = [1_usize, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12],
    sample_count = 50,
    items_count = 1_u64
)]
fn combine_arity(bencher: divan::Bencher<'_, '_>, arity: usize) {
    macro_rules! bench_arity {
        ($($leaf:expr),+ $(,)?) => {
            bench_store(bencher, combine(($($leaf,)+)), 0, arity)
        };
    }

    match arity {
        1 => bench_arity!(Leaf),
        2 => bench_arity!(Leaf, Leaf),
        3 => bench_arity!(Leaf, Leaf, Leaf),
        4 => bench_arity!(Leaf, Leaf, Leaf, Leaf),
        5 => bench_arity!(Leaf, Leaf, Leaf, Leaf, Leaf),
        6 => bench_arity!(Leaf, Leaf, Leaf, Leaf, Leaf, Leaf),
        7 => bench_arity!(Leaf, Leaf, Leaf, Leaf, Leaf, Leaf, Leaf),
        8 => bench_arity!(Leaf, Leaf, Leaf, Leaf, Leaf, Leaf, Leaf, Leaf),
        9 => bench_arity!(Leaf, Leaf, Leaf, Leaf, Leaf, Leaf, Leaf, Leaf, Leaf),
        10 => bench_arity!(Leaf, Leaf, Leaf, Leaf, Leaf, Leaf, Leaf, Leaf, Leaf, Leaf),
        11 => bench_arity!(
            Leaf, Leaf, Leaf, Leaf, Leaf, Leaf, Leaf, Leaf, Leaf, Leaf, Leaf
        ),
        12 => bench_arity!(
            Leaf, Leaf, Leaf, Leaf, Leaf, Leaf, Leaf, Leaf, Leaf, Leaf, Leaf, Leaf,
        ),
        _ => unreachable!("Divan supplies only supported Combine arities"),
    }
}

#[divan::bench(args = [1_usize, 4, 16], sample_count = 50, items_count = 1_u64)]
fn matching_scope(bencher: divan::Bencher<'_, '_>, depth: usize) {
    match depth {
        1 => bench_store(bencher, depth_1(), depth, 1),
        4 => bench_store(bencher, depth_4(), depth, 1),
        16 => bench_store(bencher, depth_16(), depth, 1),
        _ => unreachable!("Divan supplies only supported scope depths"),
    }
}

#[divan::bench(args = [1_usize, 4, 16], sample_count = 50, items_count = 1_u64)]
fn nonmatching_scope(bencher: divan::Bencher<'_, '_>, depth: usize) {
    let action_depth = depth - 1;
    match depth {
        1 => bench_store(bencher, depth_1(), action_depth, 0),
        4 => bench_store(bencher, depth_4(), action_depth, 0),
        16 => bench_store(bencher, depth_16(), action_depth, 0),
        _ => unreachable!("Divan supplies only supported scope depths"),
    }
}

/// Retains the hit oracle while an optional or case child is installed.
struct LifecycleChildState {
    /// Counts only child hit actions, not parent transitions or pending tasks.
    hits: usize,
}

/// Makes child lifetime transitions explicit for the optional reducer fixtures.
struct LifecycleState {
    /// Absence retires the child's pending effects.
    child: Option<LifecycleChildState>,
}

/// Selects a synchronous hit or a declared group of indefinitely pending effects.
enum LifecycleChildAction {
    Hit,
    StartTasks(usize),
}

/// Routes child work or changes the optional child's lifetime.
enum LifecycleAction {
    Child(LifecycleChildAction),
    Enter,
    Leave,
    Miss,
}

/// Counts hits and creates pending work owned by the enclosing child lifetime.
struct LifecycleChild;

impl Reducer for LifecycleChild {
    type State = LifecycleChildState;
    type Action = LifecycleChildAction;

    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &Self::Action,
        _context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        match action {
            LifecycleChildAction::Hit => {
                state.hits = state.hits.wrapping_add(1);
                Effect::none()
            }
            LifecycleChildAction::StartTasks(task_count) => {
                Effect::merge((0..*task_count).map(|_| Effect::run(|_sender| pending())))
            }
        }
    }
}

/// Enters and retires the optional child without producing its own effects.
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
            LifecycleAction::Enter if state.child.is_none() => {
                state.child = Some(LifecycleChildState { hits: 0 });
            }
            LifecycleAction::Leave => state.child = None,
            LifecycleAction::Child(_) | LifecycleAction::Enter | LifecycleAction::Miss => {}
        }
        Effect::none()
    }
}

/// Keeps the common borrowed child route concrete without captured state.
type LifecycleActionPath = ActionPath<
    LifecycleAction,
    LifecycleChildAction,
    fn(&LifecycleAction) -> Option<&LifecycleChildAction>,
    fn(LifecycleChildAction) -> LifecycleAction,
>;

/// Routes only child actions and re-embeds their effects into the parent action.
fn lifecycle_action_path() -> LifecycleActionPath {
    ActionPath::new(
        (|action: &LifecycleAction| match action {
            LifecycleAction::Child(action) => Some(action),
            LifecycleAction::Enter | LifecycleAction::Leave | LifecycleAction::Miss => None,
        }) as fn(&LifecycleAction) -> Option<&LifecycleChildAction>,
        LifecycleAction::Child as fn(LifecycleChildAction) -> LifecycleAction,
    )
}

/// Composes parent transitions with child-lifetime effect cancellation.
fn optional_reducer() -> impl Reducer<State = LifecycleState, Action = LifecycleAction> {
    if_some(
        LifecycleParent,
        OptionalStatePath::new(
            |state: &LifecycleState| state.child.as_ref(),
            |state: &mut LifecycleState| state.child.as_mut(),
        ),
        lifecycle_action_path(),
        LifecycleChild,
    )
}

/// Chooses the initial child presence without timing fixture construction.
fn optional_fixture(child_present: bool) -> Fixture<LifecycleState, LifecycleAction> {
    Fixture::new(
        LifecycleState {
            child: child_present.then_some(LifecycleChildState { hits: 0 }),
        },
        optional_reducer(),
    )
}

#[divan::bench(sample_count = 50, max_time = 0.5, items_count = 1_u64)]
fn optional_hit(bencher: divan::Bencher<'_, '_>) {
    let fixture = optional_fixture(true);
    finish_task(&fixture.commit(LifecycleAction::Child(LifecycleChildAction::Hit)));
    let mut hits = 1;
    bencher.bench_local(|| {
        finish_task(&fixture.commit(divan::black_box(LifecycleAction::Child(
            LifecycleChildAction::Hit,
        ))));
        hits += 1;
    });
    assert_eq!(
        futures_executor::block_on(
            fixture
                .store
                .read(|state| { state.child.as_ref().map(|child| child.hits) })
        )
        .expect("final optional state"),
        Some(hits)
    );
}

#[divan::bench(sample_count = 50, max_time = 0.5, items_count = 1_u64)]
fn optional_miss(bencher: divan::Bencher<'_, '_>) {
    let fixture = optional_fixture(true);
    finish_task(&fixture.commit(LifecycleAction::Miss));
    bencher.bench_local(|| {
        finish_task(&fixture.commit(divan::black_box(LifecycleAction::Miss)));
    });
    assert_eq!(
        futures_executor::block_on(
            fixture
                .store
                .read(|state| { state.child.as_ref().map(|child| child.hits) })
        )
        .expect("final optional state"),
        Some(0)
    );
}

#[divan::bench(
    sample_count = 50,
    sample_size = 1,
    max_time = 0.5,
    skip_ext_time = true,
    items_count = 1_u64
)]
fn optional_enter(bencher: divan::Bencher<'_, '_>) {
    bencher
        .with_inputs(|| optional_fixture(false))
        .bench_local_values(|fixture| {
            finish_task(&fixture.commit(divan::black_box(LifecycleAction::Enter)));
            fixture
        });
}

#[divan::bench(
    sample_count = 50,
    sample_size = 1,
    max_time = 0.5,
    skip_ext_time = true,
    items_count = 1_u64
)]
fn optional_leave(bencher: divan::Bencher<'_, '_>) {
    bencher
        .with_inputs(|| optional_fixture(true))
        .bench_local_values(|fixture| {
            finish_task(&fixture.commit(divan::black_box(LifecycleAction::Leave)));
            fixture
        });
}

/// Models mutually exclusive parent cases, not any rendered UI.
enum ScreenState {
    List,
    Detail(LifecycleChildState),
}

/// Routes detail work or exits the detail case.
enum ScreenAction {
    Detail(LifecycleChildAction),
    Switch,
    Miss,
}

/// Retires the detail case when the switch action commits.
struct ScreenParent;

impl Reducer for ScreenParent {
    type State = ScreenState;
    type Action = ScreenAction;

    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &Self::Action,
        _context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        if let ScreenAction::Switch = action {
            *state = ScreenState::List;
        }
        Effect::none()
    }
}

/// Installs the child only while the parent remains in its detail case.
fn case_reducer() -> impl Reducer<State = ScreenState, Action = ScreenAction> {
    if_case(
        ScreenParent,
        CaseStatePath::new(
            |state: &ScreenState| match state {
                ScreenState::Detail(child) => Some(child),
                ScreenState::List => None,
            },
            |state: &mut ScreenState| match state {
                ScreenState::Detail(child) => Some(child),
                ScreenState::List => None,
            },
        ),
        ActionPath::new(
            |action: &ScreenAction| match action {
                ScreenAction::Detail(action) => Some(action),
                ScreenAction::Switch | ScreenAction::Miss => None,
            },
            ScreenAction::Detail,
        ),
        LifecycleChild,
    )
}

/// Starts in the detail case so hit, miss and exit rows share their initial graph.
fn case_fixture() -> Fixture<ScreenState, ScreenAction> {
    Fixture::new(
        ScreenState::Detail(LifecycleChildState { hits: 0 }),
        case_reducer(),
    )
}

#[divan::bench(sample_count = 50, max_time = 0.5, items_count = 1_u64)]
fn case_hit(bencher: divan::Bencher<'_, '_>) {
    let fixture = case_fixture();
    finish_task(&fixture.commit(ScreenAction::Detail(LifecycleChildAction::Hit)));
    let mut hits = 1;
    bencher.bench_local(|| {
        finish_task(&fixture.commit(divan::black_box(ScreenAction::Detail(
            LifecycleChildAction::Hit,
        ))));
        hits += 1;
    });
    assert_eq!(
        futures_executor::block_on(fixture.store.read(|state| match state {
            ScreenState::Detail(child) => Some(child.hits),
            ScreenState::List => None,
        }))
        .expect("final case state"),
        Some(hits)
    );
}

#[divan::bench(sample_count = 50, max_time = 0.5, items_count = 1_u64)]
fn case_miss(bencher: divan::Bencher<'_, '_>) {
    let fixture = case_fixture();
    finish_task(&fixture.commit(ScreenAction::Miss));
    bencher.bench_local(|| {
        finish_task(&fixture.commit(divan::black_box(ScreenAction::Miss)));
    });
    assert_eq!(
        futures_executor::block_on(fixture.store.read(|state| match state {
            ScreenState::Detail(child) => Some(child.hits),
            ScreenState::List => None,
        }))
        .expect("final case state"),
        Some(0)
    );
}

#[divan::bench(
    sample_count = 50,
    sample_size = 1,
    max_time = 0.5,
    skip_ext_time = true,
    items_count = 1_u64
)]
fn case_switch(bencher: divan::Bencher<'_, '_>) {
    bencher
        .with_inputs(case_fixture)
        .bench_local_values(|fixture| {
            finish_task(&fixture.commit(divan::black_box(ScreenAction::Switch)));
            fixture
        });
}

/// Adds one required descendant below the optional cancellation boundary.
fn descendant_reducer() -> impl Reducer<State = LifecycleState, Action = LifecycleAction> {
    let nested = scope(
        StatePath::new(
            |state: &LifecycleChildState| state,
            |state: &mut LifecycleChildState| state,
        ),
        ActionPath::new(
            |action: &LifecycleChildAction| Some(action),
            |action| action,
        ),
        LifecycleChild,
    );
    if_some(
        LifecycleParent,
        OptionalStatePath::new(
            |state: &LifecycleState| state.child.as_ref(),
            |state: &mut LifecycleState| state.child.as_mut(),
        ),
        lifecycle_action_path(),
        nested,
    )
}

/// Creates live descendant work before timing the structural removal action.
struct CancellationFixture {
    /// Owns the optional child and its nested reducer.
    fixture: Fixture<LifecycleState, LifecycleAction>,
    /// Keeps the original causal cohort available for cleanup after removal.
    start_task: ActionTask,
}

impl CancellationFixture {
    /// Commits pending work before timing the child's removal.
    fn new(task_count: usize) -> Self {
        let fixture = Fixture::new(
            LifecycleState {
                child: Some(LifecycleChildState { hits: 0 }),
            },
            descendant_reducer(),
        );
        let start_task = fixture.commit(LifecycleAction::Child(LifecycleChildAction::StartTasks(
            divan::black_box(task_count),
        )));
        Self {
            fixture,
            start_task,
        }
    }

    /// Removes the optional child and awaits cancellation of its descendants.
    fn cancel(&self) {
        finish_task(&self.fixture.commit(LifecycleAction::Leave));
    }
}

impl Drop for CancellationFixture {
    fn drop(&mut self) {
        // Divan may discard an input before its measured cancellation runs.
        self.start_task.cancel();
        finish_task(&self.start_task);
    }
}

#[divan::bench(
    args = [1_usize, 100, 10_000],
    sample_count = 50,
    sample_size = 1,
    max_time = 0.5,
    skip_ext_time = true,
    items_count = 1_u64
)]
fn descendant_cancellation(bencher: divan::Bencher<'_, '_>, task_count: usize) {
    bencher
        .with_inputs(|| CancellationFixture::new(task_count))
        .bench_local_values(|fixture| {
            fixture.cancel();
            fixture
        });
}

#[expect(
    clippy::trivially_copy_pass_by_ref,
    reason = "controls match KeyedState's generic borrowed-ID API"
)]
/// Matches common ordered-map operations without pretending storage contracts match.
trait OrderedBenchCollection: Sized {
    /// Creates IDs and values `0..size`, reserving spare capacity where supported.
    fn with_size(size: usize, spare_capacity: usize) -> Self;
    /// Borrows the value for an existing or absent ID.
    fn get(&self, id: &u64) -> Option<&u64>;
    /// Adds a fresh ID; these fixtures never benchmark duplicate replacement.
    fn insert(&mut self, id: u64, value: u64);
    /// Removes an ID while retaining the order of remaining entries.
    fn ordered_remove(&mut self, id: &u64) -> Option<u64>;
    /// Removes an ID by moving the final entry into its position.
    fn swap_remove(&mut self, id: &u64) -> Option<u64>;
    /// Moves one existing position while retaining all entries.
    fn move_index(&mut self, from: usize, to: usize);
    /// Removes all entries without promising equal capacity retention across controls.
    fn clear(&mut self);
    /// Reports logical entries, not backing allocation capacity.
    fn len(&self) -> usize;
}

/// Exercises RCA's ordered collection, including its insertion identity metadata.
struct KeyedStateCollection(KeyedState<u64, u64>);

impl OrderedBenchCollection for KeyedStateCollection {
    fn with_size(size: usize, spare_capacity: usize) -> Self {
        let mut collection = KeyedState::with_capacity(size + spare_capacity);
        collection.extend((0..size).map(|id| {
            let id = u64::try_from(id).expect("benchmark size fits u64");
            (id, id)
        }));
        Self(collection)
    }

    fn get(&self, id: &u64) -> Option<&u64> {
        self.0.get(id)
    }

    fn insert(&mut self, id: u64, value: u64) {
        self.0.insert(id, value);
    }

    fn ordered_remove(&mut self, id: &u64) -> Option<u64> {
        self.0.remove(id)
    }

    fn swap_remove(&mut self, id: &u64) -> Option<u64> {
        self.0.swap_remove(id)
    }

    fn move_index(&mut self, from: usize, to: usize) {
        self.0.move_index(from, to);
    }

    fn clear(&mut self) {
        self.0.clear();
    }

    fn len(&self) -> usize {
        self.0.len()
    }
}

/// Provides an ordered hash-map control without RCA insertion identities.
struct IndexMapCollection(IndexMap<u64, u64>);

impl OrderedBenchCollection for IndexMapCollection {
    fn with_size(size: usize, spare_capacity: usize) -> Self {
        let mut collection = IndexMap::with_capacity(size + spare_capacity);
        collection.extend((0..size).map(|id| {
            let id = u64::try_from(id).expect("benchmark size fits u64");
            (id, id)
        }));
        Self(collection)
    }

    fn get(&self, id: &u64) -> Option<&u64> {
        self.0.get(id)
    }

    fn insert(&mut self, id: u64, value: u64) {
        assert!(self.0.insert(id, value).is_none(), "benchmark ID is new");
    }

    fn ordered_remove(&mut self, id: &u64) -> Option<u64> {
        self.0.shift_remove(id)
    }

    fn swap_remove(&mut self, id: &u64) -> Option<u64> {
        self.0.swap_remove(id)
    }

    fn move_index(&mut self, from: usize, to: usize) {
        self.0.move_index(from, to);
    }

    fn clear(&mut self) {
        self.0.clear();
    }

    fn len(&self) -> usize {
        self.0.len()
    }
}

/// Separates ordered values from hashed ID-to-position lookup.
struct VecHashCollection {
    /// Defines iteration and removal order independently of hash layout.
    entries: Vec<(u64, u64)>,
    /// Must be updated after a positional mutation.
    indexes: HashMap<u64, usize>,
}

impl VecHashCollection {
    /// Repairs lookup positions from the first index changed by an ordered edit.
    fn reindex_from(&mut self, start: usize) {
        for (index, (id, _)) in self.entries.iter().enumerate().skip(start) {
            self.indexes.insert(*id, index);
        }
    }
}

impl OrderedBenchCollection for VecHashCollection {
    fn with_size(size: usize, spare_capacity: usize) -> Self {
        let mut entries = Vec::with_capacity(size + spare_capacity);
        let mut indexes = HashMap::with_capacity(size + spare_capacity);
        for index in 0..size {
            let id = u64::try_from(index).expect("benchmark size fits u64");
            entries.push((id, id));
            indexes.insert(id, index);
        }
        Self { entries, indexes }
    }

    fn get(&self, id: &u64) -> Option<&u64> {
        self.indexes.get(id).map(|index| &self.entries[*index].1)
    }

    fn insert(&mut self, id: u64, value: u64) {
        let index = self.entries.len();
        assert!(
            self.indexes.insert(id, index).is_none(),
            "benchmark ID is new"
        );
        self.entries.push((id, value));
    }

    fn ordered_remove(&mut self, id: &u64) -> Option<u64> {
        let index = self.indexes.remove(id)?;
        let (_, value) = self.entries.remove(index);
        self.reindex_from(index);
        Some(value)
    }

    fn swap_remove(&mut self, id: &u64) -> Option<u64> {
        let index = self.indexes.remove(id)?;
        let (_, value) = self.entries.swap_remove(index);
        if let Some((moved_id, _)) = self.entries.get(index) {
            self.indexes.insert(*moved_id, index);
        }
        Some(value)
    }

    fn move_index(&mut self, from: usize, to: usize) {
        let entry = self.entries.remove(from);
        self.entries.insert(to, entry);
        self.reindex_from(from.min(to));
    }

    fn clear(&mut self) {
        self.entries.clear();
        self.indexes.clear();
    }

    fn len(&self) -> usize {
        self.entries.len()
    }
}

/// Ordered-value control with tree lookup rather than a hash table.
struct VecBTreeCollection {
    /// Defines value order independently of the sorted lookup index.
    entries: Vec<(u64, u64)>,
    /// Has no spare-capacity reservation API, unlike the hash-based controls.
    indexes: BTreeMap<u64, usize>,
}

impl VecBTreeCollection {
    /// Repairs sorted lookup positions after the ordered value vector changes.
    fn reindex_from(&mut self, start: usize) {
        for (index, (id, _)) in self.entries.iter().enumerate().skip(start) {
            self.indexes.insert(*id, index);
        }
    }
}

impl OrderedBenchCollection for VecBTreeCollection {
    fn with_size(size: usize, spare_capacity: usize) -> Self {
        let mut entries = Vec::with_capacity(size + spare_capacity);
        let mut indexes = BTreeMap::new();
        for index in 0..size {
            let id = u64::try_from(index).expect("benchmark size fits u64");
            entries.push((id, id));
            indexes.insert(id, index);
        }
        Self { entries, indexes }
    }

    fn get(&self, id: &u64) -> Option<&u64> {
        self.indexes.get(id).map(|index| &self.entries[*index].1)
    }

    fn insert(&mut self, id: u64, value: u64) {
        let index = self.entries.len();
        assert!(
            self.indexes.insert(id, index).is_none(),
            "benchmark ID is new"
        );
        self.entries.push((id, value));
    }

    fn ordered_remove(&mut self, id: &u64) -> Option<u64> {
        let index = self.indexes.remove(id)?;
        let (_, value) = self.entries.remove(index);
        self.reindex_from(index);
        Some(value)
    }

    fn swap_remove(&mut self, id: &u64) -> Option<u64> {
        let index = self.indexes.remove(id)?;
        let (_, value) = self.entries.swap_remove(index);
        if let Some((moved_id, _)) = self.entries.get(index) {
            self.indexes.insert(*moved_id, index);
        }
        Some(value)
    }

    fn move_index(&mut self, from: usize, to: usize) {
        let entry = self.entries.remove(from);
        self.entries.insert(to, entry);
        self.reindex_from(from.min(to));
    }

    fn clear(&mut self) {
        self.entries.clear();
        self.indexes.clear();
    }

    fn len(&self) -> usize {
        self.entries.len()
    }
}

#[derive(Clone, Copy)]
/// Selects one edit on a fresh nonempty collection input.
enum CollectionMutation {
    Insert,
    OrderedRemove,
    SwapRemove,
    Move,
    Clear,
}

/// Checks the selected value before timing repeated borrowed-ID lookups.
fn bench_collection_lookup<C: OrderedBenchCollection>(
    bencher: divan::Bencher<'_, '_>,
    size: usize,
) {
    let collection = C::with_size(size, 0);
    let id = u64::try_from(size / 2).expect("benchmark size fits u64");
    assert_eq!(collection.get(&id), Some(&id));
    bencher.bench_local(|| {
        divan::black_box(collection.get(divan::black_box(&id)));
    });
}

/// Times one edit and its oracle; returned collection destruction is excluded.
fn bench_collection_mutation<C: OrderedBenchCollection>(
    bencher: divan::Bencher<'_, '_>,
    size: usize,
    mutation: CollectionMutation,
) {
    bencher
        .with_inputs(|| C::with_size(size, 1))
        .bench_local_values(|mut collection| {
            let middle = u64::try_from(size / 2).expect("benchmark size fits u64");
            match mutation {
                CollectionMutation::Insert => {
                    let id = u64::try_from(size).expect("benchmark size fits u64");
                    collection.insert(divan::black_box(id), id);
                    assert_eq!(collection.get(&id), Some(&id));
                    assert_eq!(collection.len(), size + 1);
                }
                CollectionMutation::OrderedRemove => {
                    assert_eq!(
                        divan::black_box(collection.ordered_remove(divan::black_box(&middle))),
                        Some(middle)
                    );
                    assert_eq!(collection.get(&middle), None);
                    assert_eq!(collection.len(), size - 1);
                }
                CollectionMutation::SwapRemove => {
                    assert_eq!(
                        divan::black_box(collection.swap_remove(divan::black_box(&middle))),
                        Some(middle)
                    );
                    assert_eq!(collection.get(&middle), None);
                    assert_eq!(collection.len(), size - 1);
                }
                CollectionMutation::Move => {
                    collection.move_index(divan::black_box(size - 1), 0);
                    assert_eq!(collection.len(), size);
                }
                CollectionMutation::Clear => {
                    collection.clear();
                    assert_eq!(collection.len(), 0);
                }
            }
            divan::black_box(collection.len());
            collection
        });
}

/// Instantiates the same operation rows for each explicitly different control.
macro_rules! collection_benches {
    ($module:ident, $collection:ty) => {
        mod $module {
            use super::*;

            #[divan::bench(args = [10_usize, 1_000, 100_000], sample_count = 50, max_time = 0.5, items_count = 1_u64)]
            fn lookup(bencher: divan::Bencher<'_, '_>, size: usize) {
                bench_collection_lookup::<$collection>(bencher, size);
            }

            macro_rules! mutation_bench {
                ($name:ident, $mutation:expr) => {
                    #[divan::bench(args = [10_usize, 1_000, 100_000], sample_count = 10, sample_size = 4, max_time = 0.5, skip_ext_time = true, items_count = 1_u64)]
                    fn $name(bencher: divan::Bencher<'_, '_>, size: usize) {
                        bench_collection_mutation::<$collection>(bencher, size, $mutation);
                    }
                };
            }

            mutation_bench!(insert, CollectionMutation::Insert);
            mutation_bench!(ordered_remove, CollectionMutation::OrderedRemove);
            mutation_bench!(swap_remove, CollectionMutation::SwapRemove);
            mutation_bench!(move_last_to_first, CollectionMutation::Move);
            mutation_bench!(clear, CollectionMutation::Clear);
        }
    };
}

collection_benches!(keyed_state_collection, KeyedStateCollection);
collection_benches!(indexmap_control, IndexMapCollection);
collection_benches!(vec_hashmap_control, VecHashCollection);
collection_benches!(vec_btreemap_control, VecBTreeCollection);

#[cfg(test)]
mod cleanup_tests {
    #[test]
    fn thread_driver_max_deadline_stays_pending() {
        use std::task::{Context, Waker};

        use rca_core::{MonotonicInstant, RuntimeDriver};

        let driver = super::ThreadDriver {
            pool: super::ThreadPoolBuilder::new()
                .pool_size(1)
                .create()
                .expect("clock control thread pool"),
            origin: super::Instant::now(),
        };
        let mut context = Context::from_waker(Waker::noop());
        let mut distant = driver.sleep_until(MonotonicInstant::MAX);
        assert!(
            distant.as_mut().poll(&mut context).is_pending(),
            "an unrepresentable future deadline must remain pending"
        );
        assert!(distant.as_mut().poll(&mut context).is_pending());
    }

    #[test]
    #[should_panic(expected = "composition runtime does not support finite future deadlines")]
    fn thread_driver_rejects_unsupported_finite_future_deadlines() {
        use rca_core::RuntimeDriver;

        let driver = super::ThreadDriver {
            pool: super::ThreadPoolBuilder::new()
                .pool_size(1)
                .create()
                .expect("clock control thread pool"),
            origin: super::Instant::now(),
        };
        let deadline = driver
            .now()
            .saturating_add(std::time::Duration::from_secs(3_600));
        drop(driver.sleep_until(deadline));
    }

    #[test]
    #[should_panic(expected = "paused runtime does not support finite future deadlines")]
    fn paused_driver_rejects_unsupported_finite_future_deadlines() {
        use rca_core::RuntimeDriver;

        let driver = super::PausedDriver::new();
        let deadline = driver
            .now()
            .saturating_add(std::time::Duration::from_secs(3_600));
        drop(driver.sleep_until(deadline));
    }

    #[test]
    fn paused_driver_clock_completes_elapsed_deadlines() {
        use std::task::{Context, Waker};

        use rca_core::{MonotonicInstant, RuntimeDriver};

        let driver = super::PausedDriver::new();
        let mut context = Context::from_waker(Waker::noop());
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
    }

    #[test]
    fn paused_driver_yield_suspends_and_wakes_once_before_completion() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::task::{Context, Wake, Waker};

        use rca_core::RuntimeDriver;

        /// Counts scheduling notifications without running benchmark work.
        struct Notifications(AtomicUsize);

        impl Wake for Notifications {
            fn wake(self: Arc<Self>) {
                self.0.fetch_add(1, Ordering::Relaxed);
            }

            fn wake_by_ref(self: &Arc<Self>) {
                self.0.fetch_add(1, Ordering::Relaxed);
            }
        }

        let driver = super::PausedDriver::new();
        let notifications = Arc::new(Notifications(AtomicUsize::new(0)));
        let waker = Waker::from(Arc::clone(&notifications));
        let mut context = Context::from_waker(&waker);
        let mut future = driver.yield_now();
        assert_eq!(notifications.0.load(Ordering::Relaxed), 0);
        assert!(
            future.as_mut().poll(&mut context).is_pending(),
            "yield must offer a scheduling turn"
        );
        assert_eq!(notifications.0.load(Ordering::Relaxed), 1);
        assert!(future.as_mut().poll(&mut context).is_ready());
        assert_eq!(notifications.0.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn unused_optional_cancellation_input_can_drop() {
        // Generated inputs can be discarded without entering the measured body.
        drop(super::CancellationFixture::new(1));
    }

    #[test]
    fn unused_keyed_cancellation_input_can_drop() {
        drop(super::KeyedRemovalCancellationFixture::new(10));
    }

    #[test]
    fn unused_concurrent_sender_input_can_drop() {
        drop(super::ConcurrentSendFixture::new());
    }

    #[test]
    fn partial_concurrent_launch_releases_all_started_workers() {
        use std::sync::Arc;

        for fail_after in [1, super::CONCURRENT_SENDER_COUNT] {
            let mut retained = None;
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                super::ConcurrentSendFixture::with_launch_check(|launched, start, completed| {
                    if launched == fail_after {
                        retained = Some((Arc::clone(start), Arc::clone(completed)));
                        panic!("injected partial producer launch");
                    }
                })
            }));
            let Err(panic) = result else {
                panic!("partial launch injection did not run");
            };
            assert_eq!(
                panic.downcast_ref::<&str>(),
                Some(&"injected partial producer launch")
            );
            let (start, completed) = retained.expect("requested real producer was launched");
            let released = Arc::strong_count(&start) == 1;
            if !released {
                // Rescue the old fixed barrier before failing. Already-lost
                // worker join handles cannot be recovered from the old source.
                let rescuers: Vec<_> = (fail_after..super::CONCURRENT_SENDER_COUNT)
                    .map(|_| {
                        let start = Arc::clone(&start);
                        let completed = Arc::clone(&completed);
                        std::thread::spawn(move || {
                            start.wait();
                            completed.wait();
                        })
                    })
                    .collect();
                start.wait();
                completed.wait();
                for rescuer in rescuers {
                    rescuer.join().expect("rescue producer joined");
                }
            }
            assert!(released, "partial launch retained a blocked producer");
            assert_eq!(Arc::strong_count(&completed), 1);
        }
    }

    #[test]
    fn concurrent_launch_retains_the_original_start_boundary() {
        use std::sync::atomic::Ordering;

        let fixture = super::ConcurrentSendFixture::new();
        assert_eq!(fixture.accepted.load(Ordering::Acquire), 0);
        assert_eq!(fixture.finished.load(Ordering::Acquire), 0);
        fixture.run();
        fixture.verify();
    }

    #[test]
    fn lifecycle_rows_change_the_intended_parent_shape() {
        let optional = super::optional_fixture(false);
        super::finish_task(&optional.commit(super::LifecycleAction::Enter));
        assert_eq!(
            futures_executor::block_on(
                optional
                    .store
                    .read(|state| state.child.as_ref().map(|child| child.hits))
            )
            .unwrap(),
            Some(0)
        );
        super::finish_task(&optional.commit(super::LifecycleAction::Leave));
        assert!(
            futures_executor::block_on(optional.store.read(|state| state.child.is_none())).unwrap()
        );

        let case = super::case_fixture();
        super::finish_task(&case.commit(super::ScreenAction::Switch));
        assert!(
            futures_executor::block_on(
                case.store
                    .read(|state| matches!(state, super::ScreenState::List))
            )
            .unwrap()
        );
    }

    #[test]
    fn collection_controls_preserve_order_and_lookup_after_edits() {
        use super::OrderedBenchCollection;

        // Concrete storage inspection is independent of each control's lookup
        // index. Literal order catches a no-op move that a length check misses.
        macro_rules! check {
            ($collection:expr, $ids:expr) => {{
                let mut collection = $collection;
                collection.move_index(3, 0);
                assert_eq!(($ids)(&collection), [3, 0, 1, 2]);
                assert_eq!(collection.ordered_remove(&0), Some(0));
                assert_eq!(($ids)(&collection), [3, 1, 2]);
                for id in [3, 1, 2] {
                    assert_eq!(collection.get(&id), Some(&id));
                }
                assert_eq!(collection.get(&0), None);
                assert_eq!(collection.swap_remove(&3), Some(3));
                assert_eq!(($ids)(&collection), [2, 1]);
                for id in [2, 1] {
                    assert_eq!(collection.get(&id), Some(&id));
                }
                collection.clear();
                assert!(($ids)(&collection).is_empty());
                assert_eq!(collection.get(&1), None);
                collection.insert(4, 4);
                assert_eq!(($ids)(&collection), [4]);
                assert_eq!(collection.get(&4), Some(&4));
            }};
        }

        check!(
            super::KeyedStateCollection::with_size(4, 1),
            |value: &super::KeyedStateCollection| value.0.ids().copied().collect::<Vec<_>>()
        );
        check!(
            super::IndexMapCollection::with_size(4, 1),
            |value: &super::IndexMapCollection| value.0.keys().copied().collect::<Vec<_>>()
        );
        check!(
            super::VecHashCollection::with_size(4, 1),
            |value: &super::VecHashCollection| value
                .entries
                .iter()
                .map(|(id, _)| *id)
                .collect::<Vec<_>>()
        );
        check!(
            super::VecBTreeCollection::with_size(4, 1),
            |value: &super::VecBTreeCollection| value
                .entries
                .iter()
                .map(|(id, _)| *id)
                .collect::<Vec<_>>()
        );
    }
}

/// Separates routed child mutations from parent receipt of mapped child outputs.
struct KeyedBenchState {
    /// Owns each child insertion and its reducer state.
    rows: KeyedState<u64, usize>,
    /// Counts emitted output actions without changing child hit values.
    outputs: usize,
}

/// Selects child mutation, pending work or a finite mapped-output batch.
enum KeyedBenchChildAction {
    Hit,
    StartPending,
    EmitTwo,
    Output,
}

/// Routes keyed work or edits collection membership at the parent level.
enum KeyedBenchAction {
    Row(KeyedAction<u64, KeyedBenchChildAction>),
    Unrelated,
    RemoveOne(u64),
    RemoveFirst(usize),
}

/// Provides separate synchronous, indefinitely pending and two-output workloads.
struct KeyedBenchChild;

impl Reducer for KeyedBenchChild {
    type State = usize;
    type Action = KeyedBenchChildAction;

    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &Self::Action,
        _context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        match action {
            KeyedBenchChildAction::Hit => {
                *state = state.wrapping_add(1);
                Effect::none()
            }
            KeyedBenchChildAction::StartPending => Effect::run(|_sender| pending()),
            KeyedBenchChildAction::EmitTwo => Effect::merge([
                Effect::send(KeyedBenchChildAction::Output),
                Effect::send(KeyedBenchChildAction::Output),
            ]),
            KeyedBenchChildAction::Output => Effect::none(),
        }
    }
}

/// Counts routed outputs and performs the structural edits that cancel children.
struct KeyedBenchParent;

impl Reducer for KeyedBenchParent {
    type State = KeyedBenchState;
    type Action = KeyedBenchAction;

    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &Self::Action,
        _context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        match action {
            KeyedBenchAction::Row(action)
                if matches!(action.action(), KeyedBenchChildAction::Output) =>
            {
                state.outputs = state.outputs.wrapping_add(1);
            }
            KeyedBenchAction::RemoveOne(id) => {
                let _ = state.rows.remove(id);
            }
            KeyedBenchAction::RemoveFirst(count) => {
                state.rows.retain(|id, _| {
                    usize::try_from(*id).expect("benchmark ID fits usize") >= *count
                });
            }
            KeyedBenchAction::Row(_) | KeyedBenchAction::Unrelated => {}
        }
        Effect::none()
    }
}

/// Uses the public keyed combinator to route each insertion's state and effects.
fn keyed_bench_reducer() -> impl Reducer<State = KeyedBenchState, Action = KeyedBenchAction> {
    for_each(
        KeyedBenchParent,
        KeyedStatePath::new(
            |state: &KeyedBenchState| &state.rows,
            |state: &mut KeyedBenchState| &mut state.rows,
        ),
        KeyedActionPath::new(
            |action: &KeyedBenchAction| match action {
                KeyedBenchAction::Row(action) => Some(action),
                KeyedBenchAction::Unrelated
                | KeyedBenchAction::RemoveOne(_)
                | KeyedBenchAction::RemoveFirst(_) => None,
            },
            KeyedBenchAction::Row,
        ),
        KeyedBenchChild,
    )
}

/// Creates sequential IDs with zero hit counts before any lineage warm-up.
fn keyed_fixture(size: usize) -> Fixture<KeyedBenchState, KeyedBenchAction> {
    Fixture::new(
        KeyedBenchState {
            rows: (0..size)
                .map(|id| {
                    let id = u64::try_from(id).expect("benchmark size fits u64");
                    (id, 0)
                })
                .collect(),
            outputs: 0,
        },
        keyed_bench_reducer(),
    )
}

/// Reconciles the keyed children without changing their state or emitting outputs.
fn warm_keyed_fixture(fixture: &Fixture<KeyedBenchState, KeyedBenchAction>) {
    finish_task(&fixture.commit(KeyedBenchAction::Unrelated));
}

#[divan::bench(
    args = [10_usize, 1_000, 100_000],
    sample_count = 5,
    sample_size = 1,
    max_time = 0.5,
    skip_ext_time = true
)]
fn keyed_initial_mount(bencher: divan::Bencher<'_, '_>, size: usize) {
    bencher
        .with_inputs(|| keyed_fixture(size))
        .bench_local_values(|fixture| {
            warm_keyed_fixture(&fixture);
            fixture
        });
}

#[divan::bench(
    args = [10_usize, 1_000, 100_000],
    sample_count = 50,
    max_time = 0.5,
    items_count = 1_u64
)]
fn keyed_matching_route(bencher: divan::Bencher<'_, '_>, size: usize) {
    let fixture = keyed_fixture(size);
    warm_keyed_fixture(&fixture);
    let id = u64::try_from(size / 2).expect("benchmark size fits u64");
    let mut iterations = 0;
    bencher.bench_local(|| {
        finish_task(&fixture.commit(KeyedBenchAction::Row(KeyedAction::new(
            divan::black_box(id),
            KeyedBenchChildAction::Hit,
        ))));
        iterations += 1;
    });
    let actual = futures_executor::block_on(fixture.store.read(move |state| {
        (
            state.rows.get(&id).copied(),
            state.outputs,
            state.rows.len(),
        )
    }))
    .expect("final keyed state");
    assert_eq!(actual, (Some(iterations), 0, size));
}

#[divan::bench(
    args = [10_usize, 1_000, 100_000],
    sample_count = 50,
    max_time = 0.5,
    items_count = 1_u64
)]
fn keyed_unrelated_reconciliation(bencher: divan::Bencher<'_, '_>, size: usize) {
    let fixture = keyed_fixture(size);
    warm_keyed_fixture(&fixture);
    bencher.bench_local(|| {
        finish_task(&fixture.commit(divan::black_box(KeyedBenchAction::Unrelated)));
    });
    let actual = futures_executor::block_on(fixture.store.read(|state| {
        (
            state.rows.values().copied().sum::<usize>(),
            state.outputs,
            state.rows.len(),
        )
    }))
    .expect("final keyed state");
    assert_eq!(actual, (0, 0, size));
}

#[divan::bench(
    args = [10_usize, 1_000, 100_000],
    sample_count = 50,
    max_time = 0.5,
    items_count = 2_u64
)]
fn keyed_two_output_id_mapping(bencher: divan::Bencher<'_, '_>, size: usize) {
    let fixture = keyed_fixture(size);
    warm_keyed_fixture(&fixture);
    let id = u64::try_from(size / 2).expect("benchmark size fits u64");
    let mut iterations = 0;
    bencher.bench_local(|| {
        finish_task(&fixture.commit(KeyedBenchAction::Row(KeyedAction::new(
            divan::black_box(id),
            KeyedBenchChildAction::EmitTwo,
        ))));
        iterations += 1;
    });
    let actual = futures_executor::block_on(
        fixture
            .store
            .read(move |state| (state.rows.get(&id).copied(), state.outputs)),
    )
    .expect("final mapped output state");
    assert_eq!(actual, (Some(0), iterations * 2));
}

/// Starts one pending keyed child task before timing removal of its insertion.
struct KeyedRemovalCancellationFixture {
    /// Owns the keyed collection and its routed reducer.
    fixture: Fixture<KeyedBenchState, KeyedBenchAction>,
    /// Tracks the removed child's causal work until cleanup is acknowledged.
    pending: ActionTask,
    /// Selects the middle row in the configured collection size.
    id: u64,
}

impl KeyedRemovalCancellationFixture {
    /// Warms lineage and commits pending work on the middle row.
    fn new(size: usize) -> Self {
        let fixture = keyed_fixture(size);
        warm_keyed_fixture(&fixture);
        let id = u64::try_from(size / 2).expect("benchmark size fits u64");
        let pending = fixture.commit(KeyedBenchAction::Row(KeyedAction::new(
            id,
            KeyedBenchChildAction::StartPending,
        )));
        Self {
            fixture,
            pending,
            id,
        }
    }

    /// Removes that insertion and awaits the removal action's causal completion.
    fn cancel(&self) {
        finish_task(&self.fixture.commit(KeyedBenchAction::RemoveOne(self.id)));
    }
}

impl Drop for KeyedRemovalCancellationFixture {
    fn drop(&mut self) {
        self.pending.cancel();
        finish_task(&self.pending);
    }
}

#[divan::bench(
    args = [10_usize, 1_000, 100_000],
    sample_count = 10,
    sample_size = 1,
    max_time = 0.5,
    skip_ext_time = true,
    items_count = 1_u64
)]
fn keyed_one_removal_cancellation(bencher: divan::Bencher<'_, '_>, size: usize) {
    bencher
        .with_inputs(|| KeyedRemovalCancellationFixture::new(size))
        .bench_local_values(|fixture| {
            fixture.cancel();
            fixture
        });
}

#[divan::bench(
    args = [1_000_usize, 100_000],
    sample_count = 10,
    sample_size = 1,
    max_time = 0.5,
    skip_ext_time = true,
    items_count = 1_000_u64
)]
fn keyed_remove_1_000_active_entries(bencher: divan::Bencher<'_, '_>, size: usize) {
    bencher
        .with_inputs(|| {
            let fixture = keyed_fixture(size);
            warm_keyed_fixture(&fixture);
            fixture
        })
        .bench_local_values(|fixture| {
            finish_task(&fixture.commit(KeyedBenchAction::RemoveFirst(1_000)));
            fixture
        });
}

#[derive(Clone)]
/// Gives each projected child a stable explicit identity and visible hit count.
struct ScopedBenchChild {
    /// Identifies optional children independently of the current value.
    id: u64,
    /// Changes only when the corresponding routed increment commits.
    value: usize,
}

/// Holds required, optional and keyed projections in one shared parent state.
struct ScopedBenchState {
    /// Supports the non-retiring required handle.
    required: ScopedBenchChild,
    /// Supports explicit-identity acquisition and removal-driven expiry.
    optional: Option<ScopedBenchChild>,
    /// Supports insertion-identity acquisition for the single keyed row.
    rows: KeyedState<u64, ScopedBenchChild>,
}

/// Supplies the same mutation through all three projected handle shapes.
enum ScopedBenchChildAction {
    Increment,
}

/// Embeds projected increments and the two parent-only publication/lifetime edits.
enum ScopedBenchAction {
    Required(ScopedBenchChildAction),
    Optional(ScopedBenchChildAction),
    Row(KeyedAction<u64, ScopedBenchChildAction>),
    ParentOnly,
    RemoveOptional,
}

/// Updates the selected child directly; scoped handles supply the public routing.
struct ScopedBenchReducer;

impl Reducer for ScopedBenchReducer {
    type State = ScopedBenchState;
    type Action = ScopedBenchAction;

    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &Self::Action,
        _context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        match action {
            ScopedBenchAction::Required(ScopedBenchChildAction::Increment) => {
                state.required.value = state.required.value.wrapping_add(1);
            }
            ScopedBenchAction::Optional(ScopedBenchChildAction::Increment) => {
                let child = state
                    .optional
                    .as_mut()
                    .expect("benchmark child remains live");
                child.value = child.value.wrapping_add(1);
            }
            ScopedBenchAction::Row(action) => {
                if let Some(child) = state.rows.get_mut(action.id()) {
                    child.value = child.value.wrapping_add(1);
                }
            }
            ScopedBenchAction::RemoveOptional => state.optional = None,
            ScopedBenchAction::ParentOnly => {}
        }
        Effect::none()
    }
}

/// Starts all three projected child shapes at zero with distinct fixture IDs.
fn scoped_bench_fixture() -> Fixture<ScopedBenchState, ScopedBenchAction> {
    Fixture::new(
        ScopedBenchState {
            required: ScopedBenchChild { id: 0, value: 0 },
            optional: Some(ScopedBenchChild { id: 1, value: 0 }),
            rows: [(2, ScopedBenchChild { id: 2, value: 0 })]
                .into_iter()
                .collect(),
        },
        ScopedBenchReducer,
    )
}

/// Projects the always-present child without installing a retiring identity.
fn required_scoped_handle(
    store: &Store<ScopedBenchState, ScopedBenchAction>,
) -> ScopedStore<ScopedBenchChild, ScopedBenchChildAction> {
    store.scope(
        StatePath::new(
            |state: &ScopedBenchState| &state.required,
            |state: &mut ScopedBenchState| &mut state.required,
        ),
        ActionPath::new(
            |action: &ScopedBenchAction| match action {
                ScopedBenchAction::Required(action) => Some(action),
                ScopedBenchAction::Optional(_)
                | ScopedBenchAction::Row(_)
                | ScopedBenchAction::ParentOnly
                | ScopedBenchAction::RemoveOptional => None,
            },
            ScopedBenchAction::Required,
        ),
    )
}

/// Registers the current optional identity before returning the child handle.
async fn optional_scoped_handle(
    store: &Store<ScopedBenchState, ScopedBenchAction>,
) -> ScopedStore<ScopedBenchChild, ScopedBenchChildAction> {
    store
        .scope_some_identified(
            OptionalStatePath::new(
                |state: &ScopedBenchState| state.optional.as_ref(),
                |state: &mut ScopedBenchState| state.optional.as_mut(),
            ),
            ActionPath::new(
                |action: &ScopedBenchAction| match action {
                    ScopedBenchAction::Optional(action) => Some(action),
                    ScopedBenchAction::Required(_)
                    | ScopedBenchAction::Row(_)
                    | ScopedBenchAction::ParentOnly
                    | ScopedBenchAction::RemoveOptional => None,
                },
                ScopedBenchAction::Optional,
            ),
            |child| &child.id,
        )
        .await
        .expect("optional benchmark child remains live")
}

/// Registers the sole keyed insertion through the public acquisition API.
async fn keyed_scoped_handle(
    store: &Store<ScopedBenchState, ScopedBenchAction>,
) -> ScopedStore<ScopedBenchChild, ScopedBenchChildAction> {
    store
        .scope_keyed(
            KeyedStatePath::new(
                |state: &ScopedBenchState| &state.rows,
                |state: &mut ScopedBenchState| &mut state.rows,
            ),
            KeyedActionPath::new(
                |action: &ScopedBenchAction| match action {
                    ScopedBenchAction::Row(action) => Some(action),
                    ScopedBenchAction::Required(_)
                    | ScopedBenchAction::Optional(_)
                    | ScopedBenchAction::ParentOnly
                    | ScopedBenchAction::RemoveOptional => None,
                },
                ScopedBenchAction::Row,
            ),
            2,
        )
        .await
        .expect("keyed benchmark child remains live")
}

#[divan::bench(sample_count = 50, max_time = 0.5, items_count = 1_u64)]
fn scoped_required_construction(bencher: divan::Bencher<'_, '_>) {
    let fixture = scoped_bench_fixture();
    bencher.bench_local(|| divan::black_box(required_scoped_handle(&fixture.store)));
}

#[divan::bench(sample_count = 50, max_time = 0.5, items_count = 1_u64)]
fn scoped_optional_construction(bencher: divan::Bencher<'_, '_>) {
    let fixture = scoped_bench_fixture();
    bencher.bench_local(|| {
        divan::black_box(futures_executor::block_on(optional_scoped_handle(
            &fixture.store,
        )))
    });
}

#[divan::bench(sample_count = 50, max_time = 0.5, items_count = 1_u64)]
fn scoped_keyed_construction(bencher: divan::Bencher<'_, '_>) {
    let fixture = scoped_bench_fixture();
    bencher.bench_local(|| {
        divan::black_box(futures_executor::block_on(keyed_scoped_handle(
            &fixture.store,
        )))
    });
}

#[divan::bench(sample_count = 50, max_time = 0.5, items_count = 1_u64)]
fn scoped_send(bencher: divan::Bencher<'_, '_>) {
    let fixture = scoped_bench_fixture();
    let handle = required_scoped_handle(&fixture.store);
    let mut iterations = 0;
    bencher.bench_local(|| {
        let task = futures_executor::block_on(
            handle.send(divan::black_box(ScopedBenchChildAction::Increment)),
        )
        .expect("scoped action commits");
        finish_task(&task);
        iterations += 1;
    });
    assert_eq!(
        futures_executor::block_on(handle.read(|child| child.value)).expect("final scoped state"),
        iterations
    );
}

/// Fixed fan-in shared by timing and allocation rows.
const CONCURRENT_SENDER_COUNT: usize = 32;

/// Owns a partial launch before any producer may enter the fixed-size barriers.
struct LaunchingSenders {
    /// Disconnection cancels workers if construction unwinds before full launch.
    admissions: Vec<std::sync::mpsc::Sender<()>>,
    /// Keeps every successful launch joinable until ownership transfers.
    workers: Vec<std::thread::JoinHandle<()>>,
}

impl LaunchingSenders {
    /// Admits the complete group and transfers its join handles to the fixture.
    fn release(mut self) -> Vec<std::thread::JoinHandle<()>> {
        for admission in self.admissions.drain(..) {
            admission
                .send(())
                .expect("waiting producer retains its admission receiver");
        }
        std::mem::take(&mut self.workers)
    }
}

impl Drop for LaunchingSenders {
    fn drop(&mut self) {
        self.admissions.clear();
        for worker in self.workers.drain(..) {
            // Constructor unwind must join every worker without hiding its cause.
            let _ = worker.join();
        }
    }
}

/// Creates producer threads outside timing, then releases one simultaneous batch.
struct ConcurrentSendFixture {
    /// Serialises all producers through one scoped Store path.
    fixture: Fixture<ScopedBenchState, ScopedBenchAction>,
    /// Releases the already-created producers exactly once.
    start: Arc<Barrier>,
    /// Rendezvous after launch, then acknowledges the completed measured batch.
    completed: Arc<Barrier>,
    /// Separates admission success from task completion and final state.
    accepted: Arc<AtomicUsize>,
    /// Counts successful causal-completion waits.
    finished: Arc<AtomicUsize>,
    /// Retains workers until their batch completes, including unused-input cleanup.
    senders: Vec<std::thread::JoinHandle<()>>,
    /// Records whether the caller has released the one-shot start barrier.
    started: Cell<bool>,
}

impl ConcurrentSendFixture {
    /// Starts 32 producers that remain behind the barrier until one batch runs.
    fn new() -> Self {
        Self::with_launch_check(|_, _, _| {})
    }

    /// Exposes completed launches to the constructor-failure regression only.
    fn with_launch_check(
        mut after_launch: impl FnMut(usize, &Arc<Barrier>, &Arc<Barrier>),
    ) -> Self {
        let fixture = scoped_bench_fixture();
        let handle = required_scoped_handle(&fixture.store);
        let start = Arc::new(Barrier::new(CONCURRENT_SENDER_COUNT + 1));
        let completed = Arc::new(Barrier::new(CONCURRENT_SENDER_COUNT + 1));
        let accepted = Arc::new(AtomicUsize::new(0));
        let finished = Arc::new(AtomicUsize::new(0));
        let mut launching = LaunchingSenders {
            admissions: Vec::with_capacity(CONCURRENT_SENDER_COUNT),
            workers: Vec::with_capacity(CONCURRENT_SENDER_COUNT),
        };

        for launched in 1..=CONCURRENT_SENDER_COUNT {
            let handle = handle.clone();
            let worker_start = Arc::clone(&start);
            let worker_completed = Arc::clone(&completed);
            let accepted = Arc::clone(&accepted);
            let finished = Arc::clone(&finished);
            let (admit, admitted) = std::sync::mpsc::channel();
            launching.admissions.push(admit);
            launching.workers.push(std::thread::spawn(move || {
                if admitted.recv().is_err() {
                    return;
                }
                drop(admitted);
                // Finish construction-only channel work before sample timing.
                worker_completed.wait();
                worker_start.wait();
                if let Ok(task) =
                    futures_executor::block_on(handle.send(ScopedBenchChildAction::Increment))
                {
                    accepted.fetch_add(1, Ordering::Relaxed);
                    finish_task(&task);
                    finished.fetch_add(1, Ordering::Release);
                }
                worker_completed.wait();
            }));
            after_launch(launched, &start, &completed);
        }

        let senders = launching.release();
        completed.wait();

        Self {
            fixture,
            start,
            completed,
            accepted,
            finished,
            senders,
            started: Cell::new(false),
        }
    }

    /// Releases the one-shot start barrier, then waits for all producers to finish.
    fn run(&self) {
        self.started.set(true);
        self.start.wait();
        self.completed.wait();
    }

    /// Requires all sends to commit, finish and mutate the projected child.
    fn verify(&self) {
        assert_eq!(
            self.accepted.load(Ordering::Acquire),
            CONCURRENT_SENDER_COUNT,
            "every concurrent scoped action is accepted"
        );
        assert_eq!(
            self.finished.load(Ordering::Acquire),
            CONCURRENT_SENDER_COUNT,
            "every concurrent scoped action task finishes"
        );
        assert_eq!(
            futures_executor::block_on(self.fixture.store.read(|state| state.required.value),)
                .expect("concurrent sender state remains readable"),
            CONCURRENT_SENDER_COUNT,
            "one Store reduces all scoped actions serially"
        );
    }
}

impl Drop for ConcurrentSendFixture {
    fn drop(&mut self) {
        if !self.started.get() {
            // Complete an unused input's workers before joining their handles.
            self.run();
        }
        while let Some(sender) = self.senders.pop() {
            sender.join().expect("concurrent sender thread completes");
        }
        self.verify();
    }
}

#[divan::bench(
    sample_count = 50,
    sample_size = 1,
    max_time = 0.5,
    skip_ext_time = true,
    items_count = 32_u64
)]
fn scoped_concurrent_senders_32(bencher: divan::Bencher<'_, '_>) {
    bencher
        .with_inputs(ConcurrentSendFixture::new)
        .bench_local_values(|fixture| {
            fixture.run();
            fixture
        });
}

#[divan::bench(sample_count = 50, max_time = 0.5, items_count = 1_u64)]
fn scoped_try_send(bencher: divan::Bencher<'_, '_>) {
    let fixture = scoped_bench_fixture();
    let handle = required_scoped_handle(&fixture.store);
    let mut iterations = 0;
    bencher.bench_local(|| {
        let receipt = handle
            .try_send(divan::black_box(ScopedBenchChildAction::Increment))
            .expect("benchmark mailbox remains available");
        let task = futures_executor::block_on(receipt.finish()).expect("scoped action commits");
        finish_task(&task);
        iterations += 1;
    });
    assert_eq!(
        futures_executor::block_on(handle.read(|child| child.value)).expect("final scoped state"),
        iterations
    );
}

#[derive(Clone)]
/// Holds the actor for capacity checks; it has no finite timer scheduler.
struct PausedDriver {
    /// Holds the actor unpolled while the bounded mailbox is filled.
    tasks: Arc<Mutex<Vec<RuntimeFuture>>>,
    /// Executes retained tasks only after explicit release.
    pool: ThreadPool,
    /// Supplies the runtime facade's common time origin.
    origin: Instant,
}

impl PausedDriver {
    /// Creates the pool but retains submitted tasks instead of polling them.
    fn new() -> Self {
        static POOL: OnceLock<ThreadPool> = OnceLock::new();
        Self {
            tasks: Arc::new(Mutex::new(Vec::new())),
            pool: POOL
                .get_or_init(|| ThreadPool::new().expect("paused benchmark thread pool"))
                .clone(),
            origin: Instant::now(),
        }
    }

    /// Submits the retained actor after the full-mailbox operation has been tested.
    fn start(&self) {
        let tasks = std::mem::take(
            &mut *self
                .tasks
                .lock()
                .expect("paused benchmark task lock remains available"),
        );
        for task in tasks {
            self.pool.spawn_ok(task);
        }
    }
}

impl fmt::Debug for PausedDriver {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PausedDriver").finish_non_exhaustive()
    }
}

impl RuntimeDriver for PausedDriver {
    fn spawn(&self, task: RuntimeFuture) {
        self.tasks
            .lock()
            .expect("paused benchmark task lock remains available")
            .push(task);
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
            "paused runtime does not support finite future deadlines"
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

/// Retains an admitted action while its actor is deliberately unpolled.
struct FullMailboxFixture {
    /// Owns the bounded queue and final shutdown.
    store: Store<ScopedBenchState, ScopedBenchAction>,
    /// Exercises scoped admission without bypassing public handle validation.
    handle: ScopedStore<ScopedBenchChild, ScopedBenchChildAction>,
    /// Is consumed once by recovery or cleanup after the actor starts.
    first: Option<ActionReceipt>,
    /// Releases the retained actor before any completion wait.
    driver: PausedDriver,
}

/// Fills a capacity-one mailbox while its actor remains unpolled.
fn full_mailbox_fixture() -> FullMailboxFixture {
    let driver = PausedDriver::new();
    let store = Store::builder(
        ScopedBenchState {
            required: ScopedBenchChild { id: 0, value: 0 },
            optional: Some(ScopedBenchChild { id: 1, value: 0 }),
            rows: KeyedState::new(),
        },
        ScopedBenchReducer,
        Runtime::new(driver.clone()),
    )
    .mailbox_capacity(1)
    .build()
    .expect("one-slot benchmark mailbox");
    let handle = required_scoped_handle(&store);
    let first = handle
        .try_send(ScopedBenchChildAction::Increment)
        .expect("first action fills paused mailbox");
    FullMailboxFixture {
        store,
        handle,
        first: Some(first),
        driver,
    }
}

impl Drop for FullMailboxFixture {
    fn drop(&mut self) {
        self.driver.start();
        if let Some(first) = self.first.take()
            && let Ok(task) = futures_executor::block_on(first.finish())
        {
            finish_task(&task);
        }
        let _ = futures_executor::block_on(self.store.shutdown());
    }
}

#[divan::bench(sample_count = 50, max_time = 0.5, items_count = 1_u64)]
fn scoped_try_send_full(bencher: divan::Bencher<'_, '_>) {
    let fixture = full_mailbox_fixture();
    bencher.bench_local(|| {
        let error = fixture
            .handle
            .try_send(divan::black_box(ScopedBenchChildAction::Increment))
            .expect_err("paused one-slot mailbox stays full");
        assert!(error.is_full());
        divan::black_box(error.into_action());
    });
}

#[divan::bench(
    sample_count = 50,
    sample_size = 1,
    max_time = 0.5,
    skip_ext_time = true,
    items_count = 1_u64
)]
fn scoped_try_send_recovery(bencher: divan::Bencher<'_, '_>) {
    bencher
        .with_inputs(full_mailbox_fixture)
        .bench_local_values(|mut fixture| {
            fixture.driver.start();
            let first = fixture.first.take().expect("one paused receipt");
            let first_task = futures_executor::block_on(first.finish())
                .expect("paused action commits after recovery");
            finish_task(&first_task);
            let receipt = fixture
                .handle
                .try_send(ScopedBenchChildAction::Increment)
                .expect("mailbox accepts after recovery");
            let task =
                futures_executor::block_on(receipt.finish()).expect("recovery action commits");
            finish_task(&task);
            fixture
        });
}

#[divan::bench(sample_count = 50, max_time = 0.5, items_count = 1_u64)]
fn scoped_read(bencher: divan::Bencher<'_, '_>) {
    let fixture = scoped_bench_fixture();
    let handle = required_scoped_handle(&fixture.store);
    bencher.bench_local(|| {
        divan::black_box(
            futures_executor::block_on(handle.read(|child| child.value))
                .expect("scoped read succeeds"),
        );
    });
}

/// Removes the known zero initial value before measuring later publications.
fn consume_initial(observation: &mut Observation<usize>) {
    assert_eq!(futures_executor::block_on(observation.next()), Some(0));
}

#[divan::bench(sample_count = 50, max_time = 0.5, items_count = 1_u64)]
fn scoped_observe_always(bencher: divan::Bencher<'_, '_>) {
    let fixture = scoped_bench_fixture();
    let handle = required_scoped_handle(&fixture.store);
    let mut observation = futures_executor::block_on(handle.observe(|child| child.value))
        .expect("observation registers");
    consume_initial(&mut observation);
    bencher.bench_local(|| {
        let task = fixture.commit(ScopedBenchAction::ParentOnly);
        finish_task(&task);
        let value = futures_executor::block_on(observation.next());
        assert_eq!(value, Some(0));
        divan::black_box(value);
    });
}

#[divan::bench(sample_count = 50, max_time = 0.5, items_count = 1_u64)]
fn scoped_observe_distinct_equal(bencher: divan::Bencher<'_, '_>) {
    let fixture = scoped_bench_fixture();
    let handle = required_scoped_handle(&fixture.store);
    let mut observation = futures_executor::block_on(handle.observe_distinct(|child| child.value))
        .expect("distinct observation registers");
    consume_initial(&mut observation);
    bencher.bench_local(|| finish_task(&fixture.commit(ScopedBenchAction::ParentOnly)));
    let mut next = std::pin::pin!(observation.next());
    let mut context = std::task::Context::from_waker(std::task::Waker::noop());
    assert!(next.as_mut().poll(&mut context).is_pending());
}

#[divan::bench(sample_count = 50, max_time = 0.5, items_count = 1_u64)]
fn scoped_observe_distinct_non_equal(bencher: divan::Bencher<'_, '_>) {
    let fixture = scoped_bench_fixture();
    let handle = required_scoped_handle(&fixture.store);
    let mut observation = futures_executor::block_on(handle.observe_distinct(|child| child.value))
        .expect("distinct observation registers");
    consume_initial(&mut observation);
    let mut expected = 0;
    bencher.bench_local(|| {
        let task = futures_executor::block_on(handle.send(ScopedBenchChildAction::Increment))
            .expect("scoped action commits");
        finish_task(&task);
        expected += 1;
        let value = futures_executor::block_on(observation.next());
        assert_eq!(value, Some(expected));
        divan::black_box(value);
    });
}

/// Keeps a registered observation alive while its optional child is retired.
struct InvalidationFixture {
    /// Owns the parent and completes structural removal before shutdown.
    fixture: Fixture<ScopedBenchState, ScopedBenchAction>,
    /// Keeps the expired public handle alive through the measured operation.
    _handle: ScopedStore<ScopedBenchChild, ScopedBenchChildAction>,
    /// Has consumed its initial value and must terminate after child removal.
    observation: Observation<usize>,
}

/// Registers the optional observation before timing invalidation.
fn invalidation_fixture() -> InvalidationFixture {
    let fixture = scoped_bench_fixture();
    let handle = futures_executor::block_on(optional_scoped_handle(&fixture.store));
    let mut observation = futures_executor::block_on(handle.observe(|child| child.value))
        .expect("observation registers");
    consume_initial(&mut observation);
    InvalidationFixture {
        fixture,
        _handle: handle,
        observation,
    }
}

#[divan::bench(
    sample_count = 50,
    sample_size = 1,
    max_time = 0.5,
    skip_ext_time = true,
    items_count = 1_u64
)]
fn scoped_invalidation(bencher: divan::Bencher<'_, '_>) {
    bencher
        .with_inputs(invalidation_fixture)
        .bench_local_values(|mut fixture| {
            finish_task(&fixture.fixture.commit(ScopedBenchAction::RemoveOptional));
            assert_eq!(futures_executor::block_on(fixture.observation.next()), None);
            fixture
        });
}

#[divan::bench(
    args = [1_usize, 100, 10_000],
    sample_count = 5,
    sample_size = 1,
    max_time = 0.5
)]
fn scoped_handle_construction(bencher: divan::Bencher<'_, '_>, count: usize) {
    let fixture = scoped_bench_fixture();
    bencher.counter(count).bench_local(|| {
        futures_executor::block_on(async {
            let mut handles = Vec::with_capacity(count);
            for _ in 0..count {
                handles.push(optional_scoped_handle(&fixture.store).await);
            }
            divan::black_box(handles)
        })
    });
}

#[divan::bench(
    args = [1_usize, 100, 10_000],
    sample_count = 5,
    sample_size = 1,
    max_time = 0.5
)]
fn scoped_observation_construction(bencher: divan::Bencher<'_, '_>, count: usize) {
    let fixture = scoped_bench_fixture();
    let handle = required_scoped_handle(&fixture.store);
    bencher.counter(count).bench_local(|| {
        futures_executor::block_on(async {
            let mut observations = Vec::with_capacity(count);
            for _ in 0..count {
                observations.push(
                    handle
                        .observe(|child| child.value)
                        .await
                        .expect("observation registers"),
                );
            }
            divan::black_box(observations)
        })
    });
}

/// Retains one required-child observation across a batch of parent-only commits.
struct ObservationCoalescingFixture {
    /// Owns the actor that replaces the observation's latest slot.
    fixture: Fixture<ScopedBenchState, ScopedBenchAction>,
    /// Keeps the required projection alive through the batch.
    _handle: ScopedStore<ScopedBenchChild, ScopedBenchChildAction>,
    /// Has no initial value left; one latest value remains after the batch.
    observation: Observation<usize>,
}

/// Excludes registration and initial-value consumption from coalescing timing.
fn observation_coalescing_fixture() -> ObservationCoalescingFixture {
    let fixture = scoped_bench_fixture();
    let handle = required_scoped_handle(&fixture.store);
    let mut observation = futures_executor::block_on(handle.observe(|child| child.value))
        .expect("observation registers");
    consume_initial(&mut observation);
    ObservationCoalescingFixture {
        fixture,
        _handle: handle,
        observation,
    }
}

#[divan::bench(
    args = [1_usize, 100, 10_000],
    sample_count = 5,
    sample_size = 1,
    max_time = 0.5
)]
fn scoped_observation_coalescing(bencher: divan::Bencher<'_, '_>, commits: usize) {
    bencher
        .counter(commits)
        .with_inputs(observation_coalescing_fixture)
        .bench_local_values(|mut fixture| {
            for _ in 0..commits {
                finish_task(&fixture.fixture.commit(ScopedBenchAction::ParentOnly));
            }
            assert_eq!(
                futures_executor::block_on(fixture.observation.next()),
                Some(0),
                "one retained latest slot remains readable"
            );
            fixture
        });
}

/// Provides an observable shared counter for dependency lookup at each depth.
struct DependencyBenchKey;

impl DependencyKey for DependencyBenchKey {
    type Value = Arc<AtomicUsize>;

    fn live_value() -> Self::Value {
        Arc::new(AtomicUsize::new(0))
    }
}

/// Performs one dependency lookup and one state increment per committed action.
struct DependencyBenchReducer;

impl Reducer for DependencyBenchReducer {
    type State = usize;
    type Action = ();

    fn reduce(
        &mut self,
        state: &mut Self::State,
        _action: &Self::Action,
        context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        context
            .dependency::<DependencyBenchKey>()
            .fetch_add(1, Ordering::Relaxed);
        *state = state.wrapping_add(1);
        Effect::none()
    }
}

/// Checks reduction cardinality after timing the configured dependency graph.
fn dependency_bench<R>(bencher: divan::Bencher<'_, '_>, reducer: R)
where
    R: Reducer<State = usize, Action = ()> + Send + 'static,
{
    let fixture = Fixture::new(0, reducer);
    let mut iterations = 0;
    bencher.bench_local(|| {
        finish_task(&fixture.commit(()));
        iterations += 1;
    });
    let actual = futures_executor::block_on(fixture.store.read(|state| *state))
        .expect("final dependency consumer state");
    assert_eq!(actual, iterations);
}

/// Adds one override scope with its own shared counter.
fn dependency_level<R>(child: R) -> impl Reducer<State = usize, Action = ()>
where
    R: Reducer<State = usize, Action = ()>,
{
    with_dependency::<DependencyBenchKey, _>(Arc::new(AtomicUsize::new(0)), child)
}

fn dependency_depth_4() -> impl Reducer<State = usize, Action = ()> {
    dependency_level(dependency_level(dependency_level(dependency_level(
        DependencyBenchReducer,
    ))))
}

fn dependency_depth_16() -> impl Reducer<State = usize, Action = ()> {
    dependency_level(dependency_level(dependency_level(dependency_level(
        dependency_level(dependency_level(dependency_level(dependency_level(
            dependency_level(dependency_level(dependency_level(dependency_level(
                dependency_level(dependency_level(dependency_level(dependency_level(
                    DependencyBenchReducer,
                )))),
            )))),
        )))),
    ))))
}

#[divan::bench(args = [0_usize, 1, 4, 16], sample_count = 50, max_time = 0.5)]
fn dependency_lookup(bencher: divan::Bencher<'_, '_>, depth: usize) {
    match depth {
        0 => dependency_bench(bencher, DependencyBenchReducer),
        1 => dependency_bench(bencher, dependency_level(DependencyBenchReducer)),
        4 => dependency_bench(bencher, dependency_depth_4()),
        16 => dependency_bench(bencher, dependency_depth_16()),
        _ => unreachable!("Divan supplies only supported dependency depths"),
    }
}

#[cfg(feature = "allocation-measurement")]
/// Snapshots cumulative work while returned output and the warmed fixture live.
///
/// The nonnegative retained-byte delta hides frees of prior storage. Peak bytes
/// cover the profiler's whole window, not an isolated operation-owned peak.
fn record_allocations<Output>(
    scenario: &str,
    samples: usize,
    logical_operations: usize,
    operation: impl FnOnce() -> Output,
) -> Output {
    let profiler = dhat::Profiler::builder().testing().build();
    let before = dhat::HeapStats::get();
    let output = divan::black_box(operation());
    let after = dhat::HeapStats::get();
    println!(
        "scenario={scenario} samples={samples} logical_operations={logical_operations} allocations={} allocated_bytes={} retained_bytes={} peak_live_bytes={}",
        after.total_blocks.saturating_sub(before.total_blocks),
        after.total_bytes.saturating_sub(before.total_bytes),
        after.curr_bytes.saturating_sub(before.curr_bytes),
        after.max_bytes,
    );
    drop(profiler);
    output
}

#[cfg(feature = "allocation-measurement")]
fn record_scoped_allocations<Output>(
    scenario: &str,
    samples: usize,
    operation: impl FnOnce() -> Output,
) -> Output {
    record_allocations(scenario, samples, samples, operation)
}

#[cfg(feature = "allocation-measurement")]
fn measure_required_construction(samples: usize) {
    let fixture = scoped_bench_fixture();
    record_scoped_allocations("scoped_required_construction", samples, || {
        (0..samples)
            .map(|_| required_scoped_handle(&fixture.store))
            .collect::<Vec<_>>()
    });
}

#[cfg(feature = "allocation-measurement")]
fn measure_optional_construction(samples: usize) {
    let fixture = scoped_bench_fixture();
    record_scoped_allocations("scoped_optional_construction", samples, || {
        futures_executor::block_on(async {
            let mut handles = Vec::with_capacity(samples);
            for _ in 0..samples {
                handles.push(optional_scoped_handle(&fixture.store).await);
            }
            handles
        })
    });
}

#[cfg(feature = "allocation-measurement")]
fn measure_keyed_construction(samples: usize) {
    let fixture = scoped_bench_fixture();
    record_scoped_allocations("scoped_keyed_construction", samples, || {
        futures_executor::block_on(async {
            let mut handles = Vec::with_capacity(samples);
            for _ in 0..samples {
                handles.push(keyed_scoped_handle(&fixture.store).await);
            }
            handles
        })
    });
}

#[cfg(feature = "allocation-measurement")]
fn measure_scoped_send(samples: usize) {
    let fixture = scoped_bench_fixture();
    let handle = required_scoped_handle(&fixture.store);
    record_scoped_allocations("scoped_send", samples, || {
        for _ in 0..samples {
            let task = futures_executor::block_on(handle.send(ScopedBenchChildAction::Increment))
                .expect("scoped action commits");
            finish_task(&task);
        }
    });
}

#[cfg(feature = "allocation-measurement")]
fn measure_scoped_try_send(samples: usize) {
    let fixture = scoped_bench_fixture();
    let handle = required_scoped_handle(&fixture.store);
    record_scoped_allocations("scoped_try_send", samples, || {
        for _ in 0..samples {
            let receipt = handle
                .try_send(ScopedBenchChildAction::Increment)
                .expect("benchmark mailbox remains available");
            let task = futures_executor::block_on(receipt.finish()).expect("scoped action commits");
            finish_task(&task);
        }
    });
}

#[cfg(feature = "allocation-measurement")]
fn measure_scoped_try_send_full(samples: usize) {
    let fixture = full_mailbox_fixture();
    record_scoped_allocations("scoped_try_send_full", samples, || {
        (0..samples)
            .map(|_| {
                fixture
                    .handle
                    .try_send(ScopedBenchChildAction::Increment)
                    .expect_err("paused one-slot mailbox stays full")
                    .into_action()
            })
            .collect::<Vec<_>>()
    });
}

#[cfg(feature = "allocation-measurement")]
fn measure_scoped_try_send_recovery(samples: usize) {
    assert_eq!(samples, 1, "recovery measurement consumes one fixture");
    let mut fixture = full_mailbox_fixture();
    record_scoped_allocations("scoped_try_send_recovery", samples, || {
        fixture.driver.start();
        let first = fixture.first.take().expect("one paused receipt");
        let first_task = futures_executor::block_on(first.finish())
            .expect("paused action commits after recovery");
        finish_task(&first_task);
        let receipt = fixture
            .handle
            .try_send(ScopedBenchChildAction::Increment)
            .expect("mailbox accepts after recovery");
        let task = futures_executor::block_on(receipt.finish()).expect("recovery action commits");
        finish_task(&task);
    });
}

#[cfg(feature = "allocation-measurement")]
fn measure_scoped_read(samples: usize) {
    let fixture = scoped_bench_fixture();
    let handle = required_scoped_handle(&fixture.store);
    record_scoped_allocations("scoped_read", samples, || {
        for _ in 0..samples {
            divan::black_box(
                futures_executor::block_on(handle.read(|child| child.value))
                    .expect("scoped read succeeds"),
            );
        }
    });
}

#[cfg(feature = "allocation-measurement")]
fn measure_observe_always(samples: usize) {
    let fixture = scoped_bench_fixture();
    let handle = required_scoped_handle(&fixture.store);
    let mut observation = futures_executor::block_on(handle.observe(|child| child.value))
        .expect("observation registers");
    consume_initial(&mut observation);
    record_scoped_allocations("scoped_observe_always", samples, || {
        for _ in 0..samples {
            finish_task(&fixture.commit(ScopedBenchAction::ParentOnly));
            divan::black_box(futures_executor::block_on(observation.next()));
        }
    });
}

#[cfg(feature = "allocation-measurement")]
fn measure_observe_distinct_equal(samples: usize) {
    let fixture = scoped_bench_fixture();
    let handle = required_scoped_handle(&fixture.store);
    let mut observation = futures_executor::block_on(handle.observe_distinct(|child| child.value))
        .expect("distinct observation registers");
    consume_initial(&mut observation);
    record_scoped_allocations("scoped_observe_distinct_equal", samples, || {
        for _ in 0..samples {
            finish_task(&fixture.commit(ScopedBenchAction::ParentOnly));
        }
        observation
    });
}

#[cfg(feature = "allocation-measurement")]
fn measure_observe_distinct_non_equal(samples: usize) {
    let fixture = scoped_bench_fixture();
    let handle = required_scoped_handle(&fixture.store);
    let mut observation = futures_executor::block_on(handle.observe_distinct(|child| child.value))
        .expect("distinct observation registers");
    consume_initial(&mut observation);
    record_scoped_allocations("scoped_observe_distinct_non_equal", samples, || {
        for _ in 0..samples {
            let task = futures_executor::block_on(handle.send(ScopedBenchChildAction::Increment))
                .expect("scoped action commits");
            finish_task(&task);
            divan::black_box(futures_executor::block_on(observation.next()));
        }
        observation
    });
}

#[cfg(feature = "allocation-measurement")]
fn measure_invalidation(samples: usize) {
    assert_eq!(samples, 1, "invalidation measurement consumes one fixture");
    let mut fixture = invalidation_fixture();
    record_scoped_allocations("scoped_invalidation", samples, || {
        finish_task(&fixture.fixture.commit(ScopedBenchAction::RemoveOptional));
        assert_eq!(futures_executor::block_on(fixture.observation.next()), None);
    });
}

#[cfg(feature = "allocation-measurement")]
fn measure_observation_construction(samples: usize) {
    let fixture = scoped_bench_fixture();
    let handle = required_scoped_handle(&fixture.store);
    record_scoped_allocations("scoped_observation_construction", samples, || {
        futures_executor::block_on(async {
            let mut observations = Vec::with_capacity(samples);
            for _ in 0..samples {
                observations.push(
                    handle
                        .observe(|child| child.value)
                        .await
                        .expect("observation registers"),
                );
            }
            observations
        })
    });
}

#[cfg(feature = "allocation-measurement")]
fn measure_observation_coalescing(samples: usize) {
    let fixture = scoped_bench_fixture();
    let handle = required_scoped_handle(&fixture.store);
    let mut observation = futures_executor::block_on(handle.observe(|child| child.value))
        .expect("observation registers");
    consume_initial(&mut observation);
    record_scoped_allocations("scoped_observation_coalescing", samples, || {
        for _ in 0..samples {
            finish_task(&fixture.commit(ScopedBenchAction::ParentOnly));
        }
        observation
    });
}

#[cfg(feature = "allocation-measurement")]
fn measure_dependency_allocations<R>(scenario: &str, samples: usize, reducer: R)
where
    R: Reducer<State = usize, Action = ()> + Send + 'static,
{
    let fixture = Fixture::new(0, reducer);
    record_scoped_allocations(scenario, samples, || {
        for _ in 0..samples {
            finish_task(&fixture.commit(()));
        }
    });
}

#[cfg(feature = "allocation-measurement")]
fn measure_effect_map_allocations(scenario: &str, samples: usize, depth: usize, outputs: usize) {
    let fixture = Fixture::new(
        EffectMapState {
            outputs: 0,
            checksum: 0,
        },
        EffectMapReducer { depth, outputs },
    );
    finish_task(&fixture.commit(EffectMapAction::Start));
    let before =
        futures_executor::block_on(fixture.store.read(|state| (state.outputs, state.checksum)))
            .expect("effect-map allocation state remains readable");
    let logical_operations = samples
        .checked_mul(outputs)
        .expect("effect-map logical operation count fits usize");
    record_allocations(scenario, samples, logical_operations, || {
        for _ in 0..samples {
            finish_task(&fixture.commit(EffectMapAction::Start));
        }
    });
    let after =
        futures_executor::block_on(fixture.store.read(|state| (state.outputs, state.checksum)))
            .expect("effect-map allocation state remains readable");
    assert_eq!(
        after.0 - before.0,
        logical_operations,
        "every mapped output is reduced"
    );
    assert_eq!(
        after.1.wrapping_sub(before.1),
        effect_map_batch_checksum(depth, outputs)
            .wrapping_mul(u64::try_from(samples).expect("sample count fits u64")),
        "every allocation sample traverses the configured map chain"
    );
}

#[cfg(feature = "allocation-measurement")]
fn measure_keyed_initial_lineage(scenario: &str, samples: usize, size: usize) {
    assert_eq!(
        samples, 1,
        "initial lineage measurement consumes one fixture"
    );
    let fixture = keyed_fixture(size);
    record_allocations(scenario, samples, size, || warm_keyed_fixture(&fixture));
    assert_eq!(
        futures_executor::block_on(fixture.store.read(|state| state.rows.len()))
            .expect("keyed allocation state remains readable"),
        size,
        "initial reconciliation retains every keyed row"
    );
}

#[cfg(feature = "allocation-measurement")]
fn measure_keyed_matching_route(scenario: &str, samples: usize, size: usize) {
    let fixture = keyed_fixture(size);
    warm_keyed_fixture(&fixture);
    let id = u64::try_from(size / 2).expect("benchmark size fits u64");
    let before = futures_executor::block_on(fixture.store.read(move |state| state.rows[&id]))
        .expect("keyed allocation state remains readable");
    record_allocations(scenario, samples, samples, || {
        for _ in 0..samples {
            finish_task(&fixture.commit(KeyedBenchAction::Row(KeyedAction::new(
                id,
                KeyedBenchChildAction::Hit,
            ))));
        }
    });
    let after = futures_executor::block_on(fixture.store.read(move |state| state.rows[&id]))
        .expect("keyed allocation state remains readable");
    assert_eq!(after - before, samples, "every keyed action is reduced");
}

#[cfg(feature = "allocation-measurement")]
fn measure_keyed_unrelated(scenario: &str, samples: usize, size: usize) {
    let fixture = keyed_fixture(size);
    warm_keyed_fixture(&fixture);
    record_allocations(scenario, samples, samples, || {
        for _ in 0..samples {
            finish_task(&fixture.commit(KeyedBenchAction::Unrelated));
        }
    });
}

#[cfg(feature = "allocation-measurement")]
fn measure_keyed_two_output(scenario: &str, samples: usize, size: usize) {
    let fixture = keyed_fixture(size);
    warm_keyed_fixture(&fixture);
    let id = u64::try_from(size / 2).expect("benchmark size fits u64");
    let before = futures_executor::block_on(fixture.store.read(|state| state.outputs))
        .expect("keyed allocation state remains readable");
    let logical_operations = samples
        .checked_mul(2)
        .expect("keyed output count fits usize");
    record_allocations(scenario, samples, logical_operations, || {
        for _ in 0..samples {
            finish_task(&fixture.commit(KeyedBenchAction::Row(KeyedAction::new(
                id,
                KeyedBenchChildAction::EmitTwo,
            ))));
        }
    });
    let after = futures_executor::block_on(fixture.store.read(|state| state.outputs))
        .expect("keyed allocation state remains readable");
    assert_eq!(
        after - before,
        logical_operations,
        "every keyed effect output is reduced"
    );
}

#[cfg(feature = "allocation-measurement")]
fn measure_keyed_removal_cancellation(scenario: &str, samples: usize, size: usize) {
    assert_eq!(samples, 1, "removal measurement consumes one fixture");
    let fixture = KeyedRemovalCancellationFixture::new(size);
    record_allocations(scenario, samples, samples, || fixture.cancel());
    let removed_id = fixture.id;
    assert!(
        futures_executor::block_on(
            fixture
                .fixture
                .store
                .read(move |state| state.rows.get(&removed_id).is_none())
        )
        .expect("keyed allocation state remains readable"),
        "removed keyed row remains absent"
    );
}

#[cfg(feature = "allocation-measurement")]
fn measure_keyed_bulk_removal(scenario: &str, samples: usize, size: usize) {
    assert_eq!(samples, 1, "bulk removal measurement consumes one fixture");
    let fixture = keyed_fixture(size);
    warm_keyed_fixture(&fixture);
    record_allocations(scenario, samples, 1_000, || {
        finish_task(&fixture.commit(KeyedBenchAction::RemoveFirst(1_000)));
    });
    assert_eq!(
        futures_executor::block_on(fixture.store.read(|state| state.rows.len()))
            .expect("keyed allocation state remains readable"),
        size - 1_000,
        "bulk removal deletes the requested keyed rows"
    );
}

#[cfg(feature = "allocation-measurement")]
fn measure_descendant_task_creation(scenario: &str, samples: usize, task_count: usize) {
    assert_eq!(samples, 1, "task creation measurement consumes one fixture");
    let fixture = Fixture::new(
        LifecycleState {
            child: Some(LifecycleChildState { hits: 0 }),
        },
        descendant_reducer(),
    );
    finish_task(&fixture.commit(LifecycleAction::Child(LifecycleChildAction::Hit)));
    let start_task = record_allocations(scenario, samples, task_count, || {
        fixture.commit(LifecycleAction::Child(LifecycleChildAction::StartTasks(
            task_count,
        )))
    });
    finish_task(&fixture.commit(LifecycleAction::Leave));
    finish_task(&start_task);
}

#[cfg(feature = "allocation-measurement")]
fn measure_descendant_cancellation(scenario: &str, samples: usize, task_count: usize) {
    assert_eq!(samples, 1, "cancellation measurement consumes one fixture");
    let fixture = CancellationFixture::new(task_count);
    record_allocations(scenario, samples, task_count, || fixture.cancel());
}

#[cfg(feature = "allocation-measurement")]
fn measure_concurrent_senders(samples: usize) {
    assert_eq!(
        samples, 1,
        "concurrent measurement consumes one sender batch"
    );
    let fixture = ConcurrentSendFixture::new();
    record_allocations(
        "scoped_concurrent_senders_32",
        samples,
        CONCURRENT_SENDER_COUNT,
        || fixture.run(),
    );
}

#[cfg(feature = "allocation-measurement")]
#[expect(
    clippy::too_many_lines,
    reason = "one exhaustive match keeps stable allocation scenario names auditable"
)]
fn run_allocation_scenario(scenario: &str, samples: usize) {
    match scenario {
        "effect_map_depth_1_outputs_1" => {
            measure_effect_map_allocations(scenario, samples, 1, 1);
        }
        "effect_map_depth_4_outputs_1" => {
            measure_effect_map_allocations(scenario, samples, 4, 1);
        }
        "effect_map_depth_16_outputs_1" => {
            measure_effect_map_allocations(scenario, samples, 16, 1);
        }
        "effect_map_depth_4_outputs_16" => {
            measure_effect_map_allocations(scenario, samples, 4, 16);
        }
        "effect_map_depth_4_outputs_256" => {
            measure_effect_map_allocations(scenario, samples, 4, 256);
        }
        "keyed_initial_lineage_10" => measure_keyed_initial_lineage(scenario, samples, 10),
        "keyed_initial_lineage_1000" => measure_keyed_initial_lineage(scenario, samples, 1_000),
        "keyed_initial_lineage_100000" => {
            measure_keyed_initial_lineage(scenario, samples, 100_000);
        }
        "keyed_matching_route_10" => measure_keyed_matching_route(scenario, samples, 10),
        "keyed_matching_route_1000" => measure_keyed_matching_route(scenario, samples, 1_000),
        "keyed_matching_route_100000" => {
            measure_keyed_matching_route(scenario, samples, 100_000);
        }
        "keyed_unrelated_reconciliation_10" => measure_keyed_unrelated(scenario, samples, 10),
        "keyed_unrelated_reconciliation_1000" => {
            measure_keyed_unrelated(scenario, samples, 1_000);
        }
        "keyed_unrelated_reconciliation_100000" => {
            measure_keyed_unrelated(scenario, samples, 100_000);
        }
        "keyed_two_output_id_mapping_10" => measure_keyed_two_output(scenario, samples, 10),
        "keyed_two_output_id_mapping_1000" => {
            measure_keyed_two_output(scenario, samples, 1_000);
        }
        "keyed_two_output_id_mapping_100000" => {
            measure_keyed_two_output(scenario, samples, 100_000);
        }
        "keyed_one_removal_cancellation_10" => {
            measure_keyed_removal_cancellation(scenario, samples, 10);
        }
        "keyed_one_removal_cancellation_1000" => {
            measure_keyed_removal_cancellation(scenario, samples, 1_000);
        }
        "keyed_one_removal_cancellation_100000" => {
            measure_keyed_removal_cancellation(scenario, samples, 100_000);
        }
        "keyed_remove_1000_active_entries_1000" => {
            measure_keyed_bulk_removal(scenario, samples, 1_000);
        }
        "keyed_remove_1000_active_entries_100000" => {
            measure_keyed_bulk_removal(scenario, samples, 100_000);
        }
        "descendant_task_creation_1" => {
            measure_descendant_task_creation(scenario, samples, 1);
        }
        "descendant_task_creation_100" => {
            measure_descendant_task_creation(scenario, samples, 100);
        }
        "descendant_task_creation_10000" => {
            measure_descendant_task_creation(scenario, samples, 10_000);
        }
        "descendant_cancellation_1" => {
            measure_descendant_cancellation(scenario, samples, 1);
        }
        "descendant_cancellation_100" => {
            measure_descendant_cancellation(scenario, samples, 100);
        }
        "descendant_cancellation_10000" => {
            measure_descendant_cancellation(scenario, samples, 10_000);
        }
        "scoped_concurrent_senders_32" => measure_concurrent_senders(samples),
        "scoped_required_construction" => measure_required_construction(samples),
        "scoped_optional_construction" | "scoped_handle_construction" => {
            measure_optional_construction(samples);
        }
        "scoped_keyed_construction" => measure_keyed_construction(samples),
        "scoped_send" => measure_scoped_send(samples),
        "scoped_try_send" => measure_scoped_try_send(samples),
        "scoped_try_send_full" => measure_scoped_try_send_full(samples),
        "scoped_try_send_recovery" => measure_scoped_try_send_recovery(samples),
        "scoped_read" => measure_scoped_read(samples),
        "scoped_observe_always" => measure_observe_always(samples),
        "scoped_observe_distinct_equal" => measure_observe_distinct_equal(samples),
        "scoped_observe_distinct_non_equal" => measure_observe_distinct_non_equal(samples),
        "scoped_invalidation" => measure_invalidation(samples),
        "scoped_observation_construction" => measure_observation_construction(samples),
        "scoped_observation_coalescing" => measure_observation_coalescing(samples),
        "dependency_lookup_0" => {
            measure_dependency_allocations(scenario, samples, DependencyBenchReducer);
        }
        "dependency_lookup_1" => {
            measure_dependency_allocations(
                scenario,
                samples,
                dependency_level(DependencyBenchReducer),
            );
        }
        "dependency_lookup_4" => {
            measure_dependency_allocations(scenario, samples, dependency_depth_4());
        }
        "dependency_lookup_16" => {
            measure_dependency_allocations(scenario, samples, dependency_depth_16());
        }
        _ => panic!("unknown composition allocation scenario: {scenario}"),
    }
}
