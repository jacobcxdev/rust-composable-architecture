//! Checks imported dependency caches, registered defaults and explicit optional values.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use rca_core::{
    DependencyDefaultPolicy, DependencyDefaults, DependencyKey, DependencyValues, Effect,
    ReduceContext, Reducer,
};
use rca_test::TestStore;

/// Uses trait defaults with different live and test values.
struct LegacyKey;

impl DependencyKey for LegacyKey {
    type Value = &'static str;

    fn live_value() -> Self::Value {
        "live"
    }

    fn test_value() -> Self::Value {
        "test"
    }
}

/// Copies the selected trait default into fully comparable state.
struct ReadLegacy;

impl Reducer for ReadLegacy {
    type State = &'static str;
    type Action = ();

    fn reduce(
        &mut self,
        state: &mut Self::State,
        (): &(),
        context: &mut ReduceContext<'_>,
    ) -> Effect<()> {
        *state = *context.dependency::<LegacyKey>();
        Effect::none()
    }
}

/// Checks that an imported live cache cannot replace a test default.
#[test]
fn imported_live_cache_cannot_become_a_test_default() {
    let mut environment = DependencyValues::new();
    assert_eq!(*environment.get_or_insert::<LegacyKey>(), "live");

    futures_executor::block_on(async {
        let mut store = TestStore::builder("", ReadLegacy)
            .with_dependency_values(environment)
            .build();
        let _ = store.send_with((), |state| *state = "test").await.unwrap();
        store.finish().await.unwrap();
    });
}

/// Requires registered defaults and distinguishes explicit `None` from absence.
struct RegisteredKey;

impl DependencyKey for RegisteredKey {
    type Value = Option<&'static str>;
}

/// Copies a registered optional value without changing its meaning.
struct ReadRegistered;

impl Reducer for ReadRegistered {
    type State = Option<&'static str>;
    type Action = ();

    fn reduce(
        &mut self,
        state: &mut Self::State,
        (): &(),
        context: &mut ReduceContext<'_>,
    ) -> Effect<()> {
        *state = *context.dependency::<RegisteredKey>();
        Effect::none()
    }
}

/// Checks test-policy selection without rebuilding an existing test cache.
#[test]
fn test_store_uses_registered_test_factories_after_environment_replacement() {
    for (policy, initial, expected_test_calls) in [
        (DependencyDefaultPolicy::Live, "live", 1),
        (DependencyDefaultPolicy::Test, "test", 1),
        (DependencyDefaultPolicy::Preview, "preview", 1),
    ] {
        let calls = Arc::new(AtomicUsize::new(0));
        let test_calls = Arc::clone(&calls);
        let mut values = DependencyValues::for_policy(policy);
        values
            .register_defaults(
                DependencyDefaults::<RegisteredKey>::new()
                    .with_live(|| Some("live"))
                    .with_test(move || {
                        test_calls.fetch_add(1, Ordering::Relaxed);
                        Some("test")
                    })
                    .with_preview(|| Some("preview")),
            )
            .unwrap();
        assert_eq!(*values.get_or_insert::<RegisteredKey>(), Some(initial));

        futures_executor::block_on(async {
            let mut store = TestStore::builder(None, ReadRegistered)
                .with_dependency_values(values)
                .build();
            let _ = store
                .send_with((), |state| *state = Some("test"))
                .await
                .unwrap();
            let _ = store.send(()).await.unwrap();
            store.finish().await.unwrap();
        });

        assert_eq!(calls.load(Ordering::Relaxed), expected_test_calls);
    }
}

/// Checks imported `None` masking and later explicit test precedence.
#[test]
fn imported_explicit_none_masks_factories_and_later_test_overrides_still_win() {
    futures_executor::block_on(async {
        let mut values = DependencyValues::new();
        values
            .register_defaults(
                DependencyDefaults::<RegisteredKey>::new()
                    .with_live(|| Some("live"))
                    .with_test(|| panic!("masked test factory must not run")),
            )
            .unwrap();
        assert_eq!(*values.get_or_insert::<RegisteredKey>(), Some("live"));
        values.set::<RegisteredKey>(None);

        let mut store = TestStore::builder(Some("initial"), ReadRegistered)
            .with_dependency_values(values.clone())
            .build();
        let _ = store.send_with((), |state| *state = None).await.unwrap();
        store.finish().await.unwrap();

        let mut overridden = TestStore::builder(None, ReadRegistered)
            .with_dependency_values(values)
            .with_dependency::<RegisteredKey>(Some("explicit test"))
            .build();
        let _ = overridden
            .send_with((), |state| *state = Some("explicit test"))
            .await
            .unwrap();
        overridden.finish().await.unwrap();
    });
}
