use rca_core::{
    Effect, ForEach, KeyedAction, KeyedActionPath, KeyedState, KeyedStatePath, ReduceContext,
    Reducer,
};

struct ChildState;

struct ParentState {
    rows: KeyedState<u8, ChildState>,
}

struct ChildAction;

enum ParentAction {
    Child(KeyedAction<u16, ChildAction>),
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
        _state: &mut Self::State,
        _action: &Self::Action,
        _context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        Effect::none()
    }
}

fn main() {
    let state = KeyedStatePath::new(
        |state: &ParentState| &state.rows,
        |state: &mut ParentState| &mut state.rows,
    );
    let action = KeyedActionPath::new(
        |action: &ParentAction| match action {
            ParentAction::Child(action) => Some(action),
        },
        ParentAction::Child,
    );

    let _ = ForEach::new(Parent, state, action, Child);
}
