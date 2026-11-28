//! Checks generic and keyed action paths through a renamed RCA dependency.

use renamed_rca::{ActionPaths, KeyedAction};

#[derive(ActionPaths)]
pub enum Exported<T> {
    Value(T),
    Tuple(T, String),
    Named { value: T, label: String },
    Idle,
}

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
    let _ = renamed_rca::reducers![()];
    let source = String::from("local");
    let path = ExportedPaths::<&str>::tuple();
    let action = path.embed((source.as_str(), String::from("label")));
    let (value, label) = path.extract(&action).unwrap();
    assert_eq!((*value, label.as_str()), ("local", "label"));
    let named = ExportedPaths::<&str>::named();
    let _ = named.extract(&named.embed((source.as_str(), String::new())));
    assert!(
        ExportedPaths::<&str>::idle()
            .extract(&Exported::Idle)
            .is_some()
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renamed_paths_preserve_payloads_and_reject_other_cases() {
        paths_compile();
        let named = ExportedPaths::<&str>::named();
        let action = named.embed(("named", String::from("payload")));
        let (value, label) = named.extract(&action).unwrap();
        assert_eq!((*value, label.as_str()), ("named", "payload"));
        assert!(named.extract(&Exported::Idle).is_none());

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
