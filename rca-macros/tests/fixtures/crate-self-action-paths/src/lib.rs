//! Checks action paths when generated references must target the current crate.

pub use rca_core::{ActionPath, KeyedAction, KeyedActionPath, KeyedActionPayload, combine};

use rca_macros::{ActionPaths, reducers};

struct Child;

struct Row;

#[derive(ActionPaths)]
enum Action<Id> {
    Child(Child),
    #[rca(keyed)]
    Row(KeyedAction<Id, Row>),
}

pub fn paths_compile() {
    let _ = ActionPaths::<u8>::child();
    let _ = ActionPaths::<u8>::row();
    let _ = reducers![()];
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_paths_preserve_keyed_routing() {
        paths_compile();
        let child = ActionPaths::<u8>::child();
        let row = ActionPaths::<u8>::row();
        let child_action = child.embed(Child);
        let row_action = row.embed(7, Row);
        assert!(matches!(child.extract(&child_action), Some(Child)));
        assert!(matches!(row.extract(&row_action), Some((7, Row))));
        assert!(child.extract(&row_action).is_none());
        assert!(row.extract(&child_action).is_none());
    }
}
