//! Manual counterpart of the macro-derived composition compile fixture.
//!
//! Both fixtures build required, optional and keyed children. Smoke execution
//! sends one required-child action; it is not a runtime comparison of every route.

use rca_core::{
    ActionPath, DependencyKey, Effect, KeyedAction, KeyedActionPath, KeyedState, KeyedStatePath,
    OptionalStatePath, ReduceContext, Reducer, StatePath, combine, for_each, if_some, scope,
    with_dependency,
};

/// Scalar child shared by all three composition shapes.
struct ChildState {
    /// Accumulated payload length after the local dependency multiplier.
    count: usize,
}

/// Same state graph as the macro-derived counterpart.
struct AppState {
    /// Always-present child with a dependency override.
    required: ChildState,
    /// Present optional child left unchanged by the smoke action.
    optional: Option<ChildState>,
    /// One keyed child left unchanged by the smoke action.
    rows: KeyedState<u64, ChildState>,
}

/// Owned child payload mapped through each composition shape.
enum ChildAction {
    /// Adds the payload length multiplied by the current dependency.
    Add(String),
}

/// Explicit parent routes matching the derived counterpart.
enum AppAction {
    /// Routes to the always-present child.
    Required(ChildAction),
    /// Routes to the optional child when present.
    Optional(ChildAction),
    /// Routes to one keyed child.
    Row(KeyedAction<u64, ChildAction>),
}

/// Defaults to one; the required child overrides it with two.
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

/// Leaves root state unchanged while children handle their routes.
struct ParentReducer;

impl Reducer for ParentReducer {
    type State = AppState;
    type Action = AppAction;

    fn reduce(
        &mut self,
        _state: &mut Self::State,
        _action: &Self::Action,
        _context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        Effect::none()
    }
}

/// Builds the same reducer graph as the macro-derived counterpart.
fn reducer() -> impl Reducer<State = AppState, Action = AppAction> {
    let required = scope(
        StatePath::new(
            |state: &AppState| &state.required,
            |state: &mut AppState| &mut state.required,
        ),
        ActionPath::new(
            |action: &AppAction| match action {
                AppAction::Required(action) => Some(action),
                AppAction::Optional(_) | AppAction::Row(_) => None,
            },
            AppAction::Required,
        ),
        with_dependency::<StepKey, _>(2, ChildReducer),
    );
    let optional = if_some(
        ParentReducer,
        OptionalStatePath::new(
            |state: &AppState| state.optional.as_ref(),
            |state: &mut AppState| state.optional.as_mut(),
        ),
        ActionPath::new(
            |action: &AppAction| match action {
                AppAction::Optional(action) => Some(action),
                AppAction::Required(_) | AppAction::Row(_) => None,
            },
            AppAction::Optional,
        ),
        ChildReducer,
    );
    let keyed = for_each(
        optional,
        KeyedStatePath::new(
            |state: &AppState| &state.rows,
            |state: &mut AppState| &mut state.rows,
        ),
        KeyedActionPath::new(
            |action: &AppAction| match action {
                AppAction::Row(action) => Some(action),
                AppAction::Required(_) | AppAction::Optional(_) => None,
            },
            AppAction::Row,
        ),
        ChildReducer,
    );
    combine((required, keyed))
}

fn main() {
    rca_compile_fixture_support::exercise(
        AppState {
            required: ChildState { count: 0 },
            optional: Some(ChildState { count: 0 }),
            rows: [(1, ChildState { count: 0 })].into_iter().collect(),
        },
        reducer(),
        AppAction::Required(ChildAction::Add(String::from("composition"))),
        |state| {
            assert_eq!(state.required.count, 22);
            assert_eq!(state.optional.as_ref().map(|child| child.count), Some(0));
            assert_eq!(state.rows.get(&1).map(|child| child.count), Some(0));
            assert_eq!(state.rows.len(), 1);
        },
    );
}
