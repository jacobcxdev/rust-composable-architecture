//! Checks explicit endpoint stubs without replacing caller-owned results or failures.

#![cfg(feature = "test-util")]

use std::cell::{Cell, RefCell};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::rc::Rc;
use std::sync::Arc;

use rca_core::{EndpointStub, IssueCollector};
use rca_core::{Issue, IssueReporter, IssueReporters, IssueSeverity};
use rca_macros::DependencyClient;

// Eager reporting at construction or Drop would increment this counter.
#[test]
fn unused_stub_does_not_report() {
    let calls = Cell::new(0);
    {
        let stub = EndpointStub::new("Client.load", |_: Issue| calls.set(calls.get() + 1));
        let _debug = format!("{stub:?}");
    }
    assert_eq!(calls.get(), 0);
}

// Reordering, omitting, repeating or caching any stage breaks the literal sequence.
#[test]
fn each_invocation_reports_before_one_lazy_fallback() {
    let events = RefCell::new(Vec::new());
    let stub = EndpointStub::new("Client.load", |_: Issue| {
        events.borrow_mut().push("report");
        19
    });
    for value in [3, 7] {
        let outcome = stub.call_with(
            || {
                events.borrow_mut().push("diagnostic");
                Issue::new("unimplemented", "load")
            },
            || {
                events.borrow_mut().push("fallback");
                value
            },
        );
        assert_eq!(outcome, (value, 19));
    }
    assert_eq!(
        *events.borrow(),
        [
            "diagnostic",
            "report",
            "fallback",
            "diagnostic",
            "report",
            "fallback"
        ]
    );
}

// Locating the diagnostic at call_with/the factory would lose its definition origin.
#[test]
fn diagnostic_retains_stub_construction_origin() {
    let origin = line!() + 1;
    let stub = EndpointStub::new("Client.load", |issue: Issue| issue);
    let ((), issue) = stub.call_with(|| Issue::new("stub", "load"), || ());
    assert_eq!(issue.location.file(), file!());
    assert_eq!(issue.location.line(), origin);
    assert_eq!(issue.code, "stub");
}

/// Proves fallback errors need neither transport bounds nor `Clone` or `Debug`.
struct BorrowedError<'a> {
    /// Borrows the caller's original error text.
    text: &'a str,
    /// Makes the returned value's identity observable without requiring it to be `Send`.
    identity: Rc<Cell<u8>>,
}

// Cloning, stringifying or imposing transport bounds would reject/change this error.
#[test]
fn fallback_preserves_borrowed_move_only_typed_error() {
    let text = String::from("original domain failure");
    let identity = Rc::new(Cell::new(41));
    let original = BorrowedError {
        text: &text,
        identity: Rc::clone(&identity),
    };
    let stub = EndpointStub::new("Client.load", |_: Issue| ());
    let (result, ()) = stub.call_with(|| Issue::new("stub", "load"), || Err::<(), _>(original));
    let Err(error) = result else {
        panic!("typed failure became success")
    };
    assert_eq!(error.text, "original domain failure");
    assert!(Rc::ptr_eq(&error.identity, &identity));
}

// Treating an absent optional value as an unimplemented value changes success.
#[test]
fn fallback_preserves_successful_none() {
    let stub = EndpointStub::new("Client.optional", |_: Issue| ());
    let (result, ()) = stub.call_with(
        || Issue::new("stub", "optional"),
        || Ok::<Option<u8>, &'static str>(None),
    );
    assert_eq!(result, Ok(None));
}

// Rejected diagnostic delivery must not suppress a permitted fallback or lose evidence.
#[test]
fn disabled_delivery_retains_diagnostic_and_fallback() {
    let reporter = IssueReporter::default();
    let stub = EndpointStub::new("Client.load", |issue| reporter.report(issue));
    let calls = Cell::new(0);
    let (value, delivery) = stub.call_with(
        || {
            Issue::new("stub", "original")
                .with_severity(IssueSeverity::Error)
                .with_cause("cause")
        },
        || {
            calls.set(calls.get() + 1);
            23
        },
    );
    assert_eq!(value, 23);
    assert_eq!(calls.get(), 1);
    let rejected = delivery.unwrap_err();
    assert!(rejected.is_disabled());
    assert_eq!(rejected.to_string(), "issue reporting is disabled");
    let issue = rejected.into_issue();
    assert_eq!(issue.code, "stub");
    assert_eq!(issue.message, "original");
    assert_eq!(issue.severity, IssueSeverity::Error);
    assert_eq!(issue.cause.as_deref(), Some("cause"));
}

#[test]
fn closed_delivery_retains_diagnostic_and_fallback() {
    let collector = IssueCollector::new();
    let reporter = collector.reporter();
    assert!(collector.finish().is_empty());
    let stub = EndpointStub::new("Client.load", |issue| reporter.report(issue));
    let (value, delivery) = stub.call_with(|| Issue::new("stub", "closed"), || 29);
    assert_eq!(value, 29);
    let rejected = delivery.unwrap_err();
    assert!(rejected.is_closed());
    assert_eq!(rejected.to_string(), "issue reporter is closed");
    assert_eq!(rejected.into_issue().message, "closed");
}

// The generic delivery result is independent of the fallback result, not coerced.
#[test]
fn typed_delivery_failure_is_preserved_separately() {
    let text = String::from("delivery failure");
    let identity = Rc::new(Cell::new(1));
    let stub = EndpointStub::new("Client.load", |issue| {
        Err::<(), _>((issue, &text, Rc::clone(&identity)))
    });
    let (value, delivery) = stub.call_with(|| Issue::new("stub", "load"), || 31);
    assert_eq!(value, 31);
    let (issue, error, original) = delivery.unwrap_err();
    assert_eq!(issue.message, "load");
    assert!(std::ptr::eq(
        std::ptr::from_ref(error),
        std::ptr::from_ref(&text)
    ));
    assert!(Rc::ptr_eq(&original, &identity));
}

// Returning successfully when the sink rejects would turn a forbidden endpoint into a no-op.
#[test]
fn forbidden_reports_then_panics_with_disabled_sink() {
    let events = RefCell::new(Vec::new());
    let reporter = IssueReporter::default();
    let stub = EndpointStub::new("Client.forbidden", |issue| {
        events.borrow_mut().push("report");
        let delivery = reporter.report(issue).unwrap_err();
        assert!(delivery.is_disabled());
        delivery
    });
    let panic = catch_unwind(AssertUnwindSafe(|| {
        stub.forbidden(|| {
            events.borrow_mut().push("diagnostic");
            Issue::new("stub", "forbidden")
        })
    }))
    .expect_err("forbidden endpoint must fail even when reporting is disabled");
    assert_eq!(*events.borrow(), ["diagnostic", "report"]);
    assert_eq!(
        panic.downcast_ref::<String>().unwrap(),
        "forbidden dependency endpoint `Client.forbidden` invoked"
    );
}

#[test]
fn forbidden_reports_then_panics_with_closed_sink() {
    let collector = IssueCollector::new();
    let reporter = collector.reporter();
    let _issues = collector.finish();
    let reports = Cell::new(0);
    let stub = EndpointStub::new("Client.forbidden", |issue| {
        reports.set(reports.get() + 1);
        assert!(reporter.report(issue).unwrap_err().is_closed());
    });
    let panic = catch_unwind(AssertUnwindSafe(|| {
        stub.forbidden(|| Issue::new("stub", "forbidden"));
    }))
    .expect_err("forbidden endpoint must fail even when its reporter is closed");
    assert_eq!(
        panic.downcast_ref::<String>().unwrap(),
        "forbidden dependency endpoint `Client.forbidden` invoked"
    );
    assert_eq!(reports.get(), 1);
}

// A typed callback rejection is not permission to succeed at a forbidden endpoint.
#[test]
fn forbidden_fails_despite_typed_delivery_error() {
    let error = String::from("typed delivery rejection");
    let reports = Cell::new(0);
    let stub = EndpointStub::new("Client.forbidden", |_: Issue| {
        reports.set(reports.get() + 1);
        Err::<(), _>(&error)
    });
    let panic = catch_unwind(AssertUnwindSafe(|| {
        stub.forbidden(|| Issue::new("stub", "forbidden"));
    }))
    .expect_err("delivery error must not suppress mandatory failure");
    assert_eq!(reports.get(), 1);
    assert_eq!(
        panic.downcast_ref::<String>().unwrap(),
        "forbidden dependency endpoint `Client.forbidden` invoked"
    );
}

// Holding delivery until unwinding can turn its destructor panic into a double panic.
#[test]
fn forbidden_drops_delivery_before_starting_its_invariant_panic() {
    /// Records whether the delivery result is released before unwinding begins.
    struct Delivery<'a>(&'a Cell<Option<bool>>);

    impl Drop for Delivery<'_> {
        fn drop(&mut self) {
            self.0.set(Some(std::thread::panicking()));
        }
    }

    let dropped_while_panicking = Cell::new(None);
    let stub = EndpointStub::new("Client.forbidden", |_: Issue| {
        Delivery(&dropped_while_panicking)
    });
    let panic = catch_unwind(AssertUnwindSafe(|| {
        stub.forbidden(|| Issue::new("stub", "forbidden"));
    }))
    .expect_err("forbidden endpoint must still panic after delivery is dropped");

    assert_eq!(dropped_while_panicking.get(), Some(false));
    assert_eq!(
        panic.downcast_ref::<String>().unwrap(),
        "forbidden dependency endpoint `Client.forbidden` invoked"
    );
}

#[test]
fn forbidden_propagates_delivery_destructor_panic_without_double_panicking() {
    /// Makes dropping the delivery result fail before the stub's own panic starts.
    struct Delivery;

    impl Drop for Delivery {
        fn drop(&mut self) {
            // Wrong drop order must fail the payload assertion, not abort on a second panic.
            assert!(std::thread::panicking(), "delivery destructor panic");
        }
    }

    let stub = EndpointStub::new("Client.forbidden", |_: Issue| Delivery);
    let panic = catch_unwind(AssertUnwindSafe(|| {
        stub.forbidden(|| Issue::new("stub", "forbidden"));
    }))
    .expect_err("delivery destructor panic must propagate");

    assert_eq!(
        panic.downcast_ref::<&str>(),
        Some(&"delivery destructor panic")
    );
}

// Swallowing callback panic or evaluating the fallback first would run forbidden work.
#[test]
fn reporting_panic_propagates_without_evaluating_fallback() {
    let reporter = IssueReporter::new(|_| panic!("report panic"));
    let stub = EndpointStub::new("Client.load", |issue| reporter.report(issue));
    let calls = Cell::new(0);
    let panic = catch_unwind(AssertUnwindSafe(|| {
        stub.call_with(|| Issue::new("stub", "load"), || calls.set(calls.get() + 1))
    }))
    .expect_err("reporter panic must propagate");
    assert_eq!(panic.downcast_ref::<&str>(), Some(&"report panic"));
    assert_eq!(calls.get(), 0);
}

// Swallowing a fallback panic or reporting afterward would lose the admitted diagnostic.
#[test]
fn fallback_panic_propagates_after_reporting() {
    let collector = IssueCollector::new();
    let reporter = collector.reporter();
    let stub = EndpointStub::new("Client.load", |issue| reporter.report(issue));
    let panic = catch_unwind(AssertUnwindSafe(|| {
        stub.call_with(
            || Issue::new("stub", "before panic"),
            || panic!("fallback panic"),
        )
    }))
    .expect_err("fallback panic must propagate");
    assert_eq!(panic.downcast_ref::<&str>(), Some(&"fallback panic"));
    let issues = collector.finish();
    assert_eq!(issues.len(), 1);
    assert_eq!(issues[0].message, "before panic");
}

// Reporting or falling back after diagnostic creation panics would increment this counter.
#[test]
fn diagnostic_factory_panic_prevents_reporting_and_fallback() {
    let calls = Cell::new(0);
    let stub = EndpointStub::new("Client.load", |_: Issue| calls.set(calls.get() + 1));
    let panic = catch_unwind(AssertUnwindSafe(|| {
        stub.call_with(
            || panic!("diagnostic panic"),
            || calls.set(calls.get() + 10),
        )
    }))
    .expect_err("diagnostic panic must propagate");
    assert_eq!(panic.downcast_ref::<&str>(), Some(&"diagnostic panic"));
    assert_eq!(calls.get(), 0);
}

// Looking up later scope selection instead of capturing it would use the parent/disabled sink.
#[test]
fn owned_stub_retains_selected_reporters_after_lexical_scope() {
    let inherited = IssueReporters::default();
    let collector = IssueCollector::new();
    let selected = IssueReporters::new([collector.reporter()]);
    let stub = inherited.with_scope(Some(&selected), |context| {
        let owned = context.clone();
        EndpointStub::new("Client.load", move |issue| owned.report(issue))
    });
    drop(selected);
    let (value, deliveries) = stub.call_with(|| Issue::new("stub", "selected"), || 37);
    assert_eq!(value, 37);
    assert_eq!(deliveries.len(), 1);
    deliveries.into_iter().next().unwrap().unwrap();
    let issues = collector.finish();
    assert_eq!(issues.len(), 1);
    assert_eq!(issues[0].message, "selected");
}

// Eager async work or fallback creation before reporting breaks these counters/order.
#[test]
fn caller_async_wrapper_is_lazy_until_first_poll() {
    let events = RefCell::new(Vec::new());
    let stub = EndpointStub::new("Client.load", |_: Issue| events.borrow_mut().push("report"));
    let discarded = async {
        stub.call_with(
            || panic!("unpolled diagnostic"),
            || panic!("unpolled fallback"),
        )
    };
    drop(discarded);
    let future = async {
        let (fallback, ()) = stub.call_with(
            || {
                events.borrow_mut().push("diagnostic");
                Issue::new("stub", "async")
            },
            || {
                events.borrow_mut().push("fallback creation");
                async { 43 }
            },
        );
        fallback.await
    };
    assert!(events.borrow().is_empty());
    assert_eq!(futures_executor::block_on(future), 43);
    assert_eq!(
        *events.borrow(),
        ["diagnostic", "report", "fallback creation"]
    );
}

/// Combines an explicitly overridden stub with an untouched mandatory endpoint.
#[derive(DependencyClient)]
struct Client {
    /// Returns the caller's explicit typed failure after reporting its diagnostic.
    load: Arc<dyn Fn() -> Result<u8, String> + Send + Sync>,
    /// Must retain the derive's fail-closed default when only `load` is overridden.
    mandatory: Arc<dyn Fn() -> u8 + Send + Sync>,
}

// Installing one explicit stub must not make the other mandatory endpoint succeed.
#[test]
fn explicit_stub_override_keeps_unconfigured_client_endpoint_fail_closed() {
    let collector = IssueCollector::new();
    let reporter = collector.reporter();
    let stub = EndpointStub::new("Client.load", move |issue| reporter.report(issue));
    let client = Client {
        load: Arc::new(move || {
            let (result, delivery) = stub.call_with(
                || Issue::new("stub", "load"),
                || Err(String::from("typed failure")),
            );
            delivery.unwrap();
            result
        }),
        ..Client::test_value()
    };
    assert_eq!((client.load)(), Err(String::from("typed failure")));
    let panic = catch_unwind(AssertUnwindSafe(|| (client.mandatory)()))
        .expect_err("unconfigured mandatory endpoint must still fail");
    assert!(
        panic
            .downcast_ref::<String>()
            .unwrap()
            .starts_with(&format!(
                "unconfigured test dependency endpoint {}.mandatory (test configuration at ",
                std::any::type_name::<Client>()
            ))
    );
    let issues = collector.finish();
    assert_eq!(issues.len(), 1);
    assert_eq!(issues[0].message, "load");
}
