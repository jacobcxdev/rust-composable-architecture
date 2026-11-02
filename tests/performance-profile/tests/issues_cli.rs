use std::process::{Command, Output};
use std::time::Duration;

mod support;

/// Preserves the 30-second watchdog and bounds diagnostics outside workload measurement.
fn run(args: &[&str]) -> Output {
    support::output(
        Command::new(env!("CARGO_BIN_EXE_rca-performance-profile")).args(args),
        Duration::from_secs(30),
    )
}

#[test]
fn direct_issue_modes_report_exact_public_outcomes() {
    for (mode, size, callbacks, accepted, disabled, failed, clones) in [
        ("issue-direct", "1", 2, 2, 0, 0, 0),
        ("issue-owned", "1", 2, 2, 0, 0, 0),
        ("issue-owned-disabled", "1", 0, 0, 2, 0, 0),
        ("issue-borrowed", "1", 2, 2, 0, 0, 0),
        ("issue-borrowed-failed", "1", 2, 0, 0, 2, 0),
        ("issue-owned", "16", 32, 32, 0, 0, 30),
        ("issue-owned-disabled", "16", 0, 0, 32, 0, 30),
        ("issue-borrowed", "16", 32, 32, 0, 0, 0),
        ("issue-borrowed-failed", "16", 32, 0, 0, 32, 30),
        ("issue-owned", "64", 128, 128, 0, 0, 126),
        ("issue-owned-disabled", "64", 0, 0, 128, 0, 126),
        ("issue-borrowed", "64", 128, 128, 0, 0, 0),
        ("issue-borrowed-failed", "64", 128, 0, 0, 128, 126),
        ("issue-owned", "0", 0, 0, 2, 0, 0),
        ("issue-owned-disabled", "0", 0, 0, 2, 0, 0),
        ("issue-borrowed", "0", 0, 0, 2, 0, 0),
        ("issue-borrowed-failed", "0", 0, 0, 2, 0, 0),
    ] {
        let output = run(&[mode, size, "2"]);
        assert!(
            output.status.success(),
            "{mode}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let record: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(record["schema"], "rca-issue-performance-profile-v1");
        assert_eq!(record["workload"], mode);
        assert_eq!(record["size"], size.parse::<u64>().unwrap());
        assert_eq!(record["iterations"], 2);
        assert_eq!(record["cycles"], 2);
        assert_eq!(record["cold_reports"], 1);
        assert_eq!(record["steady_reports"], 1);
        assert_eq!(record["callbacks"], callbacks);
        assert_eq!(record["accepted"], accepted);
        assert_eq!(record["disabled"], disabled);
        assert_eq!(record["failed"], failed);
        assert_eq!(record["source_expectations"]["issue_clones"], clones);
        assert_eq!(
            record["source_expectations"]["owned_payload_copies"],
            2 * clones
        );
        assert_eq!(record["recovery_count"], disabled + failed);
        assert_eq!(record["faults_created"], failed);
        assert_eq!(record["faults_dropped"], failed);
        let resources = if mode == "issue-owned-disabled" {
            0
        } else {
            size.parse::<u64>().unwrap()
        };
        assert_eq!(record["resources_created"], resources);
        assert_eq!(record["resources_dropped"], resources);
        if size == "0" {
            assert_eq!(record["callback_byte_digest"], 0);
            assert!(record["recovery_byte_digest"].as_u64().unwrap() > 0);
        }
        assert_eq!(record["inputs_constructed"], 2);
        assert_eq!(record["inputs_remaining"], 0);
        assert_eq!(record["owners_released"], true);
        assert_eq!(record["outcome"], "validated");
        assert_eq!(record["driver_contract"], "direct-synchronous-v1");
        assert_eq!(record["recorder_used"], false);
        assert_eq!(
            record["features"]["allocations"],
            cfg!(feature = "allocations")
        );
        if cfg!(feature = "allocations") {
            assert_eq!(
                record["allocations"]["snapshots"].as_array().unwrap().len(),
                6
            );
            assert!(record["allocations"]["phases"]["cold"].is_object());
        } else {
            assert!(record["allocations"].is_null());
        }
    }
}

#[test]
fn issue_inputs_reject_before_a_success_record() {
    for args in [
        vec!["issue-direct", "16", "1"],
        vec!["issue-direct", "0", "1"],
        vec!["issue-direct", "64", "1"],
        vec!["issue-owned", "65", "1"],
        vec!["issue-borrowed-failed", "8", "1"],
        vec!["issue-owned", "1", "0"],
        vec!["issue-owned", "1", "1001"],
        vec!["issue-direct", "+1", "1"],
        vec!["issue-direct", "1", "1.5"],
        vec!["issue-direct", "1", "18446744073709551615"],
        vec!["issue-direct", "1", "1", "extra"],
        vec!["issue-unknown", "1", "1"],
    ] {
        let output = run(&args);
        assert_eq!(output.status.code(), Some(1), "{args:?}");
        assert!(output.stdout.is_empty(), "{args:?}");
        assert!(!output.stderr.is_empty(), "{args:?}");
    }
}
