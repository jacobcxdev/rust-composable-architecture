#![deny(warnings)]

use rca_core::KeyedState;

struct State(String);

fn main() {
    let mut states: KeyedState<_, _> = [(1_u8, State(String::from("one")))].into_iter().collect();
    // Counterpart to keyed_filtered_view_escape: retain an owned projection,
    // not references into the move-only collection.
    let lengths = {
        let selected = states.try_filter(|_, _| Ok::<_, ()>(true)).unwrap();
        selected
            .iter()
            .map(|(_, state)| state.0.len())
            .collect::<Vec<_>>()
    };
    // Counterpart to keyed_filtered_view_mutation: mutation is valid after the
    // selected view's final use, without requiring the state to implement Clone.
    states
        .try_insert_at(1, 2, State(String::from("two")))
        .unwrap();
    drop(states);
    assert_eq!(lengths, [3]);
}
