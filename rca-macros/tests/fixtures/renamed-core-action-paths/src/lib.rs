//! Checks path and case derives through a renamed core dependency.

use rca_macros::{ActionPaths, CaseDescriptors, StatePaths, reducers};
use renamed_core::KeyedAction;

struct Child;

struct Row;

#[derive(ActionPaths, CaseDescriptors)]
enum Action<Id> {
    Idle,
    Pair(Child, Row),
    Named {
        child: Child,
        id: Id,
    },
    Child(Child),
    #[rca(keyed)]
    Row(KeyedAction<Id, Row>),
}

#[derive(StatePaths)]
enum State<'a> {
    Idle,
    Pair { name: &'a str, count: usize },
}

pub fn paths_compile() {
    let _: &[ActionCase] = ActionCases::<u8>::ALL;
    assert_eq!(ActionCases::case(&Action::<u8>::Idle), ActionCase::Idle);
    let root = Action::<u8>::Named {
        child: Child,
        id: 3,
    };
    let ActionCaseRef::Named { child, id } = ActionCases::project(&root) else {
        panic!("wrong case")
    };
    let Action::Named {
        child: original, ..
    } = &root
    else {
        panic!("wrong root")
    };
    assert!(std::ptr::eq(child, original));
    assert_eq!(*id, 3);
    let owned = renamed_core::OwnedCasePath::new(Action::<u8>::Child);
    assert!(matches!(
        owned.try_embed::<Action<u8>>(Box::new(Child)).unwrap(),
        Action::Child(Child)
    ));
    let _ = ActionPaths::<u8>::child();
    let _ = ActionPaths::<u8>::row();
    let _ = ActionPaths::<u8>::idle().extract(&Action::Idle);
    let action = ActionPaths::<u8>::pair().embed((Child, Row));
    let _ = ActionPaths::<u8>::pair().extract(&action);
    let _ = ActionPaths::<u8>::named().embed((Child, 1));
    let mut state = StatePaths::pair().embed(("local", 1));
    let _ = StatePaths::pair().get_mut(&mut state);
    let _ = StatePaths::idle().get(&State::Idle);
    let _ = reducers![()];
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renamed_core_paths_preserve_routing_and_state_mutation() {
        paths_compile();
        let child = ActionPaths::<u8>::child();
        let row = ActionPaths::<u8>::row();
        let child_action = child.embed(Child);
        let row_action = row.embed(7, Row);
        assert!(matches!(child.extract(&child_action), Some(Child)));
        assert!(matches!(row.extract(&row_action), Some((7, Row))));
        assert!(child.extract(&row_action).is_none());
        assert!(row.extract(&child_action).is_none());

        let path = StatePaths::pair();
        let mut state = path.embed(("local", 1));
        let (_, count) = path.get_mut(&mut state).unwrap();
        *count = 2;
        let (name, count) = path.get(&state).unwrap();
        assert_eq!((*name, *count), ("local", 2));
        assert!(path.get(&State::Idle).is_none());
        let mut wrong_case = State::Idle;
        assert!(path.get_mut(&mut wrong_case).is_none());
        assert!(matches!(wrong_case, State::Idle));
    }
}
