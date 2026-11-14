//! Checks recovery of owned keyed inputs without requiring payload cloning or formatting.

use rca::{
    KeyedDecodeError, KeyedMergeDecision, KeyedMutationError, KeyedPositionError,
    KeyedReserveError, KeyedState,
};

/// Deliberately lacks `Clone` and `Debug` so recovery must retain the owned payload.
struct Payload(u8);

#[test]
fn facade_recovers_move_only_inputs_and_preserves_existing_order() {
    let mut states = KeyedState::new();
    states.try_insert_at(0, 1, Payload(10)).unwrap();
    let rejected: KeyedMutationError<u8, Payload> = states
        .try_insert_many_at(1, vec![(2, Payload(20)), (1, Payload(99))])
        .unwrap_err();
    assert!(rejected.is_duplicate());
    let recovered = rejected.into_inputs();
    assert_eq!(recovered.len(), 2);
    assert_eq!(recovered[0].0, 2);
    assert_eq!(recovered[0].1.0, 20);
    assert_eq!(recovered[1].0, 1);
    assert_eq!(recovered[1].1.0, 99);
    assert_eq!(states.len(), 1);
    assert_eq!(states.get(&1).unwrap().0, 10);

    let unused = states
        .try_merge(vec![(1, Payload(11)), (2, Payload(20))], |_, _, _| {
            Ok::<_, ()>(KeyedMergeDecision::ReplaceWithIncoming)
        })
        .unwrap();
    assert_eq!(unused.len(), 1);
    assert_eq!(unused[0].0, 1);
    assert_eq!(unused[0].1.0, 10);
    assert_eq!(states.get_index(0).unwrap().1.0, 11);
    assert_eq!(states.get_index(1).unwrap().1.0, 20);

    let _position: KeyedPositionError = states.try_swap_indices(0, 2).unwrap_err();
    let _reserve: KeyedReserveError = states.try_reserve(usize::MAX).unwrap_err();
    assert_eq!(states.index_of(&1), Some(0));
    assert_eq!(states.index_of(&2), Some(1));
}

#[test]
fn facade_codec_rejects_duplicates_without_losing_owned_values() {
    let rejected: KeyedDecodeError<u8, Payload, ()> =
        match KeyedState::decode_with(vec![(1, Payload(10)), (1, Payload(11))], Ok::<_, ()>) {
            Err(error) => error,
            Ok(_) => panic!("duplicate decoded identifiers must be rejected"),
        };
    let recovered = rejected.into_inputs().unwrap();
    assert_eq!(recovered.len(), 2);
    assert_eq!(recovered[0].1.0, 10);
    assert_eq!(recovered[1].1.0, 11);
}
