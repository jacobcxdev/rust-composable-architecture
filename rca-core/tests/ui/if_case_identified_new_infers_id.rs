use rca_core::{ActionPath, CaseStatePath, Effect, IfCaseIdentified, ReduceContext, Reducer};

struct ChildState {
    id: u8,
}

struct ParentState {
    child: Option<ChildState>,
}

struct ChildAction;

enum ParentAction {
    Child(ChildAction),
    Other,
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
    let state = CaseStatePath::new(
        |state: &ParentState| state.child.as_ref(),
        |state: &mut ParentState| state.child.as_mut(),
    );
    let action = ActionPath::new(
        |action: &ParentAction| match action {
            ParentAction::Child(action) => Some(action),
            ParentAction::Other => None,
        },
        ParentAction::Child,
    );

    let _ = IfCaseIdentified::new(Parent, state, action, |state: &ChildState| &state.id, Child);

    let _ = ParentAction::Other;
}
