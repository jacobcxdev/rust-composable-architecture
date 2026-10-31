use rca_core::{
    Effect, ForEach, KeyedAction, KeyedActionPath, KeyedState, KeyedStatePath, ReduceContext,
    Reducer, ReducerExt, for_each,
};

struct ChildState(Vec<u8>);

struct ParentState {
    rows: KeyedState<u8, ChildState>,
}

struct ChildAction(Vec<u8>);

enum ParentAction {
    Child(KeyedAction<u8, ChildAction>),
    Other(Vec<u8>),
}

struct Parent;

impl Reducer for Parent {
    type State = ParentState;
    type Action = ParentAction;

    fn reduce(
        &mut self,
        _state: &mut Self::State,
        _action: &Self::Action,
        _context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        Effect::none()
    }
}

struct Child;

impl Reducer for Child {
    type State = ChildState;
    type Action = ChildAction;

    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &Self::Action,
        _context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        state.0.extend_from_slice(&action.0);
        Effect::none()
    }
}

type StatePath = KeyedStatePath<
    ParentState,
    u8,
    ChildState,
    fn(&ParentState) -> &KeyedState<u8, ChildState>,
    fn(&mut ParentState) -> &mut KeyedState<u8, ChildState>,
>;

type ActionPath = KeyedActionPath<
    ParentAction,
    u8,
    ChildAction,
    fn(&ParentAction) -> Option<&KeyedAction<u8, ChildAction>>,
    fn(KeyedAction<u8, ChildAction>) -> ParentAction,
>;

fn state_path() -> StatePath {
    KeyedStatePath::new(
        (|state: &ParentState| &state.rows) as fn(&ParentState) -> &KeyedState<u8, ChildState>,
        (|state: &mut ParentState| &mut state.rows)
            as fn(&mut ParentState) -> &mut KeyedState<u8, ChildState>,
    )
}

fn action_path() -> ActionPath {
    KeyedActionPath::new(
        (|action: &ParentAction| match action {
            ParentAction::Child(action) => Some(action),
            ParentAction::Other(_) => None,
        }) as fn(&ParentAction) -> Option<&KeyedAction<u8, ChildAction>>,
        ParentAction::Child as fn(KeyedAction<u8, ChildAction>) -> ParentAction,
    )
}

fn assert_reducer<R>(_reducer: R)
where
    R: Reducer<State = ParentState, Action = ParentAction>,
{
}

fn main() {
    assert_reducer(ForEach::new(Parent, state_path(), action_path(), Child));
    assert_reducer(for_each(Parent, state_path(), action_path(), Child));
    assert_reducer(Parent.for_each(state_path(), action_path(), Child));

    let action = ParentAction::Other(Vec::new());
    if let ParentAction::Other(bytes) = action {
        let _ = bytes.len();
    }
}
