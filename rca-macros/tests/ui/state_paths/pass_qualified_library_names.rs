#![deny(warnings)]

use ::rca_macros::{ActionPaths, CaseDescriptors, StatePaths};

// Consumer modules and type parameters must not capture generated library paths.
mod core {}
mod std {}
mod rca {}

#[derive(StatePaths)]
struct State<StatePath, Option, Fn> {
    value: StatePath,
    #[rca(optional)]
    optional: ::core::option::Option<Option>,
    callback: Fn,
}

#[derive(ActionPaths, CaseDescriptors)]
enum Action<ActionPath, Clone, Debug> {
    Value(ActionPath),
    Pair(Clone, Debug),
}

fn main() {
    let mut state = State {
        value: 3_u8,
        optional: Some(5_u16),
        callback: || 7_u8,
    };
    *StatePaths::value().get_mut(&mut state) = 11;
    assert_eq!(*StatePaths::value().get(&state), 11);
    assert_eq!(StatePaths::optional().get(&state), Some(&5));
    assert_eq!(StatePaths::callback().get(&state)(), 7);

    let path = ActionPaths::<u8, u16, u32>::value();
    let action = path.embed(13);
    assert_eq!(path.extract(&action), Some(&13));
    assert_eq!(ActionCases::case(&action), ActionCase::Value);
    assert_eq!(ActionCase::Value.label(), Some("Value"));

    let pair = ActionPaths::<u8, u16, u32>::pair().embed((17, 19));
    let ActionCaseRef::Pair(first, second) = ActionCases::project(&pair) else {
        panic!("the generated view changed the selected case");
    };
    assert_eq!((*first, *second), (17, 19));
}
