use std::process::Command;
use std::time::Duration;

mod support;

// Missing dispatch, selection-on-equal savings, skipped takes, or false owner
// claims must fail independently of the fixture's own oracle.
#[test]
fn finite_observation_matrix_has_exact_real_traces() {
    for lane in ["native", "deterministic"] {
        if lane == "deterministic" && !cfg!(feature = "deterministic") {
            continue;
        }
        for mode in ["plain", "distinct", "coalesced", "distinct-coalesced"] {
            for (n, r) in [(1, 2), (16, 2), (64, 2), (1, 64), (16, 64), (64, 64)] {
                assert_record(mode, lane, n, r);
            }
        }
        assert_record("plain", lane, 0, 2);
    }
}

#[expect(
    clippy::too_many_lines,
    reason = "keep the complete record's independent work, subscriber and owner oracles together"
)]
fn assert_record(mode: &str, lane: &str, n: u64, r: u64) {
    let name = format!("observe-{mode}-{lane}");
    let output = support::output(
        Command::new(env!("CARGO_BIN_EXE_rca-performance-profile")).args([
            name.clone(),
            n.to_string(),
            r.to_string(),
        ]),
        Duration::from_secs(60),
    );
    assert!(
        output.status.success(),
        "{name} {n} {r}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let record: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let distinct = u64::from(mode.starts_with("distinct"));
    let coalesced = mode.ends_with("coalesced");
    let half = r / 2;
    let per_delivery = if coalesced {
        2
    } else if distinct == 1 {
        half + 1
    } else {
        r + 1
    };
    let sum = if coalesced {
        half
    } else if distinct == 1 {
        half * (half + 1) / 2
    } else {
        half * half
    };
    assert_eq!(record["schema"], "rca-observation-performance-profile-v1");
    assert_eq!(record["workload"], name);
    assert_eq!(record["size"], n);
    assert_eq!(record["iterations"], r);
    assert_eq!(record["drive_contract"], "ready-bounded-v1");
    assert_eq!(
        record["feature_profile"],
        if cfg!(feature = "deterministic") {
            "deterministic-enabled"
        } else {
            "minimal-production"
        }
    );
    assert_eq!(
        record["features"]["deterministic"],
        cfg!(feature = "deterministic")
    );
    assert_eq!(
        record["features"]["allocations"],
        cfg!(feature = "allocations")
    );
    let scheduler = 128 * (r * (n + 1) + n + 1) + 2048;
    assert_eq!(record["bounds"]["scheduler_task_polls"], scheduler);
    assert_eq!(record["bounds"]["outer_drive_polls"], 2 * scheduler + 16);
    assert_eq!(record["bounds"]["native_deadline_seconds"], 10);
    for (field, expected) in [
        ("warmup_roots", 0),
        ("roots_committed", r),
        ("root_commit_success", r),
        ("root_cancelled", 0),
        ("responses_committed", 0),
        ("reducer_calls", r),
        ("causal_completed", r),
        ("final_revision", r),
        ("root_checksum", r * (r + 1) / 2),
        ("selected_originals", n * (r + 1)),
        ("selector_calls", n * (r + 1)),
        ("selected_clones", distinct * n * (half + 1)),
        ("equality_calls", distinct * n * r),
        ("selected_created", n * (r + 1) + distinct * n * (half + 1)),
        ("selected_dropped", n * (r + 1) + distinct * n * (half + 1)),
        ("selected_live", 0),
        ("deliveries", n * per_delivery),
        ("registrations", n),
        ("registration_accepted", n),
        ("registration_rejected", 0),
        ("selector_captures_dropped", n),
        ("closed_next", n),
    ] {
        assert_eq!(record[field], expected, "{name}: {field}");
    }
    for field in [
        "state_extracted",
        "state_dropped",
        "reducer_dropped",
        "host_task_dropped",
        "registration_owners_released",
        "value_owners_released",
        "owners_released",
        "host_stopped",
    ] {
        assert_eq!(record[field], true, "{field}");
    }
    let rows = record["subscribers"].as_array().unwrap();
    assert_eq!(rows.len(), usize::try_from(n).unwrap());
    for row in rows {
        assert_eq!(*row, serde_json::json!([per_delivery, sum, half]));
    }
    assert_phase_counts(&record, n, r, distinct, per_delivery);
    assert_allocations(&record, lane);
}

/// Separates initial registration from committed updates and final capture release.
fn assert_phase_counts(
    record: &serde_json::Value,
    n: u64,
    r: u64,
    distinct: u64,
    per_delivery: u64,
) {
    let half = r / 2;
    let cold = &record["counters"]["cold"];
    let work = &record["counters"]["work"];
    for (field, c, w) in [
        ("selector_calls", n, n * r),
        ("selected_originals", n, n * r),
        ("selected_clones", distinct * n, distinct * n * half),
        ("equality_calls", 0, distinct * n * r),
        ("selected_dropped", n, n * r + distinct * n * half),
        ("deliveries", n, n * (per_delivery - 1)),
        ("registrations", n, 0),
        ("selector_captures_dropped", 0, 0),
    ] {
        assert_eq!(cold[field], c, "cold {field}");
        assert_eq!(work[field], w, "work {field}");
    }
    assert_eq!(cold["selected_live"], distinct * n);
    assert_eq!(work["selected_live"], distinct * n);
    assert_eq!(cold["selector_captures_live"], n);
    assert_eq!(work["selector_captures_live"], n);
}

fn assert_allocations(record: &serde_json::Value, lane: &str) {
    #[cfg(feature = "allocations")]
    {
        let allocations = &record["allocations"];
        assert_eq!(allocations["snapshots"].as_array().unwrap().len(), 6);
        assert_eq!(allocations["phases"].as_object().unwrap().len(), 5);
        assert!(allocations["phases"]["cold"].is_object());
        assert!(allocations["phases"].get("warmup").is_none());
        if lane == "deterministic" {
            assert_eq!(allocations["executor_thread_local"]["source"], "none");
            assert_eq!(allocations["executor_thread_local"]["retained_blocks"], 0);
            assert_eq!(allocations["executor_thread_local"]["retained_bytes"], 0);
        } else {
            assert_eq!(
                allocations["executor_thread_local"]["source"],
                "tokio-1.53.3::runtime::park::CURRENT_PARKER"
            );
        }
    }
    #[cfg(not(feature = "allocations"))]
    let _ = lane;
    #[cfg(not(feature = "allocations"))]
    assert!(record["allocations"].is_null());
}

#[test]
fn invalid_observation_inputs_emit_no_success_record() {
    for args in [
        vec!["observe-plain-native"],
        vec!["observe-plain-native", "1"],
        vec!["observe-unknown-native", "1", "2"],
        vec!["observe-plain-native", "2", "2"],
        vec!["observe-plain-native", "17", "2"],
        vec!["observe-plain-native", "65", "2"],
        vec!["observe-plain-native", "1", "0"],
        vec!["observe-plain-native", "1", "1"],
        vec!["observe-plain-native", "1", "3"],
        vec!["observe-plain-native", "1", "1000002"],
        vec!["observe-plain-native", "1", "18446744073709551615"],
        vec!["observe-plain-native", "-1", "2"],
        vec!["observe-plain-native", "+1", "2"],
        vec!["observe-plain-native", "1", "2.0"],
        vec!["observe-plain-native", "1", "2", "extra"],
        #[cfg(not(feature = "deterministic"))]
        vec!["observe-plain-deterministic", "1", "2"],
    ] {
        let output = support::output(
            Command::new(env!("CARGO_BIN_EXE_rca-performance-profile")).args(args),
            Duration::from_secs(60),
        );
        assert_eq!(output.status.code(), Some(1));
        assert!(output.stdout.is_empty());
        assert!(!output.stderr.is_empty());
    }
}
