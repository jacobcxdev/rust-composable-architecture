// Keyed mutation and extraction move state without requiring Clone.

use rca_core::KeyedState;

struct State(String);

fn main() {
    let mut states = KeyedState::with_capacity(0);
    states.insert(1_u8, State(String::from("one")));
    states.get_mut(&1).expect("state exists").0.push('!');
    let removed = states.remove(&1).expect("state exists");
    states.insert(2, removed);
    let owned = states.into_iter().next().expect("state exists").1;
    assert_eq!(owned.0, "one!");
}
