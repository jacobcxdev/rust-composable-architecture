//! Keeps cache reset, preparation eligibility and previously captured handles distinct.

use std::cell::Cell;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use rca::{DependencyDefaults, DependencyKey, DependencyPreparationError, DependencyValues};

/// Uses shared allocation identity to distinguish retained values from fresh defaults.
struct Name;

impl DependencyKey for Name {
    type Value = Arc<String>;
}

#[test]
fn facade_reset_is_local_and_does_not_reopen_preparation_or_revoke_handles() {
    let calls = Arc::new(AtomicUsize::new(0));
    let factory_calls = Arc::clone(&calls);
    let mut values = DependencyValues::new();
    values
        .prepare(|values| {
            values
                .register_defaults(DependencyDefaults::<Name>::new().with_live(move || {
                    let count = factory_calls.fetch_add(1, Ordering::SeqCst);
                    Arc::new(format!("value {count}"))
                }))
                .unwrap();
        })
        .unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    let original = Arc::clone(values.get_or_insert::<Name>());
    let mut other = values.clone();
    values.reset_cache();
    let next = Arc::clone(values.get_or_insert::<Name>());
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(original.as_str(), "value 0");
    assert_eq!(next.as_str(), "value 1");
    assert!(!Arc::ptr_eq(&original, &next));
    assert!(Arc::ptr_eq(other.get_or_insert::<Name>(), &original));
    let invoked = Cell::new(false);
    let _error: DependencyPreparationError = values.prepare(|_| invoked.set(true)).unwrap_err();
    assert!(!invoked.get());
}
