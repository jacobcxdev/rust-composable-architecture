//! Compile fixture for a scoped Store handle over one required child.
//!
//! This adds public handle acquisition, child admission and projected reading.
//! Its workload is intentionally different from the reducer-only compositions.

use rca_core::{
    ActionPath, DependencyKey, Effect, ReduceContext, Reducer, StatePath, Store, scope,
    with_dependency,
};

/// Child state projected through the reducer and public handle.
struct ChildState {
    /// Payload length after applying the overridden multiplier.
    count: usize,
}

/// Root containing one always-present child.
struct AppState {
    /// State selected by both independently constructed paths.
    child: ChildState,
}

/// Owned action sent through the child handle.
enum ChildAction {
    /// Adds the scaled payload length.
    Add(String),
}

/// Root route produced by embedding a child action.
enum AppAction {
    /// The fixture's sole child route.
    Child(ChildAction),
}

/// Defaults to one; this child overrides it with two.
struct StepKey;

impl DependencyKey for StepKey {
    type Value = usize;

    fn live_value() -> Self::Value {
        1
    }
}

/// Effect-free child with one dependency read per matching action.
struct ChildReducer;

impl Reducer for ChildReducer {
    type State = ChildState;
    type Action = ChildAction;

    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &Self::Action,
        context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        match action {
            ChildAction::Add(value) => {
                state.count = state
                    .count
                    .wrapping_add(value.len() * context.dependency::<StepKey>());
            }
        }
        Effect::none()
    }
}

/// Selects the same required child for reduction and handle operations.
fn child_state() -> StatePath<
    AppState,
    ChildState,
    impl Fn(&AppState) -> &ChildState + Clone,
    impl Fn(&mut AppState) -> &mut ChildState + Clone,
> {
    StatePath::new(
        |state: &AppState| &state.child,
        |state: &mut AppState| &mut state.child,
    )
}

/// Projects borrowed child actions and embeds owned child actions.
fn child_action() -> ActionPath<
    AppAction,
    ChildAction,
    impl Fn(&AppAction) -> Option<&ChildAction>,
    impl Fn(ChildAction) -> AppAction + Clone,
> {
    ActionPath::new(
        |action: &AppAction| match action {
            AppAction::Child(action) => Some(action),
        },
        AppAction::Child,
    )
}

fn main() {
    let store = Store::new(
        AppState {
            child: ChildState { count: 0 },
        },
        scope(
            child_state(),
            child_action(),
            with_dependency::<StepKey, _>(2, ChildReducer),
        ),
        rca_compile_fixture_support::runtime(),
    );
    let child = store.scope(child_state(), child_action());
    let task = futures_executor::block_on(child.send(ChildAction::Add(String::from("scoped"))))
        .expect("scoped action must commit");
    futures_executor::block_on(task.finish());
    let count = futures_executor::block_on(child.read(|state| state.count))
        .expect("scoped state remains readable");
    assert_eq!(count, 12);
    futures_executor::block_on(store.shutdown()).expect("fixture Store shuts down");
}
