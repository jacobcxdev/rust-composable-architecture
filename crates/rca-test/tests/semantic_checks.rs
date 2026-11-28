//! Checks semantic comparison independently from rendering, operation results and cleanup.

use std::any::Any;
use std::cell::{Cell, RefCell};
use std::fmt;
use std::future::{pending, poll_fn};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::rc::Rc;
use std::task::{Context, Poll, Waker};

use futures_executor::block_on;
use rca_core::IssueCollector;
use rca_core::{Issue, IssueReporter};
use rca_test::{
    ChangeCheck, check_change, check_change_async, check_equal, check_expected_changes,
};

/// Borrows the original panic message so another failure cannot satisfy a stage check.
fn panic_message(payload: &(dyn Any + Send)) -> &str {
    payload
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| payload.downcast_ref::<&str>().copied())
        .expect("expected a string panic payload")
}

mod cleanup {
    //! Separates independent result and snapshot owners from the cleanup failure matrix.

    use super::*;

    /// Tracks the operation result independently of the captured snapshots.
    struct Output<'a>(&'a Cell<usize>);

    impl Drop for Output<'_> {
        fn drop(&mut self) {
            self.0.set(self.0.get() + 1);
        }
    }

    /// Can fail when the capture closure releases its owned resource.
    struct CaptureDrop<'a> {
        panic: bool,
        events: &'a RefCell<Vec<&'static str>>,
    }

    impl Drop for CaptureDrop<'_> {
        fn drop(&mut self) {
            self.events.borrow_mut().push("capture");
            assert!(!self.panic, "capture cleanup");
        }
    }

    /// Identifies each snapshot's destruction without relying on its formatted value.
    struct Value<'a> {
        number: i32,
        actual: bool,
        panic: bool,
        events: &'a RefCell<Vec<&'static str>>,
    }

    impl PartialEq for Value<'_> {
        fn eq(&self, other: &Self) -> bool {
            self.number == other.number
        }
    }

    impl Drop for Value<'_> {
        fn drop(&mut self) {
            self.events
                .borrow_mut()
                .push(if self.actual { "actual" } else { "expected" });
            assert!(!self.panic, "snapshot cleanup");
        }
    }

    #[test]
    fn cleanup_panics_release_completed_operation_results() {
        let mut observed = Vec::new();
        for asynchronous in [false, true] {
            for stage in ["expected", "actual", "capture"] {
                let dropped = Cell::new(0);
                let events = RefCell::new(Vec::new());
                let mut count = 0;
                let capture_drop = CaptureDrop {
                    panic: stage == "capture",
                    events: &events,
                };
                let capture = move |number: &i32| {
                    std::hint::black_box(&capture_drop);
                    let actual = *number != 0;
                    Value {
                        number: *number,
                        actual,
                        panic: if actual {
                            stage == "actual"
                        } else {
                            stage == "expected"
                        },
                        events: capture_drop.events,
                    }
                };
                let result = catch_unwind(AssertUnwindSafe(|| {
                    if asynchronous {
                        drop(block_on(check_change_async(
                            &mut count,
                            capture,
                            async |number| {
                                *number += 1;
                                Ok::<_, ()>(Output(&dropped))
                            },
                            |expected| expected.number += 1,
                            |_| String::new(),
                            || None,
                        )));
                    } else {
                        drop(check_change(
                            &mut count,
                            capture,
                            |number| {
                                *number += 1;
                                Ok::<_, ()>(Output(&dropped))
                            },
                            |expected| expected.number += 1,
                            |_| String::new(),
                            || None,
                        ));
                    }
                }));
                let panic = result.expect_err("the selected cleanup callback must panic");
                assert_eq!(
                    panic_message(panic.as_ref()),
                    if stage == "capture" {
                        "capture cleanup"
                    } else {
                        "snapshot cleanup"
                    },
                    "{asynchronous}/{stage}"
                );
                assert_eq!(*events.borrow(), ["expected", "actual", "capture"]);
                observed.push(dropped.get());
            }
        }
        assert_eq!(
            observed, [1; 6],
            "completed operation results must not leak during cleanup"
        );
    }
}

mod required_changes {
    //! Checks the adapter's saved history and each user-controlled failure stage.

    use super::*;

    /// Deliberately lacks `Debug`, `Send` and `'static`: the adapter needs none of them.
    struct Value<'a> {
        number: u32,
        events: &'a RefCell<Vec<&'static str>>,
        panic_at: &'static str,
        saved: bool,
    }
    impl Clone for Value<'_> {
        fn clone(&self) -> Self {
            self.events.borrow_mut().push("clone");
            assert_ne!(self.panic_at, "clone", "clone panic");
            Self {
                number: self.number,
                events: self.events,
                panic_at: self.panic_at,
                saved: true,
            }
        }
    }
    impl PartialEq for Value<'_> {
        fn eq(&self, other: &Self) -> bool {
            self.events.borrow_mut().push("compare");
            assert_ne!(self.panic_at, "compare", "equality panic");
            self.number == other.number
        }
    }
    impl Drop for Value<'_> {
        fn drop(&mut self) {
            if self.saved {
                self.events.borrow_mut().push("drop");
                assert_ne!(self.panic_at, "drop", "saved clone destructor panic");
            }
        }
    }

    #[test]
    fn adapter_clones_edits_compares_once_and_drops_saved_history_before_return() {
        let events = RefCell::new(Vec::new());
        let mut value = Value {
            number: 0,
            events: &events,
            panic_at: "",
            saved: false,
        };
        rca_test::requiring_change(|value: &mut Value<'_>| {
            events.borrow_mut().push("edit");
            value.number += 1;
        })(&mut value);
        assert_eq!(value.number, 1);
        assert_eq!(*events.borrow(), ["clone", "edit", "compare", "drop"]);
    }

    #[test]
    fn no_change_is_a_value_free_assertion_and_user_panics_stop_the_adapter() {
        for (stage, expected_message) in [
            ("", "expected value did not change"),
            ("clone", "clone panic"),
            ("edit", "edit panic"),
            ("compare", "equality panic"),
            ("drop", "saved clone destructor panic"),
        ] {
            let events = RefCell::new(Vec::new());
            let mut value = Value {
                number: 0,
                events: &events,
                panic_at: stage,
                saved: false,
            };
            let failure = catch_unwind(AssertUnwindSafe(|| {
                rca_test::requiring_change(|value: &mut Value<'_>| {
                    events.borrow_mut().push("edit");
                    assert_ne!(stage, "edit", "edit panic");
                    if !stage.is_empty() {
                        value.number += 1;
                    }
                })(&mut value);
            }))
            .expect_err("no-op and each user panic must propagate");
            let expected: &[&str] = match stage {
                "clone" => &["clone"],
                "edit" => &["clone", "edit", "drop"],
                _ => &["clone", "edit", "compare", "drop"],
            };
            assert_eq!(*events.borrow(), expected);
            let message = panic_message(failure.as_ref());
            if stage.is_empty() {
                assert_eq!(message, expected_message);
            } else {
                assert!(message.contains(expected_message));
            }
        }
    }
}

#[test]
fn change_check_debug_never_renders_private_operation_values() {
    let mut leaked = Vec::new();
    for operation_result in [Ok("private-success-token"), Err("private-error-token")] {
        let checked = ChangeCheck {
            operation_result,
            comparison: check_equal(
                || 0,
                || 1,
                |_| "redacted".to_owned(),
                || Some("private-comparison-token".to_owned()),
            ),
        };
        let rendered = format!("{checked:?}");
        if rendered.contains("private-") {
            leaked.push(rendered);
        }
        assert_eq!(checked.operation_result, operation_result);
        let failure = checked.comparison.unwrap_err();
        assert!(failure.is_mismatch());
        assert_eq!(failure.message(), Some("private-comparison-token"));
    }
    assert!(leaked.is_empty(), "leaked operation contents: {leaked:?}");
}

#[test]
fn change_check_debug_accepts_non_debug_success_and_error_values() {
    /// Deliberately lacks `Debug`; diagnostics must not impose it on operation results.
    struct PrivateValue(u32);
    for operation_result in [Ok(PrivateValue(7)), Err(PrivateValue(9))] {
        let checked = ChangeCheck {
            operation_result,
            comparison: Ok(()),
        };
        let rendered = format!("{checked:?}");
        assert!(rendered.contains("comparison_succeeded: true"));
        match checked.operation_result {
            Ok(value) => {
                assert_eq!(value.0, 7);
                assert!(rendered.contains("operation_succeeded: true"));
            }
            Err(value) => {
                assert_eq!(value.0, 9);
                assert!(rendered.contains("operation_succeeded: false"));
            }
        }
        checked.comparison.unwrap();
    }
}

/// Records semantic equality calls separately from capture and rendering callbacks.
struct Snapshot<'a> {
    value: i32,
    events: &'a RefCell<Vec<&'static str>>,
}

impl PartialEq for Snapshot<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.events.borrow_mut().push("compare");
        self.value == other.value
    }
}

#[test]
fn equality_captures_once_and_never_formats_success() {
    let events = RefCell::new(Vec::new());
    check_equal(
        || {
            events.borrow_mut().push("expected");
            Snapshot {
                value: 1,
                events: &events,
            }
        },
        || {
            events.borrow_mut().push("actual");
            Snapshot {
                value: 1,
                events: &events,
            }
        },
        |_| panic!("success must not render"),
        || panic!("success must not build a message"),
    )
    .unwrap();
    assert_eq!(*events.borrow(), ["expected", "actual", "compare"]);
}

#[test]
fn redacted_mismatch_retains_semantics_order_and_caller() {
    let events = RefCell::new(Vec::new());
    let line = line!() + 1;
    let failure = check_equal(
        || Snapshot {
            value: 1,
            events: &events,
        },
        || Snapshot {
            value: 2,
            events: &events,
        },
        |snapshot| {
            events.borrow_mut().push(if snapshot.value == 1 {
                "render expected"
            } else {
                "render actual"
            });
            "<redacted>".to_owned()
        },
        || {
            events.borrow_mut().push("message");
            Some("selected snapshot".to_owned())
        },
    )
    .unwrap_err();
    assert!(!failure.is_no_change());
    assert!(failure.is_mismatch());
    assert_eq!(failure.location().line(), line);
    assert_eq!(failure.message(), Some("selected snapshot"));
    assert!(failure.excerpt().unwrap().contains("differ semantically"));
    assert!(failure.to_string().contains("selected snapshot"));
    assert_eq!(
        *events.borrow(),
        ["compare", "message", "render expected", "render actual"]
    );
}

#[test]
fn comparison_display_propagates_each_section_failure_without_losing_diagnostics() {
    /// Rejects one output section and counts any writes attempted after that failure.
    struct RejectSection<'a> {
        section: &'a str,
        output: String,
        rejected: bool,
        writes_after_rejection: usize,
    }

    impl fmt::Write for RejectSection<'_> {
        fn write_str(&mut self, s: &str) -> fmt::Result {
            if self.rejected {
                self.writes_after_rejection += 1;
                return Err(fmt::Error);
            }
            self.output.push_str(s);
            if self.output.contains(self.section) {
                self.rejected = true;
                return Err(fmt::Error);
            }
            Ok(())
        }
    }

    let message = String::from("caller-owned comparison message");
    let message_pointer = message.as_ptr();
    let checked = check_change(
        &mut 0,
        |value| *value,
        |_| Ok::<_, ()>(()),
        |expected| *expected = 1,
        |_| "<redacted>".to_owned(),
        || Some(message),
    );
    checked.operation_result.unwrap();
    let failure = checked.comparison.unwrap_err();
    assert!(failure.is_no_change());
    assert!(failure.is_mismatch());
    let excerpt = failure.excerpt().unwrap();
    let excerpt_pointer = excerpt.as_ptr();
    assert!(excerpt.contains("differ semantically"));
    let sections = [
        "Semantic check failed at",
        "Operation did not change the captured value.",
        "Expected and actual values differ.",
        "caller-owned comparison message",
        excerpt,
    ];
    let mut baseline = String::new();
    fmt::write(&mut baseline, format_args!("{failure}")).unwrap();
    let positions = sections.map(|section| baseline.find(section).unwrap());
    assert!(positions.windows(2).all(|pair| pair[0] < pair[1]));

    for section in sections {
        let mut sink = RejectSection {
            section,
            output: String::new(),
            rejected: false,
            writes_after_rejection: 0,
        };
        assert_eq!(
            fmt::write(&mut sink, format_args!("{failure}")),
            Err(fmt::Error)
        );
        assert!(sink.rejected, "section was not reached: {section}");
        assert!(baseline.starts_with(&sink.output));
        assert_eq!(sink.writes_after_rejection, 0, "section: {section}");
        assert_eq!(failure.message(), Some("caller-owned comparison message"));
        assert_eq!(failure.message().unwrap().as_ptr(), message_pointer);
        assert_eq!(failure.excerpt(), Some(excerpt));
        assert_eq!(failure.excerpt().unwrap().as_ptr(), excerpt_pointer);
    }
    assert_eq!(failure.to_string(), baseline);
}

#[test]
fn operationless_changes_compare_once_and_allow_unchanged() {
    for value in [0, 1] {
        let events = RefCell::new(Vec::new());
        check_expected_changes(
            || {
                events.borrow_mut().push("before");
                Snapshot {
                    value: 0,
                    events: &events,
                }
            },
            || {
                events.borrow_mut().push("actual");
                Snapshot {
                    value,
                    events: &events,
                }
            },
            |snapshot| {
                events.borrow_mut().push("update");
                snapshot.value = value;
            },
            |_| panic!("success must not render"),
            || panic!("success must not build a message"),
        )
        .unwrap();
        assert_eq!(*events.borrow(), ["before", "actual", "update", "compare"]);
    }
}

#[test]
fn operationless_redaction_never_falls_back_to_private_snapshot_contents() {
    /// Compares private content without allowing its diagnostic formatting.
    #[derive(PartialEq)]
    struct Secret(String);
    let failure = check_expected_changes(
        || Secret("before-private-token".to_owned()),
        || Secret("actual-private-token".to_owned()),
        |expected| expected.0 = "expected-private-token".to_owned(),
        |_| "<redacted>".to_owned(),
        || None,
    )
    .unwrap_err();
    assert!(failure.is_mismatch());
    assert!(!failure.is_no_change());
    for text in [failure.to_string(), format!("{failure:?}")] {
        assert!(!text.contains("private-token"));
        assert!(text.contains("differ semantically"));
    }
}

#[test]
fn operation_error_does_not_skip_capture_update_or_independent_comparison() {
    /// Borrows a thread-local value to reject accidental ownership or `Send` bounds.
    struct LocalError<'a>(&'a Rc<String>);
    let secret = Rc::new(String::from("private operation error"));
    for expected in [1, 2] {
        let events = RefCell::new(Vec::new());
        let mut context = 0;
        let outcome = check_change(
            &mut context,
            |value| {
                events.borrow_mut().push("capture");
                Snapshot {
                    value: *value,
                    events: &events,
                }
            },
            |value| {
                events.borrow_mut().push("operation");
                *value = 1;
                Err::<(), _>(LocalError(&secret))
            },
            |snapshot| {
                events.borrow_mut().push("update");
                snapshot.value = expected;
            },
            |snapshot| {
                events.borrow_mut().push("render");
                snapshot.value.to_string()
            },
            || {
                events.borrow_mut().push("message");
                None
            },
        );
        let Err(error) = outcome.operation_result else {
            panic!("typed error was lost")
        };
        assert!(Rc::ptr_eq(error.0, &secret));
        if expected == 1 {
            outcome.comparison.unwrap();
            assert_eq!(
                *events.borrow(),
                [
                    "capture",
                    "operation",
                    "capture",
                    "compare",
                    "update",
                    "compare"
                ]
            );
        } else {
            assert!(outcome.comparison.unwrap_err().is_mismatch());
            assert_eq!(
                *events.borrow(),
                [
                    "capture",
                    "operation",
                    "capture",
                    "compare",
                    "update",
                    "compare",
                    "message",
                    "render",
                    "render"
                ]
            );
        }
    }
}

#[test]
fn unchanged_operation_fails_and_does_not_mask_wrong_expectation() {
    for expected in [0, 1] {
        let events = RefCell::new(Vec::new());
        let outcome = check_change(
            &mut 0,
            |value| {
                events.borrow_mut().push("capture");
                Snapshot {
                    value: *value,
                    events: &events,
                }
            },
            |_| {
                events.borrow_mut().push("operation");
                Ok::<_, ()>("retained")
            },
            |snapshot| {
                events.borrow_mut().push("update");
                snapshot.value = expected;
            },
            |snapshot| {
                events.borrow_mut().push("render");
                snapshot.value.to_string()
            },
            || {
                events.borrow_mut().push("message");
                None
            },
        );
        assert_eq!(outcome.operation_result, Ok("retained"));
        let failure = outcome.comparison.unwrap_err();
        assert!(failure.is_no_change());
        assert_eq!(failure.is_mismatch(), expected == 1);
        assert_eq!(failure.excerpt().is_some(), expected == 1);
        let display = failure.to_string();
        assert!(display.contains("Operation did not change the captured value."));
        assert_eq!(
            display.contains("Expected and actual values differ."),
            expected == 1,
        );
        assert!(display.ends_with(&failure.backtrace().to_string()));
        let mut expected_events = vec![
            "capture",
            "operation",
            "capture",
            "compare",
            "update",
            "compare",
            "message",
        ];
        if expected == 1 {
            expected_events.extend(["render", "render"]);
        }
        assert_eq!(*events.borrow(), expected_events);
    }
}

#[test]
fn async_lending_operation_keeps_local_borrows_error_and_constructor_location() {
    let local = Rc::new(String::from("local"));
    let events = RefCell::new(Vec::new());
    let mut context = (0, local.as_str());
    let line = line!() + 1;
    let future = check_change_async(
        &mut context,
        |value| {
            events.borrow_mut().push("capture");
            (value.0, value.1)
        },
        async |value| {
            events.borrow_mut().push("operation");
            std::future::ready(()).await;
            value.0 = 1;
            Err::<(), _>(&local)
        },
        |expected| {
            events.borrow_mut().push("update");
            expected.0 = 2;
        },
        |_| {
            events.borrow_mut().push("render");
            "redacted".to_owned()
        },
        || {
            events.borrow_mut().push("message");
            None
        },
    );
    assert!(events.borrow().is_empty());
    let outcome = block_on(future);
    let Err(error) = outcome.operation_result else {
        panic!("typed error was lost")
    };
    assert!(Rc::ptr_eq(error, &local));
    assert_eq!(outcome.comparison.unwrap_err().location().line(), line);
    assert_eq!(
        *events.borrow(),
        [
            "capture",
            "operation",
            "capture",
            "update",
            "message",
            "render",
            "render"
        ]
    );
}

#[test]
fn async_resumption_does_not_repeat_capture_or_operation_and_compares_twice() {
    let events = RefCell::new(Vec::new());
    let mut context = 0;
    let mut future = Box::pin(check_change_async(
        &mut context,
        |value| {
            events.borrow_mut().push("capture");
            Snapshot {
                value: *value,
                events: &events,
            }
        },
        async |value| {
            events.borrow_mut().push("operation");
            let mut first = true;
            poll_fn(|_| {
                events.borrow_mut().push("poll operation");
                if std::mem::take(&mut first) {
                    Poll::Pending
                } else {
                    Poll::Ready(())
                }
            })
            .await;
            *value = 1;
            Ok::<_, ()>(Box::new(7))
        },
        |snapshot| {
            events.borrow_mut().push("update");
            snapshot.value = 1;
        },
        |_| panic!("success must not render"),
        || panic!("success must not build a message"),
    ));
    let mut cx = Context::from_waker(Waker::noop());
    assert!(matches!(future.as_mut().poll(&mut cx), Poll::Pending));
    assert_eq!(*events.borrow(), ["capture", "operation", "poll operation"]);
    let Poll::Ready(checked) = future.as_mut().poll(&mut cx) else {
        panic!("operation should complete")
    };
    assert_eq!(*checked.operation_result.unwrap(), 7);
    checked.comparison.unwrap();
    assert_eq!(
        *events.borrow(),
        [
            "capture",
            "operation",
            "poll operation",
            "poll operation",
            "capture",
            "compare",
            "update",
            "compare"
        ]
    );
}

#[test]
fn unpolled_and_pending_drop_do_not_complete_a_check() {
    for poll in [false, true] {
        let events = RefCell::new(Vec::new());
        let mut context = 0;
        let owned = Rc::new(());
        let capture_owner = Rc::clone(&owned);
        let operation_events = &events;
        let mut future = Box::pin(check_change_async(
            &mut context,
            |value| {
                events.borrow_mut().push("capture");
                *value
            },
            async move |_| {
                operation_events.borrow_mut().push("operation");
                let result = pending::<Result<(), ()>>().await;
                drop(capture_owner);
                result
            },
            |_| panic!("pending operation must not update"),
            |_| panic!("pending operation must not render"),
            || panic!("pending operation must not build a message"),
        ));
        if poll {
            assert!(matches!(
                future
                    .as_mut()
                    .poll(&mut Context::from_waker(Waker::noop())),
                Poll::Pending
            ));
        }
        drop(future);
        assert_eq!(Rc::strong_count(&owned), 1);
        let expected: &[&str] = if poll { &["capture", "operation"] } else { &[] };
        assert_eq!(*events.borrow(), expected);
    }
}

#[test]
fn disabled_and_closed_reporting_cannot_hide_mismatch() {
    let collector = IssueCollector::new();
    let closed = collector.reporter();
    assert_eq!(collector.finish(), [] as [rca_core::Issue; 0]);
    for reporter in [IssueReporter::default(), closed] {
        let failure = check_equal(
            || 1,
            || 2,
            i32::to_string,
            || {
                let error = reporter
                    .report(Issue::new("comparison", "required mismatch"))
                    .unwrap_err();
                Some(error.to_string())
            },
        )
        .unwrap_err();
        assert!(failure.is_mismatch());
    }
}

#[test]
fn callback_panics_propagate_without_running_later_callbacks() {
    let updates = Cell::new(0);
    let panic = catch_unwind(AssertUnwindSafe(|| {
        let _outcome = check_change(
            &mut 0,
            |value| *value,
            |_| -> Result<(), ()> { panic!("operation panic") },
            |_| updates.set(updates.get() + 1),
            i32::to_string,
            || None,
        );
    }))
    .unwrap_err();
    assert_eq!(panic.downcast_ref::<&str>(), Some(&"operation panic"));
    assert_eq!(updates.get(), 0);
    let panic = catch_unwind(|| {
        let _comparison = check_equal(
            || 0,
            || 1,
            |_| panic!("must not render"),
            || -> Option<String> { panic!("message panic") },
        );
    })
    .unwrap_err();
    assert_eq!(panic.downcast_ref::<&str>(), Some(&"message panic"));
}
