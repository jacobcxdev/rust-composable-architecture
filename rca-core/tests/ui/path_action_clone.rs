// Copying a path does not require its state or action to implement Clone.

use rca_core::{ActionPath, CaseStatePath, OptionalStatePath, StatePath};

struct ChildState(Vec<u8>);

struct ParentState {
    required: ChildState,
    optional: Option<ChildState>,
    case: CaseState,
}

enum CaseState {
    Child(ChildState),
    Parent,
}

enum ParentAction {
    Child(Vec<u8>),
    Parent,
}

fn assert_copy<T: Copy>(_: T) {}

fn main() {
    let state = StatePath::new(
        |state: &ParentState| &state.required,
        |state: &mut ParentState| &mut state.required,
    );
    let optional = OptionalStatePath::new(
        |state: &ParentState| state.optional.as_ref(),
        |state: &mut ParentState| state.optional.as_mut(),
    );
    let case = CaseStatePath::new(
        |state: &ParentState| match &state.case {
            CaseState::Child(child) => Some(child),
            CaseState::Parent => None,
        },
        |state: &mut ParentState| match &mut state.case {
            CaseState::Child(child) => Some(child),
            CaseState::Parent => None,
        },
    );
    let action = ActionPath::new(
        |action: &ParentAction| match action {
            ParentAction::Child(child) => Some(child),
            ParentAction::Parent => None,
        },
        ParentAction::Child,
    );

    assert_copy(state);
    assert_copy(optional);
    assert_copy(case);
    assert_copy(action);

    let parent = ParentState {
        required: ChildState(vec![1]),
        optional: None,
        case: CaseState::Parent,
    };
    assert_eq!(state.get(&parent).0, [1]);
    assert!(optional.get(&parent).is_none());
    assert!(case.get(&parent).is_none());
    assert!(action.extract(&ParentAction::Parent).is_none());
    assert!(matches!(action.embed(vec![2]), ParentAction::Child(bytes) if bytes == [2]));

    let _ = CaseState::Child(ChildState(Vec::new()));
}
