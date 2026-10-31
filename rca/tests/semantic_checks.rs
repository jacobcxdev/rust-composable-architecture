//! Keeps semantic comparison, private operation errors and lazy callbacks independent.

#![cfg(feature = "test-util")]

use std::cell::Cell;

use rca::{
    ChangeCheck, ComparisonFailure, check_change, check_change_async, check_equal,
    check_expected_changes,
};

#[test]
fn facade_checks_semantics_without_rendered_equality_or_error_masking() {
    /// Borrows a private cause without requiring error formatting or a static lifetime.
    struct LocalError<'a>(&'a str);

    let failure: ComparisonFailure =
        check_equal(|| 1, || 2, |_| "[private]".into(), || None).unwrap_err();
    assert!(failure.is_mismatch());
    assert!(failure.excerpt().unwrap().contains("semantically"));

    check_expected_changes(|| 1, || 2, |value| *value += 1, i32::to_string, || None).unwrap();

    let private = String::from("private operation error");
    let mut value = 0;
    let checked: ChangeCheck<(), LocalError<'_>> = check_change(
        &mut value,
        |value| *value,
        |value| {
            *value += 1;
            Err(LocalError(&private))
        },
        |value| *value += 1,
        |_| panic!("successful comparison must not render"),
        || panic!("successful comparison must not construct a message"),
    );
    assert!(!format!("{checked:?}").contains(&private));
    match checked.operation_result {
        Err(error) => assert_eq!(error.0, private),
        Ok(()) => panic!("typed operation error must survive comparison success"),
    }
    checked.comparison.unwrap();
}

#[test]
fn facade_async_check_does_no_work_when_unpolled() {
    let called = Cell::new(false);
    let mut value = 0;
    let future = check_change_async(
        &mut value,
        |value| *value,
        async |value| {
            called.set(true);
            *value += 1;
            Ok::<_, ()>(())
        },
        |value| *value += 1,
        i32::to_string,
        || None,
    );
    drop(future);
    assert!(!called.get());
    assert_eq!(value, 0);
}
