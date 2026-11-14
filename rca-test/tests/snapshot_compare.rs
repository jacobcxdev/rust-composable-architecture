//! Checks exact byte comparisons, portable names and per-namespace reservations.

use rca_test::snapshots::{SnapshotFailureKind, SnapshotNamespace};

// Rejecting case variants must not reserve their lowercase spelling or invoke callbacks.
#[test]
fn case_only_names_remain_invalid_across_both_comparison_paths() {
    use rca_test::snapshots::{
        ArtifactKind, RecordMode, ReferenceWriter, SnapshotPath, SnapshotRequest,
    };
    use std::sync::Arc;

    /// Rejects every write so comparison cannot acquire recording authority.
    struct Writer;

    impl ReferenceWriter for Writer {
        type Error = ();

        fn write(
            &mut self,
            _: &SnapshotPath,
            _: ArtifactKind,
            _: Option<&[u8]>,
            _: &[u8],
        ) -> Result<(), Self::Error> {
            panic!("comparison must not write");
        }
    }

    for name in ["Case", "cAsE", "CASE"] {
        let mut namespace = SnapshotNamespace::new("suite").unwrap();
        let owner = Arc::new(());
        let captured = Arc::clone(&owner);
        let line = line!() + 2;
        let failure = namespace
            .check(
                SnapshotRequest {
                    name,
                    reference: Some(b"same"),
                    actual: b"same",
                    mode: RecordMode::CompareOnly,
                },
                move |_| -> Result<Vec<u8>, ()> {
                    drop(captured);
                    panic!("invalid name must not redact");
                },
                None::<&mut Writer>,
            )
            .unwrap_err();
        assert!(matches!(failure.kind(), SnapshotFailureKind::InvalidName));
        assert!(failure.path().is_none());
        assert_eq!(failure.location().line(), line);
        assert_eq!(Arc::strong_count(&owner), 1);
        let line = line!() + 1;
        let failure = namespace.compare(name, Some(b"same"), b"same").unwrap_err();
        assert!(matches!(failure.kind(), SnapshotFailureKind::InvalidName));
        assert!(failure.path().is_none());
        assert_eq!(failure.location().line(), line);
        namespace.compare("case", Some(b"same"), b"same").unwrap();
        let failure = namespace
            .compare("case", Some(b"same"), b"same")
            .unwrap_err();
        assert!(matches!(failure.kind(), SnapshotFailureKind::Collision));
        assert_eq!(failure.path().unwrap().as_str(), "suite/case");
    }
}

// Missing inference-friendly comparison or ordinary error propagation breaks this call.
#[test]
fn ordinary_compare_needs_no_writer_or_error_annotation() -> Result<(), Box<dyn std::error::Error>>
{
    let mut namespace = SnapshotNamespace::new("suite")?;
    namespace.compare("case", Some(b"exact\n"), b"exact\n")?;
    Ok(())
}

// Any decoding, trimming, newline conversion or Unicode normalisation breaks this table.
#[test]
fn comparison_preserves_arbitrary_bytes_and_newlines() {
    for bytes in [b"".as_slice(), b"\0\xff\xfe\n\r\n", "é".as_bytes()] {
        let mut namespace = SnapshotNamespace::new("suite").unwrap();
        namespace.compare("case", Some(bytes), bytes).unwrap();
    }
    for (reference, actual) in [
        (b"".as_slice(), b"\n".as_slice()),
        (b"x".as_slice(), b"x ".as_slice()),
        (b"x\n".as_slice(), b"x\r\n".as_slice()),
        (b"\xff".as_slice(), b"\xfe".as_slice()),
        (b"\0x".as_slice(), b"x".as_slice()),
        ("é".as_bytes(), "e\u{301}".as_bytes()),
    ] {
        let mut namespace = SnapshotNamespace::new("suite").unwrap();
        let failure = namespace
            .compare("case", Some(reference), actual)
            .unwrap_err();
        assert!(matches!(failure.kind(), SnapshotFailureKind::Mismatch));
    }
}

// Conflating absence with empty bytes or forwarding the helper's call site breaks this test.
#[test]
fn missing_empty_and_mismatch_preserve_failure_path_and_caller() {
    for (reference, actual, missing) in [
        (None, b"".as_slice(), true),
        (None, b"private-payload".as_slice(), true),
        (Some(b"".as_slice()), b"private-payload".as_slice(), false),
    ] {
        let mut namespace = SnapshotNamespace::new("suite").unwrap();
        let line = line!() + 1;
        let result = namespace.compare("case", reference, actual);
        let failure = result.unwrap_err();
        if missing {
            assert!(matches!(failure.kind(), SnapshotFailureKind::Missing));
        } else {
            assert!(matches!(failure.kind(), SnapshotFailureKind::Mismatch));
        }
        assert_eq!(failure.path().unwrap().as_str(), "suite/case");
        assert_eq!(failure.location().file(), file!());
        assert_eq!(failure.location().line(), line);
        assert!(!format!("{failure:?} {failure}").contains("private-payload"));
    }
}

// Reserving invalid names or weakening portable component validation breaks this table.
#[test]
fn invalid_names_have_no_path_and_are_not_reserved() {
    let mut namespace = SnapshotNamespace::new("suite").unwrap();
    for name in [
        "",
        "..",
        "../escape",
        "a/b",
        "a\\b",
        "Case",
        "é",
        "a.",
        "a b",
        "con",
        "prn",
        "aux",
        "nul",
        "com1",
        "com9",
        "lpt1",
        "lpt9",
    ] {
        for _ in 0..2 {
            let line = line!() + 1;
            let result = namespace.compare(name, Some(b"same"), b"same");
            let failure = result.unwrap_err();
            assert!(
                matches!(failure.kind(), SnapshotFailureKind::InvalidName),
                "{name}"
            );
            assert!(failure.path().is_none());
            assert_eq!(failure.location().file(), file!());
            assert_eq!(failure.location().line(), line);
        }
    }
    for name in ["a", "case_1-2", "com0", "lpt0", "com10"] {
        namespace.compare(name, Some(b"same"), b"same").unwrap();
    }
}

// Releasing a name after success, missing or mismatch would allow ambiguous reruns.
#[test]
fn every_valid_attempt_reserves_its_name_before_comparison() {
    for reference in [
        None,
        Some(b"same".as_slice()),
        Some(b"different".as_slice()),
    ] {
        let mut namespace = SnapshotNamespace::new("suite").unwrap();
        let first = namespace.compare("case", reference, b"same");
        match reference {
            None => assert!(matches!(
                first.unwrap_err().kind(),
                SnapshotFailureKind::Missing
            )),
            Some(b"same") => first.unwrap(),
            Some(_) => assert!(matches!(
                first.unwrap_err().kind(),
                SnapshotFailureKind::Mismatch
            )),
        }

        let line = line!() + 1;
        let result = namespace.compare("case", Some(b"same"), b"same");
        let failure = result.unwrap_err();
        assert!(matches!(failure.kind(), SnapshotFailureKind::Collision));
        assert_eq!(failure.path().unwrap().as_str(), "suite/case");
        assert_eq!(failure.location().file(), file!());
        assert_eq!(failure.location().line(), line);
        SnapshotNamespace::new("suite")
            .unwrap()
            .compare("case", Some(b"same"), b"same")
            .unwrap();
    }
}
