//! Checks explicit client state, shared ownership, and failures without ambient services.

use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, SystemTime};

use rca_core::{ApplicationIds, CivilData, DependencyKey, DependencyValues, RandomBytes, WallTime};

/// Lets each environment choose an independent or shared ID sequence.
struct Ids;

impl DependencyKey for Ids {
    type Value = ApplicationIds<u8>;
}

// Cloning/restarting the stream implicitly would break the explicit ownership choice.
#[test]
fn injected_clients_choose_independent_or_shared_sequences() {
    let mut first = DependencyValues::new();
    first.set::<Ids>(ApplicationIds::sequence([7, 7, 9]));
    let mut independent = DependencyValues::new();
    independent.set::<Ids>(ApplicationIds::sequence([7, 7, 9]));
    let mut shared = DependencyValues::new();
    shared.set::<Ids>(first.get_or_insert::<Ids>().clone());
    assert_eq!(first.get_or_insert::<Ids>().next(), Some(7));
    assert_eq!(shared.get_or_insert::<Ids>().next(), Some(7));
    assert_eq!(first.get_or_insert::<Ids>().next(), Some(9));
    assert_eq!(shared.get_or_insert::<Ids>().next(), None);
    assert_eq!(independent.get_or_insert::<Ids>().next(), Some(7));
    assert_eq!(ApplicationIds::sequence([7, 7, 9]).next(), Some(7));
    let constant = ApplicationIds::constant(7);
    assert_eq!((constant.next(), constant.next()), (Some(7), Some(7)));
}

/// Counts destruction without giving client values `Clone` or `Debug` bounds.
struct Opaque(Arc<AtomicUsize>);

impl Drop for Opaque {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}

// Requiring domain Clone/Debug or dropping values with an earlier alias breaks this.
#[test]
fn sequence_moves_opaque_ids_and_last_alias_releases_pending_values() {
    let drops = Arc::new(AtomicUsize::new(0));
    let ids = ApplicationIds::sequence([Opaque(Arc::clone(&drops)), Opaque(Arc::clone(&drops))]);
    let alias = ids.clone();
    assert_eq!(format!("{ids:?}"), "ApplicationIds { .. }");
    assert_eq!(drops.load(Ordering::Relaxed), 0);
    let value = ids.next().unwrap();
    drop(ids);
    assert_eq!(drops.load(Ordering::Relaxed), 0);
    drop(value);
    assert_eq!(drops.load(Ordering::Relaxed), 1);
    drop(alias);
    assert_eq!(drops.load(Ordering::Relaxed), 2);
}

// Resuming a non-fused iterator after None would incorrectly resurrect an exhausted fixture.
#[test]
fn sequence_permanently_fuses_exhaustion() {
    let mut step = 0;
    let ids = ApplicationIds::sequence(std::iter::from_fn(move || {
        step += 1;
        match step {
            1 => Some(3),
            2 => None,
            _ => Some(8),
        }
    }));
    assert_eq!(ids.next(), Some(3));
    assert_eq!(ids.next(), None);
    assert_eq!(ids.next(), None);
    assert_eq!(ApplicationIds::<u8>::sequence([]).next(), None);
}

#[test]
fn iterator_panic_poisoning_prevents_resuming_the_shared_sequence() {
    let calls = Arc::new(AtomicUsize::new(0));
    let iterator_calls = Arc::clone(&calls);
    let ids = ApplicationIds::sequence(std::iter::from_fn(move || -> Option<u8> {
        iterator_calls.fetch_add(1, Ordering::Relaxed);
        panic!("iterator failed");
    }));
    let alias = ids.clone();
    let first = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| ids.next()))
        .expect_err("iterator panic must propagate");
    assert_eq!(first.downcast_ref::<&str>(), Some(&"iterator failed"));
    let second = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| alias.next()))
        .expect_err("an alias must observe the poisoned iterator lock");
    assert!(
        second
            .downcast_ref::<String>()
            .unwrap()
            .contains("ID sequence lock poisoned")
    );
    assert_eq!(calls.load(Ordering::Relaxed), 1);
}

/// Produces consecutive bytes for repeatable tests, not secure randomness.
#[cfg(feature = "test-util")]
fn seeded(seed: u8) -> RandomBytes<&'static str> {
    RandomBytes::seeded(seed, |state, bytes| {
        for byte in bytes {
            *byte = *state;
            *state = state.wrapping_add(1);
        }
        Ok(())
    })
}

// Erasing fill failures or changing little-endian interpretation breaks the number contract.
#[test]
fn random_numbers_preserve_typed_failures_and_byte_order() {
    /// Keeps error handling independent of domain `Clone` and `Debug` support.
    struct Failure;

    let calls = Arc::new(AtomicUsize::new(0));
    let callback_calls = Arc::clone(&calls);
    let failing = RandomBytes::new(move |_| {
        callback_calls.fetch_add(1, Ordering::Relaxed);
        Err(Failure)
    });
    assert_eq!(format!("{failing:?}"), "RandomBytes { .. }");
    assert_eq!(calls.load(Ordering::Relaxed), 0);
    failing.next_u64().unwrap_err();
    assert_eq!(calls.load(Ordering::Relaxed), 1);
    let bytes = RandomBytes::<Failure>::new(|output| {
        output.copy_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);
        Ok(())
    });
    assert_eq!(bytes.next_u64().ok(), Some(0x0807_0605_0403_0201));
}

#[cfg(feature = "test-util")]
#[test]
fn explicit_seed_replays_and_clones_share_only_the_chosen_stream() {
    let random = seeded(4);
    let alias = random.clone();
    let mut bytes = [0; 3];
    random.fill(&mut bytes).unwrap();
    assert_eq!(bytes, [4, 5, 6]);
    assert_eq!(alias.next_u64().unwrap(), 0x0e0d_0c0b_0a09_0807);
    let mut restarted = [0; 3];
    seeded(4).fill(&mut restarted).unwrap();
    assert_eq!(restarted, [4, 5, 6]);
    random.fill(&mut []).unwrap();
    random.fill(&mut bytes).unwrap();
    assert_eq!(bytes, [15, 16, 17]);
}

// Cloning caller data or substituting ambient time/locale breaks explicit ownership.
#[test]
fn civil_inputs_are_owned_without_domain_clone_or_debug() {
    let calendar_drops = Arc::new(AtomicUsize::new(0));
    let locale_drops = Arc::new(AtomicUsize::new(0));
    let data = CivilData::new(
        Opaque(Arc::clone(&calendar_drops)),
        Opaque(Arc::clone(&locale_drops)),
    );
    assert_eq!(format!("{data:?}"), "CivilData { .. }");
    assert_eq!(calendar_drops.load(Ordering::Relaxed), 0);
    assert_eq!(locale_drops.load(Ordering::Relaxed), 0);
    assert!(Arc::ptr_eq(&data.calendar().0, &calendar_drops));
    assert!(Arc::ptr_eq(&data.locale().0, &locale_drops));

    let time = SystemTime::UNIX_EPOCH - Duration::from_secs(1);
    let wall = WallTime::constant(time);
    assert_eq!(format!("{wall:?}"), "WallTime { .. }");
    assert_eq!(wall.now(), time);
    assert_eq!(wall.clone().now(), time);

    drop(data);
    assert_eq!(calendar_drops.load(Ordering::Relaxed), 1);
    assert_eq!(locale_drops.load(Ordering::Relaxed), 1);
}

// Requiring returned domain values/errors to be Send or Debug would break valid owned endpoints.
#[test]
fn endpoint_captures_not_returned_values_determine_sendability() {
    /// Checks endpoint bounds at compile time, without constraining returned values.
    fn require_send_sync<T: Send + Sync>(_: &T) {}

    let ids = ApplicationIds::new(|| Some(Rc::new(7)));
    require_send_sync(&ids);
    assert_eq!(*ids.next().unwrap(), 7);
    let random = RandomBytes::new(|bytes| {
        if let Some(first) = bytes.first_mut() {
            *first = 42;
        }
        Err(Rc::new(9))
    });
    require_send_sync(&random);
    let mut output = [0, 0];
    assert_eq!(*random.fill(&mut output).unwrap_err(), 9);
    assert_eq!(output, [42, 0]);
}

#[cfg(feature = "test-util")]
#[test]
fn seeded_endpoint_releases_owned_state_only_after_last_alias() {
    let drops = Arc::new(AtomicUsize::new(0));
    let random = RandomBytes::<()>::seeded(Opaque(Arc::clone(&drops)), |_, _| Ok(()));
    let alias = random.clone();
    drop(random);
    assert_eq!(drops.load(Ordering::Relaxed), 0);
    alias.fill(&mut []).unwrap();
    drop(alias);
    assert_eq!(drops.load(Ordering::Relaxed), 1);
}

// Restarting after a failed fill or discarding partial state breaks explicit endpoint ownership.
#[cfg(feature = "test-util")]
#[test]
fn failed_seeded_fill_preserves_caller_state_changes() {
    let random = RandomBytes::seeded(0_u8, |state, bytes| {
        *state += 1;
        bytes.fill(*state);
        if *state == 1 { Err("first") } else { Ok(()) }
    });
    let mut output = [0; 2];
    assert_eq!(random.fill(&mut output), Err("first"));
    assert_eq!(output, [1, 1]);
    random.fill(&mut output).unwrap();
    assert_eq!(output, [2, 2]);
}

#[cfg(feature = "test-util")]
#[test]
fn generator_panic_keeps_partial_bytes_and_poisons_every_alias() {
    let calls = Arc::new(AtomicUsize::new(0));
    let generator_calls = Arc::clone(&calls);
    let random = RandomBytes::<()>::seeded(7_u8, move |state, output| {
        generator_calls.fetch_add(1, Ordering::Relaxed);
        output[0] = *state;
        *state += 1;
        panic!("generator failed");
    });
    let alias = random.clone();
    let mut output = [0, 0];
    let first = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| random.fill(&mut output)))
        .expect_err("generator panic must propagate");
    assert_eq!(first.downcast_ref::<&str>(), Some(&"generator failed"));
    assert_eq!(output, [7, 0]);
    let second = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| alias.fill(&mut output)))
        .expect_err("an alias must observe the poisoned generator lock");
    assert!(
        second
            .downcast_ref::<String>()
            .unwrap()
            .contains("random endpoint lock poisoned")
    );
    assert_eq!(output, [7, 0]);
    assert_eq!(calls.load(Ordering::Relaxed), 1);
}
