use rca_core::{
    ActionPath, CaseStatePath, Effect, OptionalStatePath, ReduceContext, Reducer, ReducerExt,
    StatePath, if_case, if_case_identified, if_some, if_some_identified, scope_identified,
};

struct ChildState {
    id: u8,
}

struct ParentState {
    child: Option<ChildState>,
    required: ChildState,
}

struct ChildAction(Vec<u8>);

enum ParentAction {
    Child(ChildAction),
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
        _state: &mut Self::State,
        action: &Self::Action,
        _context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        let _ = action.0.len();
        Effect::none()
    }
}

type OptionalChildPath = OptionalStatePath<
    ParentState,
    ChildState,
    fn(&ParentState) -> Option<&ChildState>,
    fn(&mut ParentState) -> Option<&mut ChildState>,
>;

type ChildActionPath = ActionPath<
    ParentAction,
    ChildAction,
    fn(&ParentAction) -> Option<&ChildAction>,
    fn(ChildAction) -> ParentAction,
>;

fn optional_state() -> OptionalChildPath {
    OptionalStatePath::new(
        (|state: &ParentState| state.child.as_ref()) as fn(&ParentState) -> Option<&ChildState>,
        (|state: &mut ParentState| state.child.as_mut())
            as fn(&mut ParentState) -> Option<&mut ChildState>,
    )
}

fn child_action() -> ChildActionPath {
    ActionPath::new(
        (|action: &ParentAction| match action {
            ParentAction::Child(action) => Some(action),
            ParentAction::Other(_) => None,
        }) as fn(&ParentAction) -> Option<&ChildAction>,
        ParentAction::Child as fn(ChildAction) -> ParentAction,
    )
}

fn assert_reducer<R>(_reducer: R)
where
    R: Reducer<State = ParentState, Action = ParentAction>,
{
}

fn main() {
    assert_reducer(if_some(Parent, optional_state(), child_action(), Child));
    assert_reducer(if_some_identified(
        Parent,
        optional_state(),
        child_action(),
        |state: &ChildState| &state.id,
        Child,
    ));
    assert_reducer(Parent.if_some(optional_state(), child_action(), Child));
    assert_reducer(Parent.if_some_identified(
        optional_state(),
        child_action(),
        |state: &ChildState| &state.id,
        Child,
    ));
    assert_reducer(if_case(
        Parent,
        CaseStatePath::new(
            |state: &ParentState| state.child.as_ref(),
            |state: &mut ParentState| state.child.as_mut(),
        ),
        child_action(),
        Child,
    ));
    assert_reducer(if_case_identified(
        Parent,
        CaseStatePath::new(
            |state: &ParentState| state.child.as_ref(),
            |state: &mut ParentState| state.child.as_mut(),
        ),
        child_action(),
        |state: &ChildState| &state.id,
        Child,
    ));
    assert_reducer(Parent.if_case(
        CaseStatePath::new(
            |state: &ParentState| state.child.as_ref(),
            |state: &mut ParentState| state.child.as_mut(),
        ),
        child_action(),
        Child,
    ));
    assert_reducer(Parent.if_case_identified(
        CaseStatePath::new(
            |state: &ParentState| state.child.as_ref(),
            |state: &mut ParentState| state.child.as_mut(),
        ),
        child_action(),
        |state: &ChildState| &state.id,
        Child,
    ));
    assert_reducer(scope_identified(
        Parent,
        StatePath::new(
            |state: &ParentState| &state.required,
            |state: &mut ParentState| &mut state.required,
        ),
        child_action(),
        |state: &ChildState| &state.id,
        Child,
    ));
    assert_reducer(Parent.scope_identified(
        StatePath::new(
            |state: &ParentState| &state.required,
            |state: &mut ParentState| &mut state.required,
        ),
        child_action(),
        |state: &ChildState| &state.id,
        Child,
    ));

    let action = ParentAction::Other(Vec::new());
    if let ParentAction::Other(bytes) = action {
        let _ = bytes.len();
    }
}
