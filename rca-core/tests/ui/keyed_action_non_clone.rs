// A copied keyed path embeds and extracts move-only action payloads.

use rca_core::{KeyedAction, KeyedActionPath};

struct ChildAction(Vec<u8>);

enum ParentAction {
    Child(KeyedAction<u64, ChildAction>),
    Parent,
}

fn assert_copy<T: Copy>(_: T) {}

fn main() {
    let path = KeyedActionPath::new(
        |action: &ParentAction| match action {
            ParentAction::Child(action) => Some(action),
            ParentAction::Parent => None,
        },
        ParentAction::Child,
    );
    assert_copy(path);
    let parent = path.embed(7, ChildAction(vec![1, 2, 3]));
    let (id, action) = path.extract(&parent).expect("child action matches");

    assert_eq!(*id, 7);
    assert_eq!(action.0, [1, 2, 3]);

    let ParentAction::Child(action) = parent else {
        unreachable!("embedded child action remains a child")
    };
    let (id, action) = action.into_parts();
    assert_eq!(id, 7);
    assert_eq!(action.0, [1, 2, 3]);

    let _ = ParentAction::Parent;
}
