use std::process::Command;
use std::time::Duration;

mod support;

#[test]
fn rejects_document_dimensions_and_missing_features_before_construction() {
    for args in [
        vec!["document-coalesced-native"],
        vec!["document-coalesced-native", "2"],
        vec!["document-coalesced-native", "0", "1"],
        vec!["document-coalesced-native", "65", "1"],
        vec!["document-coalesced-native", "2", "0"],
        vec!["document-coalesced-native", "2", "21"],
        vec!["document-coalesced-native", "+2", "1"],
        vec!["document-coalesced-native", "2", "+1"],
        vec!["document-coalesced-native", "2", "-1"],
        vec!["document-coalesced-native", "2", "18446744073709551616"],
        vec!["document-coalesced-native", "-2", "1"],
        vec!["document-coalesced-native", "x", "1"],
        vec!["document-coalesced-native", "18446744073709551616", "1"],
        vec!["document-coalesced-native", "2", "1", "extra"],
        vec!["document-coalesced-8-native", "64", "1"],
        vec!["document-coalesced-8-native", "2", "3"],
        vec!["document-coalesced-native", "1000", "2"],
        vec!["document-coalesced-native", "1001", "1"],
        vec!["document-coalesced-65536-native", "2", "2"],
        vec!["document-coalesced-65536-native", "16", "1"],
    ] {
        reject(&args, None);
    }
    if !cfg!(feature = "persistence") {
        reject(
            &["document-coalesced-native", "2", "1"],
            Some("persistence feature"),
        );
    } else if !cfg!(feature = "deterministic") {
        reject(
            &["document-coalesced-deterministic", "2", "1"],
            Some("deterministic feature"),
        );
    }
}

/// Requires a normal input error, not a panic or signal termination.
fn reject(args: &[&str], message: Option<&str>) {
    let output = support::output(
        Command::new(env!("CARGO_BIN_EXE_rca-performance-profile")).args(args),
        Duration::from_secs(60),
    );
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert!(!output.stderr.is_empty());
    if let Some(message) = message {
        assert!(String::from_utf8_lossy(&output.stderr).contains(message));
    }
}

#[test]
#[cfg(feature = "persistence")]
fn finite_document_points_have_independent_exact_oracles() {
    for lane in ["native", "deterministic"] {
        if lane == "deterministic" && !cfg!(feature = "deterministic") {
            continue;
        }
        for (b, i) in [(2, 1), (2, 3), (2, 20), (64, 1), (64, 3), (64, 20)] {
            assert_record(lane, b, i, 4096);
        }
        for (b, i) in [(1, 1), (1, 3), (1, 20), (16, 3), (16, 20), (1000, 1)] {
            assert_record(lane, b, i, 4096);
        }
        assert_record(lane, 2, 1, 8);
        assert_record(lane, 2, 1, 65536);
    }
}

#[cfg(feature = "persistence")]
#[expect(
    clippy::too_many_lines,
    reason = "one public record keeps its independent work, byte and owner assertions together"
)]
fn assert_record(lane: &str, b: u64, i: u64, p: u64) {
    let name = if p == 4096 {
        format!("document-coalesced-{lane}")
    } else {
        format!("document-coalesced-{p}-{lane}")
    };
    let output = support::output(
        Command::new(env!("CARGO_BIN_EXE_rca-performance-profile")).args([
            name.clone(),
            b.to_string(),
            i.to_string(),
        ]),
        Duration::from_secs(60),
    );
    assert!(
        output.status.success(),
        "{name} {b} {i}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let record: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(record["schema"], "rca-document-performance-profile-v1");
    assert_eq!(record["drive_contract"], "ready-bounded-v1");
    assert_eq!(record["features"]["persistence"], true);
    assert_eq!(
        record["features"]["deterministic"],
        cfg!(feature = "deterministic")
    );
    assert_eq!(
        record["features"]["allocations"],
        cfg!(feature = "allocations")
    );
    assert_eq!(
        record["feature_profile"],
        if cfg!(feature = "deterministic") {
            "persistence-deterministic-enabled"
        } else {
            "persistence-production"
        }
    );
    let edits = b * i;
    let saves = if b == 1 { i } else { 2 * i };
    assert_eq!(record["payload_bytes"], p);
    assert_eq!(record["size"], b);
    assert_eq!(record["iterations"], i);
    if (b, i) == (2, 3) {
        assert_tiny_repeated_bursts(&record);
    }
    if (b, i) == (1, 3) {
        assert_single_edit_bursts(&record);
    }
    for (field, cold, work) in [
        ("roots", 1, i * (b + 1)),
        ("responses", 1, saves),
        ("reductions", 2, edits + i + saves),
        ("completed", 1, i * (b + 1)),
        ("loads", 1, 0),
        ("decodes", 1, 0),
        ("encodes", 0, saves),
        ("saves", 0, saves),
        ("writes", 0, saves),
        ("receipts", 0, i),
        ("clones", 1, edits + saves),
        ("dropped", 2, 2 * edits + saves),
    ] {
        assert_eq!(record["counters"]["cold"][field], cold, "cold {field}");
        assert_eq!(record["counters"]["work"][field], work, "work {field}");
    }
    for (field, value) in [
        ("final_revision", edits),
        ("final_tag", edits),
        ("acknowledged_revision", edits),
        ("originals", edits + 2),
        ("clones", edits + saves + 1),
        ("payload_dropped", 2 * edits + saves + 3),
        ("payload_live", 0),
        (
            "write_tag_sum",
            if b == 1 {
                i * (i + 1) / 2
            } else {
                b * i * i + i
            },
        ),
        ("saved_bytes", saves * p),
        ("loaded_bytes", p),
        ("physical_created", saves + 1),
        ("physical_dropped", saves + 1),
        ("delivery_created", saves + 1),
        ("delivery_dropped", saves + 1),
        ("pending_physical", 0),
        ("pending_delivery", 0),
    ] {
        assert_eq!(record[field], value, "{field}");
    }
    for field in [
        "physical_joined",
        "state_extracted",
        "owners_released",
        "host_task_dropped",
        "host_stopped",
    ] {
        assert_eq!(record[field], true, "{field}");
    }
    let scheduler = 64 * (2 + i * (b + 3)) + 4096;
    assert_eq!(record["bounds"]["scheduler_task_polls"], scheduler);
    assert_eq!(record["bounds"]["outer_drive_polls"], scheduler + 1);
    if cfg!(feature = "allocations") {
        let tls = &record["allocations"]["executor_thread_local"];
        if lane == "deterministic" {
            assert_eq!(tls["retained_blocks"], 0);
            assert_eq!(tls["retained_bytes"], 0);
        }
        assert!(record["allocations"]["phases"].get("cold").is_some());
    } else {
        assert!(record["allocations"].is_null());
    }
}

#[cfg(feature = "persistence")]
fn assert_single_edit_bursts(record: &serde_json::Value) {
    // Three separately acknowledged edits cannot create a coalesced second save.
    for (field, expected) in [
        ("roots", 6),
        ("responses", 3),
        ("reductions", 9),
        ("completed", 6),
        ("encodes", 3),
        ("saves", 3),
        ("writes", 3),
        ("receipts", 3),
        ("clones", 6),
        ("dropped", 9),
    ] {
        assert_eq!(
            record["counters"]["work"][field], expected,
            "single {field}"
        );
    }
    for (field, expected) in [
        ("originals", 5),
        ("clones", 7),
        ("payload_dropped", 12),
        ("write_tag_sum", 6),
        ("final_revision", 3),
        ("physical_created", 4),
        ("delivery_created", 4),
    ] {
        assert_eq!(record[field], expected, "{field}");
    }
}

#[cfg(feature = "persistence")]
fn assert_tiny_repeated_bursts(record: &serde_json::Value) {
    // Hand-authored arithmetic witness: one cold lifetime, three bursts.
    for (field, expected) in [
        ("roots", 9),
        ("responses", 6),
        ("reductions", 15),
        ("completed", 9),
        ("encodes", 6),
        ("saves", 6),
        ("writes", 6),
        ("receipts", 3),
        ("clones", 12),
        ("dropped", 18),
    ] {
        assert_eq!(record["counters"]["work"][field], expected, "tiny {field}");
    }
    for (field, expected) in [
        ("originals", 8),
        ("clones", 13),
        ("payload_dropped", 21),
        ("write_tag_sum", 21),
    ] {
        assert_eq!(record[field], expected, "tiny {field}");
    }
}
