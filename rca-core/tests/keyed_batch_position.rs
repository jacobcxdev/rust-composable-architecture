//! Checks error precedence and exact ownership for rejected positional batches.

use std::{cell::Cell, rc::Rc};

use rca_core::KeyedState;

/// Records which original value is released without making it cloneable.
struct Owned {
    /// Selects one counter per resident or incoming value.
    slot: usize,
    /// Its address distinguishes the original allocation from a replacement.
    marker: Box<u8>,
    /// Remains observable after the collection and rejected batch are dropped.
    drops: Rc<Cell<[u8; 5]>>,
}

impl Drop for Owned {
    fn drop(&mut self) {
        let mut counts = self.drops.get();
        counts[self.slot] += 1;
        self.drops.set(counts);
    }
}

#[test]
fn invalid_batch_position_precedes_duplicates_and_recovers_exact_owners() {
    // Removing the position check, moving it after duplicate validation, or
    // accepting an empty batch before validating its position breaks this test.
    for populated in [false, true] {
        for incoming in [false, true] {
            let drops = Rc::new(Cell::new([0; 5]));
            let pair = |id: &str, slot| {
                (
                    Box::<str>::from(id),
                    Owned {
                        slot,
                        marker: Box::new(7),
                        drops: Rc::clone(&drops),
                    },
                )
            };
            let mut states = KeyedState::new();
            if populated {
                let (id, value) = pair("one", 0);
                states.insert(id, value);
                let (id, value) = pair("two", 1);
                states.insert(id, value);
            }
            let retained: Vec<_> = states
                .iter()
                .map(|(id, value)| (id.as_ptr(), std::ptr::from_ref(&*value.marker)))
                .collect();
            let inputs = if incoming {
                vec![pair("one", 2), pair("new", 3), pair("new", 4)]
            } else {
                Vec::new()
            };
            let addresses: Vec<_> = inputs
                .iter()
                .map(|(id, value)| (id.as_ptr(), std::ptr::from_ref(&*value.marker)))
                .collect();
            let error = states
                .try_insert_many_at(states.len() + 1, inputs)
                .unwrap_err();
            assert!(error.is_invalid_position());
            assert!(!error.is_duplicate());
            assert!(error.resolver_error().is_none());
            let expected: &[(&str, usize)] = if populated {
                &[("one", 0), ("two", 1)]
            } else {
                &[]
            };
            assert_eq!(
                states
                    .iter()
                    .map(|(id, value)| (id.as_ref(), value.slot))
                    .collect::<Vec<_>>(),
                expected
            );
            assert!(!states.contains_id(&Box::<str>::from("new")));
            for (index, (id, value)) in states.iter().enumerate() {
                assert_eq!(states.index_of(id), Some(index));
                assert!(std::ptr::eq(states.get(id).unwrap(), value));
                assert!(std::ptr::eq(states.get_index(index).unwrap().1, value));
                assert_eq!(id.as_ptr(), retained[index].0);
                assert_eq!(std::ptr::from_ref(&*value.marker), retained[index].1);
            }
            let (recovered, resolver) = error.into_parts();
            assert!(resolver.is_none());
            let expected: &[(&str, usize)] = if incoming {
                &[("one", 2), ("new", 3), ("new", 4)]
            } else {
                &[]
            };
            assert_eq!(
                recovered
                    .iter()
                    .map(|(id, value)| (id.as_ref(), value.slot))
                    .collect::<Vec<_>>(),
                expected
            );
            for ((id, value), (key, marker)) in recovered.iter().zip(addresses) {
                assert_eq!(id.as_ptr(), key);
                assert_eq!(std::ptr::from_ref(&*value.marker), marker);
            }
            assert_eq!(drops.get(), [0; 5]);
            drop(recovered);
            assert_eq!(drops.get(), if incoming { [0, 0, 1, 1, 1] } else { [0; 5] });
            drop(states);
            assert_eq!(
                drops.get(),
                match (populated, incoming) {
                    (false, false) => [0; 5],
                    (false, true) => [0, 0, 1, 1, 1],
                    (true, false) => [1, 1, 0, 0, 0],
                    (true, true) => [1; 5],
                }
            );
        }
    }
}
