use std::process::{Command, Output};
use std::time::Duration;

mod support;

/// Keeps the existing deadline while bounding diagnostics and completing reader cleanup.
fn run(args: &[&str]) -> Output {
    support::output(
        Command::new(env!("CARGO_BIN_EXE_rca-performance-profile")).args(args),
        Duration::from_secs(30),
    )
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "keep the independent literal matrix alongside its exact CLI assertions"
)]
fn direct_dependency_modes_have_independent_exact_oracles() {
    // Literal oracles intentionally do not reuse the workload's arithmetic.
    let metadata = [
        (0, 0, 0),
        (0, 0, 0),
        (0, 0, 0),
        (0, 0, 0),
        (1, 11, 0),
        (1, 11, 0),
        (1, 11, 0),
        (1, 11, 29),
        (2, 22, 0),
        (16, 176, 0),
        (16, 176, 0),
        (16, 176, 0),
        (16, 176, 464),
        (1024, 11264, 0),
        (1024, 11264, 0),
        (1024, 11264, 0),
        (1024, 11264, 29696),
    ];
    for (
        (mode, size, iterations, reads, checksum, warm, work, work_drops, total),
        (operations, parent_checksum, child_checksum),
    ) in [
        ("dependency-cold", "0", "1", 0, 0, 0, 0, 0, 0),
        ("dependency-cached", "0", "64", 0, 0, 0, 0, 0, 0),
        ("dependency-reset", "0", "1", 0, 0, 0, 0, 0, 0),
        ("dependency-scope", "0", "64", 0, 0, 0, 0, 0, 0),
        ("dependency-cold", "1", "1", 1, 11, 0, 1, 0, 1),
        ("dependency-cached", "1", "1", 1, 11, 1, 0, 0, 1),
        ("dependency-reset", "1", "1", 1, 11, 1, 1, 1, 2),
        ("dependency-scope", "1", "1", 2, 40, 0, 0, 1, 2),
        ("dependency-cached", "1", "2", 2, 22, 1, 0, 0, 1),
        ("dependency-cold", "16", "1", 16, 176, 0, 16, 0, 16),
        ("dependency-cached", "16", "1", 16, 176, 16, 0, 0, 16),
        ("dependency-reset", "16", "1", 16, 176, 16, 16, 16, 32),
        ("dependency-scope", "16", "1", 32, 640, 0, 0, 16, 32),
        ("dependency-cold", "16", "64", 1024, 11264, 0, 1024, 0, 1024),
        ("dependency-cached", "16", "64", 1024, 11264, 16, 0, 0, 16),
        (
            "dependency-reset",
            "16",
            "64",
            1024,
            11264,
            16,
            1024,
            1024,
            1040,
        ),
        (
            "dependency-scope",
            "16",
            "64",
            2048,
            40960,
            0,
            0,
            1024,
            1040,
        ),
    ]
    .into_iter()
    .zip(metadata)
    {
        let output = run(&[mode, size, iterations]);
        assert!(
            output.status.success(),
            "{mode}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let record: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(record["schema"], "rca-dependency-performance-profile-v1");
        assert_eq!(record["workload"], mode);
        assert_eq!(record["size"], size.parse::<u64>().unwrap());
        assert_eq!(record["iterations"], iterations.parse::<u64>().unwrap());
        assert_eq!(record["operations"], operations);
        assert_eq!(record["reads"], reads);
        assert_eq!(record["checksum"], checksum);
        assert_eq!(record["parent_checksum"], parent_checksum);
        assert_eq!(record["child_checksum"], child_checksum);
        assert_eq!(record["warm_factories"], warm);
        assert_eq!(record["work_factories"], work);
        assert_eq!(record["work_drops"], work_drops);
        assert_eq!(record["providers_created"], total);
        assert_eq!(record["providers_dropped"], total);
        assert_eq!(record["providers_remaining"], 0);
        assert_eq!(record["domain_clones"], 0);
        assert_eq!(record["policy"], "live");
        assert_eq!(record["driver_contract"], "direct-synchronous-v1");
        assert_eq!(record["outcome"], "validated");
        assert_eq!(
            record["features"]["deterministic"],
            cfg!(feature = "deterministic")
        );
        assert_eq!(
            record["feature_profile"],
            if cfg!(feature = "deterministic") {
                "deterministic-enabled"
            } else {
                "minimal-production"
            }
        );
        assert_eq!(
            record["features"]["allocations"],
            cfg!(feature = "allocations")
        );
        if cfg!(feature = "allocations") {
            assert_allocation_phases(
                &record["allocations"],
                matches!(mode, "dependency-cached" | "dependency-reset"),
            );
        } else {
            assert!(record["allocations"].is_null());
        }
    }
}

fn assert_allocation_phases(record: &serde_json::Value, has_warmup: bool) {
    let names: &[&str] = if has_warmup {
        &["construction", "warmup", "work", "validation", "teardown"]
    } else {
        &["construction", "work", "validation", "teardown"]
    };
    let snapshots = record["snapshots"].as_array().unwrap();
    assert_eq!(snapshots.len(), if has_warmup { 6 } else { 5 });
    let phases = record["phases"].as_object().unwrap();
    assert_eq!(phases.len(), names.len());
    for (name, pair) in names.iter().zip(snapshots.windows(2)) {
        let phase = &phases[*name];
        for field in ["allocated_blocks", "allocated_bytes"] {
            let before = pair[0][field].as_u64().unwrap();
            let after = pair[1][field].as_u64().unwrap();
            assert_eq!(phase[field], after.checked_sub(before).unwrap());
        }
        for (field, before, after) in [
            ("live_blocks", "live_blocks_before", "live_blocks_after"),
            ("live_bytes", "live_bytes_before", "live_bytes_after"),
        ] {
            assert_eq!(phase[before], pair[0][field].as_u64().unwrap());
            assert_eq!(phase[after], pair[1][field].as_u64().unwrap());
        }
    }
    assert_eq!(record["executor_thread_local"]["source"], "none");
    assert_eq!(record["executor_thread_local"]["retained_blocks"], 0);
    assert_eq!(record["executor_thread_local"]["retained_bytes"], 0);
    let final_snapshot = snapshots.last().unwrap();
    assert_eq!(final_snapshot["live_blocks"], 0);
    assert_eq!(final_snapshot["live_bytes"], 0);
}

#[test]
fn invalid_dependency_inputs_emit_no_success_record() {
    for args in [
        vec!["dependency-cold"],
        vec!["dependency-cold", "1"],
        vec!["dependency-cold", "2", "1"],
        vec!["dependency-cached", "1", "0"],
        vec!["dependency-reset", "16", "65"],
        vec!["dependency-reset", "0", "1025"],
        vec!["dependency-scope", "16", "18446744073709551615"],
        vec!["dependency-cold", "+1", "1"],
        vec!["dependency-cold", "1", "1.5"],
        vec!["dependency-cold", "1", "1", "extra"],
        vec!["dependency-unknown", "1", "1"],
    ] {
        let output = run(&args);
        assert!(!output.status.success(), "{args:?}");
        assert!(output.stdout.is_empty(), "{args:?}");
    }
}
