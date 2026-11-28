//! Checks iterator order, remaining length, borrowing and partial-drop ownership.

use rca_core::KeyedState;
use std::cell::Cell;
use std::hash::{Hash, Hasher};
use std::rc::Rc;

// Debug must report the live cursor without requiring or exposing payload Debug.
#[test]
fn iterator_diagnostics_preserve_non_debug_pairs_and_live_remaining_counts() {
    /// Deliberately has no `Debug` implementation, even when used as a key.
    #[derive(Eq, PartialEq, Hash)]
    struct Opaque(u8);

    let mut states: KeyedState<_, _> = [
        (Opaque(7), Opaque(70)),
        (Opaque(2), Opaque(20)),
        (Opaque(9), Opaque(90)),
    ]
    .into_iter()
    .collect();

    {
        let mut iter = (&states).into_iter();
        assert_eq!(format!("{iter:?}"), "KeyedIter { remaining: 3 }");
        let (id, state) = iter.next().unwrap();
        assert_eq!((id.0, state.0), (7, 70));
        assert_eq!(format!("{iter:?}"), "KeyedIter { remaining: 2 }");
        let (id, state) = iter.next_back().unwrap();
        assert_eq!((id.0, state.0), (9, 90));
        assert_eq!(format!("{iter:?}"), "KeyedIter { remaining: 1 }");
        let (id, state) = iter.next().unwrap();
        assert_eq!((id.0, state.0), (2, 20));
        assert_eq!(format!("{iter:?}"), "KeyedIter { remaining: 0 }");
        assert!(iter.next_back().is_none());
    };
    {
        let mut iter = (&mut states).into_iter();
        assert_eq!(format!("{iter:?}"), "KeyedIterMut { remaining: 3 }");
        let (id, state) = iter.next_back().unwrap();
        assert_eq!((id.0, state.0), (9, 90));
        state.0 = 91;
        assert_eq!(format!("{iter:?}"), "KeyedIterMut { remaining: 2 }");
        let (id, state) = iter.next().unwrap();
        assert_eq!((id.0, state.0), (7, 70));
        assert_eq!(format!("{iter:?}"), "KeyedIterMut { remaining: 1 }");
        let (id, state) = iter.next_back().unwrap();
        assert_eq!((id.0, state.0), (2, 20));
        assert_eq!(format!("{iter:?}"), "KeyedIterMut { remaining: 0 }");
        assert!(iter.next().is_none());
    };
    let mut iter = states.into_iter();
    assert_eq!(format!("{iter:?}"), "KeyedIntoIter { remaining: 3 }");
    let (id, state) = iter.next().unwrap();
    assert_eq!((id.0, state.0), (7, 70));
    assert_eq!(format!("{iter:?}"), "KeyedIntoIter { remaining: 2 }");
    let (id, state) = iter.next_back().unwrap();
    assert_eq!((id.0, state.0), (9, 91));
    assert_eq!(format!("{iter:?}"), "KeyedIntoIter { remaining: 1 }");
    let (id, state) = iter.next().unwrap();
    assert_eq!((id.0, state.0), (2, 20));
    assert_eq!(format!("{iter:?}"), "KeyedIntoIter { remaining: 0 }");
    assert!(iter.next_back().is_none());
}

#[test]
fn borrowed_carrier_preserves_pairs_and_remaining_length_from_both_ends() {
    let empty = KeyedState::<u8, i32>::new();
    let mut iter = (&empty).into_iter();
    assert_eq!(iter.size_hint(), (0, Some(0)));
    assert_eq!(iter.len(), 0);
    assert_eq!(iter.next(), None);
    assert_eq!(iter.next_back(), None);
    assert_eq!(iter.next(), None);
    assert_eq!(iter.next_back(), None);
    assert_eq!(iter.size_hint(), (0, Some(0)));
    assert_eq!(iter.len(), 0);

    let singleton: KeyedState<_, _> = [(7, 70)].into_iter().collect();
    let mut iter = (&singleton).into_iter();
    assert_eq!(iter.size_hint(), (1, Some(1)));
    assert_eq!(iter.len(), 1);
    assert_eq!(iter.next_back(), Some((&7, &70)));
    assert_eq!(iter.size_hint(), (0, Some(0)));
    assert_eq!(iter.len(), 0);
    assert_eq!(iter.next(), None);
    assert_eq!(iter.next_back(), None);
    assert_eq!(iter.next(), None);
    assert_eq!(iter.next_back(), None);

    let states: KeyedState<_, _> = [(7, 70), (2, 20), (9, 90)].into_iter().collect();
    let mut iter = (&states).into_iter();
    assert_eq!(iter.size_hint(), (3, Some(3)));
    assert_eq!(iter.len(), 3);
    assert_eq!(iter.next(), Some((&7, &70)));
    assert_eq!(iter.size_hint(), (2, Some(2)));
    assert_eq!(iter.len(), 2);
    assert_eq!(iter.next_back(), Some((&9, &90)));
    assert_eq!(iter.size_hint(), (1, Some(1)));
    assert_eq!(iter.len(), 1);
    assert_eq!(iter.next(), Some((&2, &20)));
    assert_eq!(iter.size_hint(), (0, Some(0)));
    assert_eq!(iter.len(), 0);
    assert_eq!(iter.next_back(), None);
    assert_eq!(iter.next(), None);
    assert_eq!(iter.next_back(), None);
    assert_eq!(iter.next(), None);
    assert_eq!(iter.size_hint(), (0, Some(0)));
    assert_eq!(iter.len(), 0);
}

#[test]
fn mutable_carrier_updates_selected_states_without_changing_ids_or_order() {
    let mut empty = KeyedState::<u8, i32>::new();
    let mut iter = (&mut empty).into_iter();
    assert_eq!(iter.size_hint(), (0, Some(0)));
    assert_eq!(iter.len(), 0);
    assert_eq!(iter.next(), None);
    assert_eq!(iter.next_back(), None);
    assert_eq!(iter.next(), None);
    assert_eq!(iter.next_back(), None);
    assert_eq!(iter.size_hint(), (0, Some(0)));
    assert_eq!(iter.len(), 0);

    let mut singleton: KeyedState<_, _> = [(7, 70)].into_iter().collect();
    {
        let mut iter = (&mut singleton).into_iter();
        assert_eq!(iter.size_hint(), (1, Some(1)));
        assert_eq!(iter.len(), 1);
        let (id, state) = iter.next().unwrap();
        assert_eq!((*id, *state), (7, 70));
        *state = 71;
        assert_eq!(iter.size_hint(), (0, Some(0)));
        assert_eq!(iter.len(), 0);
        assert_eq!(iter.next_back(), None);
        assert_eq!(iter.next(), None);
        assert_eq!(iter.next_back(), None);
        assert_eq!(iter.next(), None);
    };
    assert_eq!(singleton.into_iter().collect::<Vec<_>>(), [(7, 71)]);

    let mut states: KeyedState<_, _> = [(7, 70), (2, 20), (9, 90)].into_iter().collect();
    {
        let mut iter = (&mut states).into_iter();
        assert_eq!(iter.size_hint(), (3, Some(3)));
        assert_eq!(iter.len(), 3);
        let (id, state) = iter.next_back().unwrap();
        assert_eq!((*id, *state), (9, 90));
        *state = 91;
        assert_eq!(iter.size_hint(), (2, Some(2)));
        assert_eq!(iter.len(), 2);
        let (id, state) = iter.next().unwrap();
        assert_eq!((*id, *state), (7, 70));
        *state = 72;
        assert_eq!(iter.size_hint(), (1, Some(1)));
        assert_eq!(iter.len(), 1);
        let (id, state) = iter.next_back().unwrap();
        assert_eq!((*id, *state), (2, 20));
        *state = 23;
        assert_eq!(iter.size_hint(), (0, Some(0)));
        assert_eq!(iter.len(), 0);
        assert_eq!(iter.next(), None);
        assert_eq!(iter.next_back(), None);
        assert_eq!(iter.next(), None);
        assert_eq!(iter.next_back(), None);
        assert_eq!(iter.size_hint(), (0, Some(0)));
        assert_eq!(iter.len(), 0);
    };
    assert_eq!(
        states.into_iter().collect::<Vec<_>>(),
        [(7, 72), (2, 23), (9, 91)]
    );
}

#[test]
fn owned_carrier_preserves_pairs_and_remaining_length_from_both_ends() {
    let mut iter = KeyedState::<u8, i32>::new().into_iter();
    assert_eq!(iter.size_hint(), (0, Some(0)));
    assert_eq!(iter.len(), 0);
    assert_eq!(iter.next(), None);
    assert_eq!(iter.next_back(), None);
    assert_eq!(iter.next(), None);
    assert_eq!(iter.next_back(), None);
    assert_eq!(iter.size_hint(), (0, Some(0)));
    assert_eq!(iter.len(), 0);

    let singleton: KeyedState<_, _> = [(7, 70)].into_iter().collect();
    let mut iter = singleton.into_iter();
    assert_eq!(iter.size_hint(), (1, Some(1)));
    assert_eq!(iter.len(), 1);
    assert_eq!(iter.next_back(), Some((7, 70)));
    assert_eq!(iter.size_hint(), (0, Some(0)));
    assert_eq!(iter.len(), 0);
    assert_eq!(iter.next(), None);
    assert_eq!(iter.next_back(), None);
    assert_eq!(iter.next(), None);
    assert_eq!(iter.next_back(), None);

    let states: KeyedState<_, _> = [(7, 70), (2, 20), (9, 90)].into_iter().collect();
    let mut iter = states.into_iter();
    assert_eq!(iter.size_hint(), (3, Some(3)));
    assert_eq!(iter.len(), 3);
    assert_eq!(iter.next(), Some((7, 70)));
    assert_eq!(iter.size_hint(), (2, Some(2)));
    assert_eq!(iter.len(), 2);
    assert_eq!(iter.next_back(), Some((9, 90)));
    assert_eq!(iter.size_hint(), (1, Some(1)));
    assert_eq!(iter.len(), 1);
    assert_eq!(iter.next(), Some((2, 20)));
    assert_eq!(iter.size_hint(), (0, Some(0)));
    assert_eq!(iter.len(), 0);
    assert_eq!(iter.next_back(), None);
    assert_eq!(iter.next(), None);
    assert_eq!(iter.next_back(), None);
    assert_eq!(iter.next(), None);
    assert_eq!(iter.size_hint(), (0, Some(0)));
    assert_eq!(iter.len(), 0);
}

/// Compares and hashes only its value while recording its own destruction.
#[derive(Debug)]
struct DropProbe {
    value: u8,
    /// Separate counters distinguish keys, values and individual consumed pairs.
    drops: Rc<Cell<usize>>,
}

impl PartialEq for DropProbe {
    fn eq(&self, other: &Self) -> bool {
        self.value == other.value
    }
}

impl Eq for DropProbe {}

impl Hash for DropProbe {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.value.hash(state);
    }
}

impl Drop for DropProbe {
    fn drop(&mut self) {
        self.drops.set(self.drops.get() + 1);
    }
}

/// Shares a caller-owned counter without sharing ownership of the tested value.
fn probe(value: u8, drops: &Rc<Cell<usize>>) -> DropProbe {
    DropProbe {
        value,
        drops: Rc::clone(drops),
    }
}

#[test]
fn partial_borrowed_carrier_drops_no_elements_and_releases_collection() {
    let id_drops = Rc::new(Cell::new(0));
    let state_drops = Rc::new(Cell::new(0));
    let mut states: KeyedState<_, _> = [
        (probe(7, &id_drops), probe(70, &state_drops)),
        (probe(2, &id_drops), probe(20, &state_drops)),
        (probe(9, &id_drops), probe(90, &state_drops)),
    ]
    .into_iter()
    .collect();
    {
        let mut iter = (&states).into_iter();
        let (id, state) = iter.next_back().unwrap();
        assert_eq!((id.value, state.value), (9, 90));
        assert_eq!(iter.len(), 2);
        assert_eq!(iter.size_hint(), (2, Some(2)));
    };
    assert_eq!((id_drops.get(), state_drops.get()), (0, 0));
    assert_eq!(
        states
            .iter()
            .map(|(id, state)| (id.value, state.value))
            .collect::<Vec<_>>(),
        [(7, 70), (2, 20), (9, 90)]
    );
    // A mutable collection operation after carrier scope proves continued usability.
    states.get_index_mut(1).unwrap().1.value = 21;
    assert_eq!(
        states
            .get_index(1)
            .map(|(id, state)| (id.value, state.value)),
        Some((2, 21))
    );
    assert_eq!((id_drops.get(), state_drops.get()), (0, 0));
    drop(states);
    assert_eq!((id_drops.get(), state_drops.get()), (3, 3));
}

#[test]
fn partial_mutable_carrier_drops_no_elements_and_keeps_updates() {
    let id_drops = Rc::new(Cell::new(0));
    let state_drops = Rc::new(Cell::new(0));
    let mut states: KeyedState<_, _> = [
        (probe(7, &id_drops), probe(70, &state_drops)),
        (probe(2, &id_drops), probe(20, &state_drops)),
        (probe(9, &id_drops), probe(90, &state_drops)),
    ]
    .into_iter()
    .collect();
    {
        let mut iter = (&mut states).into_iter();
        let (id, state) = iter.next().unwrap();
        assert_eq!((id.value, state.value), (7, 70));
        state.value = 71;
        let (id, state) = iter.next_back().unwrap();
        assert_eq!((id.value, state.value), (9, 90));
        state.value = 91;
        assert_eq!(iter.len(), 1);
        assert_eq!(iter.size_hint(), (1, Some(1)));
    };
    assert_eq!((id_drops.get(), state_drops.get()), (0, 0));
    assert_eq!(
        states
            .iter()
            .map(|(id, state)| (id.value, state.value))
            .collect::<Vec<_>>(),
        [(7, 71), (2, 20), (9, 91)]
    );
    states.get_index_mut(1).unwrap().1.value = 21;
    assert_eq!(
        states
            .get_index(1)
            .map(|(id, state)| (id.value, state.value)),
        Some((2, 21))
    );
    assert_eq!((id_drops.get(), state_drops.get()), (0, 0));
    drop(states);
    assert_eq!((id_drops.get(), state_drops.get()), (3, 3));
}

#[test]
fn partial_owned_carrier_drops_only_unyielded_elements_exactly_once() {
    let id_drops = [
        Rc::new(Cell::new(0)),
        Rc::new(Cell::new(0)),
        Rc::new(Cell::new(0)),
    ];
    let state_drops = [
        Rc::new(Cell::new(0)),
        Rc::new(Cell::new(0)),
        Rc::new(Cell::new(0)),
    ];
    let states: KeyedState<_, _> = [
        (probe(7, &id_drops[0]), probe(70, &state_drops[0])),
        (probe(2, &id_drops[1]), probe(20, &state_drops[1])),
        (probe(9, &id_drops[2]), probe(90, &state_drops[2])),
    ]
    .into_iter()
    .collect();
    let (front, back) = {
        let mut iter = states.into_iter();
        let front = iter.next().unwrap();
        let back = iter.next_back().unwrap();
        assert_eq!((front.0.value, front.1.value), (7, 70));
        assert_eq!((back.0.value, back.1.value), (9, 90));
        assert_eq!(iter.len(), 1);
        assert_eq!(iter.size_hint(), (1, Some(1)));
        assert_eq!(id_drops.each_ref().map(|count| count.get()), [0, 0, 0]);
        assert_eq!(state_drops.each_ref().map(|count| count.get()), [0, 0, 0]);
        (front, back)
    };
    assert_eq!(id_drops.each_ref().map(|count| count.get()), [0, 1, 0]);
    assert_eq!(state_drops.each_ref().map(|count| count.get()), [0, 1, 0]);
    assert_eq!((front.0.value, front.1.value), (7, 70));
    assert_eq!((back.0.value, back.1.value), (9, 90));
    drop(front.0);
    assert_eq!(id_drops.each_ref().map(|count| count.get()), [1, 1, 0]);
    assert_eq!(state_drops.each_ref().map(|count| count.get()), [0, 1, 0]);
    drop(back.1);
    assert_eq!(id_drops.each_ref().map(|count| count.get()), [1, 1, 0]);
    assert_eq!(state_drops.each_ref().map(|count| count.get()), [0, 1, 1]);
    assert_eq!((front.1.value, back.0.value), (70, 9));
    drop(front.1);
    drop(back.0);
    assert_eq!(id_drops.each_ref().map(|count| count.get()), [1, 1, 1]);
    assert_eq!(state_drops.each_ref().map(|count| count.get()), [1, 1, 1]);
}
