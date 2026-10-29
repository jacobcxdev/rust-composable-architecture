//! Checks private diagnostics, callback ownership and cleanup at snapshot boundaries.

use std::cell::Cell;
use std::error::Error;
use std::future::{Ready, pending, ready};
use std::rc::Rc;

use futures_executor::block_on;
use rca_test::snapshots::{
    Codec, CodecContext, CodecOperation, FormatRollback, RecordMode, SnapshotFailureKind,
    SnapshotRequest, SnapshotStrategy, capture_ordered,
};

// Formatting must not capture, compare, encode or decode caller-owned data.
#[test]
fn strategy_and_codec_diagnostics_leave_callbacks_uninvoked_and_usable() {
    let calls = Cell::new(0);
    let mut strategy = SnapshotStrategy::new(
        |value: &str| {
            calls.set(calls.get() + 1);
            Ok::<_, ()>(value.as_bytes().to_vec())
        },
        |left: &[u8], right: &[u8]| {
            calls.set(calls.get() + 1);
            Ok::<_, ()>(left == right)
        },
    );
    assert!(format!("{strategy:?}").contains("SnapshotStrategy"));
    assert_eq!(calls.get(), 0);
    let captured = strategy.capture("selected").unwrap();
    assert!(strategy.compare(b"selected".as_slice(), &captured).unwrap());
    assert_eq!(calls.get(), 2);

    let mut codec = Codec::new(
        |value: &u8, context: &mut CodecContext<u8, ()>| {
            calls.set(calls.get() + 1);
            Ok::<_, ()>(vec![*value + context.context])
        },
        |bytes: &[u8], context: &mut CodecContext<u8, ()>| {
            calls.set(calls.get() + 1);
            Ok::<_, ()>(bytes[0] - context.context)
        },
    );
    assert!(format!("{codec:?}").contains("Codec"));
    assert_eq!(calls.get(), 2);
    let mut context = CodecContext {
        context: 2,
        format: (),
    };
    assert_eq!(codec.encode(&3, &mut context).unwrap(), [5]);
    assert_eq!(codec.decode(&[5], &mut context).unwrap(), 3);
    assert_eq!(calls.get(), 4);
}

// Deriving Debug or adding either context field to diagnostics discloses private data.
#[test]
fn codec_context_diagnostics_hide_options_and_selected_format() {
    let context = CodecContext {
        context: "private-options",
        format: "private-format",
    };
    let rendered = format!("{context:?}");
    assert!(rendered.contains("CodecContext"));
    assert!(!rendered.contains("private-options"));
    assert!(!rendered.contains("private-format"));
}

// Request diagnostics expose policy and reference presence, never selected bytes or names.
#[test]
fn request_diagnostics_distinguish_absence_without_disclosing_selected_data() {
    for (reference, expected_presence) in [
        (None, "has_reference: false"),
        (Some(b"private-reference".as_slice()), "has_reference: true"),
    ] {
        let request = SnapshotRequest {
            name: "private-name",
            reference,
            actual: b"private-actual",
            mode: RecordMode::Missing,
        };
        let rendered = format!("{request:?}");
        assert!(rendered.contains("Missing"));
        assert!(rendered.contains(expected_presence));
        for private in ["private-name", "private-reference", "private-actual"] {
            assert!(!rendered.contains(private));
            assert!(!rendered.contains(&format!("{:?}", private.as_bytes())));
        }
    }
}

// Confusing directions or formatting the opaque cause breaks error routing or privacy.
#[test]
fn codec_failures_retain_operation_callsite_and_owned_private_cause() {
    /// Holds private codec data without `Debug`, `Clone` or `Send` bounds.
    struct Cause(Rc<String>);

    let private = Rc::new("private-codec-cause".to_owned());
    let mut codec = Codec::new(
        |_: &u8, _: &mut CodecContext<(), ()>| Err(Cause(Rc::clone(&private))),
        |_: &[u8], _: &mut CodecContext<(), ()>| Err::<u8, _>(Cause(Rc::clone(&private))),
    );
    let mut context = CodecContext {
        context: (),
        format: (),
    };
    for operation in [CodecOperation::Encode, CodecOperation::Decode] {
        let (failure, line) = match operation {
            CodecOperation::Encode => {
                let line = line!() + 1;
                let failure = codec.encode(&1, &mut context).unwrap_err();
                (failure, line)
            }
            CodecOperation::Decode => {
                let line = line!() + 1;
                let failure = codec.decode(&[], &mut context).unwrap_err();
                (failure, line)
            }
        };
        assert_eq!(failure.operation(), operation);
        assert_eq!(failure.location().file(), file!());
        assert_eq!(failure.location().line(), line);
        assert!(Rc::ptr_eq(&failure.cause().0, &private));
        let error: &dyn Error = &failure;
        for rendered in [error.to_string(), format!("{error:?}")] {
            assert!(rendered.contains(&format!("{operation:?}")));
            assert!(rendered.contains(file!()));
            assert!(rendered.contains(&line.to_string()));
            assert!(!rendered.contains(private.as_str()));
        }
        assert!(error.source().is_none());
        assert!(Rc::ptr_eq(&failure.into_cause().0, &private));
    }
}

// Restoring format after success or under KeepDetected erases the decoder's chosen format.
#[test]
fn codec_format_rollback_is_limited_to_failed_on_error_decodes() {
    let mut codec = Codec::new(
        |_: &u8, _: &mut CodecContext<u8, &str>| Ok::<_, ()>(Vec::<u8>::new()),
        |bytes: &[u8], context: &mut CodecContext<u8, &str>| {
            context.context += 1;
            context.format = "detected";
            bytes.first().copied().ok_or("missing value")
        },
    );
    for (policy, bytes, result, format) in [
        (
            FormatRollback::KeepDetected,
            &b""[..],
            Err("missing value"),
            "detected",
        ),
        (
            FormatRollback::KeepDetected,
            &b"x"[..],
            Ok(b'x'),
            "detected",
        ),
        (FormatRollback::OnError, &b"x"[..], Ok(b'x'), "detected"),
        (
            FormatRollback::OnError,
            &b""[..],
            Err("missing value"),
            "selected",
        ),
    ] {
        let mut context = CodecContext {
            context: 0,
            format: "selected",
        };
        assert_eq!(
            codec
                .decode_with(bytes, &mut context, policy)
                .map_err(rca_test::snapshots::CodecFailure::into_cause),
            result,
        );
        assert_eq!(context.format, format);
        assert_eq!(context.context, 1, "context is never rolled back");
    }
}

// Reusing a diagnostic label loses the distinction between mandatory snapshot outcomes.
#[test]
fn snapshot_outcome_diagnostics_are_distinct_and_keep_callback_causes_private() {
    /// Holds private text without implementing diagnostic formatting.
    struct Cause(&'static str);

    let failures = [
        SnapshotFailureKind::InvalidName,
        SnapshotFailureKind::Collision,
        SnapshotFailureKind::Missing,
        SnapshotFailureKind::Mismatch,
        SnapshotFailureKind::WriterRequired,
        SnapshotFailureKind::UpdateRequired,
        SnapshotFailureKind::Redaction(Cause("private-redaction")),
        SnapshotFailureKind::Write(Cause("private-write")),
    ];
    let mut diagnostics = std::collections::BTreeSet::new();
    for failure in failures {
        let rendered = format!("{failure:?}");
        assert!(!rendered.is_empty());
        if let SnapshotFailureKind::Redaction(cause) | SnapshotFailureKind::Write(cause) = failure {
            assert!(!rendered.contains(cause.0));
        }
        assert!(
            diagnostics.insert(rendered),
            "each outcome must remain diagnosable"
        );
    }
}

// Capture diagnostics must identify the failed case without requiring or exposing cause Debug.
#[test]
fn capture_failure_diagnostics_preserve_index_callsite_and_private_cause() {
    /// Retains the exact local cause of one failed capture.
    struct Cause(Rc<String>);

    let private = Rc::new("private-capture-cause".to_owned());
    let cases = [Ok(7u8), Err(Cause(Rc::clone(&private)))]
        .into_iter()
        .map(|result| move || ready(result))
        .collect();
    let line = line!() + 1;
    let future = capture_ordered(cases, pending::<()>(), |_| panic!("capture failed"));
    let failure = block_on(future).unwrap_err();
    assert!(!failure.is_timeout());
    let (index, cause) = failure.cause().unwrap();
    assert_eq!(index, 1);
    assert!(Rc::ptr_eq(&cause.0, &private));
    assert_eq!(failure.location().file(), file!());
    assert_eq!(failure.location().line(), line);
    let error: &dyn Error = &failure;
    for rendered in [error.to_string(), format!("{error:?}")] {
        assert!(rendered.contains("index: Some(1)"));
        assert!(rendered.contains(file!()));
        assert!(rendered.contains(&line.to_string()));
        assert!(!rendered.contains(private.as_str()));
    }
    assert!(error.source().is_none());
    let (index, cause) = failure.into_cause().unwrap();
    assert_eq!(index, 1);
    assert!(Rc::ptr_eq(&cause.0, &private));
}

// Timeout diagnostics must not invent a failed capture index or cause.
#[test]
fn timeout_diagnostics_have_no_case_cause_and_preserve_construction_callsite() {
    /// Names the uncalled factory type for an empty capture list.
    type Case = fn() -> Ready<Result<u8, ()>>;
    let cases: Vec<Case> = Vec::new();
    let line = line!() + 1;
    let future = capture_ordered(cases, ready(()), |_| panic!("deadline won"));
    let failure = block_on(future).unwrap_err();
    assert!(failure.is_timeout());
    assert!(failure.cause().is_none());
    assert_eq!(failure.location().file(), file!());
    assert_eq!(failure.location().line(), line);
    let error: &dyn Error = &failure;
    for rendered in [error.to_string(), format!("{error:?}")] {
        assert!(rendered.contains("index: None"));
        assert!(rendered.contains(file!()));
        assert!(rendered.contains(&line.to_string()));
    }
    assert!(error.source().is_none());
    assert!(failure.into_cause().is_none());
}

// Terminal polling must release callback authority before the completed future is dropped.
#[test]
fn terminal_capture_releases_owners_before_completed_future_drop() {
    for timeout_ready in [false, true] {
        let owner = Rc::new(());
        let cases = (0..2)
            .map(|index| {
                let captured = Rc::clone(&owner);
                move || async move {
                    std::hint::black_box(&captured);
                    if index == 1 {
                        Err("capture failed")
                    } else {
                        pending::<Result<(), &str>>().await
                    }
                }
            })
            .collect();
        let deadline_owner = Rc::clone(&owner);
        let deadline = std::future::poll_fn(move |_| {
            std::hint::black_box(&deadline_owner);
            if timeout_ready {
                std::task::Poll::Ready(())
            } else {
                std::task::Poll::Pending
            }
        });
        let finish_owner = Rc::clone(&owner);
        let mut future = Box::pin(capture_ordered(cases, deadline, move |_| {
            drop(finish_owner);
            panic!("terminal failure must not invoke finish");
        }));
        assert_eq!(Rc::strong_count(&owner), 5);

        let mut context = std::task::Context::from_waker(std::task::Waker::noop());
        let std::task::Poll::Ready(Err(failure)) = future.as_mut().poll(&mut context) else {
            panic!("the selected terminal condition must complete on the first poll");
        };
        assert_eq!(failure.is_timeout(), timeout_ready);
        assert_eq!(Rc::strong_count(&owner), 1);
        drop(future);
        assert_eq!(Rc::strong_count(&owner), 1);
    }
}

// A projection destructor can unwind after capture produces an independently owned value.
#[test]
fn pullback_releases_captured_value_when_projection_cleanup_panics() {
    /// Fails during projection cleanup after the strategy produces its value.
    struct Projection;

    impl Drop for Projection {
        fn drop(&mut self) {
            panic!("projection cleanup failed");
        }
    }

    let owner = Rc::new(());
    let mut strategy = SnapshotStrategy::new(
        |_: &Projection| Ok::<_, ()>(Rc::clone(&owner)),
        |expected: &Rc<()>, actual: &Rc<()>| Ok::<_, ()>(Rc::ptr_eq(expected, actual)),
    )
    .try_pullback(|(): &()| Ok::<_, ()>(Projection));
    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = strategy.capture(&());
    }));

    let panic = panic.unwrap_err();
    assert_eq!(
        panic.downcast_ref::<&str>(),
        Some(&"projection cleanup failed")
    );
    assert_eq!(Rc::strong_count(&owner), 1);
}

// Successful decoding still destroys the rollback copy before returning its owned value.
#[test]
fn codec_releases_decoded_value_when_unused_rollback_cleanup_panics() {
    /// Arms only the rollback copy to panic when destroyed.
    struct Format {
        panic_on_drop: bool,
    }

    impl Clone for Format {
        fn clone(&self) -> Self {
            Self {
                panic_on_drop: true,
            }
        }
    }

    impl Drop for Format {
        fn drop(&mut self) {
            assert!(!self.panic_on_drop, "rollback format cleanup failed");
        }
    }

    let owner = Rc::new(());
    let mut codec = Codec::new(
        |(): &(), _: &mut CodecContext<(), Format>| Ok::<_, ()>(Vec::<u8>::new()),
        |_: &[u8], _: &mut CodecContext<(), Format>| Ok::<_, ()>(Rc::clone(&owner)),
    );
    let mut context = CodecContext {
        context: (),
        format: Format {
            panic_on_drop: false,
        },
    };
    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = codec.decode_with(&[], &mut context, FormatRollback::OnError);
    }));

    let panic = panic.unwrap_err();
    assert_eq!(
        panic.downcast_ref::<&str>(),
        Some(&"rollback format cleanup failed")
    );
    assert_eq!(Rc::strong_count(&owner), 1);
}

// Successful capture values stay owned while the deadline future is destroyed.
#[test]
fn ordered_capture_releases_values_when_deadline_cleanup_panics() {
    /// Unwinds from deadline cleanup after all cases succeed.
    struct DeadlineCleanup;

    impl Drop for DeadlineCleanup {
        fn drop(&mut self) {
            std::panic::resume_unwind(Box::new("deadline cleanup failed"));
        }
    }

    let owner = Rc::new(());
    let cleanup = DeadlineCleanup;
    let deadline = std::future::poll_fn(move |_| {
        std::hint::black_box(&cleanup);
        std::task::Poll::<()>::Pending
    });
    let cases = vec![|| ready(Ok::<_, ()>(Rc::clone(&owner)))];
    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        block_on(capture_ordered(cases, deadline, |values| values))
    }));

    let panic = panic.unwrap_err();
    assert_eq!(
        panic.downcast_ref::<&str>(),
        Some(&"deadline cleanup failed")
    );
    assert_eq!(Rc::strong_count(&owner), 1);
}

// An owned capture error stays owned while a pending sibling is destroyed.
#[test]
fn ordered_capture_releases_error_when_sibling_cleanup_panics() {
    /// Unwinds while a failed capture releases its pending sibling.
    struct SiblingCleanup;

    impl Drop for SiblingCleanup {
        fn drop(&mut self) {
            std::panic::resume_unwind(Box::new("sibling cleanup failed"));
        }
    }

    let owner = Rc::new(());
    let cases = (0..2)
        .map(|index| {
            let cause = Rc::clone(&owner);
            move || async move {
                if index == 0 {
                    let _cleanup = SiblingCleanup;
                    pending::<Result<(), Rc<()>>>().await
                } else {
                    Err(cause)
                }
            }
        })
        .collect();
    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        block_on(capture_ordered(cases, pending::<()>(), |_| {
            panic!("failed capture must not invoke finish");
        }))
    }));

    let panic = panic.unwrap_err();
    assert_eq!(
        panic.downcast_ref::<&str>(),
        Some(&"sibling cleanup failed")
    );
    assert_eq!(Rc::strong_count(&owner), 1);
}

// A failed capture keeps its error owned while the unused finish callback drops.
#[test]
fn ordered_capture_releases_error_when_finish_cleanup_panics() {
    /// Unwinds from the uncalled finish callback's captured resource.
    struct FinishCleanup;

    impl Drop for FinishCleanup {
        fn drop(&mut self) {
            std::panic::resume_unwind(Box::new("finish cleanup failed"));
        }
    }

    let owner = Rc::new(());
    let cases = vec![|| ready(Err::<(), _>(Rc::clone(&owner)))];
    let cleanup = FinishCleanup;
    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        block_on(capture_ordered(cases, pending::<()>(), move |_| {
            std::hint::black_box(&cleanup);
            panic!("failed capture must not invoke finish");
        }))
    }));

    let panic = panic.unwrap_err();
    assert_eq!(panic.downcast_ref::<&str>(), Some(&"finish cleanup failed"));
    assert_eq!(Rc::strong_count(&owner), 1);
}

// Cancellation drops the deadline before pending captures and completed values.
#[test]
fn ordered_capture_cancellation_keeps_deadline_before_cases_and_values() {
    /// Records destruction order without owning the observed collection exclusively.
    struct Trace {
        label: &'static str,
        events: Rc<std::cell::RefCell<Vec<&'static str>>>,
    }

    impl Drop for Trace {
        fn drop(&mut self) {
            self.events.borrow_mut().push(self.label);
        }
    }

    let events = Rc::new(std::cell::RefCell::new(Vec::new()));
    let cases = (0..2)
        .map(|index| {
            let events = Rc::clone(&events);
            move || async move {
                if index == 0 {
                    Ok(Trace {
                        label: "value",
                        events,
                    })
                } else {
                    let _pending = Trace {
                        label: "pending",
                        events,
                    };
                    pending::<Result<Trace, ()>>().await
                }
            }
        })
        .collect();
    let deadline = Trace {
        label: "deadline",
        events: Rc::clone(&events),
    };
    let deadline = std::future::poll_fn(move |_| {
        std::hint::black_box(&deadline);
        std::task::Poll::<()>::Pending
    });
    let finish = Trace {
        label: "finish",
        events: Rc::clone(&events),
    };
    let mut capture = Box::pin(capture_ordered(cases, deadline, move |_| drop(finish)));
    let mut context = std::task::Context::from_waker(std::task::Waker::noop());
    assert!(capture.as_mut().poll(&mut context).is_pending());
    assert!(events.borrow().is_empty());

    drop(capture);

    assert_eq!(*events.borrow(), ["deadline", "pending", "value", "finish"]);
}

// Both recording outcomes can move the path already borrowed by the writer.
#[test]
fn recording_failures_reuse_the_writer_path_allocation() {
    use rca_test::snapshots::{ArtifactKind, ReferenceWriter, SnapshotNamespace, SnapshotPath};

    /// Records path identity while accepting or rejecting one reference write.
    struct Writer {
        path: Option<*const u8>,
        fail: bool,
    }

    impl ReferenceWriter for Writer {
        type Error = ();

        fn write(
            &mut self,
            path: &SnapshotPath,
            kind: ArtifactKind,
            expected: Option<&[u8]>,
            bytes: &[u8],
        ) -> Result<(), Self::Error> {
            assert!(self.path.replace(path.as_str().as_ptr()).is_none());
            assert_eq!(kind, ArtifactKind::Reference);
            assert_eq!(expected, None);
            assert_eq!(bytes, b"new");
            if self.fail {
                return Err(());
            }
            Ok(())
        }
    }

    for fail in [false, true] {
        let mut namespace = SnapshotNamespace::new("suite").unwrap();
        let mut writer = Writer { path: None, fail };
        let failure = namespace
            .check(
                SnapshotRequest {
                    name: "case",
                    reference: None,
                    actual: b"new",
                    mode: RecordMode::Missing,
                },
                |bytes| Ok::<_, ()>(bytes.to_vec()),
                Some(&mut writer),
            )
            .unwrap_err();

        if fail {
            assert!(matches!(failure.kind(), SnapshotFailureKind::Write(())));
        } else {
            assert!(matches!(
                failure.kind(),
                SnapshotFailureKind::UpdateRequired
            ));
        }
        assert_eq!(failure.path().unwrap().as_str(), "suite/case");
        assert_eq!(Some(failure.path().unwrap().as_str().as_ptr()), writer.path);
    }
}
