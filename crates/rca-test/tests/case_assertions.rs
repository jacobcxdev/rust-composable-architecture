//! Checks borrowed case extraction, callback failures and diagnostic output.

use rca_test::{check_case, check_case_with};
use std::cell::Cell;

/// Borrows text without providing `Clone`, `Debug` or equality.
struct Payload<'a>(&'a str);

/// Provides one matching case and one case without a payload.
enum Domain<'a> {
    /// Retains the borrowed payload that extraction must not copy.
    Value(Payload<'a>),
    /// Prevents the payload callback from running.
    Other,
}

/// Borrows the original payload from the matching case.
fn extract<'a, 'b>(root: &'b Domain<'a>) -> Option<&'b Payload<'a>> {
    match root {
        Domain::Value(value) => Some(value),
        Domain::Other => None,
    }
}

#[test]
fn matching_case_returns_original_local_non_debug_payload() {
    let label = String::from("local");
    let root = Domain::Value(Payload(&label));
    let calls = Cell::new(0);
    let value = check_case(&root, |root| {
        calls.set(calls.get() + 1);
        extract(root)
    })
    .unwrap();
    assert_eq!(value.0, "local");
    assert!(std::ptr::eq(value, extract(&root).unwrap()));
    assert_eq!(calls.get(), 1);
}

#[test]
fn wrong_case_fails_at_caller_without_running_payload_check() {
    let root = Domain::Other;
    let line = line!() + 2;
    let failure =
        check_case_with(&root, extract, |_| -> Result<(), ()> { panic!("check") }).unwrap_err();
    assert_eq!(failure.location().line(), line);
    assert!(failure.to_string().contains("case"));
    let _ = failure.backtrace();
}

#[test]
fn case_failure_display_stops_when_the_destination_rejects_output() {
    use std::fmt::{self, Write};

    /// Counts write attempts and rejects every attempt.
    struct RejectOutput(usize);

    impl Write for RejectOutput {
        fn write_str(&mut self, _: &str) -> fmt::Result {
            self.0 += 1;
            Err(fmt::Error)
        }
    }

    let failure = check_case(&None::<usize>, Option::as_ref).unwrap_err();
    let origin = failure.location();
    let trace = failure.backtrace();
    let mut output = RejectOutput(0);
    assert_eq!(write!(&mut output, "{failure}"), Err(fmt::Error));
    assert_eq!(output.0, 1, "formatting must stop after the first failure");
    assert!(std::ptr::eq(failure.location(), origin));
    assert!(std::ptr::eq(failure.backtrace(), trace));
    assert!(failure.to_string().contains("Required case did not match"));
}

#[test]
fn typed_check_error_is_distinct_from_case_failure() {
    /// Keeps the callback error distinct without a formatting requirement.
    struct LocalError;

    let label = String::from("local");
    let root = Domain::Value(Payload(&label));
    let checks = Cell::new(0);
    let outcome = check_case_with(&root, extract, |_| {
        checks.set(checks.get() + 1);
        Err::<(), _>(LocalError)
    });
    assert!(matches!(outcome, Ok(Err(LocalError))));
    assert_eq!(checks.get(), 1);
    assert_eq!(
        check_case_with(&Some(3), Option::as_ref, |value| Ok::<_, ()>(*value + 1)).unwrap(),
        Ok(4)
    );
    check_case(&None::<usize>, Option::as_ref).unwrap_err();
}

#[test]
fn payload_semantic_check_failure_remains_an_inner_error() {
    let root = Some(4);
    let result = check_case_with(&root, Option::as_ref, |value| {
        rca_test::check_equal(|| 3, || *value, i32::to_string, || None)
    });
    assert!(result.unwrap().unwrap_err().is_mismatch());
}
