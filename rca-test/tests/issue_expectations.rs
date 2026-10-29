//! Checks issue policies, owned outcomes and scope closure through public helpers.
//!
//! Manual polls distinguish completion, cancellation and unwinding without a runtime.
//! Literal issue sequences and panic payloads keep failure checks independent.

use std::cell::{Cell, RefCell};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::rc::Rc;
use std::sync::mpsc;
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use rca_core::{Issue, IssueCollector, IssueReporter};
use rca_test::{IssueExpectationMode, expect_issues, expect_issues_async};

/// Preserves a borrowed, non-`Send` error without requiring `Debug` or ownership.
struct LocalError<'a>(&'a Rc<String>);

/// Checks that matcher destruction releases completed results in both helper variants.
#[test]
fn matcher_cleanup_panic_releases_completed_body_results() {
    /// Counts destruction of the body result after matcher cleanup fails.
    struct Output<'a>(&'a Cell<usize>);

    impl Drop for Output<'_> {
        fn drop(&mut self) {
            self.0.set(self.0.get() + 1);
        }
    }

    /// Injects the exact cleanup panic after the body returns.
    struct MatcherDrop;

    impl Drop for MatcherDrop {
        fn drop(&mut self) {
            panic!("matcher cleanup");
        }
    }

    let mut observed = Vec::new();
    for asynchronous in [false, true] {
        let dropped = Cell::new(0);
        let collector = IssueCollector::new();
        let reporter = collector.reporter();
        let probe = MatcherDrop;
        let matches = move |_: &Issue| {
            std::hint::black_box(&probe);
            true
        };
        let result = catch_unwind(AssertUnwindSafe(|| {
            if asynchronous {
                drop(futures_executor::block_on(expect_issues_async(
                    collector,
                    IssueExpectationMode::Intermittent,
                    matches,
                    |_| async { Ok::<_, ()>(Output(&dropped)) },
                )));
            } else {
                drop(expect_issues(
                    collector,
                    IssueExpectationMode::Intermittent,
                    matches,
                    |_| Ok::<_, ()>(Output(&dropped)),
                ));
            }
        }));
        let panic = result.expect_err("matcher destruction must propagate");
        assert_eq!(panic.downcast_ref::<&str>(), Some(&"matcher cleanup"));
        assert!(
            reporter
                .report(Issue::new("late", "closed"))
                .unwrap_err()
                .is_closed()
        );
        observed.push(dropped.get());
    }
    assert_eq!(
        observed,
        [1, 1],
        "completed body results must not leak during cleanup"
    );
}

/// Checks capture destruction before the completed future releases its storage.
#[test]
fn completed_async_scope_releases_captures_before_future_is_dropped() {
    /// Records matcher destruction after body storage has been released.
    struct Matcher<'a>(&'a RefCell<Vec<&'static str>>);

    impl Drop for Matcher<'_> {
        fn drop(&mut self) {
            self.0.borrow_mut().push("matcher");
        }
    }

    /// Returns immediately and records the destruction of its future storage.
    struct Body<'a>(&'a RefCell<Vec<&'static str>>);

    impl Future for Body<'_> {
        type Output = Result<usize, ()>;

        fn poll(self: std::pin::Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Self::Output> {
            Poll::Ready(Ok(7))
        }
    }

    impl Drop for Body<'_> {
        fn drop(&mut self) {
            self.0.borrow_mut().push("body");
        }
    }

    let dropped = RefCell::new(Vec::new());
    let matcher = Matcher(&dropped);
    let body = Body(&dropped);
    let collector = IssueCollector::new();
    let reporter = collector.reporter();
    let future = expect_issues_async(
        collector,
        IssueExpectationMode::Intermittent,
        move |_| {
            std::hint::black_box(&matcher);
            true
        },
        |_| body,
    );
    let mut future = std::pin::pin!(future);
    let Poll::Ready(outcome) = future
        .as_mut()
        .poll(&mut Context::from_waker(Waker::noop()))
    else {
        panic!("ready body");
    };
    assert_eq!(&*dropped.borrow(), &["body", "matcher"]);
    assert!(
        reporter
            .report(Issue::new("late", "closed"))
            .unwrap_err()
            .is_closed()
    );
    assert_eq!(outcome.body_result, Ok(7));
    assert!(outcome.expectation.unwrap().is_empty());
    // Use the completed future again without polling it; it still owns its storage.
    std::hint::black_box(future.as_mut());
    assert_eq!(&*dropped.borrow(), &["body", "matcher"]);
}

/// Checks result destruction when the completed body future panics during cleanup.
#[test]
fn completed_body_future_cleanup_panic_releases_its_output() {
    /// Counts destruction of the output produced before the body destructor panics.
    struct Output<'a>(&'a Cell<usize>);

    impl Drop for Output<'_> {
        fn drop(&mut self) {
            self.0.set(self.0.get() + 1);
        }
    }

    /// Produces one output, then injects the exact destructor panic.
    struct Body<'a>(&'a Cell<usize>);

    impl<'a> Future for Body<'a> {
        type Output = Result<Output<'a>, ()>;

        fn poll(self: std::pin::Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Self::Output> {
            Poll::Ready(Ok(Output(self.0)))
        }
    }

    impl Drop for Body<'_> {
        fn drop(&mut self) {
            panic!("body future cleanup");
        }
    }

    let dropped = Cell::new(0);
    let collector = IssueCollector::new();
    let reporter = collector.reporter();
    let future = expect_issues_async(
        collector,
        IssueExpectationMode::Intermittent,
        |_| true,
        |_| Body(&dropped),
    );
    let mut future = std::pin::pin!(future);
    let result = catch_unwind(AssertUnwindSafe(|| {
        drop(
            future
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop())),
        );
    }));
    let panic = result.expect_err("body future destruction must propagate");
    assert_eq!(panic.downcast_ref::<&str>(), Some(&"body future cleanup"));
    assert!(
        reporter
            .report(Issue::new("late", "closed"))
            .unwrap_err()
            .is_closed()
    );
    assert_eq!(dropped.get(), 1);
    std::hint::black_box(future.as_mut());
}

/// Checks string storage after collection, independently of recording-time copies.
#[test]
fn owned_issue_text_moves_into_outcomes_without_cloning() {
    let mut preserved = Vec::new();
    for (mode, accept_all) in [
        (IssueExpectationMode::Strict, false),
        (IssueExpectationMode::Strict, true),
        (IssueExpectationMode::Disabled, true),
    ] {
        let issues: Vec<_> = ["first", "known", "last"]
            .map(|code| {
                Issue::new(code, format!("owned message for {code}"))
                    .with_cause(format!("owned cause for {code}"))
            })
            .into();
        // Recording clones payloads; measure ownership from the sealed collector's
        // issues presented to the matcher, not from the caller's reported inputs.
        let expected = RefCell::new(Vec::new());
        let calls = RefCell::new(Vec::new());
        let outcome = expect_issues(
            IssueCollector::new(),
            mode,
            |issue| {
                calls.borrow_mut().push(issue.code);
                if accept_all || mode == IssueExpectationMode::Disabled || issue.code != "known" {
                    expected.borrow_mut().push((
                        issue.code,
                        issue.message.as_ptr(),
                        issue.cause.as_ref().unwrap().as_ptr(),
                    ));
                }
                accept_all || issue.code == "known"
            },
            |reporter| {
                for issue in issues {
                    reporter.report(issue).unwrap();
                }
                Err::<(), _>("original error")
            },
        );
        let expected = expected.into_inner();
        assert_eq!(outcome.body_result, Err("original error"));
        assert_eq!(&*calls.borrow(), &["first", "known", "last"]);
        let received = match &outcome.expectation {
            Ok(issues) => {
                assert!(accept_all && mode == IssueExpectationMode::Strict);
                issues.as_slice()
            }
            Err(error) => {
                assert!(!error.is_missing());
                error.unexpected_issues()
            }
        };
        let expected_codes: &[_] = if accept_all {
            &["first", "known", "last"]
        } else {
            &["first", "last"]
        };
        assert_eq!(
            received.iter().map(|issue| issue.code).collect::<Vec<_>>(),
            expected_codes
        );
        assert_eq!(received.len(), expected.len());
        let mut same_storage = true;
        for (issue, &(code, message, cause)) in received.iter().zip(&expected) {
            assert_eq!(issue.code, code);
            assert_eq!(issue.message, format!("owned message for {code}"));
            assert_eq!(
                issue.cause.as_deref(),
                Some(format!("owned cause for {code}").as_str())
            );
            same_storage &= issue.message.as_ptr() == message;
            same_storage &= issue.cause.as_ref().unwrap().as_ptr() == cause;
        }
        preserved.push(same_storage);
    }
    assert_eq!(
        preserved, [true; 3],
        "owned issue strings should move without cloning"
    );
}

/// Keeps issue diagnostics and call-site evidence separate from the body error.
#[test]
fn failure_display_keeps_missing_and_unexpected_evidence_independent_of_body_error() {
    let issue = Issue::new("unexpected-code", "explicit diagnostic message")
        .with_severity(rca_core::IssueSeverity::Error)
        .with_cause("explicit diagnostic cause");
    let origin = line!() + 1;
    let outcome = expect_issues(
        IssueCollector::new(),
        IssueExpectationMode::Strict,
        |_| false,
        |reporter| {
            reporter.report(issue.clone()).unwrap();
            Err::<(), _>("private-body-error")
        },
    );
    assert_eq!(outcome.body_result, Err("private-body-error"));
    let failure = outcome.expectation.unwrap_err();
    let display = failure.to_string();
    assert!(display.starts_with("issue expectation failed at "));
    assert!(display.contains(&format!("{}:{origin}:", file!())));
    assert!(display.contains("missing required match=true"));
    assert!(display.contains("unexpected-code"));
    assert!(display.contains("explicit diagnostic message"));
    assert!(display.contains("explicit diagnostic cause"));
    assert!(!display.contains("private-body-error"));
    assert!(display.ends_with(&failure.backtrace().to_string()));
    assert_eq!(failure.unexpected_issues(), [issue]);
    assert!(failure.is_missing());
}

/// Requires a match while preserving every unexpected report in admission order.
#[test]
fn strict_requires_matches_and_checks_every_unexpected_issue_even_after_failure() {
    let body_calls = Cell::new(0);
    let matcher_calls = RefCell::new(Vec::new());
    let outcome = expect_issues(
        IssueCollector::new(),
        IssueExpectationMode::Strict,
        |issue| {
            matcher_calls.borrow_mut().push(issue.code);
            issue.code == "known"
        },
        |reporter| {
            body_calls.set(body_calls.get() + 1);
            for code in ["unexpected-first", "known", "unexpected-last"] {
                reporter.report(Issue::new(code, "evidence")).unwrap();
            }
            Err::<(), _>("original error")
        },
    );
    assert_eq!(outcome.body_result, Err("original error"));
    let error = outcome.expectation.unwrap_err();
    assert!(!error.is_missing());
    assert_eq!(
        error
            .unexpected_issues()
            .iter()
            .map(|issue| issue.code)
            .collect::<Vec<_>>(),
        ["unexpected-first", "unexpected-last"]
    );
    assert_eq!(
        *matcher_calls.borrow(),
        ["unexpected-first", "known", "unexpected-last"]
    );
    assert_eq!(body_calls.get(), 1);

    let missing = expect_issues(
        IssueCollector::new(),
        IssueExpectationMode::Strict,
        |_| true,
        |_| Ok::<_, ()>(None::<usize>),
    );
    assert_eq!(missing.body_result, Ok(None));
    let missing = missing.expectation.unwrap_err();
    assert!(missing.is_missing());
    assert!(missing.unexpected_issues().is_empty());

    let unexpected = expect_issues(
        IssueCollector::new(),
        IssueExpectationMode::Strict,
        |_| false,
        |reporter| {
            reporter
                .report(Issue::new("unexpected", "missing too"))
                .unwrap();
            Ok::<_, ()>(())
        },
    )
    .expectation
    .unwrap_err();
    assert!(unexpected.is_missing());
    assert_eq!(unexpected.unexpected_issues().len(), 1);
    assert_eq!(unexpected.unexpected_issues()[0].code, "unexpected");
}

/// Separates optional matches from disabled allowances without skipping body execution.
#[test]
fn intermittent_and_disabled_have_explicit_zero_policy_and_always_run_body() {
    for mode in [
        IssueExpectationMode::Intermittent,
        IssueExpectationMode::Disabled,
    ] {
        let runs = Cell::new(0);
        let outcome = expect_issues(
            IssueCollector::new(),
            mode,
            |_| panic!("no reports"),
            |_| {
                runs.set(runs.get() + 1);
                Ok::<_, ()>(41)
            },
        );
        assert_eq!(outcome.body_result, Ok(41));
        assert!(outcome.expectation.unwrap().is_empty());
        assert_eq!(runs.get(), 1);
    }

    let predicates = Cell::new(0);
    let disabled = expect_issues(
        IssueCollector::new(),
        IssueExpectationMode::Disabled,
        |_| {
            predicates.set(predicates.get() + 1);
            true
        },
        |reporter| {
            reporter.report(Issue::new("known", "still fails")).unwrap();
            reporter
                .report(Issue::new("also-known", "checked too"))
                .unwrap();
            Ok::<_, ()>(())
        },
    );
    assert_eq!(disabled.body_result, Ok(()));
    let disabled = disabled.expectation.unwrap_err();
    assert!(!disabled.is_missing());
    assert_eq!(
        disabled
            .unexpected_issues()
            .iter()
            .map(|issue| issue.code)
            .collect::<Vec<_>>(),
        ["known", "also-known"]
    );
    assert_eq!(predicates.get(), 2);

    let intermittent = expect_issues(
        IssueCollector::new(),
        IssueExpectationMode::Intermittent,
        |_| false,
        |reporter| {
            reporter.report(Issue::new("unexpected", "fails")).unwrap();
            Ok::<_, ()>(())
        },
    );
    assert_eq!(intermittent.body_result, Ok(()));
    let intermittent = intermittent.expectation.unwrap_err();
    assert!(!intermittent.is_missing());
    assert_eq!(intermittent.unexpected_issues().len(), 1);
    assert_eq!(intermittent.unexpected_issues()[0].code, "unexpected");
}

/// Checks lazy local futures and helper call sites before asynchronous polling.
#[test]
fn local_borrowed_errors_and_future_keep_typed_result_and_exact_origin() {
    let value = Rc::new(String::from("borrowed local"));
    let calls = Cell::new(0);
    let calls_ref = &calls;
    let value_ref = &value;
    let mut future = Box::pin(expect_issues_async(
        IssueCollector::new(),
        IssueExpectationMode::Strict,
        |_| true,
        |reporter| async move {
            calls_ref.set(calls_ref.get() + 1);
            reporter.report(Issue::new("known", "reported")).unwrap();
            Err::<(), _>(value_ref.as_str())
        },
    ));
    assert_eq!(calls.get(), 0);
    let Poll::Ready(outcome) = future
        .as_mut()
        .poll(&mut Context::from_waker(Waker::noop()))
    else {
        panic!("ready")
    };
    assert_eq!(outcome.body_result, Err("borrowed local"));
    let issues = outcome.expectation.unwrap();
    assert_eq!(issues.len(), 1);
    assert_eq!(issues[0].code, "known");
    assert_eq!(calls.get(), 1);

    let missing_line = line!() + 1;
    let missing = expect_issues(
        IssueCollector::new(),
        IssueExpectationMode::Strict,
        |_| true,
        |_| Ok::<_, ()>(()),
    );
    assert_eq!(
        missing.expectation.unwrap_err().location().line(),
        missing_line
    );

    let missing_line = line!() + 1;
    let mut future = Box::pin(expect_issues_async(
        IssueCollector::new(),
        IssueExpectationMode::Strict,
        |_| true,
        |_| async { Ok::<_, ()>(()) },
    ));
    let Poll::Ready(outcome) = future
        .as_mut()
        .poll(&mut Context::from_waker(Waker::noop()))
    else {
        panic!("ready")
    };
    assert_eq!(
        outcome.expectation.unwrap_err().location().line(),
        missing_line
    );
}

/// Checks that cancellation seals collection without constructing or completing work.
#[test]
fn unpolled_and_pending_drop_close_without_running_or_finishing_the_body() {
    let calls = Cell::new(0);
    let factories = Cell::new(0);
    let collector = IssueCollector::new();
    let retained = collector.reporter();
    let future = expect_issues_async(
        collector,
        IssueExpectationMode::Strict,
        |_| panic!("an unpolled scope must not evaluate predicates"),
        |_| {
            factories.set(factories.get() + 1);
            async {
                calls.set(calls.get() + 1);
                Ok::<_, ()>(())
            }
        },
    );
    drop(future);
    assert_eq!(calls.get(), 0);
    assert_eq!(factories.get(), 0);
    assert!(
        retained
            .report(Issue::new("late", "unpolled"))
            .unwrap_err()
            .is_closed()
    );

    let collector = IssueCollector::new();
    let retained = collector.reporter();
    let mut future = Box::pin(expect_issues_async(
        collector,
        IssueExpectationMode::Strict,
        |_| panic!("a cancelled pending scope must not evaluate predicates"),
        |reporter| {
            factories.set(factories.get() + 1);
            let calls = &calls;
            async move {
                calls.set(calls.get() + 1);
                reporter
                    .report(Issue::new("before-pending", "captured"))
                    .unwrap();
                std::future::pending::<()>().await;
                Ok::<_, ()>(())
            }
        },
    ));
    assert!(
        future
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_pending()
    );
    assert!(
        future
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_pending()
    );
    assert_eq!(factories.get(), 1);
    drop(future);
    assert_eq!(calls.get(), 1);
    assert!(
        retained
            .report(Issue::new("late", "pending"))
            .unwrap_err()
            .is_closed()
    );
}

/// Requires the injected body or predicate panic and closes its collector.
#[test]
fn sync_body_and_predicate_panics_propagate_and_close_the_collector() {
    for panic_in_matcher in [false, true] {
        let collector = IssueCollector::new();
        let retained = collector.reporter();
        let panic = catch_unwind(AssertUnwindSafe(|| {
            expect_issues(
                collector,
                IssueExpectationMode::Strict,
                |_| panic!("matcher"),
                |reporter| {
                    reporter.report(Issue::new("known", "retained")).unwrap();
                    assert!(panic_in_matcher, "body panic");
                    Ok::<_, ()>(())
                },
            )
        }))
        .unwrap_err();
        let expected = if panic_in_matcher {
            "matcher"
        } else {
            "body panic"
        };
        assert_eq!(panic.downcast_ref::<&str>(), Some(&expected));
        assert!(
            retained
                .report(Issue::new("late", "panic"))
                .unwrap_err()
                .is_closed()
        );
    }
}

/// Checks immediate collection closure while a panicked future remains owned.
#[test]
fn poll_panic_closes_even_when_the_panicked_future_is_retained() {
    let collector = IssueCollector::new();
    let retained = collector.reporter();
    let mut future = Box::pin(expect_issues_async(
        collector,
        IssueExpectationMode::Strict,
        |_| true,
        |_| async {
            panic!("poll panic");
            #[expect(
                unreachable_code,
                reason = "the panic fixture still needs a typed future output"
            )]
            Ok::<(), ()>(())
        },
    ));
    let panic = catch_unwind(AssertUnwindSafe(|| {
        future
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
    }))
    .unwrap_err();
    assert_eq!(panic.downcast_ref::<&str>(), Some(&"poll panic"));
    assert!(
        retained
            .report(Issue::new("late", "retained future"))
            .unwrap_err()
            .is_closed()
    );
    // Retain the failed future without relying on unspecified re-poll behaviour.
    std::hint::black_box(future.as_mut());
    drop(future);
}

/// Keeps pre-existing evidence attached to its own collector across interleaved polls.
#[test]
fn independent_futures_do_not_cross_route_or_inherit_other_collectors() {
    let left_collector = IssueCollector::new();
    let right_collector = IssueCollector::new();
    let left = left_collector.reporter();
    let right = right_collector.reporter();
    left.report(Issue::new("left", "own")).unwrap();
    let mut left_future = Box::pin(expect_issues_async(
        left_collector,
        IssueExpectationMode::Strict,
        |issue| issue.code == "left",
        |_| async { Ok::<_, ()>(1) },
    ));
    let mut right_future = Box::pin(expect_issues_async(
        right_collector,
        IssueExpectationMode::Strict,
        |issue| issue.code == "right",
        |_| async { Ok::<_, ()>(2) },
    ));
    let mut context = Context::from_waker(Waker::noop());
    let Poll::Ready(missing) = right_future.as_mut().poll(&mut context) else {
        panic!("ready")
    };
    assert_eq!(missing.body_result, Ok(2));
    let missing = missing.expectation.unwrap_err();
    assert!(missing.is_missing());
    assert!(missing.unexpected_issues().is_empty());
    let Poll::Ready(present) = left_future.as_mut().poll(&mut context) else {
        panic!("ready")
    };
    assert_eq!(present.body_result, Ok(1));
    let issues = present.expectation.unwrap();
    assert_eq!(issues.len(), 1);
    assert_eq!(issues[0].code, "left");
    assert!(
        right
            .report(Issue::new("late", "closed right"))
            .unwrap_err()
            .is_closed()
    );
}

/// Keeps an unrelated disabled endpoint from satisfying a strict expectation.
#[test]
fn disabled_external_reporting_does_not_satisfy_strict_or_skip_body() {
    let calls = Cell::new(0);
    let outcome = expect_issues(
        IssueCollector::new(),
        IssueExpectationMode::Strict,
        |_| true,
        |_| {
            calls.set(1);
            let error = IssueReporter::default()
                .report(Issue::new("disabled", "external"))
                .unwrap_err();
            assert!(error.is_disabled());
            assert_eq!(error.into_issue().code, "disabled");
            Ok::<_, ()>(())
        },
    );
    assert_eq!(calls.get(), 1);
    assert_eq!(outcome.body_result, Ok(()));
    let error = outcome.expectation.unwrap_err();
    assert!(error.is_missing());
    assert!(error.unexpected_issues().is_empty());
}

/// Preserves a borrowed body error and recovers reports rejected after completion.
#[test]
fn async_completion_checks_all_reports_and_recovers_late_evidence_after_err() {
    let collector = IssueCollector::new();
    let retained = collector.reporter();
    let calls = Cell::new(0);
    let predicates = RefCell::new(Vec::new());
    let original_error = Rc::new(String::from("original local error"));
    let error_ref = &original_error;
    let mut future = Box::pin(expect_issues_async(
        collector,
        IssueExpectationMode::Strict,
        |issue| {
            predicates.borrow_mut().push(issue.code);
            issue.code == "known"
        },
        |reporter| {
            let calls = &calls;
            async move {
                calls.set(calls.get() + 1);
                for code in ["unexpected", "known", "last"] {
                    reporter.report(Issue::new(code, "all checked")).unwrap();
                }
                Err::<(), _>(LocalError(error_ref))
            }
        },
    ));
    let Poll::Ready(outcome) = future
        .as_mut()
        .poll(&mut Context::from_waker(Waker::noop()))
    else {
        panic!("ready")
    };
    assert!(std::ptr::eq(
        outcome.body_result.unwrap_err().0,
        &raw const original_error
    ));
    let error = outcome.expectation.unwrap_err();
    assert!(!error.is_missing());
    assert_eq!(
        error
            .unexpected_issues()
            .iter()
            .map(|issue| issue.code)
            .collect::<Vec<_>>(),
        ["unexpected", "last"]
    );
    assert_eq!(*predicates.borrow(), ["unexpected", "known", "last"]);
    assert_eq!(calls.get(), 1);
    let rejected = retained
        .report(Issue::new("late", "recover after Err"))
        .unwrap_err();
    assert!(rejected.is_closed());
    assert_eq!(rejected.into_issue().message, "recover after Err");
}

/// Requires the injected factory or matcher panic before retained-future cleanup.
#[test]
fn async_factory_and_matcher_panics_close_retained_futures() {
    for in_matcher in [false, true] {
        let collector = IssueCollector::new();
        let retained = collector.reporter();
        let mut future = Box::pin(expect_issues_async(
            collector,
            IssueExpectationMode::Strict,
            |_| panic!("async matcher"),
            |reporter| {
                assert!(in_matcher, "factory panic");
                async move {
                    reporter
                        .report(Issue::new("known", "before matcher"))
                        .unwrap();
                    Ok::<_, ()>(())
                }
            },
        ));
        let panic = catch_unwind(AssertUnwindSafe(|| {
            future
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
        }))
        .unwrap_err();
        let expected = if in_matcher {
            "async matcher"
        } else {
            "factory panic"
        };
        assert_eq!(panic.downcast_ref::<&str>(), Some(&expected));
        assert!(
            retained
                .report(Issue::new("late", "panic"))
                .unwrap_err()
                .is_closed()
        );
        drop(future);
    }
}

/// Checks that overlapping scopes on separate threads cannot exchange issue evidence.
#[test]
fn concurrently_instantiated_scopes_keep_reporters_and_predicates_isolated() {
    let (left_ready, left_peer) = mpsc::channel();
    let (right_ready, right_peer) = mpsc::channel();
    std::thread::scope(|threads| {
        for (code, ready, peer) in [
            ("left", left_ready, right_peer),
            ("right", right_ready, left_peer),
        ] {
            threads.spawn(move || {
                let outcome = expect_issues(
                    IssueCollector::new(),
                    IssueExpectationMode::Strict,
                    |issue| issue.code == code,
                    |reporter| {
                        ready
                            .send(())
                            .expect("issue scope peer exited before rendezvous");
                        // This watchdog bounds coordination, not issue processing time.
                        peer.recv_timeout(Duration::from_secs(30))
                            .expect("issue scope peer did not reach rendezvous");
                        reporter.report(Issue::new(code, "independent")).unwrap();
                        Ok::<_, ()>(code)
                    },
                );
                assert_eq!(outcome.body_result, Ok(code));
                let issues = outcome.expectation.unwrap();
                assert_eq!(issues.len(), 1);
                assert_eq!(issues[0].code, code);
            });
        }
    });
}
