//! Keeps endpoint failure and diagnostic-delivery failure independent through the façade.

#![cfg(feature = "test-util")]

use rca::{EndpointStub, Issue, IssueReporter};
use std::cell::Cell;

#[test]
fn facade_stub_preserves_typed_failure_and_rejected_issue_evidence() {
    let fallback_calls = Cell::new(0);
    let definition_line = line!() + 1;
    let stub = EndpointStub::new("provider.read", |issue| {
        IssueReporter::default().report(issue)
    });
    let (result, delivery) = stub.call_with(
        || Issue::new("unimplemented", "explicit test endpoint"),
        || {
            fallback_calls.set(fallback_calls.get() + 1);
            Err::<(), _>("original domain error")
        },
    );
    assert_eq!(result, Err("original domain error"));
    assert_eq!(fallback_calls.get(), 1);
    let rejected = delivery.unwrap_err();
    assert!(rejected.is_disabled());
    let issue = rejected.into_issue();
    assert_eq!(issue.code, "unimplemented");
    assert_eq!(issue.location.line(), definition_line);
}

#[test]
fn facade_forbidden_stub_cannot_succeed_with_disabled_reporting() {
    let stub = EndpointStub::new("provider.forbidden", |issue| {
        IssueReporter::default().report(issue)
    });
    let failed = std::panic::catch_unwind(|| {
        stub.forbidden(|| Issue::new("forbidden", "no fallback permitted"));
    })
    .unwrap_err();
    assert_eq!(
        failed.downcast_ref::<String>().map(String::as_str),
        Some("forbidden dependency endpoint `provider.forbidden` invoked")
    );
}
