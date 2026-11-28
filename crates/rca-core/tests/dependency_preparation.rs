//! Checks preparation order, retained changes, cache ownership, and panic propagation.

use rca_core::{DependencyDefaultPolicy, DependencyDefaults};
use rca_core::{DependencyKey, DependencyValues};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

/// Supplies a trait default so overrides and cache resets have distinct outcomes.
struct Legacy;

impl DependencyKey for Legacy {
    type Value = usize;

    fn live_value() -> Self::Value {
        1
    }
}

/// Makes cached value identity and mutations observable across environment clones.
struct Handle;

impl DependencyKey for Handle {
    type Value = Arc<AtomicUsize>;
}

/// Distinguishes an explicit absent value from an unresolved dependency.
struct Optional;

impl DependencyKey for Optional {
    type Value = Option<usize>;
}

// Eager creation, repeated hits, global reset, or handle revocation breaks this.
#[test]
fn preparation_lookup_hit_reset_order_and_existing_handles() {
    let events = Arc::new(Mutex::new(Vec::new()));
    let factory_events = Arc::clone(&events);
    let mut values = DependencyValues::new();
    values
        .prepare(|values| {
            events.lock().unwrap().push("prepare");
            values
                .register_defaults(DependencyDefaults::<Handle>::new().with_live(move || {
                    factory_events.lock().unwrap().push("factory");
                    Arc::new(AtomicUsize::new(7))
                }))
                .unwrap();
        })
        .unwrap();
    assert_eq!(*events.lock().unwrap(), ["prepare"]);
    let old = Arc::clone(values.get_or_insert::<Handle>());
    assert!(Arc::ptr_eq(&old, values.get_or_insert::<Handle>()));
    events.lock().unwrap().push("hit");
    let mut sibling = values.clone();
    rca_core::DependencyValues::reset_cache(&mut values);
    events.lock().unwrap().push("reset");
    assert!(Arc::ptr_eq(&old, sibling.get_or_insert::<Handle>()));
    old.store(9, Ordering::Relaxed);
    let fresh = values.get_or_insert::<Handle>();
    assert!(!Arc::ptr_eq(&old, fresh));
    assert_eq!(old.load(Ordering::Relaxed), 9);
    assert_eq!(fresh.load(Ordering::Relaxed), 7);
    assert_eq!(
        *events.lock().unwrap(),
        ["prepare", "factory", "hit", "reset", "factory"]
    );
}

// Reset or explicit replacement must not permit another preparation callback.
#[test]
fn lookup_freezes_preparation_but_not_overrides_or_unattempted_registration() {
    let mut values = DependencyValues::new();
    values.set::<Optional>(None);
    assert_eq!(values.get_or_insert::<Optional>(), &None);
    values
        .register_defaults(DependencyDefaults::<Optional>::new().with_live(|| Some(3)))
        .unwrap();
    rca_core::DependencyValues::reset_cache(&mut values);
    let mut calls = 0;
    let error = values.prepare(|_| calls += 1).unwrap_err();
    assert_eq!(calls, 0);
    assert_eq!(
        error.to_string().lines().next().unwrap(),
        "dependency environment already used; preparation rejected"
    );
    let _ = error.backtrace();
    assert_eq!(values.get_or_insert::<Optional>(), &None);
    values.set::<Optional>(Some(8));
    assert_eq!(values.get_or_insert::<Optional>(), &Some(8));
    assert!(values.clone().prepare(|_| ()).is_err());
}

// Clones before construction must not acquire a shared mutable cache.
#[test]
fn clones_before_lookup_and_separate_preparations_have_independent_caches() {
    let calls = Arc::new(AtomicUsize::new(0));
    let factory_calls = Arc::clone(&calls);
    let mut first = DependencyValues::new();
    first
        .prepare(|values| {
            values.register_defaults(DependencyDefaults::<Handle>::new().with_live(move || {
                factory_calls.fetch_add(1, Ordering::Relaxed);
                Arc::new(AtomicUsize::new(1))
            }))
        })
        .unwrap()
        .unwrap();
    let mut clone = first.clone();
    let first_value = Arc::clone(first.get_or_insert::<Handle>());
    let cloned_value = clone.get_or_insert::<Handle>();
    assert!(!Arc::ptr_eq(&first_value, cloned_value));
    assert_eq!(calls.load(Ordering::Relaxed), 2);
    let mut second = DependencyValues::new();
    second
        .prepare(|values| {
            values.register_defaults(
                DependencyDefaults::<Handle>::new().with_live(|| Arc::new(AtomicUsize::new(2))),
            )
        })
        .unwrap()
        .unwrap();
    assert_eq!(second.get_or_insert::<Handle>().load(Ordering::Relaxed), 2);
    assert_eq!(first_value.load(Ordering::Relaxed), 1);
}

// Failed factory attempts and reset must not reopen registration or preparation.
#[test]
fn factory_panic_freezes_default_registration_and_preparation() {
    let mut values = DependencyValues::for_policy(DependencyDefaultPolicy::Test);
    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        values.get_or_insert::<Handle>();
    }))
    .expect_err("missing test defaults must fail before any value is cached");
    assert_eq!(
        panic.downcast_ref::<String>().unwrap(),
        &format!(
            "missing test dependency `{}`; inject a value or implement DependencyKey::test_value",
            std::any::type_name::<Handle>()
        )
    );

    values.reset_cache();
    assert!(values.prepare(|_| ()).is_err());
    let error = values
        .register_defaults(
            DependencyDefaults::<Handle>::new().with_test(|| Arc::new(AtomicUsize::new(4))),
        )
        .unwrap_err();
    assert!(error.is_initialized());
    assert!(!error.is_duplicate());
}

// Callback errors are caller-owned results, not transactional rollback promises.
#[test]
fn preparation_preserves_callback_result_and_partial_changes() {
    let mut values = DependencyValues::new();
    let result: Result<(), &str> = values
        .prepare(|values| {
            values.set::<Legacy>(9);
            Err("caller failure")
        })
        .unwrap();
    assert_eq!(result, Err("caller failure"));
    values
        .prepare(|values| {
            assert_eq!(*values.get_or_insert::<Legacy>(), 9);
            values.set::<Legacy>(8);
        })
        .unwrap();
    assert_eq!(*values.get_or_insert::<Legacy>(), 8);
}

// Clearing the cache must not clear admission history for unregistered keys.
#[test]
fn reset_does_not_reopen_registration_and_other_keys_remain_eligible() {
    let mut values = DependencyValues::new();
    assert_eq!(*values.get_or_insert::<Legacy>(), 1);
    values.reset_cache();
    assert!(
        values
            .register_defaults(DependencyDefaults::<Legacy>::new())
            .unwrap_err()
            .is_initialized()
    );
    values
        .register_defaults(DependencyDefaults::<Optional>::new().with_live(|| None))
        .unwrap();
    assert_eq!(values.get_or_insert::<Optional>(), &None);
    values.reset_cache();
    assert_eq!(values.get_or_insert::<Optional>(), &None);
}

// A callback panic retains mutations but no lookup means preparation is still eligible.
#[test]
fn callback_panic_retains_mutations_without_inventing_lookup() {
    let mut values = DependencyValues::new();
    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = values.prepare(|values| {
            values.set::<Legacy>(6);
            panic!("callback");
        });
    }))
    .expect_err("preparation must propagate the callback panic");
    assert_eq!(panic.downcast_ref::<&str>(), Some(&"callback"));

    values.prepare(|_| ()).unwrap();
    assert_eq!(*values.get_or_insert::<Legacy>(), 6);
}

// Removing a cached default through an override must not reopen registration.
#[test]
fn replacing_cached_default_does_not_reopen_registration() {
    let mut values = DependencyValues::new();
    assert_eq!(*values.get_or_insert::<Legacy>(), 1);
    values.set::<Legacy>(2);
    let error = values
        .register_defaults(DependencyDefaults::<Legacy>::new().with_live(|| 3))
        .expect_err("used default registration remains frozen");
    assert!(error.is_initialized());
    assert_eq!(*values.get_or_insert::<Legacy>(), 2);
}

// Retaining cleared values or releasing a sibling's cached owner breaks these counts.
#[test]
fn reset_releases_only_the_final_cached_owner() {
    /// Counts the final release without allowing the stored domain value to be cloned.
    struct CountedDrop(Arc<AtomicUsize>);

    impl Drop for CountedDrop {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Stores the destruction counter behind the environment's shared ownership.
    struct Counted;

    impl DependencyKey for Counted {
        type Value = CountedDrop;
    }

    let drops = Arc::new(AtomicUsize::new(0));
    let factory_drops = Arc::clone(&drops);
    let mut values = DependencyValues::new();
    values
        .register_defaults(
            DependencyDefaults::<Counted>::new()
                .with_live(move || CountedDrop(Arc::clone(&factory_drops))),
        )
        .unwrap();
    values.get_or_insert::<Counted>();
    assert_eq!(drops.load(Ordering::Relaxed), 0);
    values.reset_cache();
    assert_eq!(drops.load(Ordering::Relaxed), 1);
    values.reset_cache();
    assert_eq!(drops.load(Ordering::Relaxed), 1);

    values.get_or_insert::<Counted>();
    let mut sibling = values.clone();
    values.reset_cache();
    assert_eq!(drops.load(Ordering::Relaxed), 1);
    sibling.get_or_insert::<Counted>();
    assert_eq!(drops.load(Ordering::Relaxed), 1);
    sibling.reset_cache();
    assert_eq!(drops.load(Ordering::Relaxed), 2);
    drop(values);
    drop(sibling);
    assert_eq!(drops.load(Ordering::Relaxed), 2);
}

// Swallowing a cached destructor's panic or deferring its drop past reset breaks this.
#[test]
fn reset_propagates_the_cached_value_destructor_panic() {
    /// Panics on its first normal drop, but never adds a second panic during unwinding.
    struct PanickingDrop(Arc<AtomicUsize>);

    impl Drop for PanickingDrop {
        fn drop(&mut self) {
            // A failed assertion before reset drops us must not abort the test process.
            if self.0.fetch_add(1, Ordering::Relaxed) == 0 && !std::thread::panicking() {
                std::panic::panic_any("cached destructor");
            }
        }
    }

    /// Supplies a cached value whose original destructor panic must propagate.
    struct Panicking;

    impl DependencyKey for Panicking {
        type Value = PanickingDrop;
    }

    let drops = Arc::new(AtomicUsize::new(0));
    let factory_drops = Arc::clone(&drops);
    let mut values = DependencyValues::new();
    values
        .register_defaults(
            DependencyDefaults::<Panicking>::new()
                .with_live(move || PanickingDrop(Arc::clone(&factory_drops))),
        )
        .unwrap();
    values.get_or_insert::<Panicking>();
    assert_eq!(drops.load(Ordering::Relaxed), 0);
    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        values.reset_cache();
    }))
    .expect_err("reset must propagate the cached value's destructor panic");
    assert_eq!(panic.downcast_ref::<&str>(), Some(&"cached destructor"));
    assert_eq!(drops.load(Ordering::Relaxed), 1);
    drop(values);
    assert_eq!(drops.load(Ordering::Relaxed), 1);
}
