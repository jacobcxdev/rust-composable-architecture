//! Checks actual reporter output against strict shape, field and workload contracts.

use serde::Deserialize;
use std::process::Command;

use child::command_output;

#[expect(
    dead_code,
    reason = "reporter checks reuse bounded capture with their own execution deadline"
)]
mod child {
    include!("../../rca-core/src/allocation_test/child.rs");

    /// Bounds reporter execution and output, excluding compilation time.
    pub(super) fn command_output(command: &mut Command) -> Output {
        capture(command, Duration::from_secs(10))
            .expect("allocation reporter completed within capture limits")
    }
}

/// Rejects unknown or missing report fields before semantic assertions run.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Report {
    schema: String,
    engine: String,
    instrument: String,
    work: Work,
    snapshots: Snapshots,
    phases: Phases,
}

/// Keeps literal work and lifecycle claims independent from allocation totals.
#[derive(Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
struct Work {
    trace: Vec<u64>,
    updates: u64,
    publications: u64,
    final_value: u64,
    previous_value: u64,
    state_extracted: bool,
    host_stopped: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Snapshot {
    allocated_blocks: u64,
    allocated_bytes: u64,
    live_blocks: u64,
    live_bytes: u64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "UPPERCASE")]
struct Snapshots {
    a: Snapshot,
    b: Snapshot,
    c: Snapshot,
    d: Snapshot,
    e: Snapshot,
    f: Snapshot,
    g: Snapshot,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Phase {
    operations: u64,
    allocated_blocks: u64,
    allocated_bytes: u64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Phases {
    construction: Phase,
    registration: Phase,
    initial: Phase,
    checked: Phase,
    unregistration: Phase,
    teardown: Phase,
}

/// Requires a clean process exit before returning bytes for strict parsing.
fn report_output() -> Vec<u8> {
    let output = command_output(&mut Command::new(env!(
        "CARGO_BIN_EXE_observation-allocations"
    )));
    assert!(
        output.status.success(),
        "reporter failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output.stderr.is_empty(),
        "unexpected diagnostic: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

#[test]
#[ignore = "child entry point for the bounded-output regression"]
fn oversized_reporter_output() {
    use std::io::Write as _;

    let mut stdout = std::io::stdout().lock();
    for _ in 0..129 {
        stdout.write_all(&[b'x'; 8192]).unwrap();
    }
}

#[test]
fn reporter_capture_rejects_excess_output() {
    let result = std::panic::catch_unwind(|| {
        command_output(Command::new(std::env::current_exe().unwrap()).args([
            "oversized_reporter_output",
            "--ignored",
            "--exact",
            "--nocapture",
        ]))
    });
    let Err(error) = result else {
        panic!("oversized reporter output must fail");
    };
    let message = error.downcast_ref::<String>().expect("capture error text");
    assert!(message.contains("child output exceeded the 1 MiB limit"));
}

#[test]
fn real_adapter_reports_complete_allocation_ledger() {
    let output = report_output();
    // A strict typed schema rejects missing, duplicate, unknown and malformed fields.
    // Parsing the entire output also rejects extra records or non-JSON stdout.
    let report = parse_report(&output);
    assert_eq!(report.schema, "composable-observation-allocations-v1");
    assert_eq!(report.instrument, "dhat-0.3.3");
    assert_eq!(
        report.engine,
        env!("CARGO_PKG_NAME")
            .strip_prefix("composable-observation-")
            .unwrap()
    );
    assert_eq!(
        report.work,
        Work {
            trace: vec![0, 3, 1, 1, 4],
            updates: 5,
            publications: 5,
            final_value: 9,
            previous_value: 4,
            state_extracted: true,
            host_stopped: true,
        }
    );
    let snapshots = report.snapshots;
    let snapshots = [
        snapshots.a,
        snapshots.b,
        snapshots.c,
        snapshots.d,
        snapshots.e,
        snapshots.f,
        snapshots.g,
    ];
    let phases = report.phases;
    let phases = [
        phases.construction,
        phases.registration,
        phases.initial,
        phases.checked,
        phases.unregistration,
        phases.teardown,
    ];
    let operations = [1, 1, 2, 2, 1, 1];
    for snapshot in &snapshots {
        assert!(snapshot.live_blocks <= snapshot.allocated_blocks);
        assert!(snapshot.live_bytes <= snapshot.allocated_bytes);
    }
    for ((pair, phase), operations) in snapshots.windows(2).zip(phases).zip(operations) {
        assert_eq!(phase.operations, operations);
        assert_eq!(
            phase.allocated_blocks,
            pair[1]
                .allocated_blocks
                .checked_sub(pair[0].allocated_blocks)
                .unwrap()
        );
        assert_eq!(
            phase.allocated_bytes,
            pair[1]
                .allocated_bytes
                .checked_sub(pair[0].allocated_bytes)
                .unwrap()
        );
    }
}

fn parse_report(output: &[u8]) -> Report {
    // Parse original bytes first: parsing through Value alone loses duplicate keys.
    let report = serde_json::from_slice(output).unwrap();
    let shape: serde_json::Value = serde_json::from_slice(output).unwrap();
    assert!(shape.is_object(), "report must be an object");
    for field in ["work", "snapshots", "phases"] {
        assert!(shape[field].is_object(), "{field} must be an object");
    }
    for group in ["snapshots", "phases"] {
        for (name, value) in shape[group].as_object().unwrap() {
            assert!(value.is_object(), "{group}.{name} must be an object");
        }
    }
    report
}

#[test]
fn positional_work_is_rejected() {
    let mut report: serde_json::Value = serde_json::from_slice(&report_output()).unwrap();
    report["work"] = serde_json::json!([[0, 3, 1, 1, 4], 5, 5, 9, 4, true, true]);
    let output = serde_json::to_vec(&report).unwrap();
    assert!(std::panic::catch_unwind(|| parse_report(&output)).is_err());
}
