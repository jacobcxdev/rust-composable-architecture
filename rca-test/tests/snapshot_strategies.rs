//! Checks capture, comparison and explicit recording without filesystem authority.

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::future::{pending, ready};
use std::rc::Rc;
use std::task::{Context, Waker};

use futures_executor::block_on;
use rca_test::snapshots::{
    ArtifactKind, Codec, CodecContext, FormatRollback, PullbackError, RecordMode, ReferenceWriter,
    SnapshotFailureKind, SnapshotNamespace, SnapshotPath, SnapshotRequest, SnapshotStrategy,
    capture_ordered,
};

// Removing projection failure propagation or adding domain Clone/Debug/Send breaks this.
#[test]
fn fallible_pullback_preserves_move_only_local_causes_and_comparator() {
    /// Shares mutable local input without `Clone`, `Debug` or `Send` bounds.
    struct Domain(Rc<Cell<u8>>);
    /// Retains the exact failed projection source without public trait bounds.
    struct Cause(Rc<Cell<u8>>);

    let calls = Cell::new(0);
    let mut strategy = SnapshotStrategy::new(
        |value: &u8| {
            calls.set(calls.get() + 1);
            Ok::<_, Cause>(*value)
        },
        |left: &u8, right: &u8| Ok::<_, Cause>(left == right),
    )
    .try_pullback(|root: &Domain| {
        if root.0.get() == 0 {
            Err(Cause(Rc::clone(&root.0)))
        } else {
            Ok(root.0.get())
        }
    });
    let root = Domain(Rc::new(Cell::new(0)));
    let Err(PullbackError::Projection(cause)) = strategy.capture(&root) else {
        panic!("projection cause")
    };
    assert!(Rc::ptr_eq(&cause.0, &root.0));
    assert_eq!(calls.get(), 0);
    root.0.set(3);
    let Ok(value) = strategy.capture(&root) else {
        panic!("capture")
    };
    assert_eq!(value, 3);
    assert!(matches!(strategy.compare(&value, &3), Ok(true)));
    assert_eq!(calls.get(), 1);
}

#[test]
fn codec_substitution_preserves_context_and_detected_format_on_failure() {
    let mut codec = Codec::new(
        |value: &u8, context: &mut CodecContext<u8, &'static str>| {
            Ok::<_, u8>(vec![*value + context.context])
        },
        |bytes: &[u8], context: &mut CodecContext<u8, &'static str>| {
            context.format = "detected";
            if bytes.len() == 1 {
                Ok(bytes[0] - context.context)
            } else {
                Err(19u8)
            }
        },
    );
    let mut context = CodecContext {
        context: 2,
        format: "selected",
    };
    assert_eq!(codec.encode(&3, &mut context).unwrap(), [5]);
    assert_eq!(codec.decode(&[5], &mut context).unwrap(), 3);
    context.format = "selected";
    let failure = codec.decode(&[], &mut context).unwrap_err();
    assert_eq!(*failure.cause(), 19);
    assert_eq!(context.format, "detected");
    context.format = "selected";
    let failure = codec
        .decode_with(&[], &mut context, FormatRollback::OnError)
        .unwrap_err();
    assert_eq!(*failure.cause(), 19);
    assert_eq!(context.format, "selected");
}

/// Records in-memory writes and rejects changed reference values before replacement.
#[derive(Default)]
struct MemoryWriter {
    references: BTreeMap<String, Vec<u8>>,
    writes: Vec<(ArtifactKind, Vec<u8>)>,
}

impl ReferenceWriter for MemoryWriter {
    type Error = &'static str;

    fn write(
        &mut self,
        path: &SnapshotPath,
        kind: ArtifactKind,
        expected: Option<&[u8]>,
        bytes: &[u8],
    ) -> Result<(), Self::Error> {
        if kind == ArtifactKind::Reference {
            if self.references.get(path.as_str()).map(Vec::as_slice) != expected {
                return Err("conflict");
            }
            self.references
                .insert(path.as_str().to_owned(), bytes.to_vec());
        }
        self.writes.push((kind, bytes.to_vec()));
        Ok(())
    }
}

// A wrong recording branch or treating recorded updates as success breaks the table.
#[test]
fn all_four_modes_preserve_mandatory_failure_and_reference_policy() {
    use ArtifactKind::{Actual, Reference};
    use RecordMode::{All, CompareOnly, Failed, Missing};

    for (mode, reference, redactions, writes, expected) in [
        (CompareOnly, None, 0, &[] as &[ArtifactKind], Err("missing")),
        (CompareOnly, Some(&b"new"[..]), 0, &[], Ok(())),
        (CompareOnly, Some(&b"old"[..]), 0, &[], Err("mismatch")),
        (Missing, None, 1, &[Reference], Err("update")),
        (Missing, Some(&b"new"[..]), 0, &[], Ok(())),
        (Missing, Some(&b"old"[..]), 1, &[Actual], Err("mismatch")),
        (Failed, None, 1, &[Reference], Err("update")),
        (Failed, Some(&b"new"[..]), 0, &[], Ok(())),
        (
            Failed,
            Some(&b"old"[..]),
            1,
            &[Reference, Actual],
            Err("update"),
        ),
        (All, None, 1, &[Reference], Err("update")),
        (All, Some(&b"new"[..]), 1, &[Reference], Err("update")),
        (
            All,
            Some(&b"old"[..]),
            1,
            &[Reference, Actual],
            Err("update"),
        ),
    ] {
        let mut namespace = SnapshotNamespace::new("suite").unwrap();
        let mut writer = MemoryWriter::default();
        if let Some(bytes) = reference {
            writer
                .references
                .insert("suite/case".into(), bytes.to_vec());
        }
        let redaction_calls = Cell::new(0);
        let result = namespace.check(
            SnapshotRequest {
                name: "case",
                reference,
                actual: b"new",
                mode,
            },
            |bytes| {
                assert_eq!(bytes, b"new");
                redaction_calls.set(redaction_calls.get() + 1);
                Ok::<_, ()>(b"redacted".to_vec())
            },
            Some(&mut writer),
        );
        let outcome = result.map_err(|failure| match failure.into_kind() {
            SnapshotFailureKind::Missing => "missing",
            SnapshotFailureKind::Mismatch => "mismatch",
            SnapshotFailureKind::UpdateRequired => "update",
            other => panic!("unexpected snapshot failure: {other:?}"),
        });
        assert_eq!(outcome, expected, "{mode:?} {reference:?}");
        assert_eq!(redaction_calls.get(), redactions, "{mode:?} {reference:?}");
        assert_eq!(
            writer
                .writes
                .iter()
                .map(|(kind, _)| *kind)
                .collect::<Vec<_>>(),
            writes,
            "{mode:?} {reference:?}"
        );
        assert!(writer.writes.iter().all(|(_, bytes)| bytes == b"redacted"));
    }
}

#[test]
fn mismatch_is_not_erased_by_redaction_and_every_emitted_artifact_is_redacted() {
    let mut namespace = SnapshotNamespace::new("suite").unwrap();
    let mut writer = MemoryWriter::default();
    writer
        .references
        .insert("suite/case".into(), b"secret-old".to_vec());
    let result = namespace.check(
        SnapshotRequest {
            name: "case",
            reference: Some(b"secret-old"),
            actual: b"secret-new",
            mode: RecordMode::Failed,
        },
        |_| Ok::<_, ()>(b"redacted".to_vec()),
        Some(&mut writer),
    );
    assert!(matches!(
        result.unwrap_err().kind(),
        SnapshotFailureKind::UpdateRequired
    ));
    assert_eq!(
        writer.writes,
        [
            (ArtifactKind::Reference, b"redacted".to_vec()),
            (ArtifactKind::Actual, b"redacted".to_vec()),
        ]
    );
    assert!(writer.writes.iter().all(|(_, bytes)| bytes == b"redacted"));
}

// Separate reservation registries or comparison gaining writer authority breaks this test.
#[test]
fn writer_free_comparison_and_recording_share_name_reservations() {
    for compare_first in [true, false] {
        let mut namespace = SnapshotNamespace::new("suite").unwrap();
        let mut writer = MemoryWriter::default();
        if compare_first {
            namespace.compare("case", Some(b"same"), b"same").unwrap();
        }
        let result = namespace.check(
            SnapshotRequest {
                name: "case",
                reference: None,
                actual: b"private",
                mode: RecordMode::All,
            },
            |bytes| Ok::<_, ()>(bytes.to_vec()),
            Some(&mut writer),
        );
        let failure = result.unwrap_err();
        if compare_first {
            assert!(matches!(failure.kind(), SnapshotFailureKind::Collision));
            assert!(writer.writes.is_empty());
        } else {
            assert!(matches!(
                failure.kind(),
                SnapshotFailureKind::UpdateRequired
            ));
            assert_eq!(
                writer.writes,
                [(ArtifactKind::Reference, b"private".to_vec())]
            );
            let failure = namespace
                .compare("case", Some(b"private"), b"private")
                .unwrap_err();
            assert!(matches!(failure.kind(), SnapshotFailureKind::Collision));
        }
    }
}

#[test]
fn paths_collisions_permissions_and_conflicts_fail_before_unsafe_writes() {
    for invalid in ["", "..", "../out", "A", "a/b", "a\\b", "con", "x.", "é"] {
        assert!(SnapshotNamespace::new(invalid).is_err(), "{invalid}");
    }
    let mut namespace = SnapshotNamespace::new("suite").unwrap();
    let mut writer = MemoryWriter::default();
    writer
        .references
        .insert("suite/case".into(), b"concurrent".to_vec());
    let request = SnapshotRequest {
        name: "case",
        reference: Some(b"old"),
        actual: b"new",
        mode: RecordMode::All,
    };
    let failure = namespace
        .check(
            request,
            |bytes| Ok::<_, ()>(bytes.to_vec()),
            Some(&mut writer),
        )
        .unwrap_err();
    assert!(matches!(
        failure.kind(),
        SnapshotFailureKind::Write("conflict")
    ));
    assert_eq!(writer.references["suite/case"], b"concurrent");
    let failure = namespace
        .check(
            request,
            |_| panic!("collision must precede redaction"),
            Some(&mut writer),
        )
        .unwrap_err();
    assert!(matches!(
        failure.kind(),
        SnapshotFailureKind::<(), _>::Collision
    ));
    let mut namespace = SnapshotNamespace::new("suite").unwrap();
    let failure = namespace
        .check(
            request,
            |bytes| Ok::<_, ()>(bytes.to_vec()),
            None::<&mut MemoryWriter>,
        )
        .unwrap_err();
    assert!(matches!(
        failure.kind(),
        SnapshotFailureKind::WriterRequired
    ));
}

#[test]
fn ordered_async_cases_are_lazy_and_keep_reverse_completion_association() {
    let started = Rc::new(Cell::new(0));
    let completed = Rc::new(RefCell::new(Vec::new()));
    let (first_tx, first_rx) = async_channel::bounded(1);
    let (second_tx, second_rx) = async_channel::bounded(1);
    let cases = [first_rx, second_rx]
        .into_iter()
        .enumerate()
        .map(|(index, receiver)| {
            let started = Rc::clone(&started);
            let completed = Rc::clone(&completed);
            move || {
                started.set(started.get() + 1);
                async move {
                    let value = receiver.recv().await.unwrap();
                    completed.borrow_mut().push(index);
                    Ok::<_, ()>(value)
                }
            }
        })
        .collect();
    let future = capture_ordered(cases, pending::<()>(), |values| values);
    assert_eq!(started.get(), 0);
    let mut future = Box::pin(future);
    let mut cx = Context::from_waker(Waker::noop());
    assert!(future.as_mut().poll(&mut cx).is_pending());
    second_tx.try_send(20).unwrap();
    assert!(future.as_mut().poll(&mut cx).is_pending());
    first_tx.try_send(10).unwrap();
    assert_eq!(block_on(future).unwrap(), [10, 20]);
    assert_eq!(*completed.borrow(), [1, 0]);
}

#[test]
fn timeout_and_drop_close_owned_captures_without_comparison_or_writes() {
    let calls = Cell::new(0);
    let future = capture_ordered(
        vec![|| {
            calls.set(1);
            ready(Ok::<_, ()>(1))
        }],
        ready(()),
        |_| panic!("sealed"),
    );
    assert!(block_on(future).unwrap_err().is_timeout());
    assert_eq!(calls.get(), 0);
    let (tx, rx) = async_channel::bounded::<u8>(1);
    let future = capture_ordered(
        vec![|| async move { rx.recv().await }],
        pending::<()>(),
        |_| panic!("dropped"),
    );
    let mut future = Box::pin(future);
    assert!(
        future
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_pending()
    );
    drop(future);
    assert_eq!(tx.try_send(1), Err(async_channel::TrySendError::Closed(1)));
}

#[test]
fn pending_timeout_drops_late_callback_and_finish_captures() {
    let (deadline_tx, deadline_rx) = async_channel::bounded(1);
    let (late_tx, late_rx) = async_channel::bounded::<u8>(1);
    let owner = Rc::new(());
    let finish_owner = Rc::clone(&owner);
    let mut future = Box::pin(capture_ordered(
        vec![|| async move { late_rx.recv().await }],
        async move {
            deadline_rx.recv().await.unwrap();
        },
        move |_| {
            drop(finish_owner);
            panic!("no publication after timeout")
        },
    ));
    assert!(
        future
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_pending()
    );
    deadline_tx.try_send(()).unwrap();
    assert!(block_on(future).unwrap_err().is_timeout());
    assert_eq!(Rc::strong_count(&owner), 1);
    assert_eq!(
        late_tx.try_send(8),
        Err(async_channel::TrySendError::Closed(8))
    );
}

#[test]
fn typed_capture_failure_retains_original_cause_without_finish() {
    /// Retains a local cause without `Clone`, `Debug` or `Send` bounds.
    struct Cause(Rc<()>);

    let cause = Rc::new(());
    let owner = Rc::clone(&cause);
    let result = block_on(capture_ordered(
        vec![|| ready(Err::<u8, _>(Cause(owner)))],
        pending::<()>(),
        |_| panic!("failed capture must not publish"),
    ));
    let failure = result.unwrap_err();
    assert!(!failure.is_timeout());
    let (index, retained) = failure.into_cause().unwrap();
    assert_eq!(index, 0);
    assert!(Rc::ptr_eq(&retained.0, &cause));
}

#[test]
fn redaction_failure_and_private_error_debug_never_publish_payloads() {
    /// Keeps private redaction data out of diagnostic trait requirements.
    struct Cause(Rc<String>);

    let secret = Rc::new("secret-token".to_owned());
    let mut namespace = SnapshotNamespace::new("suite").unwrap();
    let mut writer = MemoryWriter::default();
    let failure = namespace
        .check(
            SnapshotRequest {
                name: "case",
                reference: None,
                actual: b"secret-token",
                mode: RecordMode::All,
            },
            |_| Err(Cause(Rc::clone(&secret))),
            Some(&mut writer),
        )
        .unwrap_err();
    assert!(!format!("{failure:?} {failure}").contains("secret-token"));
    assert!(writer.writes.is_empty());
    let SnapshotFailureKind::Redaction(cause) = failure.into_kind() else {
        panic!("cause")
    };
    assert!(Rc::ptr_eq(&cause.0, &secret));
}

#[test]
fn exact_bytes_do_not_trim_normalise_or_decode_and_safe_references_replay() {
    for (reference, actual) in [
        (&b""[..], &b"\n"[..]),
        (&b"x"[..], &b"x "[..]),
        (&b"\xff"[..], &b"\xfe"[..]),
        ("é".as_bytes(), "e\u{301}".as_bytes()),
    ] {
        let mut namespace = SnapshotNamespace::new("suite").unwrap();
        let result = namespace.check(
            SnapshotRequest {
                name: "case",
                reference: Some(reference),
                actual,
                mode: RecordMode::CompareOnly,
            },
            |_| panic!("compare-only must not render"),
            None::<&mut MemoryWriter>,
        );
        assert!(matches!(
            result.unwrap_err().kind(),
            SnapshotFailureKind::<(), _>::Mismatch
        ));
    }
    let mut namespace = SnapshotNamespace::new("suite").unwrap();
    let mut writer = MemoryWriter::default();
    let result = namespace.check(
        SnapshotRequest {
            name: "case",
            reference: None,
            actual: b"safe\n",
            mode: RecordMode::Missing,
        },
        |bytes| Ok::<_, ()>(bytes.to_vec()),
        Some(&mut writer),
    );
    assert!(matches!(
        result.unwrap_err().kind(),
        SnapshotFailureKind::UpdateRequired
    ));
    let mut rerun = SnapshotNamespace::new("suite").unwrap();
    rerun
        .check(
            SnapshotRequest {
                name: "case",
                reference: Some(&writer.references["suite/case"]),
                actual: b"safe\n",
                mode: RecordMode::CompareOnly,
            },
            |bytes| Ok::<_, ()>(bytes.to_vec()),
            None::<&mut MemoryWriter>,
        )
        .unwrap();
    insta::assert_snapshot!(std::str::from_utf8(&writer.references["suite/case"]).unwrap(), @"safe");
}

#[test]
fn optional_issue_reporting_cannot_suppress_snapshot_mismatch() {
    let collector = rca_core::IssueCollector::new();
    let closed = collector.reporter();
    assert!(collector.finish().is_empty());
    for reporter in [rca_core::IssueReporter::default(), closed] {
        let mut namespace = SnapshotNamespace::new("suite").unwrap();
        let failure = namespace
            .check(
                SnapshotRequest {
                    name: "case",
                    reference: Some(b"old"),
                    actual: b"new",
                    mode: RecordMode::CompareOnly,
                },
                |bytes| Ok::<_, ()>(bytes.to_vec()),
                None::<&mut MemoryWriter>,
            )
            .unwrap_err();
        assert!(
            reporter
                .report(rca_core::Issue::new("snapshot", "required mismatch"))
                .is_err()
        );
        assert!(matches!(failure.kind(), SnapshotFailureKind::Mismatch));
    }
}

#[test]
fn independent_captures_do_not_follow_shared_alias_mutation() {
    let alias = Rc::new(Cell::new(1));
    let mut strategy = SnapshotStrategy::new(
        |value: &Rc<Cell<u8>>| Ok::<_, ()>(value.get()),
        |left: &u8, right: &u8| Ok::<_, ()>(left == right),
    );
    let before = strategy.capture(&alias).unwrap();
    alias.set(2);
    let after = strategy.capture(&alias).unwrap();
    assert!(!strategy.compare(&before, &after).unwrap());
    assert_eq!(before, 1);
    assert_eq!(after, 2);
}

#[test]
fn codec_error_and_options_need_no_debug_clone_send_or_error_bounds() {
    /// Supplies mutable local options without convenience trait bounds.
    struct Options(Rc<Cell<u8>>);
    /// Returns the same local allocation after a codec failure.
    struct Cause(Rc<Cell<u8>>);

    let value = Rc::new(Cell::new(7));
    let mut codec = Codec::new(
        |_: &Options, context: &mut CodecContext<Options, u8>| {
            Err(Cause(Rc::clone(&context.context.0)))
        },
        |_: &[u8], context: &mut CodecContext<Options, u8>| {
            context.format = 2;
            context.context.0.set(9);
            Err::<Options, _>(Cause(Rc::clone(&context.context.0)))
        },
    );
    let mut context = CodecContext {
        context: Options(Rc::clone(&value)),
        format: 1,
    };
    let input = Options(Rc::clone(&value));
    let line = line!() + 1;
    let failure = codec.encode(&input, &mut context).unwrap_err();
    assert_eq!(failure.location().line(), line);
    assert!(Rc::ptr_eq(&failure.into_cause().0, &value));
    let Err(failure) = codec.decode_with(&[], &mut context, FormatRollback::OnError) else {
        panic!("the decoder returns its original cause")
    };
    assert!(Rc::ptr_eq(&failure.into_cause().0, &value));
    assert_eq!(context.format, 1);
    assert_eq!(value.get(), 9, "format rollback must not roll back context");
}

#[test]
fn asynchronous_timeout_retains_construction_callsite() {
    /// Names an uncalled factory for an empty set of capture cases.
    type Case = fn() -> std::future::Ready<Result<u8, ()>>;
    let cases: Vec<Case> = Vec::new();
    let line = line!() + 1;
    let future = capture_ordered(cases, ready(()), |values| values);
    let failure = block_on(future).unwrap_err();
    assert!(failure.is_timeout());
    assert_eq!(failure.location().line(), line);
}

#[test]
fn dropping_unpolled_capture_never_invokes_factory_and_releases_both_owners() {
    let owner = Rc::new(());
    let factory_owner = Rc::clone(&owner);
    let finish_owner = Rc::clone(&owner);
    let future = capture_ordered(
        vec![move || -> std::future::Ready<Result<u8, ()>> {
            drop(factory_owner);
            panic!("unpolled factory")
        }],
        pending::<()>(),
        move |_| drop(finish_owner),
    );
    assert_eq!(Rc::strong_count(&owner), 3);
    drop(future);
    assert_eq!(Rc::strong_count(&owner), 1);
}

#[test]
fn invalid_case_and_writer_causes_never_gain_implicit_authority() {
    /// Preserves a writer's local failure identity without `Debug` or `Send`.
    struct Cause(Rc<()>);
    /// Rejects every authorised write with the same owned cause.
    struct Denied(Rc<()>);

    impl ReferenceWriter for Denied {
        type Error = Cause;

        fn write(
            &mut self,
            _: &SnapshotPath,
            _: ArtifactKind,
            _: Option<&[u8]>,
            _: &[u8],
        ) -> Result<(), Cause> {
            Err(Cause(Rc::clone(&self.0)))
        }
    }
    let cause = Rc::new(());
    let mut writer = Denied(Rc::clone(&cause));
    let mut namespace = SnapshotNamespace::new("suite").unwrap();
    let invalid = SnapshotRequest {
        name: "../escape",
        reference: None,
        actual: b"private",
        mode: RecordMode::All,
    };
    let result = namespace.check(
        invalid,
        |_| panic!("invalid name must not render"),
        Some(&mut writer),
    );
    assert!(matches!(
        result.unwrap_err().kind(),
        SnapshotFailureKind::<(), _>::InvalidName
    ));
    let valid = SnapshotRequest {
        name: "valid",
        ..invalid
    };
    let line = line!() + 1;
    let result = namespace.check(valid, |_| Ok::<_, ()>(b"safe".to_vec()), Some(&mut writer));
    let failure = result.unwrap_err();
    assert_eq!(failure.location().line(), line);
    assert_eq!(failure.path().unwrap().as_str(), "suite/valid");
    assert!(!format!("{failure:?}").contains("private"));
    let SnapshotFailureKind::Write(retained) = failure.into_kind() else {
        panic!("writer cause")
    };
    assert!(Rc::ptr_eq(&retained.0, &cause));
}

#[test]
fn capture_error_drops_pending_sibling_and_retains_only_original_cause() {
    /// Identifies the failed case without copying its cause.
    struct Cause(Rc<()>);

    let owner = Rc::new(());
    let (late_tx, late_rx) = async_channel::bounded::<u8>(1);
    let (_error_tx, error_rx) = async_channel::bounded::<u8>(1);
    let cases = [late_rx, error_rx]
        .into_iter()
        .enumerate()
        .map(|(index, receiver)| {
            let owner = Rc::clone(&owner);
            move || async move {
                if index == 1 {
                    Err(Cause(owner))
                } else {
                    Ok(receiver.recv().await.unwrap())
                }
            }
        })
        .collect();
    let failure = block_on(capture_ordered(cases, pending::<()>(), |_| {
        panic!("error cannot finish")
    }))
    .unwrap_err();
    assert_eq!(failure.cause().unwrap().0, 1);
    assert!(Rc::ptr_eq(&failure.cause().unwrap().1.0, &owner));
    assert_eq!(Rc::strong_count(&owner), 2);
    assert_eq!(
        late_tx.try_send(1),
        Err(async_channel::TrySendError::Closed(1))
    );
}

#[test]
fn successful_async_capture_publishes_once_and_retains_recording_failure() {
    let (tx, rx) = async_channel::bounded(2);
    tx.try_send(7u8).unwrap();
    tx.try_send(8u8).unwrap();
    let finishes = Cell::new(0);
    let mut namespace = SnapshotNamespace::new("suite").unwrap();
    let mut writer = MemoryWriter::default();
    let recorded = block_on(capture_ordered(
        vec![|| async move { rx.recv().await }],
        pending::<()>(),
        |values| {
            finishes.set(finishes.get() + 1);
            namespace.check(
                SnapshotRequest {
                    name: "case",
                    reference: None,
                    actual: &values,
                    mode: RecordMode::All,
                },
                |bytes| Ok::<_, ()>(bytes.to_vec()),
                Some(&mut writer),
            )
        },
    ))
    .unwrap();
    assert!(matches!(
        recorded.unwrap_err().kind(),
        SnapshotFailureKind::UpdateRequired
    ));
    assert_eq!(finishes.get(), 1);
    assert_eq!(writer.writes, [(ArtifactKind::Reference, vec![7])]);
    assert_eq!(tx.try_send(9), Err(async_channel::TrySendError::Closed(9)));
}

#[test]
fn pullback_capture_and_comparator_failures_remain_distinct() {
    let mut strategy = SnapshotStrategy::new(
        |_: &u8| Err::<u8, _>("capture-cause"),
        |_: &u8, _: &u8| Err::<bool, _>("compare-cause"),
    )
    .try_pullback(|value: &u8| Ok::<_, ()>(*value));
    assert!(matches!(
        strategy.capture(&1),
        Err(PullbackError::Capture("capture-cause"))
    ));
    assert_eq!(strategy.compare(&1, &1), Err("compare-cause"));
}

#[test]
fn pullback_failures_are_standard_errors_without_payload_bounds_or_disclosure() {
    /// Holds private data that error formatting must not expose.
    struct Cause(Rc<String>);

    let private = Rc::new("private-cause".to_owned());
    for failure in [
        PullbackError::<Cause, Cause>::Projection(Cause(Rc::clone(&private))),
        PullbackError::Capture(Cause(Rc::clone(&private))),
    ] {
        let error: &dyn std::error::Error = &failure;
        assert!(!format!("{error} {error:?}").contains("private-cause"));
        assert!(error.source().is_none());
        let (cause, stage) = match failure {
            PullbackError::Projection(cause) => (cause, "Projection"),
            PullbackError::Capture(cause) => (cause, "Capture"),
        };
        assert!(Rc::ptr_eq(&cause.0, &private), "{stage}");
    }
}

#[test]
fn second_write_failure_retains_cause_without_rolling_back_recorded_reference() {
    /// Identifies the actual-artifact failure independently of the reference write.
    struct Cause(Rc<()>);
    /// Accepts the reference write, then rejects the actual artifact.
    struct FailsActual {
        memory: MemoryWriter,
        cause: Rc<()>,
        attempted: Vec<ArtifactKind>,
    }

    impl ReferenceWriter for FailsActual {
        type Error = Cause;

        fn write(
            &mut self,
            path: &SnapshotPath,
            kind: ArtifactKind,
            expected: Option<&[u8]>,
            bytes: &[u8],
        ) -> Result<(), Cause> {
            self.attempted.push(kind);
            if kind == ArtifactKind::Actual {
                return Err(Cause(Rc::clone(&self.cause)));
            }
            self.memory.write(path, kind, expected, bytes).unwrap();
            Ok(())
        }
    }
    let cause = Rc::new(());
    let mut writer = FailsActual {
        memory: MemoryWriter::default(),
        cause: Rc::clone(&cause),
        attempted: Vec::new(),
    };
    writer
        .memory
        .references
        .insert("suite/case".to_owned(), b"old".to_vec());
    let mut namespace = SnapshotNamespace::new("suite").unwrap();
    let failure = namespace
        .check(
            SnapshotRequest {
                name: "case",
                reference: Some(b"old"),
                actual: b"new",
                mode: RecordMode::Failed,
            },
            |_| Ok::<_, ()>(b"safe".to_vec()),
            Some(&mut writer),
        )
        .unwrap_err();
    assert_eq!(
        writer.attempted,
        [ArtifactKind::Reference, ArtifactKind::Actual]
    );
    assert_eq!(writer.memory.references["suite/case"], b"safe");
    assert_eq!(
        writer.memory.writes,
        [(ArtifactKind::Reference, b"safe".to_vec())]
    );
    let SnapshotFailureKind::Write(retained) = failure.into_kind() else {
        panic!("second write cause")
    };
    assert!(Rc::ptr_eq(&retained.0, &cause));
}

#[test]
fn equal_references_need_no_writer_or_redaction_except_explicit_record_all() {
    for mode in [
        RecordMode::CompareOnly,
        RecordMode::Missing,
        RecordMode::Failed,
        RecordMode::All,
    ] {
        let mut namespace = SnapshotNamespace::new("suite").unwrap();
        let request = SnapshotRequest {
            name: "case",
            reference: Some(b"private-equal"),
            actual: b"private-equal",
            mode,
        };
        let result = namespace.check(
            request,
            |_| -> Result<Vec<u8>, ()> { panic!("no authorised write needs redaction") },
            None::<&mut MemoryWriter>,
        );
        if mode == RecordMode::All {
            assert!(matches!(
                result.unwrap_err().kind(),
                SnapshotFailureKind::WriterRequired
            ));
        } else {
            result.expect("an equal existing reference needs no recording authority");
        }
        let mut writer = MemoryWriter::default();
        let retry = namespace
            .check(
                request,
                |_| -> Result<Vec<u8>, ()> { panic!("reserved name precedes redaction") },
                Some(&mut writer),
            )
            .unwrap_err();
        assert!(matches!(retry.kind(), SnapshotFailureKind::Collision));
        assert!(writer.writes.is_empty());
    }
}

#[test]
fn timeout_wins_ready_tie_after_partial_capture_and_releases_completed_values() {
    let completed_owner = Rc::new(());
    let queued_owner = Rc::new(());
    let completed = RefCell::new(Vec::new());
    let (first_tx, first_rx) = async_channel::bounded(1);
    let (second_tx, second_rx) = async_channel::bounded(1);
    let (deadline_tx, deadline_rx) = async_channel::bounded(1);
    let cases = [first_rx, second_rx]
        .into_iter()
        .enumerate()
        .map(|(index, receiver)| {
            let completed = &completed;
            move || async move {
                let value = receiver.recv().await.unwrap();
                completed.borrow_mut().push(index);
                Ok::<_, ()>(value)
            }
        })
        .collect();
    let mut future = Box::pin(capture_ordered(
        cases,
        async move { deadline_rx.recv().await.unwrap() },
        |_| panic!("a deadline tie must not publish partially captured values"),
    ));
    let mut cx = Context::from_waker(Waker::noop());
    first_tx.try_send(Rc::clone(&completed_owner)).unwrap();
    assert!(future.as_mut().poll(&mut cx).is_pending());
    assert_eq!(*completed.borrow(), [0]);
    assert_eq!(Rc::strong_count(&completed_owner), 2);

    second_tx.try_send(Rc::clone(&queued_owner)).unwrap();
    deadline_tx.try_send(()).unwrap();
    let std::task::Poll::Ready(Err(failure)) = future.as_mut().poll(&mut cx) else {
        panic!("ready deadline must win before the final ready capture");
    };
    assert!(failure.is_timeout());
    assert!(failure.cause().is_none());
    drop(future);
    assert_eq!(*completed.borrow(), [0]);
    assert_eq!(Rc::strong_count(&completed_owner), 1);
    assert!(first_tx.is_closed());
    assert!(second_tx.is_closed());
    // The caller-held sender still owns the closed channel's unconsumed value.
    assert_eq!(Rc::strong_count(&queued_owner), 2);
    drop(second_tx);
    assert_eq!(Rc::strong_count(&queued_owner), 1);
}
