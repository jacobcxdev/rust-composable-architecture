//! Checks explicit issue delivery, borrowed operation results, and caller-polled futures.

use std::cell::{Cell, RefCell};
use std::convert::Infallible;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::rc::Rc;
use std::task::{Context, Poll, Waker};

use rca_core::{Issue, IssueReporter};
use rca_core::{IssueCallback, IssueContext, IssueDelivery, IssueDeliveryFailure, IssueSeverity};

#[test]
fn disabled_endpoint_recovers_severity_cause_and_original_location() {
    let issue = Issue::new("parse", String::from("bad input"))
        .with_severity(IssueSeverity::Error)
        .with_cause(String::from("invalid byte at 3"));
    let location = issue.location;
    let recovered = IssueReporter::default()
        .report(issue)
        .unwrap_err()
        .into_issue();
    assert_eq!(recovered.severity, IssueSeverity::Error);
    assert_eq!(recovered.cause.as_deref(), Some("invalid byte at 3"));
    assert_eq!(recovered.code, "parse");
    assert_eq!(recovered.message, "bad input");
    assert_eq!(recovered.location, location);
}

/// Keeps an operation error borrowed and local while making cancellation explicit.
#[derive(Debug)]
struct BorrowedError<'a> {
    /// Identifies the original caller-owned value without copying it.
    payload: &'a Rc<usize>,
    /// Selects the cancellation path independently of the payload.
    cancelled: bool,
}

/// Reads either standard text panic payload without accepting an unrelated payload type.
fn panic_message(payload: &(dyn std::any::Any + Send)) -> &str {
    if let Some(message) = payload.downcast_ref::<&str>() {
        message
    } else {
        payload
            .downcast_ref::<String>()
            .expect("fixture must panic with text")
    }
}

#[test]
fn sync_error_keeps_original_borrow_and_reports_lazy_issue_once_at_caller() {
    let payload = Rc::new(7);
    let operations = Cell::new(0);
    let messages = Cell::new(0);
    let observed = RefCell::new(Vec::new());
    let reporter = |issue: &Issue, error: &BorrowedError<'_>| {
        assert!(Rc::ptr_eq(error.payload, &payload));
        observed.borrow_mut().push(issue.clone());
        Ok::<_, IssueDeliveryFailure<Infallible>>(())
    };
    let reporters = [&reporter as &_];
    let context = IssueContext::new(&reporters);
    let caller_line = line!() + 1;
    let outcome = context.report_operation(
        None,
        || {
            operations.set(operations.get() + 1);
            Err::<usize, _>(BorrowedError {
                payload: &payload,
                cancelled: false,
            })
        },
        |error| error.cancelled,
        |_| {
            messages.set(messages.get() + 1);
            Issue::new("operation", "failed").with_cause("local cause")
        },
    );
    let error = outcome.result.unwrap_err();
    assert!(Rc::ptr_eq(error.payload, &payload));
    assert_eq!(operations.get(), 1);
    assert_eq!(messages.get(), 1);
    assert!(matches!(
        outcome.deliveries.as_slice(),
        [IssueDelivery::Accepted]
    ));
    let observed = observed.borrow();
    assert_eq!(observed.len(), 1);
    assert_eq!(observed[0].code, "operation");
    assert_eq!(observed[0].message, "failed");
    assert_eq!(observed[0].location.line(), caller_line);
    assert_eq!(observed[0].location.file(), file!());
    assert_eq!(observed[0].cause.as_deref(), Some("local cause"));
}

#[test]
fn successful_values_and_cancelled_errors_never_evaluate_diagnostics() {
    let calls = Cell::new(0);
    let reporter = |_: &Issue, _: &usize| {
        calls.set(calls.get() + 1);
        Ok::<_, IssueDeliveryFailure<Infallible>>(())
    };
    let reporters = [&reporter as &_];
    let context = IssueContext::new(&reporters);
    for result in [Ok(Some(3)), Ok(None), Err(9usize)] {
        let outcome = context.report_operation(
            None,
            || result,
            |error| *error == 9,
            |_| panic!("successful/cancelled operation evaluated message"),
        );
        assert_eq!(outcome.result, result);
        assert!(outcome.deliveries.is_empty());
    }
    assert_eq!(calls.get(), 0);
}

#[test]
fn supplied_context_replaces_inherited_even_when_empty() {
    let inherited_calls = Cell::new(0);
    let supplied_calls = Cell::new(0);
    let inherited = |_: &Issue, _: &u8| {
        inherited_calls.set(inherited_calls.get() + 1);
        Ok::<_, IssueDeliveryFailure<Infallible>>(())
    };
    let supplied = |_: &Issue, _: &u8| {
        supplied_calls.set(supplied_calls.get() + 1);
        Ok::<_, IssueDeliveryFailure<Infallible>>(())
    };
    let inherited_reporters = [&inherited as &_];
    let supplied_reporters = [&supplied as &_];
    let context = IssueContext::new(&inherited_reporters);
    let replacement = IssueContext::new(&supplied_reporters);
    let empty = IssueContext::new(&[]);
    for selected in [None, Some(&replacement), Some(&empty)] {
        let outcome = context.report_operation(
            selected,
            || Err::<(), _>(4u8),
            |_| false,
            |_| Issue::new("selection", "kept"),
        );
        assert_eq!(outcome.result, Err(4));
        if selected.is_some_and(|context| std::ptr::eq(context, &raw const empty)) {
            let [IssueDelivery::Disabled(issue)] = outcome.deliveries.as_slice() else {
                panic!("empty replacement must recover disabled issue")
            };
            assert_eq!(issue.code, "selection");
        } else {
            assert!(matches!(
                outcome.deliveries.as_slice(),
                [IssueDelivery::Accepted]
            ));
        }
    }
    assert_eq!(inherited_calls.get(), 1);
    assert_eq!(supplied_calls.get(), 1);
}

#[test]
fn rejected_deliveries_recover_evidence_without_replacing_operation_error() {
    /// Keeps callback failure identity separate from the original operation error.
    #[derive(Debug)]
    struct DeliveryFault<'a>(&'a Rc<usize>);

    let failure = Rc::new(19);
    let disabled = |_: &Issue, _: &u8| Err(IssueDeliveryFailure::Disabled);
    let closed = |_: &Issue, _: &u8| Err(IssueDeliveryFailure::Closed);
    let failed = |_: &Issue, _: &u8| Err(IssueDeliveryFailure::Failed(DeliveryFault(&failure)));
    let reporters: [&IssueCallback<'_, u8, DeliveryFault<'_>>; 3] = [&disabled, &closed, &failed];
    let context = IssueContext::new(&reporters);
    let outcome = context.report_operation(
        None,
        || Err::<(), _>(11u8),
        |_| false,
        |_| {
            Issue::new("delivery", "retained")
                .with_severity(IssueSeverity::Error)
                .with_cause("cause")
        },
    );
    assert_eq!(outcome.result, Err(11));
    let [
        IssueDelivery::Disabled(first),
        IssueDelivery::Closed(second),
        IssueDelivery::Failed {
            issue: third,
            error,
        },
    ] = outcome.deliveries.as_slice()
    else {
        panic!("wrong delivery outcomes")
    };
    assert!(Rc::ptr_eq(error.0, &failure));
    for issue in [first, second, third] {
        assert_eq!(issue.message, "retained");
        assert_eq!(issue.severity, IssueSeverity::Error);
        assert_eq!(issue.cause.as_deref(), Some("cause"));
        assert_eq!(issue.location, first.location);
    }
}

#[test]
fn final_rejection_reuses_owned_diagnostic_buffers() {
    for count in [1, 3] {
        for kind in ["disabled", "closed", "failed"] {
            let observed = Cell::new(None);
            let calls = Cell::new(0);
            let callback = |issue: &Issue, cause: &u8| {
                assert_eq!(*cause, 7);
                let address = std::ptr::from_ref(issue);
                if let Some(first) = observed.replace(Some(address)) {
                    assert_eq!(address, first, "all callbacks borrow the same issue");
                }
                calls.set(calls.get() + 1);
                Err(match kind {
                    "disabled" => IssueDeliveryFailure::Disabled,
                    "closed" => IssueDeliveryFailure::Closed,
                    _ => IssueDeliveryFailure::Failed(9u8),
                })
            };
            let reporters = [&callback as &IssueCallback<'_, u8, u8>; 3];
            let context = IssueContext::new(&reporters[..count]);
            let issue = Issue::new("owned", String::from("original message"))
                .with_cause(String::from("original cause"));
            let message = issue.message.as_ptr();
            let cause = issue.cause.as_ref().unwrap().as_ptr();
            let location = issue.location;
            let mut deliveries = context.report(issue, &7);
            assert_eq!(calls.get(), count);
            assert_eq!(deliveries.len(), count);
            let recovered = match deliveries.pop().unwrap() {
                IssueDelivery::Disabled(issue) => {
                    assert_eq!(kind, "disabled");
                    issue
                }
                IssueDelivery::Closed(issue) => {
                    assert_eq!(kind, "closed");
                    issue
                }
                IssueDelivery::Failed { issue, error } => {
                    assert_eq!(kind, "failed");
                    assert_eq!(error, 9);
                    issue
                }
                _ => panic!("the final callback rejected this issue"),
            };
            assert_eq!(recovered.message.as_ptr(), message);
            assert_eq!(recovered.cause.as_ref().unwrap().as_ptr(), cause);
            assert_eq!(recovered.location, location);
        }
    }
}

#[test]
fn independent_context_unwind_does_not_clear_the_outer_recursion_guard() {
    let independent_calls = Cell::new(0);
    let independent_callback = |_: &Issue, cause: &u8| {
        independent_calls.set(independent_calls.get() + 1);
        assert_eq!(*cause, 0, "independent callback panic");
        Ok::<_, IssueDeliveryFailure<Infallible>>(())
    };
    let independent_reporters = [&independent_callback as &_];
    let independent = IssueContext::new(&independent_reporters);
    let target = Cell::new(None::<&IssueContext<'_, u8, Infallible>>);
    let outer_calls = Cell::new(0);
    let callback = |_: &Issue, _: &u8| {
        outer_calls.set(outer_calls.get() + 1);
        let panic = catch_unwind(AssertUnwindSafe(|| {
            independent.report(Issue::new("panic", "independent"), &1u8)
        }))
        .unwrap_err();
        assert!(panic_message(panic.as_ref()).contains("independent callback panic"));

        assert!(matches!(
            independent
                .report(Issue::new("after", "independent"), &0u8)
                .as_slice(),
            [IssueDelivery::Accepted]
        ));
        for _ in 0..2 {
            assert!(matches!(
                target
                    .get()
                    .unwrap()
                    .report(Issue::new("nested", "same context"), &0)
                    .as_slice(),
                [IssueDelivery::Reentrant(_)]
            ));
        }
        Ok::<_, IssueDeliveryFailure<Infallible>>(())
    };
    let reporters = [&callback as &_];
    let context = IssueContext::new(&reporters);
    target.set(Some(&context));
    for _ in 0..2 {
        assert!(matches!(
            context
                .report(Issue::new("outer", "ordinary delivery"), &0)
                .as_slice(),
            [IssueDelivery::Accepted]
        ));
    }
    assert_eq!(outer_calls.get(), 2);
    assert_eq!(independent_calls.get(), 4);
}

#[test]
fn callback_panic_drops_prior_failures_before_clearing_the_guard() {
    /// Re-enters the context when a retained callback failure is dropped during unwinding.
    struct Fault<'a>(&'a dyn Fn());

    impl Drop for Fault<'_> {
        fn drop(&mut self) {
            self.0();
        }
    }

    let target = Cell::new(None::<&IssueContext<'_, u8, Fault<'_>>>);
    let failure_drops = Cell::new(0);
    let drop_was_reentrant = Cell::new(None);
    let on_drop = || {
        failure_drops.set(failure_drops.get() + 1);
        drop_was_reentrant.set(Some(matches!(
            target
                .get()
                .unwrap()
                .report(Issue::new("drop", "during unwinding"), &0)
                .as_slice(),
            [IssueDelivery::Reentrant(_)]
        )));
    };
    let panicking = Cell::new(true);
    // A broken guard must not make the destructor probe panic or create another Fault.
    let first = |issue: &Issue, _: &u8| {
        if panicking.get() && issue.code != "drop" {
            Err(IssueDeliveryFailure::Failed(Fault(&on_drop)))
        } else {
            Ok(())
        }
    };
    let second = |issue: &Issue, _: &u8| {
        assert!(
            issue.code == "drop" || !panicking.get(),
            "callback panic after a retained failure"
        );
        Ok(())
    };
    let tail_calls = Cell::new(0);
    let tail = |_: &Issue, _: &u8| {
        tail_calls.set(tail_calls.get() + 1);
        Ok(())
    };
    let reporters: [&IssueCallback<'_, u8, Fault<'_>>; 3] = [&first, &second, &tail];
    let context = IssueContext::new(&reporters);
    target.set(Some(&context));
    let Err(panic) = catch_unwind(AssertUnwindSafe(|| {
        context.report(Issue::new("outer", "retained failure"), &0)
    })) else {
        panic!("the second callback must propagate its panic")
    };
    assert_eq!(
        panic_message(panic.as_ref()),
        "callback panic after a retained failure"
    );
    assert_eq!(failure_drops.get(), 1);
    assert_eq!(drop_was_reentrant.get(), Some(true));
    assert_eq!(tail_calls.get(), 0);
    panicking.set(false);
    assert!(matches!(
        context
            .report(Issue::new("after", "guard reset"), &0)
            .as_slice(),
        [
            IssueDelivery::Accepted,
            IssueDelivery::Accepted,
            IssueDelivery::Accepted
        ]
    ));
    assert_eq!(tail_calls.get(), 1);
}

#[test]
fn borrowed_future_is_lazy_and_preserves_none_error_and_caller_location() {
    let payload = Rc::new(13);
    let operations = Cell::new(0);
    let messages = Cell::new(0);
    let reports = RefCell::new(Vec::new());
    let reporter = |issue: &Issue, error: &BorrowedError<'_>| {
        assert!(Rc::ptr_eq(error.payload, &payload));
        reports.borrow_mut().push(issue.clone());
        Ok::<_, IssueDeliveryFailure<Infallible>>(())
    };
    let reporters = [&reporter as &_];
    let context = IssueContext::new(&reporters);
    let operation = async {
        operations.set(operations.get() + 1);
        Err::<Option<usize>, _>(BorrowedError {
            payload: &payload,
            cancelled: false,
        })
    };
    let caller_line = line!() + 1;
    let mut future = Box::pin(context.report_future(
        None,
        operation,
        |error| error.cancelled,
        |_| {
            messages.set(messages.get() + 1);
            Issue::new("future", "failed")
        },
    ));
    assert_eq!(operations.get(), 0);
    assert_eq!(messages.get(), 0);
    assert!(reports.borrow().is_empty());
    let mut poll_context = Context::from_waker(Waker::noop());
    let Poll::Ready(outcome) = future.as_mut().poll(&mut poll_context) else {
        panic!("ready operation must finish")
    };
    assert!(Rc::ptr_eq(outcome.result.unwrap_err().payload, &payload));
    assert_eq!(operations.get(), 1);
    assert_eq!(messages.get(), 1);
    assert_eq!(reports.borrow().len(), 1);
    assert_eq!(reports.borrow()[0].code, "future");
    assert_eq!(reports.borrow()[0].message, "failed");
    assert_eq!(reports.borrow()[0].location.line(), caller_line);
    assert_eq!(reports.borrow()[0].location.file(), file!());
    assert!(matches!(
        outcome.deliveries.as_slice(),
        [IssueDelivery::Accepted]
    ));

    let mut none = Box::pin(context.report_future(
        None,
        async { Ok::<_, BorrowedError<'_>>(None::<usize>) },
        |_| false,
        |_| panic!("None is success"),
    ));
    assert!(
        matches!(none.as_mut().poll(&mut poll_context), Poll::Ready(outcome) if matches!(outcome.result, Ok(None)) && outcome.deliveries.is_empty())
    );
}

#[test]
fn unpolled_and_dropped_pending_futures_do_not_report_completed_errors() {
    let operations = Cell::new(0);
    let reports = Cell::new(0);
    let reporter = |_: &Issue, _: &u8| {
        reports.set(reports.get() + 1);
        Ok::<_, IssueDeliveryFailure<Infallible>>(())
    };
    let reporters = [&reporter as &_];
    let context = IssueContext::new(&reporters);
    let make_future = || {
        context.report_future(
            None,
            async {
                operations.set(operations.get() + 1);
                std::future::pending::<()>().await;
                Err::<(), _>(8u8)
            },
            |_| false,
            |_| panic!("pending operation has no completed error"),
        )
    };
    drop(make_future());
    assert_eq!(operations.get(), 0);
    let mut future = Box::pin(make_future());
    let mut poll_context = Context::from_waker(Waker::noop());
    assert!(future.as_mut().poll(&mut poll_context).is_pending());
    drop(future);
    assert_eq!(operations.get(), 1);
    assert_eq!(reports.get(), 0);
}

#[test]
fn reentrant_delivery_recovers_nested_error_without_invoking_callback_again() {
    let target = Cell::new(None::<&IssueContext<'_, u8, Infallible>>);
    let calls = Cell::new(0);
    let nested = RefCell::new(None);
    let reporter = |issue: &Issue, _: &u8| {
        calls.set(calls.get() + 1);
        if issue.code == "outer" {
            assert_eq!(
                format!("{:?}", target.get().unwrap()),
                "IssueContext { reporter_count: 1, reporting: true }"
            );
            let outcome = target.get().unwrap().report_operation(
                None,
                || Err::<(), _>(8u8),
                |_| false,
                |_| Issue::new("nested", "retained"),
            );
            nested.replace(Some(outcome));
        }
        Ok::<_, IssueDeliveryFailure<Infallible>>(())
    };
    let reporters = [&reporter as &_];
    let context = IssueContext::new(&reporters);
    target.set(Some(&context));
    assert_eq!(
        format!("{context:?}"),
        "IssueContext { reporter_count: 1, reporting: false }"
    );
    assert_eq!(calls.get(), 0);
    let outcome = context.report_operation(
        None,
        || Err::<(), _>(7u8),
        |_| false,
        |_| Issue::new("outer", "first"),
    );
    assert_eq!(outcome.result, Err(7));
    assert!(matches!(
        outcome.deliveries.as_slice(),
        [IssueDelivery::Accepted]
    ));
    let nested = nested.take().unwrap();
    assert_eq!(nested.result, Err(8));
    let [IssueDelivery::Reentrant(issue)] = nested.deliveries.as_slice() else {
        panic!("nested delivery must be rejected, not recursively delivered")
    };
    assert_eq!(issue.code, "nested");
    assert_eq!(issue.message, "retained");
    assert_eq!(calls.get(), 1);
    assert_eq!(
        format!("{context:?}"),
        "IssueContext { reporter_count: 1, reporting: false }"
    );
    let later = context.report_operation(
        None,
        || Err::<(), _>(9u8),
        |_| false,
        |_| Issue::new("later", "reset"),
    );
    assert_eq!(later.result, Err(9));
    assert!(matches!(
        later.deliveries.as_slice(),
        [IssueDelivery::Accepted]
    ));
    assert_eq!(calls.get(), 2);
}

#[test]
fn sync_panics_propagate_without_completed_outcomes_and_context_resets_on_unwind() {
    for stage in ["operation", "predicate", "message", "reporter"] {
        let should_panic = Cell::new(true);
        let completed = Cell::new(false);
        let callback = |_: &Issue, _: &u8| {
            assert!(
                !(should_panic.get() && stage == "reporter"),
                "reporter panic"
            );
            Ok::<_, IssueDeliveryFailure<Infallible>>(())
        };
        let reporters = [&callback as &_];
        let context = IssueContext::new(&reporters);
        let panic = catch_unwind(AssertUnwindSafe(|| {
            let _outcome = context.report_operation(
                None,
                || {
                    assert_ne!(stage, "operation", "operation panic");
                    Err::<(), _>(3u8)
                },
                |_| {
                    assert_ne!(stage, "predicate", "predicate panic");
                    false
                },
                |_| {
                    assert_ne!(stage, "message", "message panic");
                    Issue::new("panic", "ordinary error")
                },
            );
            completed.set(true);
        }))
        .expect_err("the selected stage must propagate its panic");
        assert!(panic_message(panic.as_ref()).contains(&format!("{stage} panic")));
        assert!(!completed.get());
        should_panic.set(false);
        let outcome = context.report_operation(
            None,
            || Err::<(), _>(4u8),
            |_| false,
            |_| Issue::new("after", "context remains available"),
        );
        assert_eq!(outcome.result, Err(4));
        assert!(matches!(
            outcome.deliveries.as_slice(),
            [IssueDelivery::Accepted]
        ));
    }
}

#[test]
fn future_panics_propagate_at_poll_without_forging_completed_results() {
    for stage in ["operation", "predicate", "message", "reporter"] {
        let callback = |_: &Issue, _: &u8| {
            assert_ne!(stage, "reporter", "reporter panic");
            Ok::<_, IssueDeliveryFailure<Infallible>>(())
        };
        let reporters = [&callback as &_];
        let context = IssueContext::new(&reporters);
        let mut future = Box::pin(context.report_future(
            None,
            async {
                assert_ne!(stage, "operation", "operation panic");
                Err::<(), _>(5u8)
            },
            |_| {
                assert_ne!(stage, "predicate", "predicate panic");
                false
            },
            |_| {
                assert_ne!(stage, "message", "message panic");
                Issue::new("future", "failure")
            },
        ));
        let panic = catch_unwind(AssertUnwindSafe(|| {
            future
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
        }))
        .expect_err("the selected stage must panic during polling");
        assert!(panic_message(panic.as_ref()).contains(&format!("{stage} panic")));
    }
}

#[test]
fn future_cancellation_preserves_original_error_without_evaluating_message() {
    let payload = Rc::new(17);
    let calls = Cell::new(0);
    let callback = |_: &Issue, _: &BorrowedError<'_>| {
        calls.set(calls.get() + 1);
        Ok::<_, IssueDeliveryFailure<Infallible>>(())
    };
    let reporters = [&callback as &_];
    let context = IssueContext::new(&reporters);
    let mut future = Box::pin(context.report_future(
        None,
        async {
            Err::<(), _>(BorrowedError {
                payload: &payload,
                cancelled: true,
            })
        },
        |error| error.cancelled,
        |_| panic!("cancelled operation evaluated message"),
    ));
    let Poll::Ready(outcome) = future
        .as_mut()
        .poll(&mut Context::from_waker(Waker::noop()))
    else {
        panic!("ready cancelled operation must finish")
    };
    let original = outcome.result.unwrap_err();
    assert!(original.cancelled);
    assert!(Rc::ptr_eq(original.payload, &payload));
    assert!(outcome.deliveries.is_empty());
    assert_eq!(calls.get(), 0);
}

#[test]
fn pending_then_successful_future_keeps_borrowed_value_and_runs_operation_once() {
    let value = Rc::new(23);
    let operations = Cell::new(0);
    let polls = Cell::new(0);
    let reports = Cell::new(0);
    let callback = |_: &Issue, _: &u8| {
        reports.set(reports.get() + 1);
        Ok::<_, IssueDeliveryFailure<Infallible>>(())
    };
    let reporters = [&callback as &_];
    let context = IssueContext::new(&reporters);
    let mut future = Box::pin(context.report_future(
        None,
        async {
            operations.set(operations.get() + 1);
            // The caller supplies the second poll; this is not an executor scheduling test.
            std::future::poll_fn(|_| {
                polls.set(polls.get() + 1);
                if polls.get() == 1 {
                    Poll::Pending
                } else {
                    Poll::Ready(())
                }
            })
            .await;
            Ok::<_, u8>(&value)
        },
        |_| panic!("success has no error to classify"),
        |_| panic!("success has no diagnostic"),
    ));
    let mut poll_context = Context::from_waker(Waker::noop());
    assert!(future.as_mut().poll(&mut poll_context).is_pending());
    assert_eq!(operations.get(), 1);
    let Poll::Ready(outcome) = future.as_mut().poll(&mut poll_context) else {
        panic!("second caller poll releases successful operation")
    };
    assert!(Rc::ptr_eq(outcome.result.unwrap(), &value));
    assert!(outcome.deliveries.is_empty());
    assert_eq!(operations.get(), 1);
    assert_eq!(polls.get(), 2);
    assert_eq!(reports.get(), 0);
}

/// Adapts the owned reporter's two rejection reasons to borrowed-context delivery.
fn endpoint_delivery(
    reporter: &IssueReporter,
    issue: &Issue,
) -> Result<(), IssueDeliveryFailure<Infallible>> {
    reporter.report(issue.clone()).map_err(|error| {
        if error.is_disabled() {
            IssueDeliveryFailure::Disabled
        } else {
            IssueDeliveryFailure::Closed
        }
    })
}

#[test]
fn actual_disabled_endpoint_retains_error_and_diagnostic_in_explicit_context() {
    let disabled = IssueReporter::default();
    let callback = |issue: &Issue, _: &u8| endpoint_delivery(&disabled, issue);
    let reporters = [&callback as &_];
    let context = IssueContext::new(&reporters);
    let outcome = context.report_operation(
        None,
        || Err::<(), _>(6u8),
        |_| false,
        |_| Issue::new("disabled", "evidence"),
    );
    assert_eq!(outcome.result, Err(6));
    let [IssueDelivery::Disabled(issue)] = outcome.deliveries.as_slice() else {
        panic!("real disabled endpoint must recover diagnostic")
    };
    assert_eq!(issue.message, "evidence");
}

#[cfg(feature = "test-util")]
#[expect(dead_code, reason = "manual actor polling needs only captured tasks")]
mod support;

#[cfg(feature = "test-util")]
#[test]
fn actual_recorder_accepts_metadata_then_closed_endpoint_recovers_late_error() {
    use rca_core::{Effect, ReduceContext, Reducer, Store, testing::IssueRecorder};

    /// Keeps recorder shutdown independent of actions, state changes, or effects.
    struct NoWork;

    impl Reducer for NoWork {
        type State = ();
        type Action = ();

        fn reduce(&mut self, (): &mut (), (): &(), _: &mut ReduceContext<'_>) -> Effect<()> {
            Effect::none()
        }
    }
    let runtime = support::ManualRuntime::new();
    let recorder = IssueRecorder::new();
    let endpoint = recorder.reporter();
    let store = Store::builder((), NoWork, runtime.runtime())
        .issue_recorder(recorder.clone())
        .build()
        .unwrap();
    let callback = |issue: &Issue, _: &u8| endpoint_delivery(&endpoint, issue);
    let reporters = [&callback as &_];
    let context = IssueContext::new(&reporters);
    let caller_line = line!() + 1;
    let accepted = context.report_operation(
        None,
        || Err::<(), _>(1u8),
        |_| false,
        |_| {
            Issue::new("accepted", "original")
                .with_severity(IssueSeverity::Error)
                .with_cause("owned cause")
        },
    );
    assert_eq!(accepted.result, Err(1));
    assert!(matches!(
        accepted.deliveries.as_slice(),
        [IssueDelivery::Accepted]
    ));
    let admitted = recorder.oldest().unwrap();
    assert_eq!(admitted.code, "accepted");
    assert_eq!(admitted.message, "original");
    assert_eq!(admitted.severity, IssueSeverity::Error);
    assert_eq!(admitted.cause.as_deref(), Some("owned cause"));
    assert_eq!(admitted.location.line(), caller_line);
    assert_eq!(admitted.location.file(), file!());

    let mut actor = runtime.take_only_queued_task();
    let mut shutdown = Box::pin(store.shutdown());
    let mut poll_context = Context::from_waker(Waker::noop());
    assert!(shutdown.as_mut().poll(&mut poll_context).is_pending());
    assert!(actor.as_mut().poll(&mut poll_context).is_ready());
    assert!(matches!(
        shutdown.as_mut().poll(&mut poll_context),
        Poll::Ready(Ok(()))
    ));
    let closed = context.report_operation(
        None,
        || Err::<(), _>(2u8),
        |_| false,
        |_| {
            Issue::new("closed", "retained")
                .with_severity(IssueSeverity::Error)
                .with_cause("late cause")
        },
    );
    assert_eq!(closed.result, Err(2));
    let [IssueDelivery::Closed(issue)] = closed.deliveries.as_slice() else {
        panic!("real sealed endpoint must recover late diagnostic")
    };
    assert_eq!(issue.message, "retained");
    assert_eq!(issue.severity, IssueSeverity::Error);
    assert_eq!(issue.cause.as_deref(), Some("late cause"));
    assert_eq!(recorder.snapshot().len(), 1);
}
