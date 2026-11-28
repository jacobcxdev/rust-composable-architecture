//! Checks issue evidence separately from body errors and unpolled asynchronous work.

#![cfg(feature = "test-util")]

use rca::{
    Issue, IssueCollector, IssueExpectation, IssueExpectationError, IssueExpectationMode,
    expect_issues, expect_issues_async,
};
use std::cell::Cell;

#[test]
fn facade_keeps_operation_errors_separate_from_required_issue_evidence() {
    let collector = IssueCollector::new();
    let late = collector.reporter();
    let original_error = String::from("operation failed");
    let outcome: IssueExpectation<(), &str> = expect_issues(
        collector,
        IssueExpectationMode::Strict,
        |issue| issue.code == "known",
        |reporter| {
            reporter
                .report(Issue::new("unexpected", "retained evidence"))
                .unwrap();
            Err::<(), _>(original_error.as_str())
        },
    );
    assert_eq!(outcome.body_result, Err(original_error.as_str()));
    let failure: IssueExpectationError = outcome.expectation.unwrap_err();
    assert!(failure.is_missing());
    assert_eq!(failure.unexpected_issues().len(), 1);
    assert_eq!(failure.unexpected_issues()[0].code, "unexpected");
    assert!(
        late.report(Issue::new("late", "closed"))
            .unwrap_err()
            .is_closed()
    );
}

#[test]
fn facade_async_scope_owns_closure_before_body_runs() {
    let collector = IssueCollector::new();
    let late = collector.reporter();
    let body_ran = Cell::new(false);
    let future = expect_issues_async(
        collector,
        IssueExpectationMode::Intermittent,
        |_| true,
        |_| {
            body_ran.set(true);
            std::future::ready(Ok::<_, ()>(()))
        },
    );
    drop(future);
    assert!(!body_ran.get());
    assert!(
        late.report(Issue::new("late", "closed"))
            .unwrap_err()
            .is_closed()
    );
}
