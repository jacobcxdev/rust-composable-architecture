//! Checks lazy defaults, policy selection, private diagnostics, and rejected registration.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use rca_core::{DependencyDefaultPolicy, DependencyDefaults};
use rca_core::{DependencyKey, DependencyValues};

/// Requires an explicit value or an owned default factory.
struct InterfaceKey;

impl DependencyKey for InterfaceKey {
    type Value = Arc<str>;
}

/// Supplies trait defaults to check compatibility with registered factories.
struct LegacyKey;

impl DependencyKey for LegacyKey {
    type Value = &'static str;

    fn live_value() -> Self::Value {
        "legacy live"
    }

    fn test_value() -> Self::Value {
        "legacy test"
    }
}

// Eager construction, wrong-policy selection, or recreating cached defaults breaks this.
#[test]
fn owned_factories_are_lazy_selected_once_and_cached() {
    for (policy, expected, selected) in [
        (DependencyDefaultPolicy::Live, "owned live", 0),
        (DependencyDefaultPolicy::Test, "owned test", 1),
        (DependencyDefaultPolicy::Preview, "owned preview", 2),
    ] {
        let calls = Arc::new([
            AtomicUsize::new(0),
            AtomicUsize::new(0),
            AtomicUsize::new(0),
        ]);
        let live_calls = Arc::clone(&calls);
        let test_calls = Arc::clone(&calls);
        let preview_calls = Arc::clone(&calls);
        let defaults = DependencyDefaults::<InterfaceKey>::new()
            .with_live(move || {
                live_calls[0].fetch_add(1, Ordering::Relaxed);
                Arc::from("owned live")
            })
            .with_test(move || {
                test_calls[1].fetch_add(1, Ordering::Relaxed);
                Arc::from("owned test")
            })
            .with_preview(move || {
                preview_calls[2].fetch_add(1, Ordering::Relaxed);
                Arc::from("owned preview")
            });
        let mut independent = DependencyValues::for_policy(policy);
        independent.register_defaults(defaults.clone()).unwrap();
        let mut values = DependencyValues::for_policy(policy);
        values.register_defaults(defaults).unwrap();
        assert_eq!(
            calls.each_ref().map(|count| count.load(Ordering::Relaxed)),
            [0, 0, 0]
        );
        let first = Arc::clone(values.get_or_insert::<InterfaceKey>());
        assert_eq!(first.as_ref(), expected);
        assert!(Arc::ptr_eq(&first, values.get_or_insert::<InterfaceKey>()));
        for (index, count) in calls.iter().enumerate() {
            assert_eq!(
                count.load(Ordering::Relaxed),
                usize::from(index == selected)
            );
        }
        let mut clone = values.clone();
        assert!(Arc::ptr_eq(&first, clone.get_or_insert::<InterfaceKey>()));
        values.reset_cache();
        for (index, count) in calls.iter().enumerate() {
            assert_eq!(
                count.load(Ordering::Relaxed),
                usize::from(index == selected)
            );
        }
        let fresh = Arc::clone(values.get_or_insert::<InterfaceKey>());
        assert_eq!(fresh.as_ref(), expected);
        assert!(!Arc::ptr_eq(&first, &fresh));
        assert!(Arc::ptr_eq(&fresh, values.get_or_insert::<InterfaceKey>()));
        assert!(Arc::ptr_eq(&first, clone.get_or_insert::<InterfaceKey>()));
        for (index, count) in calls.iter().enumerate() {
            assert_eq!(
                count.load(Ordering::Relaxed),
                if index == selected { 2 } else { 0 }
            );
        }
        let independent_value = independent.get_or_insert::<InterfaceKey>();
        assert_eq!(independent_value.as_ref(), expected);
        assert!(!Arc::ptr_eq(&first, independent_value));
        assert!(!Arc::ptr_eq(&fresh, independent_value));
        for (index, count) in calls.iter().enumerate() {
            assert_eq!(
                count.load(Ordering::Relaxed),
                if index == selected { 3 } else { 0 }
            );
        }
    }
}

// Replacing an existing registration or losing rejected closures breaks this.
#[test]
fn duplicate_registration_retains_original_and_recovers_submitted_bundle() {
    let mut values = DependencyValues::new();
    values
        .register_defaults(
            DependencyDefaults::<InterfaceKey>::new().with_live(|| Arc::from("first")),
        )
        .unwrap();
    let error = values
        .register_defaults(
            DependencyDefaults::<InterfaceKey>::new().with_live(|| Arc::from("second")),
        )
        .unwrap_err();
    assert!(error.is_duplicate());
    assert!(!error.is_initialized());
    assert_eq!(error.key_name(), std::any::type_name::<InterfaceKey>());
    assert_eq!(
        error.to_string().lines().next().unwrap(),
        format!(
            "dependency `{}`: defaults already registered",
            std::any::type_name::<InterfaceKey>()
        )
    );
    let _ = error.backtrace();
    let mut other = DependencyValues::new();
    other.register_defaults(error.into_defaults()).unwrap();
    assert_eq!(values.get_or_insert::<InterfaceKey>().as_ref(), "first");
    assert_eq!(other.get_or_insert::<InterfaceKey>().as_ref(), "second");
}

#[test]
fn dependency_diagnostics_preserve_private_lazy_defaults() {
    let calls = Arc::new([AtomicUsize::new(0), AtomicUsize::new(0)]);
    let first: Arc<str> = Arc::from("private-original-value");
    let second: Arc<str> = Arc::from("private-rejected-value");
    let defaults = DependencyDefaults::<InterfaceKey>::default();
    let empty_debug = format!("{defaults:?}");
    assert!(empty_debug.contains("live: false"));

    let first_calls = Arc::clone(&calls);
    let first_capture = Arc::clone(&first);
    let defaults = defaults.with_live(move || {
        first_calls[0].fetch_add(1, Ordering::Relaxed);
        Arc::clone(&first_capture)
    });
    let configured_debug = format!("{defaults:?}");
    assert!(configured_debug.contains("live: true"));
    let mut values = DependencyValues::new();
    values.register_defaults(defaults).unwrap();

    let expected_fields = if cfg!(feature = "test-util") {
        "DependencyValues { len: 0, default_len: 0, defaults_len: 1, policy: Live, used: false, attempted_defaults_len: 0, test_len: 0 }"
    } else {
        "DependencyValues { len: 0, default_len: 0, defaults_len: 1, policy: Live, used: false, attempted_defaults_len: 0 }"
    };
    assert_eq!(format!("{values:?}"), expected_fields);
    assert_eq!(
        calls.each_ref().map(|count| count.load(Ordering::Relaxed)),
        [0, 0]
    );

    let second_calls = Arc::clone(&calls);
    let second_capture = Arc::clone(&second);
    let rejected = DependencyDefaults::<InterfaceKey>::default().with_live(move || {
        second_calls[1].fetch_add(1, Ordering::Relaxed);
        Arc::clone(&second_capture)
    });
    let error = values.register_defaults(rejected).unwrap_err();
    assert!(error.is_duplicate());
    assert!(!error.is_initialized());
    let error_debug = format!("{error:?}");
    assert!(error_debug.contains(error.key_name()));
    for rendered in [
        configured_debug,
        format!("{values:?}"),
        error_debug,
        error.to_string(),
    ] {
        assert!(!rendered.contains(first.as_ref()));
        assert!(!rendered.contains(second.as_ref()));
    }
    assert_eq!(
        calls.each_ref().map(|count| count.load(Ordering::Relaxed)),
        [0, 0]
    );

    let mut recovered = DependencyValues::new();
    recovered.register_defaults(error.into_defaults()).unwrap();
    assert!(Arc::ptr_eq(&first, values.get_or_insert::<InterfaceKey>()));
    assert!(Arc::ptr_eq(
        &second,
        recovered.get_or_insert::<InterfaceKey>()
    ));
    for rendered in [format!("{values:?}"), format!("{recovered:?}")] {
        assert!(!rendered.contains(first.as_ref()));
        assert!(!rendered.contains(second.as_ref()));
    }
    assert!(Arc::ptr_eq(&first, values.get_or_insert::<InterfaceKey>()));
    assert!(Arc::ptr_eq(
        &second,
        recovered.get_or_insert::<InterfaceKey>()
    ));
    assert_eq!(
        calls.each_ref().map(|count| count.load(Ordering::Relaxed)),
        [1, 1]
    );
}

// A late registration cannot silently change or be ignored by an initialised cache.
#[test]
fn initialized_default_rejects_registration_but_explicit_override_does_not() {
    let mut values = DependencyValues::new();
    assert_eq!(*values.get_or_insert::<LegacyKey>(), "legacy live");
    let error = values
        .register_defaults(DependencyDefaults::<LegacyKey>::new().with_live(|| "registered"))
        .unwrap_err();
    assert!(error.is_initialized());
    assert!(!error.is_duplicate());
    assert_eq!(
        error.to_string().lines().next().unwrap(),
        format!(
            "dependency `{}`: default already attempted",
            std::any::type_name::<LegacyKey>()
        )
    );
    assert_eq!(*values.get_or_insert::<LegacyKey>(), "legacy live");
    let mut explicit = DependencyValues::new();
    explicit.set::<LegacyKey>("explicit");
    explicit.register_defaults(error.into_defaults()).unwrap();
    assert_eq!(*explicit.get_or_insert::<LegacyKey>(), "explicit");
}

// Test and Preview must not select the supplied live factory when their factory is absent.
#[test]
fn missing_test_and_preview_factories_fail_closed_with_named_key() {
    for policy in [
        DependencyDefaultPolicy::Test,
        DependencyDefaultPolicy::Preview,
    ] {
        let calls = Arc::new(AtomicUsize::new(0));
        let live_calls = Arc::clone(&calls);
        let mut values = DependencyValues::for_policy(policy);
        values
            .register_defaults(
                DependencyDefaults::<InterfaceKey>::new().with_live(move || {
                    live_calls.fetch_add(1, Ordering::Relaxed);
                    Arc::from("forbidden live")
                }),
            )
            .unwrap();
        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = values.get_or_insert::<InterfaceKey>();
        }))
        .unwrap_err();
        let message = panic.downcast_ref::<String>().unwrap();
        assert_eq!(
            message,
            &format!(
                "missing test dependency `{}`; inject a value or implement DependencyKey::test_value",
                std::any::type_name::<InterfaceKey>()
            )
        );
        assert_eq!(calls.load(Ordering::Relaxed), 0);
    }
}

#[test]
fn interface_only_live_lookup_fails_closed_and_legacy_preview_uses_test() {
    let panic = std::panic::catch_unwind(|| {
        let _ = DependencyValues::new().get_or_insert::<InterfaceKey>();
    })
    .unwrap_err();
    assert_eq!(
        panic.downcast_ref::<String>().unwrap(),
        &format!(
            "missing live dependency `{}`; inject a value or register live defaults",
            std::any::type_name::<InterfaceKey>()
        )
    );
    assert_eq!(
        *DependencyValues::for_policy(DependencyDefaultPolicy::Preview)
            .get_or_insert::<LegacyKey>(),
        "legacy test"
    );
}

/// Distinguishes an explicit absent value from an unresolved dependency.
struct OptionalKey;

impl DependencyKey for OptionalKey {
    type Value = Option<Arc<str>>;
}

// Explicit None is a value, not permission to invoke the selected default.
#[test]
fn explicit_none_masks_registered_defaults_and_overriding_preserves_original() {
    let mut values = DependencyValues::for_policy(DependencyDefaultPolicy::Test);
    values
        .register_defaults(
            DependencyDefaults::<OptionalKey>::new().with_test(|| Some(Arc::from("fixture"))),
        )
        .unwrap();
    let mut masked = values.overriding::<OptionalKey>(None);
    assert_eq!(masked.get_or_insert::<OptionalKey>(), &None);
    assert_eq!(
        values.get_or_insert::<OptionalKey>().as_deref(),
        Some("fixture")
    );
}

// Missing preview must reuse registered test construction, and a returned None is final.
#[test]
fn registered_test_defaults_supply_preview_without_live_fallback_including_none() {
    let mut preview = DependencyValues::for_policy(DependencyDefaultPolicy::Preview);
    preview
        .register_defaults(
            DependencyDefaults::<InterfaceKey>::new()
                .with_live(|| panic!("preview must not call live"))
                .with_test(|| Arc::from("registered test")),
        )
        .unwrap();
    assert_eq!(
        preview.get_or_insert::<InterfaceKey>().as_ref(),
        "registered test"
    );
    let calls = Arc::new(AtomicUsize::new(0));
    let test_calls = Arc::clone(&calls);
    let mut absent = DependencyValues::for_policy(DependencyDefaultPolicy::Preview);
    absent
        .register_defaults(
            DependencyDefaults::<OptionalKey>::new()
                .with_live(|| panic!("None must not fall back to live"))
                .with_test(move || {
                    test_calls.fetch_add(1, Ordering::Relaxed);
                    None
                }),
        )
        .unwrap();
    assert_eq!(absent.get_or_insert::<OptionalKey>(), &None);
    assert_eq!(absent.get_or_insert::<OptionalKey>(), &None);
    assert_eq!(calls.load(Ordering::Relaxed), 1);
}

// An explicit replacement must release the environment's old cached default ownership.
#[test]
fn replacing_a_cached_default_releases_its_value() {
    let mut values = DependencyValues::new();
    values
        .register_defaults(
            DependencyDefaults::<InterfaceKey>::new().with_live(|| Arc::from("cached")),
        )
        .unwrap();
    let cached = Arc::downgrade(values.get_or_insert::<InterfaceKey>());
    values.set::<InterfaceKey>(Arc::from("replacement"));
    assert!(cached.upgrade().is_none());
    assert_eq!(
        values.get_or_insert::<InterfaceKey>().as_ref(),
        "replacement"
    );
}

#[test]
fn failed_registered_constructor_retries_without_caching_or_reopening_registration() {
    let calls = Arc::new(AtomicUsize::new(0));
    let constructor_calls = Arc::clone(&calls);
    let mut values = DependencyValues::new();
    values
        .register_defaults(
            DependencyDefaults::<InterfaceKey>::new().with_live(move || {
                assert_ne!(
                    constructor_calls.fetch_add(1, Ordering::Relaxed),
                    0,
                    "first construction fails"
                );
                Arc::from("recovered")
            }),
        )
        .unwrap();
    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        values.get_or_insert::<InterfaceKey>();
    }))
    .expect_err("first construction must propagate its panic");
    assert!(
        panic
            .downcast_ref::<String>()
            .unwrap()
            .contains("first construction fails")
    );

    let rejected = values
        .register_defaults(DependencyDefaults::<InterfaceKey>::new())
        .expect_err("the original bundle must remain registered");
    assert!(rejected.is_duplicate());
    assert!(!rejected.is_initialized());
    assert_eq!(calls.load(Ordering::Relaxed), 1);
    assert_eq!(values.get_or_insert::<InterfaceKey>().as_ref(), "recovered");
    assert_eq!(values.get_or_insert::<InterfaceKey>().as_ref(), "recovered");
    assert_eq!(calls.load(Ordering::Relaxed), 2);
}
