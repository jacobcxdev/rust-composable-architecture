//! Distinguishes transactional edits, owned operation errors and missing enum cases.

#![cfg(feature = "test-util")]

use std::rc::Rc;

use rca::{CaseAssertionFailure, CaseStatePath, check_case, check_case_with};

/// Requires an explicit capture because it deliberately does not implement `Clone`.
struct Payload(String);

/// Separates a present payload from a genuinely different enum case.
enum State {
    Value(Payload),
    Idle,
}

#[test]
fn facade_case_transaction_keeps_owned_errors_and_strict_case_checks_distinct() {
    let path = CaseStatePath::new(
        |state: &State| match state {
            State::Value(value) => Some(value),
            State::Idle => None,
        },
        |state: &mut State| match state {
            State::Value(value) => Some(value),
            State::Idle => None,
        },
    );
    let mut state = State::Value(Payload("original".into()));
    let error = Rc::new("typed private failure");
    let result = path.try_modify_captured(
        &mut state,
        |value| Payload(value.0.clone()),
        |mut value| {
            value.0.push_str(" must not commit");
            Err(Rc::clone(&error))
        },
    );
    assert!(Rc::ptr_eq(&result.unwrap_err(), &error));
    assert_eq!(
        check_case(&state, |state| path.get(state)).unwrap().0,
        "original"
    );
    let checked = check_case_with(
        &state,
        |state| path.get(state),
        |value| Err::<(), _>(value.0.as_str()),
    );
    assert_eq!(checked.unwrap(), Err("original"));

    let committed = path.try_modify_captured(
        &mut state,
        |value| Payload(value.0.clone()),
        |mut value| {
            value.0.push_str(" committed");
            Ok::<_, ()>(value)
        },
    );
    assert_eq!(committed, Ok(true));
    assert_eq!(
        check_case(&state, |state| path.get(state)).unwrap().0,
        "original committed"
    );

    let missing: CaseAssertionFailure = match check_case(&State::Idle, |state| path.get(state)) {
        Ok(_) => panic!("wrong case must fail independently of diagnostic configuration"),
        Err(error) => error,
    };
    assert!(missing.location().file().ends_with("case_transactions.rs"));
}
