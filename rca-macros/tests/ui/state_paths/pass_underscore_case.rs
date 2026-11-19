#![deny(warnings)]

#[derive(rca_macros::StatePaths)]
enum State {
    __1,
    __(String),
    ___ { value: String, count: u8 },
}

#[derive(rca_macros::ActionPaths)]
enum Action {
    __(String),
}

fn main() {
    let state = StatePaths::__();
    let mut value = State::__(String::from("owned"));
    state.get_mut(&mut value).unwrap().push_str(" mutable");
    assert_eq!(state.get(&value).map(String::as_str), Some("owned mutable"));

    let aggregate = StatePaths::___();
    let mut value = aggregate.embed((String::from("aggregate"), 1));
    let (label, count) = aggregate.get_mut(&mut value).unwrap();
    label.push_str(" mutable");
    *count += 1;
    assert_eq!(
        aggregate
            .get(&value)
            .map(|(label, count)| (label.as_str(), *count)),
        Some(("aggregate mutable", 2)),
    );
    assert!(matches!(StatePaths::__1().embed(()), State::__1));
    let action = ActionPaths::__();
    let value = action.embed(String::from("action"));
    assert_eq!(action.extract(&value).map(String::as_str), Some("action"));
}
