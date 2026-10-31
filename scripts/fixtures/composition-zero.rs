//! Measures the same composition reductions against either product revision.
//!
//! The controller stages this file beside `composition-zero-runner.rs`, copied
//! from the current synchronous allocation runner. A real Store supplies its
//! private reduction context; admission, actor polling and shutdown remain
//! outside each allocation window. No library implementation is overlaid.

use std::fmt;
use std::hint::black_box;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

use rca_core::{
    ActionPath, Effect, MonotonicInstant, ReduceContext, Reducer, Runtime, RuntimeDriver,
    RuntimeFuture, StatePath, Store, combine, scope,
};

/// Counts allocations throughout this single-threaded process.
#[global_allocator]
static ALLOCATOR: dhat::Alloc = dhat::Alloc;

/// Retains the exact names consumed by the composition evidence evaluator.
const CASES: &[(&str, fn())] = &[
    (
        "composition::scope::allocation_tests::scope_none_paths_do_not_allocate_after_warm_up",
        scope_none_paths_do_not_allocate_after_warm_up,
    ),
    (
        "composition::combine::tests::combine_all_none_widths_do_not_allocate",
        combine_all_none_widths_do_not_allocate,
    ),
];

include!("composition-zero-runner.rs");

/// Runs exactly one selected case without creating a libtest receiver thread.
fn main() -> std::process::ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = Options::parse(&args).and_then(|options| {
        if options.filters.len() != 1
            || CASES.iter().filter(|case| options.matches(case.0)).count() != 1
            || options.list
        {
            return Err("composition zero host requires exactly one executable filter".to_owned());
        }
        let child = std::env::var_os("RCA_ALLOCATION_TEST_CHILD");
        run(&args, false, child.as_deref(), false)
    });
    match result {
        Ok(true) => std::process::ExitCode::SUCCESS,
        Ok(false) => std::process::ExitCode::FAILURE,
        Err(error) => {
            eprintln!("{error}");
            std::process::ExitCode::FAILURE
        }
    }
}

/// Captures the single actor so the calling thread owns every poll.
#[derive(Clone)]
struct CapturedRuntime {
    /// Holds the actor until the host takes it, without starting another thread.
    actor: Arc<Mutex<Option<RuntimeFuture>>>,
}

impl fmt::Debug for CapturedRuntime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CapturedRuntime").finish_non_exhaustive()
    }
}

impl RuntimeDriver for CapturedRuntime {
    fn spawn(&self, task: RuntimeFuture) {
        let mut actor = self.actor.lock().expect("captured actor lock");
        assert!(actor.is_none(), "the zero-effect host owns only one actor");
        *actor = Some(task);
    }

    fn now(&self) -> MonotonicInstant {
        MonotonicInstant::ZERO
    }

    fn sleep_until(&self, deadline: MonotonicInstant) -> RuntimeFuture {
        assert_eq!(deadline, MonotonicInstant::MAX, "no timed work is expected");
        Box::pin(std::future::pending())
    }

    fn yield_now(&self) -> RuntimeFuture {
        let mut yielded = false;
        Box::pin(std::future::poll_fn(move |context| {
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

/// Calls one measurement from a real public reducer boundary.
struct Measurement {
    /// Receives the context owned by the Store, without constructing internals.
    run: fn(&mut ReduceContext<'_>),
    /// Proves the measurement was entered exactly once.
    entered: Arc<AtomicBool>,
}

impl Reducer for Measurement {
    type State = ();
    type Action = ();

    fn reduce(
        &mut self,
        _state: &mut (),
        _action: &(),
        context: &mut ReduceContext<'_>,
    ) -> Effect<()> {
        assert!(!self.entered.swap(true, Ordering::Relaxed));
        (self.run)(context);
        Effect::none()
    }
}

/// Obtains a context through the same public APIs on both frozen revisions.
fn with_context(measure: fn(&mut ReduceContext<'_>)) {
    let driver = CapturedRuntime {
        actor: Arc::new(Mutex::new(None)),
    };
    let entered = Arc::new(AtomicBool::new(false));
    let store = Store::new(
        (),
        Measurement {
            run: measure,
            entered: Arc::clone(&entered),
        },
        Runtime::new(driver.clone()),
    );
    let mut actor = driver
        .actor
        .lock()
        .expect("captured actor lock")
        .take()
        .expect("Store actor");
    let receipt = store.try_send(()).expect("measurement action is admitted");
    let mut context = Context::from_waker(Waker::noop());
    assert!(actor.as_mut().poll(&mut context).is_pending());
    assert!(
        entered.load(Ordering::Relaxed),
        "measurement action was reduced"
    );
    let task = futures_executor::block_on(receipt.finish()).expect("measurement action commits");
    futures_executor::block_on(task.finish());
    let ((), result) =
        futures_executor::block_on(futures_util::future::join(actor, store.shutdown()));
    result.expect("measurement Store shuts down");
    assert!(driver.actor.lock().expect("captured actor lock").is_none());
}

/// Preserves the original none-effect oracle through the shared public surface.
///
/// Neither revision exposes an `is_none` query. Both render an empty effect as
/// `Effect::None`. Formatting happens after the measured allocation snapshot.
fn assert_none<Action>(effect: &Effect<Action>) {
    assert_eq!(format!("{effect:?}"), "Effect::None");
}

/// Counts leaf calls for the direct and nested scope workloads.
struct State {
    /// Changes only when every required scope matches the action.
    calls: usize,
}

/// Selects the deepest matching scope.
struct Action {
    /// A value one below a scope depth misses at its deepest level.
    depth: usize,
}

/// Performs the original no-effect leaf mutation.
struct Leaf;

impl Reducer for Leaf {
    type State = State;
    type Action = Action;

    fn reduce(
        &mut self,
        state: &mut State,
        _action: &Action,
        _context: &mut ReduceContext<'_>,
    ) -> Effect<Action> {
        state.calls += 1;
        Effect::none()
    }
}

/// Projects the shared state without depending on the child reducer type.
fn level_state_path() -> StatePath<
    State,
    State,
    impl for<'a> Fn(&'a State) -> &'a State + Copy,
    impl for<'a> Fn(&'a mut State) -> &'a mut State + Copy,
> {
    StatePath::new(|state: &State| state, |state: &mut State| state)
}

/// Filters actions without depending on the child reducer type.
fn level_action_path(
    required_depth: usize,
) -> ActionPath<
    Action,
    Action,
    impl for<'a> Fn(&'a Action) -> Option<&'a Action> + Copy,
    impl Fn(Action) -> Action + Copy,
> {
    ActionPath::new(
        move |action: &Action| (action.depth >= required_depth).then_some(action),
        |action| action,
    )
}

/// Adds one borrowed state/action path without changing the leaf workload.
fn level<Child>(required_depth: usize, child: Child) -> impl Reducer<State = State, Action = Action>
where
    Child: Reducer<State = State, Action = Action>,
{
    scope(level_state_path(), level_action_path(required_depth), child)
}

/// Builds the original sixteen nested, borrowed scopes.
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

/// Measures only one already-warmed reduction, retaining its result until snapshot.
fn assert_no_allocation<R>(
    label: &str,
    reducer: &mut R,
    state: &mut State,
    depth: usize,
    expected_calls: usize,
    context: &mut ReduceContext<'_>,
) where
    R: Reducer<State = State, Action = Action>,
{
    let calls = state.calls;
    let before = dhat::HeapStats::get();
    let effect = black_box(reducer.reduce(state, &Action { depth }, context));
    let after = dhat::HeapStats::get();
    assert_eq!(after.total_blocks, before.total_blocks, "{label} blocks");
    assert_eq!(after.total_bytes, before.total_bytes, "{label} bytes");
    assert_eq!(state.calls - calls, expected_calls, "{label} leaf calls");
    assert_none(&effect);
}

/// Checks direct and depth-one/four/sixteen matching and nonmatching reductions.
fn scope_none_paths_do_not_allocate_after_warm_up() {
    with_context(|context| {
        let mut direct = Leaf;
        let mut depth_1 = level(1, Leaf);
        let mut depth_4 = level(1, level(2, level(3, level(4, Leaf))));
        let mut depth_16 = depth_16();
        let mut state = State { calls: 0 };

        // Keep repeated child ancestry out of compiler-generated callback types.
        // This compiler-specific check stays outside the allocation measurement.
        let one_name_len = std::any::type_name_of_val(&depth_1).len();
        let four_name_len = std::any::type_name_of_val(&depth_4).len();
        assert!(
            four_name_len <= 4 * one_name_len,
            "scope callback types must grow linearly: {one_name_len} -> {four_name_len}"
        );

        assert_none(&black_box(direct.reduce(
            &mut state,
            &Action { depth: 1 },
            context,
        )));
        assert_none(&black_box(depth_1.reduce(
            &mut state,
            &Action { depth: 1 },
            context,
        )));
        assert_none(&black_box(depth_1.reduce(
            &mut state,
            &Action { depth: 0 },
            context,
        )));
        assert_none(&black_box(depth_4.reduce(
            &mut state,
            &Action { depth: 4 },
            context,
        )));
        assert_none(&black_box(depth_4.reduce(
            &mut state,
            &Action { depth: 3 },
            context,
        )));
        assert_none(&black_box(depth_16.reduce(
            &mut state,
            &Action { depth: 16 },
            context,
        )));
        assert_none(&black_box(depth_16.reduce(
            &mut state,
            &Action { depth: 15 },
            context,
        )));
        assert_eq!(state.calls, 4, "warm-up reaches exactly four leaves");

        let profiler = dhat::Profiler::builder().testing().build();
        assert_no_allocation("direct", &mut direct, &mut state, 1, 1, context);
        assert_no_allocation("matching depth 1", &mut depth_1, &mut state, 1, 1, context);
        assert_no_allocation(
            "nonmatching depth 1",
            &mut depth_1,
            &mut state,
            0,
            0,
            context,
        );
        assert_no_allocation("matching depth 4", &mut depth_4, &mut state, 4, 1, context);
        assert_no_allocation(
            "nonmatching depth 4",
            &mut depth_4,
            &mut state,
            3,
            0,
            context,
        );
        assert_no_allocation(
            "matching depth 16",
            &mut depth_16,
            &mut state,
            16,
            1,
            context,
        );
        assert_no_allocation(
            "nonmatching depth 16",
            &mut depth_16,
            &mut state,
            15,
            0,
            context,
        );
        drop(profiler);
    });
}

/// Preserves the original empty member of every combined tuple.
struct NoneReducer;

impl Reducer for NoneReducer {
    type State = ();
    type Action = ();

    fn reduce(
        &mut self,
        _state: &mut (),
        _action: &(),
        _context: &mut ReduceContext<'_>,
    ) -> Effect<()> {
        Effect::none()
    }
}

/// Expands the original tuple widths without introducing dynamic dispatch.
macro_rules! for_each_combine_width {
    ($assertion:ident, $context:ident) => {
        for_each_combine_width!(@next $assertion, $context; [NoneReducer]; 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12);
    };
    (@next $assertion:ident, $context:ident; [$($reducers:expr),+]; $width:literal $(, $remaining:literal)*) => {
        $assertion!($width, ($($reducers,)+), $context);
        for_each_combine_width!(@next $assertion, $context; [$($reducers,)+ NoneReducer]; $($remaining),*);
    };
    (@next $assertion:ident, $context:ident; [$($reducers:expr),+];) => {};
}

/// Warms each combined tuple before profiling begins.
macro_rules! warm_combine_width {
    ($width:literal, $reducers:expr, $context:ident) => {{
        let mut reducer = combine($reducers);
        assert_none(&black_box(reducer.reduce(&mut (), &(), $context)));
    }};
}

/// Measures reduction but not construction, public rendering or owner destruction.
macro_rules! measure_combine_width {
    ($width:literal, $reducers:expr, $context:ident) => {{
        let mut reducer = combine($reducers);
        let before = dhat::HeapStats::get();
        let effect = black_box(reducer.reduce(&mut (), &(), $context));
        let after = dhat::HeapStats::get();
        assert_eq!(
            after.total_blocks, before.total_blocks,
            "Combine width {} blocks",
            $width
        );
        assert_eq!(
            after.total_bytes, before.total_bytes,
            "Combine width {} bytes",
            $width
        );
        assert_none(&effect);
    }};
}

/// Checks all twelve supported all-empty tuple widths.
fn combine_all_none_widths_do_not_allocate() {
    with_context(|context| {
        for_each_combine_width!(warm_combine_width, context);
        let profiler = dhat::Profiler::builder().testing().build();
        for_each_combine_width!(measure_combine_width, context);
        drop(profiler);
    });
}
