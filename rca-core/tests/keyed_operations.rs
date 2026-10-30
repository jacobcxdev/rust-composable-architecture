//! Checks ordered edits, returned ownership, callback failures and child lifetimes.

use rca_core::KeyedState;
use std::cell::Cell;
use std::error::Error;
use std::fmt;
use std::rc::Rc;

mod support;

#[cfg(not(miri))]
#[path = "../src/allocation_test/child.rs"]
mod subprocess;

/// Preserves an owned callback error so tests can check identity and destruction.
struct OwnedCause {
    /// Its address and contents expose replacement or accidental diagnostic leaks.
    marker: Box<str>,
    /// Remains observable after the caller recovers and drops the error.
    drops: Rc<Cell<usize>>,
}

impl fmt::Debug for OwnedCause {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("OwnedCause").field(&self.marker).finish()
    }
}

impl fmt::Display for OwnedCause {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.marker.as_ref())
    }
}

impl Error for OwnedCause {}

impl Drop for OwnedCause {
    fn drop(&mut self) {
        self.drops.set(self.drops.get() + 1);
    }
}

/// Checks agreement between lookup paths before returning pairs for literal assertions.
fn pairs(states: &KeyedState<u8, i32>) -> Vec<(u8, i32)> {
    for (index, (id, value)) in states.iter().enumerate() {
        assert_eq!(states.index_of(id), Some(index));
        assert_eq!(states.get_index(index), Some((id, value)));
        assert_eq!(states.get(id), Some(value));
    }
    states.iter().map(|(id, value)| (*id, *value)).collect()
}

#[test]
fn positional_edits_follow_current_order_and_recover_rejected_inputs() {
    let mut states: KeyedState<_, _> = [(1, 10), (3, 30)].into_iter().collect();
    states.try_insert_at(1, 2, 20).unwrap();
    assert_eq!(pairs(&states), [(1, 10), (2, 20), (3, 30)]);
    assert_eq!(states.index_of(&2), Some(1));
    assert_eq!(states.index_of(&9), None);
    assert_eq!(states.get_index(2), Some((&3, &30)));
    assert_eq!(states.get_index(3), None);
    assert_eq!(states.checked_successor(2), Some(3));
    assert_eq!(states.checked_successor(3), None);
    assert_eq!(states.checked_successor(usize::MAX), None);
    let error = states
        .try_insert_many_at(1, vec![(4, 40), (4, 41)])
        .unwrap_err();
    assert_eq!(error.into_inputs(), [(4, 40), (4, 41)]);
    assert_eq!(states.try_upsert_at(0, 2, 22).unwrap(), Some((2, 20)));
    assert_eq!(states.index_of(&2), Some(1));
    assert_eq!(states.try_remove_range(1..2).unwrap(), [(2, 22)]);
    assert_eq!(pairs(&states), [(1, 10), (3, 30)]);
}

#[test]
fn borrowed_filter_keeps_original_order_and_failure_is_atomic() {
    let states: KeyedState<_, _> = [(1, 10), (2, 20), (3, 30)].into_iter().collect();
    let mut visited = Vec::new();
    let selected = states
        .try_filter(|id, _| {
            visited.push(*id);
            Ok::<_, ()>(*id != 2)
        })
        .unwrap();
    assert_eq!(visited, [1, 2, 3]);
    assert_eq!(selected, [(&1, &10), (&3, &30)]);
    let error = states.try_filter(|id, _| if *id == 2 { Err("stop") } else { Ok(true) });
    assert_eq!(error, Err("stop"));
    assert_eq!(pairs(&states), [(1, 10), (2, 20), (3, 30)]);
}

#[test]
fn reorder_matches_hand_derived_pairs() {
    let mut states: KeyedState<_, _> = [(1, 10), (2, 20), (3, 30), (4, 40)].into_iter().collect();
    states.try_move_indices(&[2, 0], 4).unwrap();
    assert_eq!(pairs(&states), [(2, 20), (4, 40), (1, 10), (3, 30)]);
    states.reverse();
    assert_eq!(pairs(&states), [(3, 30), (1, 10), (4, 40), (2, 20)]);
    states.try_swap_indices(0, 2).unwrap();
    assert_eq!(pairs(&states), [(4, 40), (1, 10), (3, 30), (2, 20)]);
    states
        .try_sort_by(|_, a, _, b| Ok::<_, ()>(a.cmp(b)))
        .unwrap();
    assert_eq!(pairs(&states), [(1, 10), (2, 20), (3, 30), (4, 40)]);
    let boundary = states
        .try_partition(|id, _| Ok::<_, ()>(*id % 2 == 0))
        .unwrap();
    assert_eq!(boundary, 2);
    assert!(states.iter().take(2).all(|(id, _)| *id % 2 == 0));
    assert!(states.iter().skip(2).all(|(id, _)| *id % 2 == 1));
    states.shuffle_with(|| 0);
    let mut actual = pairs(&states);
    actual.sort_unstable();
    assert_eq!(actual, [(1, 10), (2, 20), (3, 30), (4, 40)]);
    for (index, (id, value)) in states.iter().enumerate() {
        assert_eq!(states.index_of(id), Some(index));
        assert_eq!(states.get(id), Some(value));
    }
}

#[test]
fn reserve_failure_leaves_observable_state_intact() {
    let mut states: KeyedState<_, _> = [(1, 10)].into_iter().collect();
    states.try_reserve(20).unwrap();
    let error = states.try_reserve(usize::MAX).unwrap_err();
    let source = error
        .source()
        .unwrap()
        .downcast_ref::<indexmap::TryReserveError>()
        .unwrap();
    let repeated_source = error
        .source()
        .unwrap()
        .downcast_ref::<indexmap::TryReserveError>()
        .unwrap();
    assert!(std::ptr::eq(source, repeated_source));
    let display = error.to_string();
    assert!(display.starts_with("KeyedState reservation failed:"));
    // Exercise the runtime accessor as well as the error's formatted diagnostic.
    let backtrace: fn(&rca_core::KeyedReserveError) -> &std::backtrace::Backtrace =
        rca_core::KeyedReserveError::backtrace;
    let backtrace = std::hint::black_box(backtrace);
    assert!(display.ends_with(&backtrace(&error).to_string()));
    assert_eq!(pairs(&states), [(1, 10)]);
}

#[test]
fn staged_merge_recovers_unused_and_rejected_ownership() {
    use rca_core::KeyedMergeDecision::{KeepExisting, ReplaceWithIncoming};
    let mut states: KeyedState<_, _> = [(1, 10), (2, 20)].into_iter().collect();
    let unused = states
        .try_merge(vec![(1, 11), (2, 22), (3, 30)], |id, _, _| {
            Ok::<_, ()>(if *id == 1 {
                KeepExisting
            } else {
                ReplaceWithIncoming
            })
        })
        .unwrap();
    assert_eq!(unused, [(1, 11), (2, 20)]);
    assert_eq!(pairs(&states), [(1, 10), (2, 22), (3, 30)]);
    let error = states
        .try_merge(vec![(1, 100), (4, 40), (2, 200)], |id, _, _| {
            if *id == 2 {
                Err("stop")
            } else {
                Ok(ReplaceWithIncoming)
            }
        })
        .unwrap_err();
    assert_eq!(
        error.into_parts(),
        (vec![(1, 100), (4, 40), (2, 200)], Some("stop"))
    );
    assert_eq!(pairs(&states), [(1, 10), (2, 22), (3, 30)]);
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "Keep both partial-commit ownership schedules beside their shared panic trigger"
)]
fn later_batch_hash_unwind_preserves_committed_pairs_and_drops_owned_inputs() {
    use core::hash::{Hash, Hasher};
    use rca_core::KeyedMergeDecision::ReplaceWithIncoming;

    /// Tracks one original key or value through a partially committed batch.
    struct Owned {
        slot: usize,
        drops: Rc<Cell<[u8; 6]>>,
    }
    impl Drop for Owned {
        fn drop(&mut self) {
            let mut counts = self.drops.get();
            counts[self.slot] += 1;
            self.drops.set(counts);
        }
    }
    /// Arms a hash panic only after the intended validation or resolver boundary.
    struct Id {
        key: u8,
        owner: Owned,
        /// Shared across keys so a later hash can observe the boundary crossing.
        armed: Rc<Cell<bool>>,
        /// Limits validation-side arming to an incoming identifier.
        batch: bool,
    }
    impl PartialEq for Id {
        fn eq(&self, other: &Self) -> bool {
            // Only the final batch input can meet resident 1 during validation.
            if self.batch && matches!((self.key, other.key), (1, 5) | (5, 1)) {
                self.armed.set(true);
            }
            self.key == other.key
        }
    }
    impl Eq for Id {}
    impl Hash for Id {
        fn hash<H: Hasher>(&self, state: &mut H) {
            assert!(
                self.key != 4 || !self.armed.get(),
                "deliberate commit hash unwind"
            );
            // Force the final validation lookup to compare every resident key.
            0_u8.hash(state);
        }
    }

    for merge in [false, true] {
        let key_drops = Rc::new(Cell::new([0; 6]));
        let value_drops = Rc::new(Cell::new([0; 6]));
        let armed = Rc::new(Cell::new(false));
        let pair = |key, slot| {
            (
                Id {
                    key,
                    owner: Owned {
                        slot,
                        drops: Rc::clone(&key_drops),
                    },
                    armed: Rc::clone(&armed),
                    batch: !merge,
                },
                Owned {
                    slot,
                    drops: Rc::clone(&value_drops),
                },
            )
        };
        let mut states: KeyedState<_, _> = [pair(1, 0), pair(9, 1)].into_iter().collect();
        let inputs = vec![
            pair(if merge { 1 } else { 2 }, 2),
            pair(3, 3),
            pair(4, 4),
            pair(if merge { 9 } else { 5 }, 5),
        ];
        let mut resolved = Vec::new();
        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            if merge {
                let _ = states.try_merge(inputs, |id, _, _| {
                    resolved.push(id.key);
                    // This last conflict follows every incoming lookup.
                    if id.key == 9 {
                        armed.set(true);
                    }
                    Ok::<_, ()>(ReplaceWithIncoming)
                });
            } else {
                let _ = states.try_insert_many_at(1, inputs);
            }
        }))
        .expect_err("the later insertion must unwind");
        assert_eq!(
            panic.downcast_ref::<&str>(),
            Some(&"deliberate commit hash unwind")
        );
        armed.set(false);
        assert_eq!(resolved, if merge { vec![1, 9] } else { vec![] });
        let expected = if merge {
            vec![(1, 0, 2), (9, 1, 1), (3, 3, 3)]
        } else {
            vec![(1, 0, 0), (2, 2, 2), (3, 3, 3), (9, 1, 1)]
        };
        assert_eq!(
            states
                .iter()
                .map(|(id, value)| (id.key, id.owner.slot, value.slot))
                .collect::<Vec<_>>(),
            expected
        );
        for (index, (id, value)) in states.iter().enumerate() {
            assert_eq!(states.index_of(id), Some(index));
            let indexed = states.get_index(index).unwrap();
            assert!(core::ptr::eq(indexed.0, id));
            assert!(core::ptr::eq(indexed.1, value));
            assert!(core::ptr::eq(states.get(id).unwrap(), value));
        }
        assert!(states.get_index(states.len()).is_none());
        assert_eq!(
            key_drops.get(),
            if merge {
                [0, 0, 1, 0, 1, 1]
            } else {
                [0, 0, 0, 0, 1, 1]
            }
        );
        assert_eq!(
            value_drops.get(),
            if merge {
                [1, 0, 0, 0, 1, 1]
            } else {
                [0, 0, 0, 0, 1, 1]
            }
        );
        drop(states);
        assert_eq!(key_drops.get(), [1; 6]);
        assert_eq!(value_drops.get(), [1; 6]);
    }
}

#[test]
fn ordered_hash_and_caller_codec_use_captured_pairs() {
    use std::hash::{DefaultHasher, Hash, Hasher};
    let states: KeyedState<_, _> = [(1, 10), (2, 20)].into_iter().collect();
    let encoded = states
        .encode_with(|pairs| {
            Ok::<_, ()>(pairs.map(|(id, state)| (*id, *state)).collect::<Vec<_>>())
        })
        .unwrap();
    let decoded = KeyedState::decode_with(encoded, Ok::<_, ()>).unwrap();
    assert_eq!(states, decoded);
    let mut first = DefaultHasher::new();
    let mut second = DefaultHasher::new();
    states.hash(&mut first);
    decoded.hash(&mut second);
    assert_eq!(first.finish(), second.finish());
    let duplicate = KeyedState::decode_with(vec![(1, 10), (1, 11)], Ok::<_, ()>).unwrap_err();
    assert_eq!(duplicate.into_inputs().unwrap(), [(1, 10), (1, 11)]);
}

#[test]
fn decode_errors_preserve_owned_pairs_and_original_cause() {
    /// Gives each rejected decoded value an allocation identity and drop count.
    struct Value {
        tag: Box<u8>,
        drops: Rc<Cell<usize>>,
    }
    impl Drop for Value {
        fn drop(&mut self) {
            self.drops.set(self.drops.get() + 1);
        }
    }
    let drops = Rc::new(Cell::new(0));
    let value = |tag| Value {
        tag: Box::new(tag),
        drops: Rc::clone(&drops),
    };
    let input = vec![(1, value(10)), (1, value(11))];
    let allocations = [
        std::ptr::from_ref(input[0].1.tag.as_ref()),
        std::ptr::from_ref(input[1].1.tag.as_ref()),
    ];
    let Err(error) = KeyedState::decode_with(input, Ok::<_, OwnedCause>) else {
        panic!("duplicate decoded IDs must be rejected");
    };
    assert!(error.codec_error().is_none());
    assert!(error.source().is_none());
    let debug = format!("{error:?}");
    assert!(debug.contains("KeyedDecodeError"));
    assert!(debug.contains("duplicate_identifiers: true"));
    let display = error.to_string();
    assert!(display.contains("duplicate identifiers"));
    assert!(display.ends_with(&error.backtrace().to_string()));
    for rendered in [&debug, &display] {
        assert!(!rendered.contains("private cause marker"));
        assert!(!rendered.contains("tag:"));
    }
    assert_eq!(drops.get(), 0);
    let (inputs, cause) = error.into_parts();
    assert!(cause.is_none());
    let inputs = inputs.unwrap();
    assert_eq!(
        inputs
            .iter()
            .map(|(id, value)| (*id, *value.tag))
            .collect::<Vec<_>>(),
        [(1, 10), (1, 11)]
    );
    assert_eq!(
        [
            std::ptr::from_ref(inputs[0].1.tag.as_ref()),
            std::ptr::from_ref(inputs[1].1.tag.as_ref())
        ],
        allocations
    );
    assert_eq!(drops.get(), 0);
    drop(inputs);
    assert_eq!(drops.get(), 2);

    let cause_drops = Rc::new(Cell::new(0));
    let cause = OwnedCause {
        marker: "private cause marker".into(),
        drops: Rc::clone(&cause_drops),
    };
    let allocation = std::ptr::from_ref(cause.marker.as_ref());
    let Err(error) = KeyedState::<u8, Value>::decode_with(cause, Err) else {
        panic!("caller codec failure must be returned");
    };
    let cause = error.codec_error().unwrap();
    assert_eq!(std::ptr::from_ref(cause.marker.as_ref()), allocation);
    let source = error
        .source()
        .unwrap()
        .downcast_ref::<OwnedCause>()
        .unwrap();
    assert!(std::ptr::eq(cause, source));
    let debug = format!("{error:?}");
    assert!(debug.contains("KeyedDecodeError"));
    assert!(debug.contains("duplicate_identifiers: false"));
    let display = error.to_string();
    assert!(display.contains("caller codec error"));
    assert!(display.ends_with(&error.backtrace().to_string()));
    assert!(!debug.contains("private cause marker"));
    assert!(!display.contains("private cause marker"));
    assert_eq!(cause_drops.get(), 0);
    let (inputs, cause) = error.into_parts();
    assert!(inputs.is_none());
    let cause = cause.unwrap();
    assert_eq!(std::ptr::from_ref(cause.marker.as_ref()), allocation);
    assert_eq!(cause_drops.get(), 0);
    drop(cause);
    assert_eq!(cause_drops.get(), 1);
}

#[test]
fn failed_decode_has_no_inputs_and_releases_its_owned_cause() {
    let drops = Rc::new(Cell::new(0));
    let cause = OwnedCause {
        marker: "private decode failure".into(),
        drops: Rc::clone(&drops),
    };
    let error = KeyedState::<u8, u8>::decode_with(cause, Err).unwrap_err();

    assert_eq!(drops.get(), 0);
    assert!(error.into_inputs().is_none());
    assert_eq!(drops.get(), 1);
}

#[test]
fn comparator_unwind_preserves_all_pairs_and_lookup() {
    let mut states: KeyedState<_, _> = (0..40_u8)
        .map(|offset| {
            let id = u8::try_from((u16::from(offset) * 17) % 40).unwrap();
            (id, i32::from(id))
        })
        .collect();
    let mut calls = 0;
    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = states.try_sort_by(|_, a, _, b| {
            calls += 1;
            assert!(calls != 20, "deliberate comparator unwind");
            Ok::<_, ()>(a.cmp(b))
        });
    }));
    let panic = panic.expect_err("the deliberate comparator panic must propagate");
    assert_eq!(
        panic.downcast_ref::<&str>(),
        Some(&"deliberate comparator unwind")
    );
    assert_eq!(calls, 20);
    let mut actual = pairs(&states);
    actual.sort_unstable();
    assert_eq!(
        actual,
        (0..40_u8).map(|id| (id, i32::from(id))).collect::<Vec<_>>()
    );
    for (index, (id, state)) in states.iter().enumerate() {
        assert_eq!(states.index_of(id), Some(index));
        assert_eq!(states.get(id), Some(state));
    }
}

#[test]
fn invalid_offsets_ranges_and_duplicate_moves_never_mutate() {
    let mut states: KeyedState<_, _> = [(1, 10), (2, 20)].into_iter().collect();
    let error = states.try_insert_at(3, 3, 30).unwrap_err();
    assert!(error.is_invalid_position());
    assert!(!error.is_duplicate());
    assert!(error.resolver_error().is_none());
    assert!(error.source().is_none());
    let debug = format!("{error:?}");
    assert!(debug.contains("KeyedMutationError"));
    assert!(debug.contains("invalid position"));
    assert!(!debug.contains("inputs:"));
    let display = error.to_string();
    assert!(display.contains("invalid position"));
    assert!(display.ends_with(&error.backtrace().to_string()));
    assert_eq!(error.into_inputs(), [(3, 30)]);
    let error = states.try_insert_at(1, 2, 99).unwrap_err();
    assert!(!error.is_invalid_position());
    assert!(error.is_duplicate());
    assert!(error.resolver_error().is_none());
    assert!(error.source().is_none());
    assert_eq!(error.into_inputs(), [(2, 99)]);
    assert_eq!(
        states
            .try_insert_many_at(0, vec![(3, 30), (2, 99)])
            .unwrap_err()
            .into_inputs(),
        [(3, 30), (2, 99)]
    );
    assert_eq!(
        states.try_upsert_at(3, 1, 99).unwrap_err().into_inputs(),
        [(1, 99)]
    );
    for (start, end) in [(2, 1), (0, 3), (usize::MAX, usize::MAX)] {
        let error = states.try_remove_range(start..end).unwrap_err();
        if (start, end) == (0, 3) {
            let display = error.to_string();
            assert!(display.starts_with("invalid KeyedState position or range"));
            assert!(display.ends_with(&error.backtrace().to_string()));
            assert!(error.source().is_none());
        }
    }
    for (sources, target) in [
        (&[0, 0][..], 1),
        (&[2][..], 0),
        (&[0, 2][..], 0),
        (&[][..], 3),
    ] {
        assert!(states.try_move_indices(sources, target).is_err());
    }
    assert!(states.try_swap_indices(2, 0).is_err());
    assert!(states.try_swap_indices(0, 2).is_err());
    assert_eq!(states.get_index_mut(2), None);
    assert_eq!(pairs(&states), [(1, 10), (2, 20)]);
}

#[test]
fn empty_single_and_many_batches_moves_and_upserts_preserve_pairs() {
    let mut states = KeyedState::new();
    states.reverse();
    assert!(states.is_empty());
    states.try_insert_many_at(0, vec![]).unwrap();
    assert_eq!(
        states
            .try_merge(vec![], |_, _, _| -> Result<_, ()> {
                panic!("empty batch has no conflicts")
            })
            .unwrap(),
        []
    );
    states.try_move_indices(&[], 0).unwrap();
    assert_eq!(states.try_remove_range(0..0).unwrap(), []);
    assert_eq!(states.checked_successor(0), None);
    assert_eq!(states.try_upsert_at(0, 2, 20).unwrap(), None);
    states.reverse();
    assert_eq!(pairs(&states), [(2, 20)]);
    states.try_move_indices(&[0], 1).unwrap();
    states.try_swap_indices(0, 0).unwrap();
    states
        .try_insert_many_at(0, vec![(1, 10), (3, 30)])
        .unwrap();
    assert_eq!(pairs(&states), [(1, 10), (3, 30), (2, 20)]);
    states.try_move_indices(&[2], 0).unwrap();
    assert_eq!(pairs(&states), [(2, 20), (1, 10), (3, 30)]);
    states.try_move_indices(&[0, 1, 2], 2).unwrap();
    assert_eq!(pairs(&states), [(2, 20), (1, 10), (3, 30)]);
    assert_eq!(
        states.try_remove_range(0..3).unwrap(),
        [(2, 20), (1, 10), (3, 30)]
    );
    assert!(states.is_empty());
}

#[test]
fn callback_errors_and_partition_unwind_preserve_pairs_and_lookup() {
    for unwind in [false, true] {
        let mut states: KeyedState<_, _> =
            [(1, 10), (2, 20), (3, 30), (4, 40)].into_iter().collect();
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            states.try_partition(|id, _| {
                if *id == 4 {
                    assert!(!unwind, "deliberate partition unwind");
                    Err("stop")
                } else {
                    Ok(*id % 2 == 0)
                }
            })
        }));
        if unwind {
            let panic = outcome.expect_err("the deliberate partition panic must propagate");
            assert_eq!(
                panic.downcast_ref::<&str>(),
                Some(&"deliberate partition unwind")
            );
        } else {
            assert_eq!(outcome.unwrap(), Err("stop"));
        }
        let mut actual = pairs(&states);
        actual.sort_unstable();
        assert_eq!(actual, [(1, 10), (2, 20), (3, 30), (4, 40)]);
        for (index, (id, state)) in states.iter().enumerate() {
            assert_eq!(states.index_of(id), Some(index));
            assert_eq!(states.get(id), Some(state));
        }
        let error = states.try_sort_by(|_, _, _, _| Err("compare"));
        assert_eq!(error, Err("compare"));
        for (index, (id, state)) in states.iter().enumerate() {
            assert_eq!(states.index_of(id), Some(index));
            assert_eq!(states.get(id), Some(state));
        }
    }
}

#[test]
fn move_only_values_are_borrowed_and_every_rejected_value_is_recovered() {
    use std::cell::Cell;
    use std::rc::Rc;

    /// Cannot be cloned, making each returned value's ownership observable.
    struct Value {
        identity: u8,
        drops: Rc<Cell<usize>>,
    }
    impl Drop for Value {
        fn drop(&mut self) {
            self.drops.set(self.drops.get() + 1);
        }
    }
    let drops = Rc::new(Cell::new(0));
    let value = |identity| Value {
        identity,
        drops: Rc::clone(&drops),
    };
    let mut states = KeyedState::new();
    states.insert(1, value(9));
    states.get_index_mut(0).unwrap().1.identity = 8;
    let selected = states
        .try_filter(|_, value| Ok::<_, ()>(value.identity == 8))
        .unwrap();
    assert_eq!(*selected[0].0, 1);
    assert!(core::ptr::eq(selected[0].1, states.get(&1).unwrap()));
    let rejected = states
        .try_insert_many_at(0, vec![(2, value(2)), (2, value(3))])
        .unwrap_err()
        .into_inputs();
    assert_eq!(drops.get(), 0);
    assert_eq!(
        rejected.iter().map(|(_, v)| v.identity).collect::<Vec<_>>(),
        [2, 3]
    );
    drop(rejected);
    assert_eq!(drops.get(), 2);
    let old = states.try_upsert_at(0, 1, value(7)).unwrap().unwrap();
    assert_eq!(old.1.identity, 8);
    assert_eq!(states.index_of(&1), Some(0));
    assert_eq!(states.index_of(&7), None);
    drop(old);
    drop(states);
    assert_eq!(drops.get(), 4);
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "Keep the resolver failure and duplicate batch ownership schedule in one fixture"
)]
fn merge_duplicate_and_resolver_failures_drop_nothing_until_ownership_is_returned() {
    use rca_core::KeyedMergeDecision::ReplaceWithIncoming;

    /// Records allocation identity and lifetime across staged resolver failures.
    struct Value {
        tag: Box<u8>,
        drops: Rc<Cell<usize>>,
    }
    impl Drop for Value {
        fn drop(&mut self) {
            self.drops.set(self.drops.get() + 1);
        }
    }
    let drops = Rc::new(Cell::new(0));
    let value = |tag| Value {
        tag: Box::new(tag),
        drops: Rc::clone(&drops),
    };
    let mut states = KeyedState::new();
    states.insert(1, value(10));
    states.insert(2, value(20));
    let existing_allocations = [
        std::ptr::from_ref(states.get(&1).unwrap().tag.as_ref()),
        std::ptr::from_ref(states.get(&2).unwrap().tag.as_ref()),
    ];
    let inputs = vec![(1, value(11)), (3, value(30)), (2, value(22))];
    let allocations = [
        std::ptr::from_ref(inputs[0].1.tag.as_ref()),
        std::ptr::from_ref(inputs[1].1.tag.as_ref()),
        std::ptr::from_ref(inputs[2].1.tag.as_ref()),
    ];
    let cause_drops = Rc::new(Cell::new(0));
    let cause = OwnedCause {
        marker: "private resolver marker".into(),
        drops: Rc::clone(&cause_drops),
    };
    let cause_allocation = std::ptr::from_ref(cause.marker.as_ref());
    let mut cause = Some(cause);
    let mut calls = Vec::new();
    let Err(error) = states.try_merge(inputs, |id, _, _| {
        calls.push(*id);
        if *id == 2 {
            Err(cause.take().unwrap())
        } else {
            Ok(ReplaceWithIncoming)
        }
    }) else {
        panic!("resolver must reject batch");
    };
    assert_eq!(calls, [1, 2]);
    assert_eq!(drops.get(), 0);
    assert_eq!(cause_drops.get(), 0);
    assert!(!error.is_duplicate());
    assert!(!error.is_invalid_position());
    let cause = error.resolver_error().unwrap();
    assert_eq!(std::ptr::from_ref(cause.marker.as_ref()), cause_allocation);
    let source = error
        .source()
        .unwrap()
        .downcast_ref::<OwnedCause>()
        .unwrap();
    assert!(std::ptr::eq(cause, source));
    let debug = format!("{error:?}");
    assert!(debug.contains("KeyedMutationError"));
    assert!(debug.contains("resolver failed"));
    let display = error.to_string();
    assert!(display.contains("resolver failed"));
    assert!(display.ends_with(&error.backtrace().to_string()));
    for rendered in [&debug, &display] {
        assert!(!rendered.contains("private resolver marker"));
        assert!(!rendered.contains("tag:"));
    }
    assert_eq!(
        states
            .iter()
            .map(|(id, value)| (*id, *value.tag))
            .collect::<Vec<_>>(),
        [(1, 10), (2, 20)]
    );
    assert_eq!(
        [
            std::ptr::from_ref(states.get(&1).unwrap().tag.as_ref()),
            std::ptr::from_ref(states.get(&2).unwrap().tag.as_ref())
        ],
        existing_allocations
    );
    let (inputs, cause) = error.into_parts();
    let cause = cause.unwrap();
    assert_eq!(std::ptr::from_ref(cause.marker.as_ref()), cause_allocation);
    assert_eq!(
        inputs
            .iter()
            .map(|(id, value)| (*id, *value.tag))
            .collect::<Vec<_>>(),
        [(1, 11), (3, 30), (2, 22)]
    );
    assert_eq!(
        [
            std::ptr::from_ref(inputs[0].1.tag.as_ref()),
            std::ptr::from_ref(inputs[1].1.tag.as_ref()),
            std::ptr::from_ref(inputs[2].1.tag.as_ref())
        ],
        allocations
    );
    assert_eq!(drops.get(), 0);
    assert_eq!(cause_drops.get(), 0);
    drop(inputs);
    assert_eq!(drops.get(), 3);
    drop(cause);
    assert_eq!(cause_drops.get(), 1);
    let Err(error) = states.try_merge(
        vec![(3, value(31)), (3, value(32))],
        |_, _, _| -> Result<_, OwnedCause> { panic!("duplicate detection must precede resolver") },
    ) else {
        panic!("duplicate batch must be rejected");
    };
    assert!(error.is_duplicate());
    assert!(error.resolver_error().is_none());
    assert!(error.source().is_none());
    assert_eq!(drops.get(), 3);
    let display = error.to_string();
    assert!(display.starts_with("KeyedState mutation rejected: duplicate identifier\n"));
    assert!(display.ends_with(&error.backtrace().to_string()));
    let (inputs, cause) = error.into_parts();
    assert!(cause.is_none());
    assert_eq!(cause_drops.get(), 1);
    assert_eq!(drops.get(), 3);
    drop(inputs);
    assert_eq!(drops.get(), 5);
    assert_eq!(
        states
            .iter()
            .map(|(id, value)| (*id, *value.tag))
            .collect::<Vec<_>>(),
        [(1, 10), (2, 20)]
    );
    assert_eq!(
        [
            std::ptr::from_ref(states.get(&1).unwrap().tag.as_ref()),
            std::ptr::from_ref(states.get(&2).unwrap().tag.as_ref())
        ],
        existing_allocations
    );
    drop(states);
    assert_eq!(drops.get(), 7);
}

#[test]
fn seeded_shuffle_is_reproducible_without_ambient_randomness() {
    /// Repeats the same small input and caller-owned random-word sequence.
    fn shuffled() -> Vec<(u8, i32)> {
        let mut states: KeyedState<_, _> =
            [(1, 10), (2, 20), (3, 30), (4, 40)].into_iter().collect();
        let mut seed = 7_u64;
        states.shuffle_with(|| {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            seed
        });
        pairs(&states)
    }
    assert_eq!(shuffled(), shuffled());
    assert_eq!(shuffled(), [(4, 40), (1, 10), (2, 20), (3, 30)]);
    let mut ordered = shuffled();
    ordered.sort_unstable();
    assert_eq!(ordered, [(1, 10), (2, 20), (3, 30), (4, 40)]);
}

/// Captures an effect sender before testing whether a parent edit invalidates it.
enum LifetimeAction {
    Start(async_channel::Sender<rca_core::EffectSender<LifetimeAction>>),
    Output(i32),
}

/// Separates identity-preserving edits from removal and full reconstruction.
enum ParentAction {
    Child(rca_core::KeyedAction<u8, LifetimeAction>),
    MutateAll,
    Reorder,
    Upsert,
    RemoveAndReinsert,
    Decode,
}

/// Applies each edit to a live collection while child effects remain pending.
struct Parent;
impl rca_core::Reducer for Parent {
    type State = KeyedState<u8, i32>;
    type Action = ParentAction;
    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &Self::Action,
        _: &mut rca_core::ReduceContext<'_>,
    ) -> rca_core::Effect<Self::Action> {
        match action {
            ParentAction::Child(_) => {}
            ParentAction::MutateAll => {
                for (id, value) in state.iter_mut() {
                    *value += i32::from(*id);
                }
            }
            ParentAction::Reorder => {
                state.reverse();
                state.try_swap_indices(0, 2).unwrap();
                state.try_move_indices(&[0, 2], 3).unwrap();
                state
                    .try_sort_by(|_, a, _, b| Ok::<_, ()>(a.cmp(b)))
                    .unwrap();
                state.try_partition(|id, _| Ok::<_, ()>(*id != 2)).unwrap();
                state.shuffle_with(|| 9);
            }
            ParentAction::Upsert => {
                state.try_upsert_at(0, 1, 100).unwrap();
                state
                    .try_merge(vec![(1, 101)], |_, _, _| {
                        Ok::<_, ()>(rca_core::KeyedMergeDecision::ReplaceWithIncoming)
                    })
                    .unwrap();
            }
            ParentAction::RemoveAndReinsert => {
                let index = state.index_of(&1).unwrap();
                let removed = state.try_remove_range(index..index + 1).unwrap();
                state.try_insert_many_at(index, removed).unwrap();
            }
            ParentAction::Decode => {
                let encoded = state
                    .encode_with(|pairs| {
                        Ok::<_, ()>(pairs.map(|(id, value)| (*id, *value)).collect::<Vec<_>>())
                    })
                    .unwrap();
                *state = KeyedState::decode_with(encoded, Ok::<_, ()>).unwrap();
            }
        }
        rca_core::Effect::none()
    }
}

/// Leaves its captured sender alive until the owning child lifetime ends.
struct Child;
impl rca_core::Reducer for Child {
    type State = i32;
    type Action = LifetimeAction;
    fn reduce(
        &mut self,
        state: &mut i32,
        action: &LifetimeAction,
        _: &mut rca_core::ReduceContext<'_>,
    ) -> rca_core::Effect<LifetimeAction> {
        match action {
            LifetimeAction::Start(capture) => {
                let capture = capture.clone();
                rca_core::Effect::run(move |sender| async move {
                    capture.send(sender).await.unwrap();
                    core::future::pending::<()>().await;
                })
            }
            LifetimeAction::Output(value) => {
                *state = *value;
                rca_core::Effect::none()
            }
        }
    }
}

#[test]
fn old_outputs_survive_reorders_and_upserts_but_not_reinsertion_or_decode() {
    // Native deadlocks are contained by the shared 60-second child watchdog.
    // Miri runs the same scenario directly under the surrounding job's limit.
    #[cfg(not(miri))]
    if subprocess::run_in_child(
        "old_outputs_survive_reorders_and_upserts_but_not_reinsertion_or_decode",
    ) {
        return;
    }
    check_lifetimes();
}

/// Reuses captured handles through live edits, then checks their exact retirement.
#[expect(
    clippy::too_many_lines,
    reason = "Keep the same captured handles through identity-preserving and retiring edits"
)]
fn check_lifetimes() {
    use rca_core::{KeyedActionPath, KeyedStatePath, Store, for_each};

    let runtime = support::ManualRuntime::new();
    let path = KeyedStatePath::new(
        |state: &KeyedState<u8, i32>| state,
        |state: &mut KeyedState<u8, i32>| state,
    );
    let action_path = KeyedActionPath::new(
        |action: &ParentAction| match action {
            ParentAction::Child(action) => Some(action),
            _ => None,
        },
        ParentAction::Child,
    );
    let reducer = for_each(Parent, path, action_path, Child);
    assert_eq!(format!("{reducer:?}"), "ForEach { .. }");
    let store = Store::new(
        [(1, 10), (2, 20), (3, 30)].into_iter().collect(),
        reducer,
        runtime.runtime(),
    );
    runtime.start_all();
    futures_executor::block_on(async {
        let child = store.scope_keyed(path, action_path, 1).await.unwrap();
        let (capture, captured) = async_channel::bounded(1);
        let task = child.send(LifetimeAction::Start(capture)).await.unwrap();
        runtime.wait_for_queued_task();
        runtime.start_all();
        let sender = captured.recv().await.unwrap();

        let mutation = store.send(ParentAction::MutateAll).await.unwrap();
        mutation.finish().await;
        assert_eq!(
            store.read(pairs).await.unwrap(),
            [(1, 11), (2, 22), (3, 33)]
        );
        assert_eq!(child.read(|value| *value).await.unwrap(), 11);
        sender.send(LifetimeAction::Output(40)).await.unwrap();
        assert_eq!(child.read(|value| *value).await.unwrap(), 40);
        store
            .send(ParentAction::Reorder)
            .await
            .unwrap()
            .finish()
            .await;
        sender.send(LifetimeAction::Output(41)).await.unwrap();
        assert_eq!(child.read(|value| *value).await.unwrap(), 41);
        store
            .send(ParentAction::Upsert)
            .await
            .unwrap()
            .finish()
            .await;
        sender.send(LifetimeAction::Output(42)).await.unwrap();
        assert_eq!(child.read(|value| *value).await.unwrap(), 42);

        store
            .send(ParentAction::RemoveAndReinsert)
            .await
            .unwrap()
            .finish()
            .await;
        assert!(
            sender
                .send(LifetimeAction::Output(99))
                .await
                .unwrap_err()
                .is_cancelled()
        );
        let ended = child.read(|value| *value).await.unwrap_err();
        assert!(ended.is_scope_ended());
        task.finish().await;

        let replacement = store.scope_keyed(path, action_path, 1).await.unwrap();
        assert_eq!(replacement.read(|value| *value).await.unwrap(), 42);
        let (capture, captured) = async_channel::bounded(1);
        let task = replacement
            .send(LifetimeAction::Start(capture))
            .await
            .unwrap();
        runtime.wait_for_queued_task();
        runtime.start_all();
        let sender = captured.recv().await.unwrap();

        store
            .send(ParentAction::Decode)
            .await
            .unwrap()
            .finish()
            .await;
        assert!(
            sender
                .send(LifetimeAction::Output(99))
                .await
                .unwrap_err()
                .is_cancelled()
        );
        let ended = replacement.read(|value| *value).await.unwrap_err();
        assert!(ended.is_scope_ended());
        task.finish().await;

        let decoded = store.scope_keyed(path, action_path, 1).await.unwrap();
        assert_eq!(decoded.read(|value| *value).await.unwrap(), 42);
        store.shutdown().await.unwrap();
    });
    for result in runtime.join_all() {
        result.unwrap();
    }
}

#[test]
fn merge_and_upsert_keep_exact_captured_keys_and_return_incoming_keys() {
    use core::hash::{Hash, Hasher};
    use rca_core::KeyedMergeDecision::{KeepExisting, ReplaceWithIncoming};

    /// Uses a token to distinguish equal keys supplied by different owners.
    struct Id {
        key: u8,
        /// Excluded from equality and hashing so replacement remains detectable.
        token: &'static str,
    }
    impl PartialEq for Id {
        fn eq(&self, other: &Self) -> bool {
            self.key == other.key
        }
    }
    impl Eq for Id {}
    impl Hash for Id {
        fn hash<H: Hasher>(&self, state: &mut H) {
            self.key.hash(state);
        }
    }
    let mut states = KeyedState::new();
    states.insert(
        Id {
            key: 1,
            token: "captured one",
        },
        10,
    );
    states.insert(
        Id {
            key: 2,
            token: "captured two",
        },
        20,
    );
    let unused = states
        .try_merge(
            vec![
                (
                    Id {
                        key: 1,
                        token: "incoming one",
                    },
                    11,
                ),
                (
                    Id {
                        key: 2,
                        token: "incoming two",
                    },
                    22,
                ),
            ],
            |id, _, _| {
                assert!(id.token.starts_with("captured"));
                Ok::<_, ()>(if id.key == 1 {
                    KeepExisting
                } else {
                    ReplaceWithIncoming
                })
            },
        )
        .unwrap();
    assert_eq!(
        unused
            .iter()
            .map(|(id, value)| (id.token, *value))
            .collect::<Vec<_>>(),
        [("incoming one", 11), ("incoming two", 20)]
    );
    let old = states
        .try_upsert_at(
            0,
            Id {
                key: 2,
                token: "upsert two",
            },
            23,
        )
        .unwrap()
        .unwrap();
    assert_eq!((old.0.token, old.1), ("upsert two", 22));
    let encoded = states
        .encode_with(|pairs| {
            Ok::<_, ()>(
                pairs
                    .map(|(id, state)| (id.token, *state))
                    .collect::<Vec<_>>(),
            )
        })
        .unwrap();
    assert_eq!(encoded, [("captured one", 10), ("captured two", 23)]);
}

#[test]
fn codec_failures_are_typed_and_hash_observes_pair_order_only() {
    use std::hash::{DefaultHasher, Hash, Hasher};
    let states: KeyedState<_, _> = [(1, 10), (2, 20)].into_iter().collect();
    assert_eq!(
        states.encode_with(|_| Err::<(), _>("encode")),
        Err("encode")
    );
    let error = KeyedState::<u8, i32>::decode_with((), |()| Err::<Vec<(u8, i32)>, _>("decode"))
        .unwrap_err();
    assert_eq!(error.into_parts(), (None, Some("decode")));
    let mut decoded = KeyedState::decode_with(vec![(1, 10), (2, 20)], Ok::<_, ()>).unwrap();
    let mut first = DefaultHasher::new();
    states.hash(&mut first);
    let mut second = DefaultHasher::new();
    decoded.hash(&mut second);
    assert_eq!(first.finish(), second.finish());
    decoded.reverse();
    assert_ne!(states, decoded);
    let mut reversed = DefaultHasher::new();
    decoded.hash(&mut reversed);
    assert_ne!(first.finish(), reversed.finish());
}

#[test]
fn comparator_errors_at_each_comparison_remain_typed_and_preserve_pairs() {
    let input: Vec<_> = (0..64_u8)
        .map(|offset| {
            let id = u8::try_from(u16::from(offset) * 17 % 64).unwrap();
            (id, i32::from(id))
        })
        .collect();
    let expected: Vec<_> = (0..64_u8).map(|id| (id, i32::from(id))).collect();
    let mut baseline: KeyedState<_, _> = input.iter().copied().collect();
    let mut comparisons = 0;
    baseline
        .try_sort_by(|_, left, _, right| {
            comparisons += 1;
            Ok::<_, ()>(left.cmp(right))
        })
        .unwrap();
    assert_eq!(pairs(&baseline), expected);
    assert!(comparisons > 0);

    // Visit every callback boundary without fixing the sorting algorithm's count.
    for fail_at in 1..=comparisons {
        let mut states: KeyedState<_, _> = input.iter().copied().collect();
        let mut calls = 0;
        let result = states.try_sort_by(|_, left, _, right| {
            calls += 1;
            if calls == fail_at {
                Err("stop")
            } else {
                Ok(left.cmp(right))
            }
        });
        assert_eq!(result, Err("stop"));
        assert_eq!(calls, fail_at);
        let mut actual = pairs(&states);
        actual.sort_unstable();
        assert_eq!(actual, expected);
    }
}

#[test]
fn sorting_empty_singleton_and_equal_values_needs_no_stable_tie_rule() {
    let mut empty = KeyedState::<u8, i32>::new();
    empty
        .try_sort_by(|_, _, _, _| -> Result<_, ()> { panic!("empty has no comparisons") })
        .unwrap();
    empty.insert(1, 10);
    empty
        .try_sort_by(|_, _, _, _| -> Result<_, ()> { panic!("singleton has no comparisons") })
        .unwrap();
    assert_eq!(pairs(&empty), [(1, 10)]);
    let mut states: KeyedState<_, _> = [(1, 30), (2, 10), (3, 30), (4, 10)].into_iter().collect();
    states
        .try_sort_by(|_, a, _, b| Ok::<_, ()>(a.cmp(b)))
        .unwrap();
    assert_eq!(
        states.values().copied().collect::<Vec<_>>(),
        [10, 10, 30, 30]
    );
    let mut actual = pairs(&states);
    actual.sort_unstable();
    assert_eq!(actual, [(1, 30), (2, 10), (3, 30), (4, 10)]);
}
