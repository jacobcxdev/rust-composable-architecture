//! Checks child lifetimes, terminal Store resources, and test-host teardown.

#![expect(
    clippy::duplicate_mod,
    reason = "independently reusable test hosts each own the same private clock module"
)]

mod support;
#[path = "support/thread_runtime.rs"]
mod thread_runtime;

use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::task::Poll;
use std::thread::JoinHandle;

use futures_util::future::{AbortHandle, Abortable};
use rca_core::{
    ActionPath, ActionTask, CaseStatePath, Effect, EffectSender, MissingChildPolicy,
    MonotonicInstant, Observation, OptionalStatePath, ReduceContext, Reducer, ReducerExt, Runtime,
    RuntimeDriver, RuntimeFuture, StatePath, Store, combine, if_case, if_case_identified, if_some,
    if_some_identified, scope, scope_identified,
};

use support::{ManualRuntime, immediate_clock};
use thread_runtime::ThreadRuntime;

#[expect(
    dead_code,
    reason = "only bounded capture is needed from the shared allocation child support"
)]
mod subprocess {
    include!("../src/allocation_test/child.rs");

    /// Captures a direct child with bounded output and kill/reap on timeout.
    pub(super) fn output(command: &mut Command) -> Output {
        capture(command, CHILD_TIMEOUT).expect("lifecycle panic child must complete")
    }
}

#[derive(Clone, Copy, Debug)]
enum Action {
    Start,
    Block,
    Crash,
    Probe,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PanicMode {
    None,
    Reducer,
    Effect,
}

struct LifecycleReducer {
    mode: PanicMode,
    block: Option<Arc<BlockGate>>,
}

impl LifecycleReducer {
    fn new(mode: PanicMode) -> Self {
        Self { mode, block: None }
    }

    fn blocking(gate: Arc<BlockGate>) -> Self {
        Self {
            mode: PanicMode::None,
            block: Some(gate),
        }
    }
}

impl Reducer for LifecycleReducer {
    type State = usize;
    type Action = Action;

    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &Self::Action,
        _context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        match action {
            Action::Start => {
                *state += 1;
                let pending = Effect::run(|_| std::future::pending());
                if self.mode == PanicMode::Effect {
                    Effect::merge([
                        pending,
                        Effect::run(|_| async { panic!("intentional effect panic") }),
                    ])
                } else {
                    pending
                }
            }
            Action::Block => {
                let gate = self.block.as_ref().expect("blocking reducer needs gate");
                gate.enter();
                *state += 1;
                Effect::run(|_| std::future::pending())
            }
            Action::Crash if self.mode == PanicMode::Reducer => {
                panic!("intentional reducer panic")
            }
            Action::Crash | Action::Probe => {
                *state += 1;
                Effect::none()
            }
        }
    }
}

/// Holds a reduction until the test releases it, without consuming idle CPU.
#[derive(Default)]
struct BlockGate {
    /// Protects both signal predicates against missed condition notifications.
    state: Mutex<BlockState>,
    /// Wakes the test after entry and the reducer after release.
    changed: Condvar,
}

/// Records entry separately from permission to leave the blocked reduction.
#[derive(Default)]
struct BlockState {
    /// Confirms that the reducer reached the controlled blocking point.
    entered: bool,
    /// Allows a waiting reducer to complete exactly when the owner releases it.
    open: bool,
}

impl BlockGate {
    /// Signals entry and waits for the owner's release.
    fn enter(&self) {
        let mut state = self.state.lock().expect("block gate lock");
        state.entered = true;
        self.changed.notify_all();
        drop(
            self.changed
                .wait_while(state, |state| !state.open)
                .expect("block gate lock"),
        );
    }

    /// Waits for actual reducer entry, not merely action admission.
    fn wait_until_entered(&self) {
        let state = self.state.lock().expect("block gate lock");
        drop(
            self.changed
                .wait_while(state, |state| !state.entered)
                .expect("block gate lock"),
        );
    }

    /// Releases a current or future waiter.
    fn open(&self) {
        self.state.lock().expect("block gate lock").open = true;
        self.changed.notify_all();
    }
}

/// Releases the blocked reducer before the host joins during test unwinding.
struct OpenGateOnDrop(Arc<BlockGate>);

impl Drop for OpenGateOnDrop {
    fn drop(&mut self) {
        self.0.open();
    }
}

#[test]
fn graceful_shutdown_invalidates_all_resources_before_ack() {
    let runtime = ThreadRuntime::new();
    let store = Store::new(0, LifecycleReducer::new(PanicMode::None), runtime.runtime());

    futures_executor::block_on(async {
        let mut observation = store.observe(|state| *state).await.expect("registration");
        assert_eq!(observation.next().await, Some(0));
        let task = store.send(Action::Start).await.expect("start commit");

        store.shutdown().await.expect("graceful shutdown");
        assert_terminal_resources(&store, &mut observation, &task).await;
    });
    runtime.join_all();
}

#[test]
fn final_owner_drop_discards_queued_actions() {
    let runtime = ThreadRuntime::new();
    let gate = Arc::new(BlockGate::default());
    let open_gate = OpenGateOnDrop(Arc::clone(&gate));
    let store = Store::new(
        0,
        LifecycleReducer::blocking(Arc::clone(&gate)),
        runtime.runtime(),
    );
    let mut observation =
        futures_executor::block_on(store.observe(|state| *state)).expect("registration");
    assert_eq!(futures_executor::block_on(observation.next()), Some(0));

    let active = store.try_send(Action::Block).expect("blocking action fits");
    let queued = store.try_send(Action::Probe).expect("queued action fits");
    gate.wait_until_entered();
    let closed = store.closed();
    drop(store);
    drop(open_gate);

    let task = futures_executor::block_on(active.finish())
        .expect("in-flight reduction commits atomically");
    futures_executor::block_on(queued.finish()).expect_err("queued action must be discarded");
    futures_executor::block_on(closed);
    runtime.join_all();
    assert!(task.is_cancelled());
    futures_executor::block_on(task.finish());
    assert!(task.is_finished());
    assert_eq!(futures_executor::block_on(observation.next()), None);
}

#[test]
fn block_gate_releases_waiters_when_the_test_unwinds() {
    let gate = Arc::new(BlockGate::default());
    let worker_gate = Arc::clone(&gate);
    let (finished, receive_finish) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        worker_gate.enter();
        finished.send(()).unwrap();
    });
    gate.wait_until_entered();

    let unwind = std::panic::catch_unwind(|| {
        let _open_gate = OpenGateOnDrop(Arc::clone(&gate));
        panic!("intentional test-body panic");
    });
    let panic = unwind.expect_err("the test body deliberately panics");
    assert_eq!(
        panic.downcast_ref::<&str>(),
        Some(&"intentional test-body panic")
    );
    receive_finish
        .recv_timeout(std::time::Duration::from_secs(5))
        .expect("unwinding must release the reducer before host teardown");
    worker.join().expect("released worker completes cleanly");
}

#[derive(Default)]
struct ScopedState {
    first: (),
    second: (),
}

#[derive(Clone, Copy)]
enum ScopedAction {
    First(ScopedChildAction),
    Second(ScopedChildAction),
    Root(ScopedChildAction),
}

#[derive(Clone, Copy)]
enum ScopedChildAction {
    Start,
    Cancel,
}

struct ScopedChild;

impl Reducer for ScopedChild {
    type State = ();
    type Action = ScopedChildAction;

    fn reduce(
        &mut self,
        _state: &mut Self::State,
        action: &Self::Action,
        _context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        match action {
            ScopedChildAction::Start => Effect::run(|_| std::future::pending()).cancellable(7_u8),
            ScopedChildAction::Cancel => Effect::cancel(7_u8),
        }
    }
}

struct ScopedRoot;

impl Reducer for ScopedRoot {
    type State = ScopedState;
    type Action = ScopedAction;

    fn reduce(
        &mut self,
        _state: &mut Self::State,
        action: &Self::Action,
        _context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        let ScopedAction::Root(action) = action else {
            return Effect::none();
        };
        match action {
            ScopedChildAction::Start => Effect::run(|_| std::future::pending()).cancellable(7_u8),
            ScopedChildAction::Cancel => Effect::cancel(7_u8),
        }
    }
}

fn scoped_reducer() -> impl Reducer<State = ScopedState, Action = ScopedAction> {
    let first = scope(
        StatePath::new(
            |state: &ScopedState| &state.first,
            |state: &mut ScopedState| &mut state.first,
        ),
        ActionPath::new(
            |action: &ScopedAction| match action {
                ScopedAction::First(action) => Some(action),
                ScopedAction::Second(_) | ScopedAction::Root(_) => None,
            },
            ScopedAction::First,
        ),
        ScopedChild,
    );
    let second = scope(
        StatePath::new(
            |state: &ScopedState| &state.second,
            |state: &mut ScopedState| &mut state.second,
        ),
        ActionPath::new(
            |action: &ScopedAction| match action {
                ScopedAction::Second(action) => Some(action),
                ScopedAction::First(_) | ScopedAction::Root(_) => None,
            },
            ScopedAction::Second,
        ),
        ScopedChild,
    );
    combine((first, second, ScopedRoot))
}

#[test]
fn registry_equal_cancellation_ids_are_isolated_by_scope() {
    let runtime = ThreadRuntime::new();
    let store = Store::new(ScopedState::default(), scoped_reducer(), runtime.runtime());

    futures_executor::block_on(async {
        let first = store
            .send(ScopedAction::First(ScopedChildAction::Start))
            .await
            .expect("first start commits");
        let second = store
            .send(ScopedAction::Second(ScopedChildAction::Start))
            .await
            .expect("second start commits");
        let root = store
            .send(ScopedAction::Root(ScopedChildAction::Start))
            .await
            .expect("root start commits");

        let _ = store
            .send(ScopedAction::First(ScopedChildAction::Cancel))
            .await
            .expect("first cancellation commits");
        first.finish().await;
        assert!(first.is_finished());
        assert!(!second.is_finished());
        assert!(!root.is_finished());

        let _ = store
            .send(ScopedAction::Second(ScopedChildAction::Cancel))
            .await
            .expect("second cancellation commits");
        second.finish().await;
        assert!(second.is_finished());
        assert!(!root.is_finished());

        let _ = store
            .send(ScopedAction::Root(ScopedChildAction::Cancel))
            .await
            .expect("root cancellation commits");
        root.finish().await;
        assert!(root.is_finished());
        store.shutdown().await.expect("shutdown succeeds");
    });
    runtime.join_all();
}

#[derive(Default)]
struct OptionalChildState {
    close_calls: usize,
    effect_started: Arc<AtomicBool>,
}

#[derive(Default)]
struct OptionalState {
    child: Option<OptionalChildState>,
    child_close_calls: usize,
}

enum OptionalChildAction {
    Close,
    Start,
}

enum OptionalAction {
    Child(OptionalChildAction),
    Insert,
    Replace,
    Remove,
}

struct OptionalChild;

impl Reducer for OptionalChild {
    type State = OptionalChildState;
    type Action = OptionalChildAction;

    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &Self::Action,
        _context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        match action {
            OptionalChildAction::Close => {
                state.close_calls += 1;
                let effect_started = Arc::clone(&state.effect_started);
                Effect::run(move |_| async move {
                    effect_started.store(true, Ordering::Release);
                })
            }
            OptionalChildAction::Start => Effect::run(|_| std::future::pending()),
        }
    }
}

struct OptionalParent;

impl Reducer for OptionalParent {
    type State = OptionalState;
    type Action = OptionalAction;

    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &Self::Action,
        _context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        match action {
            OptionalAction::Child(OptionalChildAction::Close) => {
                state.child_close_calls = state
                    .child
                    .as_ref()
                    .expect("child must reduce before parent removal")
                    .close_calls;
                state.child = None;
            }
            OptionalAction::Child(OptionalChildAction::Start) => {}
            OptionalAction::Insert | OptionalAction::Replace => {
                state.child = Some(OptionalChildState::default());
            }
            OptionalAction::Remove => state.child = None,
        }
        Effect::none()
    }
}

fn optional_reducer()
-> impl Reducer<State = OptionalState, Action = OptionalAction> + std::fmt::Debug {
    optional_reducer_with_policy(MissingChildPolicy::ReportAndBase)
}

fn optional_reducer_with_policy(
    policy: MissingChildPolicy,
) -> impl Reducer<State = OptionalState, Action = OptionalAction> + std::fmt::Debug {
    if_some(
        OptionalParent,
        OptionalStatePath::new(
            |state: &OptionalState| state.child.as_ref(),
            |state: &mut OptionalState| state.child.as_mut(),
        ),
        ActionPath::new(
            |action: &OptionalAction| match action {
                OptionalAction::Child(action) => Some(action),
                OptionalAction::Insert | OptionalAction::Replace | OptionalAction::Remove => None,
            },
            OptionalAction::Child,
        ),
        OptionalChild,
    )
    .missing_child_policy(policy)
}

#[test]
fn if_some_reduces_child_before_parent_removes_state() {
    let runtime = ManualRuntime::new();
    let effect_started = Arc::new(AtomicBool::new(false));
    let reducer = optional_reducer();
    assert_eq!(format!("{reducer:?}"), "IfSome { .. }");
    let store = Store::new(
        OptionalState {
            child: Some(OptionalChildState {
                close_calls: 0,
                effect_started: Arc::clone(&effect_started),
            }),
            child_close_calls: 0,
        },
        reducer,
        runtime.runtime(),
    );
    runtime.start_all();

    futures_executor::block_on(async {
        store
            .send(OptionalAction::Child(OptionalChildAction::Close))
            .await
            .expect("close commits")
            .finish()
            .await;
        assert_eq!(
            store
                .read(|state| (state.child_close_calls, state.child.is_none()))
                .await
                .expect("optional state read"),
            (1, true)
        );
        assert_eq!(runtime.spawn_count(), 1);
        runtime.start_all();
        assert!(!effect_started.load(Ordering::Acquire));
        store.shutdown().await.expect("optional Store shutdown");
    });
}

#[test]
fn if_some_enters_without_replay_and_preserves_present_lifetime() {
    let runtime = ManualRuntime::new();
    let store = Store::new(
        OptionalState::default(),
        optional_reducer(),
        runtime.runtime(),
    );
    runtime.start_all();

    futures_executor::block_on(async {
        store
            .send(OptionalAction::Insert)
            .await
            .expect("optional insertion commits")
            .finish()
            .await;
        assert_eq!(
            store
                .read(|state| {
                    (
                        state.child.as_ref().map(|child| child.close_calls),
                        state.child_close_calls,
                    )
                })
                .await
                .expect("inserted child read"),
            (Some(0), 0)
        );

        let task = store
            .send(OptionalAction::Child(OptionalChildAction::Start))
            .await
            .expect("present child starts");
        store
            .read(|_| ())
            .await
            .expect("effect launches are queued");
        runtime.start_all();
        store
            .send(OptionalAction::Replace)
            .await
            .expect("presence-preserving replacement commits")
            .finish()
            .await;
        assert!(!task.is_finished());

        store
            .send(OptionalAction::Remove)
            .await
            .expect("optional removal commits")
            .finish()
            .await;
        task.finish().await;
        assert!(task.is_finished());
        store.shutdown().await.expect("optional Store shutdown");
    });
}

#[test]
fn if_some_direct_missing_child_action_fails_store_with_type_context() {
    let runtime = ManualRuntime::new();
    let store = Store::new(
        OptionalState::default(),
        optional_reducer_with_policy(MissingChildPolicy::FailFast),
        runtime.runtime(),
    );
    runtime.start_all();

    futures_executor::block_on(store.send(OptionalAction::Child(OptionalChildAction::Close)))
        .expect_err("direct missing-child action must fail the Store");
    futures_executor::block_on(store.closed());
    assert!(store.is_closed());

    let panic = runtime
        .join_all()
        .into_iter()
        .find_map(Result::err)
        .expect("missing-child panic must escape the actor");
    let diagnostic = panic
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| panic.downcast_ref::<&'static str>().copied())
        .expect("panic diagnostic must be text");
    assert!(
        diagnostic.starts_with("IfSome received child action "),
        "{diagnostic}"
    );
    assert!(diagnostic.contains(std::any::type_name::<OptionalState>()));
    assert!(diagnostic.contains(std::any::type_name::<OptionalChildState>()));
    assert!(diagnostic.contains(std::any::type_name::<OptionalAction>()));
    assert!(diagnostic.contains(std::any::type_name::<OptionalChildAction>()));
}

#[derive(Default)]
struct TreeBranchState {
    nested: Option<()>,
}

#[derive(Default)]
struct TreeState {
    left: Option<TreeBranchState>,
    right: Option<TreeBranchState>,
}

enum TreeLeafAction {
    Start,
}

enum TreeBranchAction {
    Nested(TreeLeafAction),
}

enum TreeAction {
    Left(TreeBranchAction),
    Right(TreeBranchAction),
    RemoveLeft,
    RemoveRight,
}

struct TreeLeaf;

impl Reducer for TreeLeaf {
    type State = ();
    type Action = TreeLeafAction;

    fn reduce(
        &mut self,
        _state: &mut Self::State,
        action: &Self::Action,
        _context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        match action {
            TreeLeafAction::Start => Effect::run(|_| std::future::pending()).cancellable(7_u8),
        }
    }
}

struct TreeBranch;

impl Reducer for TreeBranch {
    type State = TreeBranchState;
    type Action = TreeBranchAction;

    fn reduce(
        &mut self,
        _state: &mut Self::State,
        _action: &Self::Action,
        _context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        Effect::none()
    }
}

fn tree_branch_reducer() -> impl Reducer<State = TreeBranchState, Action = TreeBranchAction> {
    TreeBranch.if_some(
        OptionalStatePath::new(
            |state: &TreeBranchState| state.nested.as_ref(),
            |state: &mut TreeBranchState| state.nested.as_mut(),
        ),
        ActionPath::new(
            |action: &TreeBranchAction| match action {
                TreeBranchAction::Nested(action) => Some(action),
            },
            TreeBranchAction::Nested,
        ),
        TreeLeaf,
    )
}

struct TreeRoot;

impl Reducer for TreeRoot {
    type State = TreeState;
    type Action = TreeAction;

    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &Self::Action,
        _context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        match action {
            TreeAction::RemoveLeft => state.left = None,
            TreeAction::RemoveRight => state.right = None,
            TreeAction::Left(_) | TreeAction::Right(_) => {}
        }
        Effect::none()
    }
}

fn tree_reducer() -> impl Reducer<State = TreeState, Action = TreeAction> {
    TreeRoot
        .if_some(
            OptionalStatePath::new(
                |state: &TreeState| state.left.as_ref(),
                |state: &mut TreeState| state.left.as_mut(),
            ),
            ActionPath::new(
                |action: &TreeAction| match action {
                    TreeAction::Left(action) => Some(action),
                    TreeAction::Right(_) | TreeAction::RemoveLeft | TreeAction::RemoveRight => None,
                },
                TreeAction::Left,
            ),
            tree_branch_reducer(),
        )
        .if_some(
            OptionalStatePath::new(
                |state: &TreeState| state.right.as_ref(),
                |state: &mut TreeState| state.right.as_mut(),
            ),
            ActionPath::new(
                |action: &TreeAction| match action {
                    TreeAction::Right(action) => Some(action),
                    TreeAction::Left(_) | TreeAction::RemoveLeft | TreeAction::RemoveRight => None,
                },
                TreeAction::Right,
            ),
            tree_branch_reducer(),
        )
}

#[test]
fn if_some_removal_cancels_nested_descendants_not_equal_id_sibling() {
    let runtime = ManualRuntime::new();
    let store = Store::new(
        TreeState {
            left: Some(TreeBranchState { nested: Some(()) }),
            right: Some(TreeBranchState { nested: Some(()) }),
        },
        tree_reducer(),
        runtime.runtime(),
    );
    runtime.start_all();

    futures_executor::block_on(async {
        let left = store
            .send(TreeAction::Left(TreeBranchAction::Nested(
                TreeLeafAction::Start,
            )))
            .await
            .expect("left descendant starts");
        let right = store
            .send(TreeAction::Right(TreeBranchAction::Nested(
                TreeLeafAction::Start,
            )))
            .await
            .expect("right descendant starts");
        store
            .read(|_| ())
            .await
            .expect("effect launches are queued");
        runtime.start_all();

        store
            .send(TreeAction::RemoveLeft)
            .await
            .expect("left subtree removal commits")
            .finish()
            .await;
        left.finish().await;
        assert!(left.is_finished());
        assert!(!right.is_finished());

        store
            .send(TreeAction::RemoveRight)
            .await
            .expect("right subtree removal commits")
            .finish()
            .await;
        right.finish().await;
        assert!(right.is_finished());
        store.shutdown().await.expect("tree Store shutdown");
    });
}

struct IdentifiedChildState {
    id: u8,
}

struct IdentifiedState {
    child: Option<IdentifiedChildState>,
}

enum IdentifiedChildAction {
    Start,
}

enum IdentifiedAction {
    Child(IdentifiedChildAction),
    Replace(u8),
}

struct IdentifiedChild;

impl Reducer for IdentifiedChild {
    type State = IdentifiedChildState;
    type Action = IdentifiedChildAction;

    fn reduce(
        &mut self,
        _state: &mut Self::State,
        action: &Self::Action,
        _context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        match action {
            IdentifiedChildAction::Start => {
                Effect::run(|_| std::future::pending()).cancellable(7_u8)
            }
        }
    }
}

struct IdentifiedParent;

impl Reducer for IdentifiedParent {
    type State = IdentifiedState;
    type Action = IdentifiedAction;

    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &Self::Action,
        _context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        match action {
            IdentifiedAction::Replace(id) => {
                state.child = Some(IdentifiedChildState { id: *id });
            }
            IdentifiedAction::Child(_) => {}
        }
        Effect::none()
    }
}

fn identified_optional_reducer() -> impl Reducer<State = IdentifiedState, Action = IdentifiedAction>
{
    identified_optional_reducer_with_policy(MissingChildPolicy::ReportAndBase)
}

fn identified_optional_reducer_with_policy(
    policy: MissingChildPolicy,
) -> impl Reducer<State = IdentifiedState, Action = IdentifiedAction> {
    if_some_identified(
        IdentifiedParent,
        OptionalStatePath::new(
            |state: &IdentifiedState| state.child.as_ref(),
            |state: &mut IdentifiedState| state.child.as_mut(),
        ),
        ActionPath::new(
            |action: &IdentifiedAction| match action {
                IdentifiedAction::Child(action) => Some(action),
                IdentifiedAction::Replace(_) => None,
            },
            IdentifiedAction::Child,
        ),
        |state: &IdentifiedChildState| &state.id,
        IdentifiedChild,
    )
    .missing_child_policy(policy)
}

#[test]
fn if_some_identified_replaces_only_changed_identity() {
    let runtime = ManualRuntime::new();
    let store = Store::new(
        IdentifiedState {
            child: Some(IdentifiedChildState { id: 1 }),
        },
        identified_optional_reducer(),
        runtime.runtime(),
    );
    runtime.start_all();

    futures_executor::block_on(async {
        let task = store
            .send(IdentifiedAction::Child(IdentifiedChildAction::Start))
            .await
            .expect("identified child starts");
        runtime.wait_for_queued_task();
        runtime.start_all();
        store
            .send(IdentifiedAction::Replace(1))
            .await
            .expect("equal identity replacement commits")
            .finish()
            .await;
        assert!(!task.is_finished());

        store
            .send(IdentifiedAction::Replace(2))
            .await
            .expect("changed identity replacement commits")
            .finish()
            .await;
        task.finish().await;
        assert!(task.is_finished());
        store.shutdown().await.expect("identified Store shutdown");
    });
}

#[test]
fn if_some_identified_missing_child_diagnostic_names_its_wrapper_and_types() {
    let runtime = ManualRuntime::new();
    let store = Store::new(
        IdentifiedState { child: None },
        identified_optional_reducer_with_policy(MissingChildPolicy::FailFast),
        runtime.runtime(),
    );
    runtime.start_all();

    futures_executor::block_on(store.send(IdentifiedAction::Child(IdentifiedChildAction::Start)))
        .expect_err("direct missing identified child action must fail the Store");
    futures_executor::block_on(store.closed());

    let diagnostic = panic_diagnostic(&runtime);
    assert!(
        diagnostic.starts_with("IfSomeIdentified received child action "),
        "{diagnostic}"
    );
    assert!(diagnostic.contains(std::any::type_name::<IdentifiedState>()));
    assert!(diagnostic.contains(std::any::type_name::<IdentifiedChildState>()));
    assert!(diagnostic.contains(std::any::type_name::<IdentifiedAction>()));
    assert!(diagnostic.contains(std::any::type_name::<IdentifiedChildAction>()));
}

struct DetailState {
    id: u8,
    close_calls: usize,
}

struct SettingsState;

enum ScreenState {
    List,
    Detail(DetailState),
    Settings(SettingsState),
}

struct CaseState {
    screen: ScreenState,
    sibling: (),
    detail_close_calls: usize,
}

enum DetailAction {
    Close,
    Start,
}

enum CaseSiblingAction {
    Start,
}

enum CaseAction {
    Detail(DetailAction),
    Sibling(CaseSiblingAction),
    ShowDetail(u8),
    ShowSettings,
}

struct DetailChild;

impl Reducer for DetailChild {
    type State = DetailState;
    type Action = DetailAction;

    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &Self::Action,
        _context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        match action {
            DetailAction::Close => {
                state.close_calls += 1;
                Effect::none()
            }
            DetailAction::Start => Effect::run(|_| std::future::pending()).cancellable(11_u8),
        }
    }
}

struct CaseSibling;

impl Reducer for CaseSibling {
    type State = ();
    type Action = CaseSiblingAction;

    fn reduce(
        &mut self,
        _state: &mut Self::State,
        action: &Self::Action,
        _context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        match action {
            CaseSiblingAction::Start => Effect::run(|_| std::future::pending()).cancellable(11_u8),
        }
    }
}

struct CaseParent;

impl Reducer for CaseParent {
    type State = CaseState;
    type Action = CaseAction;

    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &Self::Action,
        _context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        match action {
            CaseAction::Detail(DetailAction::Close) => {
                let ScreenState::Detail(detail) = &state.screen else {
                    panic!("detail child must reduce before parent case switch");
                };
                state.detail_close_calls = detail.close_calls;
                state.screen = ScreenState::List;
            }
            CaseAction::Detail(DetailAction::Start) | CaseAction::Sibling(_) => {}
            CaseAction::ShowDetail(id) => {
                state.screen = ScreenState::Detail(DetailState {
                    id: *id,
                    close_calls: 0,
                });
            }
            CaseAction::ShowSettings => state.screen = ScreenState::Settings(SettingsState),
        }
        Effect::none()
    }
}

type DetailCasePath = CaseStatePath<
    CaseState,
    DetailState,
    fn(&CaseState) -> Option<&DetailState>,
    fn(&mut CaseState) -> Option<&mut DetailState>,
>;

type DetailActionPath = ActionPath<
    CaseAction,
    DetailAction,
    fn(&CaseAction) -> Option<&DetailAction>,
    fn(DetailAction) -> CaseAction,
>;

fn detail_state_path() -> DetailCasePath {
    CaseStatePath::new(
        (|state: &CaseState| match &state.screen {
            ScreenState::Detail(detail) => Some(detail),
            ScreenState::List | ScreenState::Settings(_) => None,
        }) as fn(&CaseState) -> Option<&DetailState>,
        (|state: &mut CaseState| match &mut state.screen {
            ScreenState::Detail(detail) => Some(detail),
            ScreenState::List | ScreenState::Settings(_) => None,
        }) as fn(&mut CaseState) -> Option<&mut DetailState>,
    )
}

fn detail_action_path() -> DetailActionPath {
    ActionPath::new(
        (|action: &CaseAction| match action {
            CaseAction::Detail(detail) => Some(detail),
            CaseAction::Sibling(_) | CaseAction::ShowDetail(_) | CaseAction::ShowSettings => None,
        }) as fn(&CaseAction) -> Option<&DetailAction>,
        CaseAction::Detail as fn(DetailAction) -> CaseAction,
    )
}

fn case_parent() -> impl Reducer<State = CaseState, Action = CaseAction> {
    combine((
        scope(
            StatePath::new(
                |state: &CaseState| &state.sibling,
                |state: &mut CaseState| &mut state.sibling,
            ),
            ActionPath::new(
                |action: &CaseAction| match action {
                    CaseAction::Sibling(action) => Some(action),
                    CaseAction::Detail(_)
                    | CaseAction::ShowDetail(_)
                    | CaseAction::ShowSettings => None,
                },
                CaseAction::Sibling,
            ),
            CaseSibling,
        ),
        CaseParent,
    ))
}

fn case_reducer() -> impl Reducer<State = CaseState, Action = CaseAction> + std::fmt::Debug {
    if_case(
        case_parent(),
        detail_state_path(),
        detail_action_path(),
        DetailChild,
    )
}

fn case_identified_reducer() -> impl Reducer<State = CaseState, Action = CaseAction> {
    if_case_identified(
        case_parent(),
        detail_state_path(),
        detail_action_path(),
        |state: &DetailState| &state.id,
        DetailChild,
    )
}

fn detail_state(id: u8) -> CaseState {
    CaseState {
        screen: ScreenState::Detail(DetailState { id, close_calls: 0 }),
        sibling: (),
        detail_close_calls: 0,
    }
}

#[test]
fn if_case_switches_after_child_reduction_and_cancels_only_departed_case() {
    let runtime = ManualRuntime::new();
    let reducer = case_reducer();
    assert_eq!(format!("{reducer:?}"), "IfCase { .. }");
    let store = Store::new(detail_state(1), reducer, runtime.runtime());
    runtime.start_all();

    futures_executor::block_on(async {
        let detail = store
            .send(CaseAction::Detail(DetailAction::Start))
            .await
            .expect("detail task starts");
        let sibling = store
            .send(CaseAction::Sibling(CaseSiblingAction::Start))
            .await
            .expect("sibling task starts");
        store
            .read(|_| ())
            .await
            .expect("effect launches are queued");
        runtime.start_all();

        store
            .send(CaseAction::Detail(DetailAction::Close))
            .await
            .expect("detail close commits")
            .finish()
            .await;
        detail.finish().await;
        assert!(detail.is_finished());
        assert!(!sibling.is_finished());
        assert_eq!(
            store
                .read(|state| (
                    state.detail_close_calls,
                    matches!(state.screen, ScreenState::List)
                ))
                .await
                .expect("case state read"),
            (1, true)
        );

        store.shutdown().await.expect("case Store shutdown");
        sibling.finish().await;
    });
}

#[test]
fn if_case_enters_without_replay_and_default_presence_preserves_same_case() {
    let runtime = ManualRuntime::new();
    let store = Store::new(
        CaseState {
            screen: ScreenState::List,
            sibling: (),
            detail_close_calls: 0,
        },
        case_reducer(),
        runtime.runtime(),
    );
    runtime.start_all();

    futures_executor::block_on(async {
        store
            .send(CaseAction::ShowDetail(1))
            .await
            .expect("detail case enters")
            .finish()
            .await;
        assert_eq!(
            store
                .read(|state| match &state.screen {
                    ScreenState::Detail(detail) => Some(detail.close_calls),
                    ScreenState::List | ScreenState::Settings(_) => None,
                })
                .await
                .expect("entered case read"),
            Some(0)
        );

        let task = store
            .send(CaseAction::Detail(DetailAction::Start))
            .await
            .expect("detail task starts");
        store
            .read(|_| ())
            .await
            .expect("effect launches are queued");
        runtime.start_all();
        store
            .send(CaseAction::ShowDetail(2))
            .await
            .expect("same case replacement commits")
            .finish()
            .await;
        assert!(!task.is_finished());
        store
            .send(CaseAction::ShowSettings)
            .await
            .expect("case switch commits")
            .finish()
            .await;
        task.finish().await;
        assert!(task.is_finished());
        store.shutdown().await.expect("case Store shutdown");
    });
}

#[test]
fn if_case_identified_replaces_only_changed_identity() {
    let runtime = ManualRuntime::new();
    let store = Store::new(
        detail_state(1),
        case_identified_reducer(),
        runtime.runtime(),
    );
    runtime.start_all();

    futures_executor::block_on(async {
        let task = store
            .send(CaseAction::Detail(DetailAction::Start))
            .await
            .expect("identified case task starts");
        runtime.wait_for_queued_task();
        runtime.start_all();
        store
            .send(CaseAction::ShowDetail(1))
            .await
            .expect("equal case identity commits")
            .finish()
            .await;
        assert!(!task.is_finished());
        store
            .send(CaseAction::ShowDetail(2))
            .await
            .expect("changed case identity commits")
            .finish()
            .await;
        task.finish().await;
        assert!(task.is_finished());
        store
            .shutdown()
            .await
            .expect("identified case Store shutdown");
    });
}

#[test]
fn if_case_missing_child_diagnostics_name_each_wrapper_and_all_types() {
    for (kind, identified) in [("IfCase", false), ("IfCaseIdentified", true)] {
        let runtime = ManualRuntime::new();
        let state = CaseState {
            screen: ScreenState::List,
            sibling: (),
            detail_close_calls: 0,
        };
        if identified {
            let store = Store::new(
                state,
                if_case_identified(
                    case_parent(),
                    detail_state_path(),
                    detail_action_path(),
                    |s: &DetailState| &s.id,
                    DetailChild,
                )
                .missing_child_policy(MissingChildPolicy::FailFast),
                runtime.runtime(),
            );
            runtime.start_all();
            futures_executor::block_on(store.send(CaseAction::Detail(DetailAction::Start)))
                .expect_err("identified absent case action must fail the Store");
        } else {
            let store = Store::new(
                state,
                if_case(
                    case_parent(),
                    detail_state_path(),
                    detail_action_path(),
                    DetailChild,
                )
                .missing_child_policy(MissingChildPolicy::FailFast),
                runtime.runtime(),
            );
            runtime.start_all();
            futures_executor::block_on(store.send(CaseAction::Detail(DetailAction::Start)))
                .expect_err("absent case action must fail the Store");
        }

        let diagnostic = panic_diagnostic(&runtime);
        assert!(
            diagnostic.starts_with(&format!("{kind} received child action ")),
            "{diagnostic}"
        );
        assert!(diagnostic.contains(std::any::type_name::<CaseState>()));
        assert!(diagnostic.contains(std::any::type_name::<DetailState>()));
        assert!(diagnostic.contains(std::any::type_name::<CaseAction>()));
        assert!(diagnostic.contains(std::any::type_name::<DetailAction>()));
    }
}

fn panic_diagnostic(runtime: &ManualRuntime) -> String {
    let panic = runtime
        .join_all()
        .into_iter()
        .find_map(Result::err)
        .expect("missing-child panic must escape the actor");
    panic
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| {
            panic
                .downcast_ref::<&'static str>()
                .map(ToString::to_string)
        })
        .expect("panic diagnostic must be text")
}

struct RequiredChildState {
    id: u8,
    outputs: usize,
    started: bool,
}

struct RequiredState {
    child: RequiredChildState,
    sibling: (),
}

enum RequiredChildAction {
    Start,
    Output,
}

enum RequiredAction {
    Child(RequiredChildAction),
    Sibling(RequiredChildAction),
    Replace(u8),
}

struct RequiredChild;

impl Reducer for RequiredChild {
    type State = RequiredChildState;
    type Action = RequiredChildAction;

    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &Self::Action,
        _context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        match action {
            RequiredChildAction::Start => {
                state.started = true;
                Effect::merge([
                    Effect::run(|_| std::future::pending()).cancellable(7_u8),
                    Effect::send(RequiredChildAction::Output),
                    Effect::send_after(
                        std::time::Duration::from_secs(60),
                        RequiredChildAction::Output,
                    ),
                ])
            }
            RequiredChildAction::Output => {
                state.outputs += 1;
                Effect::none()
            }
        }
    }
}

struct RequiredSibling;

impl Reducer for RequiredSibling {
    type State = ();
    type Action = RequiredChildAction;

    fn reduce(
        &mut self,
        _state: &mut Self::State,
        action: &Self::Action,
        _context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        match action {
            RequiredChildAction::Start => Effect::run(|_| std::future::pending()).cancellable(7_u8),
            RequiredChildAction::Output => Effect::none(),
        }
    }
}

struct RequiredParent;

impl Reducer for RequiredParent {
    type State = RequiredState;
    type Action = RequiredAction;

    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &Self::Action,
        _context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        match action {
            RequiredAction::Replace(id) => {
                state.child = RequiredChildState {
                    id: *id,
                    outputs: 0,
                    started: false,
                };
            }
            RequiredAction::Child(RequiredChildAction::Start) => {
                assert!(
                    state.child.started,
                    "required child must reduce before parent"
                );
            }
            RequiredAction::Child(RequiredChildAction::Output) | RequiredAction::Sibling(_) => {}
        }
        Effect::none()
    }
}

fn identified_required_reducer() -> impl Reducer<State = RequiredState, Action = RequiredAction> {
    scope_identified(
        required_parent(),
        StatePath::new(
            |state: &RequiredState| &state.child,
            |state: &mut RequiredState| &mut state.child,
        ),
        ActionPath::new(
            |action: &RequiredAction| match action {
                RequiredAction::Child(action) => Some(action),
                RequiredAction::Sibling(_) | RequiredAction::Replace(_) => None,
            },
            RequiredAction::Child,
        ),
        |state: &RequiredChildState| &state.id,
        RequiredChild,
    )
}

fn required_parent() -> impl Reducer<State = RequiredState, Action = RequiredAction> {
    let sibling = scope(
        StatePath::new(
            |state: &RequiredState| &state.sibling,
            |state: &mut RequiredState| &mut state.sibling,
        ),
        ActionPath::new(
            |action: &RequiredAction| match action {
                RequiredAction::Sibling(action) => Some(action),
                RequiredAction::Child(_) | RequiredAction::Replace(_) => None,
            },
            RequiredAction::Sibling,
        ),
        RequiredSibling,
    );
    combine((sibling, RequiredParent))
}

struct FluentChild(Option<futures_channel::oneshot::Sender<EffectSender<RequiredChildAction>>>);

impl Reducer for FluentChild {
    type State = RequiredChildState;
    type Action = RequiredChildAction;

    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &Self::Action,
        _: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        match action {
            RequiredChildAction::Start => {
                state.started = true;
                let capture = self.0.take().unwrap();
                Effect::run(move |sender| async move {
                    capture.send(sender).unwrap();
                    std::future::pending::<()>().await;
                })
                .cancellable(7_u8)
            }
            RequiredChildAction::Output => {
                state.outputs += 1;
                Effect::none()
            }
        }
    }
}

// Reusing a dead identity leaks output; replacing an equal one cancels valid work.
fn assert_fluent_identity_lifetime(
    reducer: impl Reducer<State = RequiredState, Action = RequiredAction> + Send + 'static,
    replacement: u8,
    captured: futures_channel::oneshot::Receiver<EffectSender<RequiredChildAction>>,
) {
    let runtime = ManualRuntime::new();
    let store = Store::new(
        RequiredState {
            child: RequiredChildState {
                id: 1,
                outputs: 0,
                started: false,
            },
            sibling: (),
        },
        reducer,
        runtime.runtime(),
    );
    runtime.start_all();
    let child = store
        .try_send(RequiredAction::Child(RequiredChildAction::Start))
        .unwrap();
    let sibling = store
        .try_send(RequiredAction::Sibling(RequiredChildAction::Start))
        .unwrap();
    futures_executor::block_on(async {
        let child = child.finish().await.unwrap();
        let sibling = sibling.finish().await.unwrap();
        store
            .read(|_| ())
            .await
            .expect("effect launches are queued");
        runtime.start_all();
        let sender = captured.await.unwrap();
        store
            .send(RequiredAction::Replace(replacement))
            .await
            .unwrap()
            .finish()
            .await;
        if replacement == 1 {
            assert!(!child.is_finished());
            sender.send(RequiredChildAction::Output).await.unwrap();
            assert_eq!(store.read(|state| state.child.outputs).await.unwrap(), 1);
        } else {
            child.finish().await;
            assert!(child.is_finished());
            assert!(
                sender
                    .send(RequiredChildAction::Output)
                    .await
                    .unwrap_err()
                    .is_cancelled()
            );
            assert_eq!(store.read(|state| state.child.outputs).await.unwrap(), 0);
        }
        assert!(!sibling.is_finished());
        store.shutdown().await.unwrap();
        child.finish().await;
        sibling.finish().await;
        assert!(child.is_finished());
        assert!(sibling.is_finished());
    });
}

#[test]
fn fluent_if_some_identified_preserves_only_live_identity_authority() {
    for replacement in [1, 2] {
        let (capture, captured) = futures_channel::oneshot::channel();
        // Keep the optional projection populated to isolate identity, not presence.
        let reducer = required_parent().if_some_identified(
            OptionalStatePath::new(
                |state: &RequiredState| Some(&state.child),
                |state: &mut RequiredState| Some(&mut state.child),
            ),
            ActionPath::new(
                |action: &RequiredAction| match action {
                    RequiredAction::Child(action) => Some(action),
                    _ => None,
                },
                RequiredAction::Child,
            ),
            |state: &RequiredChildState| &state.id,
            FluentChild(Some(capture)),
        );
        assert_eq!(format!("{reducer:?}"), "IfSomeIdentified { .. }");
        assert_fluent_identity_lifetime(reducer, replacement, captured);
    }
}

#[test]
fn fluent_if_case_identified_preserves_only_live_identity_authority() {
    for replacement in [1, 2] {
        let (capture, captured) = futures_channel::oneshot::channel();
        // Keep the selected case present; existing enum fixtures cover case departure.
        let reducer = required_parent().if_case_identified(
            CaseStatePath::new(
                |state: &RequiredState| Some(&state.child),
                |state: &mut RequiredState| Some(&mut state.child),
            ),
            ActionPath::new(
                |action: &RequiredAction| match action {
                    RequiredAction::Child(action) => Some(action),
                    _ => None,
                },
                RequiredAction::Child,
            ),
            |state: &RequiredChildState| &state.id,
            FluentChild(Some(capture)),
        );
        assert_eq!(format!("{reducer:?}"), "IfCaseIdentified { .. }");
        assert_fluent_identity_lifetime(reducer, replacement, captured);
    }
}

#[test]
fn fluent_scope_identified_preserves_only_live_identity_authority() {
    for replacement in [1, 2] {
        let (capture, captured) = futures_channel::oneshot::channel();
        let reducer = required_parent().scope_identified(
            StatePath::new(
                |state: &RequiredState| &state.child,
                |state: &mut RequiredState| &mut state.child,
            ),
            ActionPath::new(
                |action: &RequiredAction| match action {
                    RequiredAction::Child(action) => Some(action),
                    _ => None,
                },
                RequiredAction::Child,
            ),
            |state: &RequiredChildState| &state.id,
            FluentChild(Some(capture)),
        );
        assert_eq!(format!("{reducer:?}"), "ScopeIdentified { .. }");
        assert_fluent_identity_lifetime(reducer, replacement, captured);
    }
}

#[test]
fn scope_identified_replaces_required_child_lifetime() {
    let runtime = ManualRuntime::new();
    let store = Store::new(
        RequiredState {
            child: RequiredChildState {
                id: 1,
                outputs: 0,
                started: false,
            },
            sibling: (),
        },
        identified_required_reducer(),
        runtime.runtime(),
    );
    let child_receipt = store
        .try_send(RequiredAction::Child(RequiredChildAction::Start))
        .expect("required child admission");
    let sibling_receipt = store
        .try_send(RequiredAction::Sibling(RequiredChildAction::Start))
        .expect("equal-ID sibling admission");
    let replacement_receipt = store
        .try_send(RequiredAction::Replace(2))
        .expect("required replacement admission");
    runtime.start_all();

    futures_executor::block_on(async {
        let task = child_receipt.finish().await.expect("required child starts");
        let sibling = sibling_receipt
            .finish()
            .await
            .expect("equal-ID sibling starts");
        replacement_receipt
            .finish()
            .await
            .expect("required replacement commits")
            .finish()
            .await;
        runtime.start_all();
        task.finish().await;
        assert!(task.is_finished());
        assert!(!sibling.is_finished());
        assert_eq!(
            store
                .read(|state| state.child.outputs)
                .await
                .expect("replacement output read"),
            0
        );
        store.shutdown().await.expect("required Store shutdown");
        sibling.finish().await;
        assert!(sibling.is_finished());
    });
}

#[test]
fn host_runtime_disposal_invalidates_live_resources() {
    let runtime = DisposableRuntime::new();
    let store = Store::new(0, LifecycleReducer::new(PanicMode::None), runtime.runtime());

    futures_executor::block_on(async {
        let mut observation = store.observe(|state| *state).await.expect("registration");
        assert_eq!(observation.next().await, Some(0));
        let task = store.send(Action::Start).await.expect("start commit");

        assert!(
            runtime.dispose().into_iter().all(|result| result.is_ok()),
            "runtime teardown must close abortively without panicking"
        );
        assert_terminal_resources(&store, &mut observation, &task).await;
    });
}

#[test]
fn dropping_accepted_effect_task_closes_store() {
    let runtime = ManualRuntime::new();
    let store = Store::new(0, LifecycleReducer::new(PanicMode::None), runtime.runtime());
    runtime.start_all();

    let closed = store.closed();
    let task = futures_executor::block_on(store.send(Action::Start)).expect("start commit");
    // The commit receipt precedes the post-return launch. Establish actual
    // runtime acceptance before testing destruction of that accepted future.
    runtime.wait_for_queued_task();
    runtime.drop_queued();

    futures_executor::block_on(closed);
    assert!(store.is_closed());
    assert!(task.is_cancelled());
    futures_executor::block_on(task.finish());
    futures_executor::block_on(store.send(Action::Probe))
        .expect_err("failed Store rejects actions");

    let actor = runtime
        .join_all()
        .into_iter()
        .next()
        .expect("actor thread must exist");
    assert!(
        actor.is_ok(),
        "actor must close abortively when an accepted task is lost"
    );
}

#[test]
#[cfg_attr(
    miri,
    ignore = "panic verification launches subprocesses, which Miri isolation forbids"
)]
fn panic_teardown_invalidates_all_resources() {
    const CHILD_ENV: &str = "RCA_LIFECYCLE_PANIC";

    if let Some(mode) = std::env::var_os(CHILD_ENV) {
        run_panic_child(mode.to_string_lossy().as_ref());
        return;
    }

    for mode in ["reducer", "selector", "effect"] {
        let mut command = std::process::Command::new(
            std::env::current_exe().expect("test executable path must exist"),
        );
        command
            .args([
                "--exact",
                "panic_teardown_invalidates_all_resources",
                "--nocapture",
            ])
            .env(CHILD_ENV, mode);
        let output = subprocess::output(&mut command);

        assert!(!output.status.success(), "{mode} panic must escape actor");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("lifecycle teardown verified"),
            "{mode} panic must complete teardown before propagating: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

async fn assert_terminal_resources(
    store: &Store<usize, Action>,
    observation: &mut Observation<usize>,
    task: &ActionTask,
) {
    store.closed().await;
    assert!(store.is_closed());
    store
        .send(Action::Probe)
        .await
        .expect_err("terminal Store rejects actions");
    assert_eq!(observation.next().await, None);
    assert!(task.is_cancelled());
    task.finish().await;
    assert!(task.is_finished());
}

fn run_panic_child(mode: &str) {
    let runtime = ManualRuntime::new();
    let panic_mode = match mode {
        "reducer" => PanicMode::Reducer,
        "selector" => PanicMode::None,
        "effect" => PanicMode::Effect,
        _ => panic!("unknown panic mode"),
    };
    let store = Store::new(0, LifecycleReducer::new(panic_mode), runtime.runtime());
    runtime.start_all();

    let mut observation =
        futures_executor::block_on(store.observe(|state| *state)).expect("registration");
    assert_eq!(futures_executor::block_on(observation.next()), Some(0));
    let task = futures_executor::block_on(store.send(Action::Start)).expect("start commit");

    match mode {
        "reducer" => {
            futures_executor::block_on(store.send(Action::Crash))
                .expect_err("reducer panic closes send");
        }
        "selector" => {
            futures_executor::block_on(
                store.observe(|_| -> usize { panic!("intentional selector panic") }),
            )
            .expect_err("selector panic closes registration");
        }
        "effect" => {
            runtime.wait_for_queued_task();
            runtime.start_all();
        }
        _ => unreachable!(),
    }

    futures_executor::block_on(assert_terminal_resources(&store, &mut observation, &task));
    eprintln!("lifecycle teardown verified");

    let panic = runtime
        .join_all()
        .into_iter()
        .find_map(Result::err)
        .expect("panic must propagate through runtime task");
    std::panic::resume_unwind(panic);
}

/// Owns abortable tasks and permanently rejects new work after disposal.
struct DisposableRuntime {
    /// Retains registration state while cloned drivers outlive the host.
    shared: Arc<DisposableShared>,
}

/// Shares one admission boundary and one virtual clock across driver handles.
struct DisposableShared {
    /// Registers complete task pairs before disposal can take ownership.
    tasks: Mutex<DisposableTasks>,
    /// Advances finite waits without blocking the thread that polls them.
    clock: immediate_clock::ImmediateClock,
}

/// Keeps each task's cancellation authority beside its join obligation.
#[derive(Default)]
struct DisposableTasks {
    /// Prevents retained driver handles from starting work after disposal.
    closed: bool,
    /// Transfers both handles together at the disposal boundary.
    running: Vec<(AbortHandle, JoinHandle<()>)>,
}

/// Registers work with its host before handing the future to its thread.
#[derive(Clone)]
struct DisposableDriver {
    /// Shares admission and virtual time with the external host.
    shared: Arc<DisposableShared>,
}

impl fmt::Debug for DisposableDriver {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DisposableDriver").finish_non_exhaustive()
    }
}

impl RuntimeDriver for DisposableDriver {
    fn spawn(&self, task: RuntimeFuture) {
        let mut tasks = self
            .shared
            .tasks
            .lock()
            .expect("runtime task lock must not be poisoned");
        if tasks.closed {
            drop(tasks);
            drop(task);
            return;
        }

        // Keep user captures outside thread creation while admission is locked.
        // An OS creation error must not destroy them under that lock.
        let (send_task, receive_task) = std::sync::mpsc::sync_channel(1);
        let (abort, registration) = AbortHandle::new_pair();
        let spawned = std::thread::Builder::new().spawn(move || {
            if let Ok(task) = receive_task.recv() {
                let _ = futures_executor::block_on(Abortable::new(task, registration));
            }
        });
        match spawned {
            Ok(handle) => tasks.running.push((abort, handle)),
            Err(error) => {
                drop(tasks);
                panic!("runtime thread creation failed: {error}");
            }
        }
        drop(tasks);

        // Disposal can already have aborted the registered task. The transfer
        // still releases its waiting thread, which drops the future unpolled.
        let _ = send_task.send(task);
    }

    fn now(&self) -> MonotonicInstant {
        self.shared.clock.now()
    }

    fn sleep_until(&self, deadline: MonotonicInstant) -> RuntimeFuture {
        self.shared.clock.sleep_until(deadline)
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

impl DisposableRuntime {
    /// Creates an open host with no registered tasks.
    fn new() -> Self {
        Self {
            shared: Arc::new(DisposableShared {
                tasks: Mutex::new(DisposableTasks::default()),
                clock: immediate_clock::ImmediateClock::new(),
            }),
        }
    }

    /// Returns a handle that can outlive, but cannot reopen, this host.
    fn runtime(&self) -> Runtime {
        Runtime::new(DisposableDriver {
            shared: Arc::clone(&self.shared),
        })
    }

    /// Closes admission, aborts accepted tasks, and joins their threads.
    ///
    /// One external owner calls disposal, not a task owned by this host. The
    /// admission lock decides which complete task pairs this call owns. Abort
    /// callbacks and joins run after that lock is released. Independent spawners
    /// can race this boundary, but cannot register work after it closes.
    fn dispose(&self) -> Vec<std::thread::Result<()>> {
        let tasks = {
            let mut tasks = self
                .shared
                .tasks
                .lock()
                .expect("runtime task lock must not be poisoned");
            tasks.closed = true;
            std::mem::take(&mut tasks.running)
        };
        for (abort, _) in &tasks {
            abort.abort();
        }
        tasks.into_iter().map(|(_, handle)| handle.join()).collect()
    }
}

impl Drop for DisposableRuntime {
    fn drop(&mut self) {
        drop(self.dispose());
    }
}

#[test]
fn disposable_maximum_clock_wait_remains_pending() {
    let host = DisposableRuntime::new();
    let driver = DisposableDriver {
        shared: Arc::clone(&host.shared),
    };
    let mut wait = driver.sleep_until(MonotonicInstant::MAX);
    let mut context = std::task::Context::from_waker(futures_util::task::noop_waker_ref());

    assert!(wait.as_mut().poll(&mut context).is_pending());
    assert!(wait.as_mut().poll(&mut context).is_pending());
    assert_eq!(driver.now(), MonotonicInstant::ZERO);
}

#[test]
fn disposable_finite_wait_advances_shared_time_without_blocking() {
    let host = DisposableRuntime::new();
    let driver = DisposableDriver {
        shared: Arc::clone(&host.shared),
    };
    let clone = driver.clone();
    let deadline = MonotonicInstant::from_elapsed(std::time::Duration::from_secs(60));
    let mut context = std::task::Context::from_waker(futures_util::task::noop_waker_ref());

    drop(driver.sleep_until(deadline));
    assert_eq!(clone.now(), MonotonicInstant::ZERO);

    let mut wait = driver.sleep_until(deadline);
    assert!(wait.as_mut().poll(&mut context).is_pending());
    assert_eq!(clone.now(), deadline);
    assert!(wait.as_mut().poll(&mut context).is_ready());
    assert!(
        clone
            .sleep_until(deadline)
            .as_mut()
            .poll(&mut context)
            .is_ready()
    );
}

#[test]
fn disposed_host_drops_later_tasks_without_polling() {
    /// Reports whether the task ran before its captures were destroyed.
    struct TaskDropProbe {
        polled: Arc<AtomicBool>,
        dropped: std::sync::mpsc::Sender<bool>,
    }

    impl Drop for TaskDropProbe {
        fn drop(&mut self) {
            let _ = self.dropped.send(self.polled.load(Ordering::Relaxed));
        }
    }

    let host = DisposableRuntime::new();
    let driver = DisposableDriver {
        shared: Arc::clone(&host.shared),
    };
    assert!(host.dispose().is_empty());
    let (dropped, receive_drop) = std::sync::mpsc::channel();
    let polled = Arc::new(AtomicBool::new(false));
    let probe = TaskDropProbe {
        polled: Arc::clone(&polled),
        dropped,
    };
    driver.spawn(Box::pin(async move {
        polled.store(true, Ordering::Relaxed);
        drop(probe);
    }));

    let was_polled = receive_drop
        .recv_timeout(std::time::Duration::from_secs(5))
        .expect("the stopped host releases accepted task captures");
    assert!(!was_polled, "the stopped host must not execute new work");
}
