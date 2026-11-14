use crate::future_outcome::{Fault, OPERATIONS};
use std::{process::Command, time::Duration};

#[expect(
    dead_code,
    reason = "this fixture uses a subset of the shared core child helper"
)]
mod child_process {
    include!(concat!(env!("CARGO_MANIFEST_DIR"), "/../child_process.rs"));
}

pub(crate) fn in_child(name: &str) -> bool {
    const CHILD: &str = "RCA_OUTCOME_TEST_CHILD";
    if std::env::var(CHILD).as_deref() == Ok(name) {
        return true;
    }
    let mut command = Command::new(std::env::current_exe().expect("test executable exists"));
    command
        .args([name, "--exact", "--nocapture"])
        .env(CHILD, name);
    // This is an external failure watchdog, outside the workload. In
    // particular, a pinned legacy host must not hang the test process forever.
    let output = child_process::capture_bounded(&mut command, Duration::from_secs(10))
        .unwrap_or_else(|error| panic!("native outcome {name}: {error}"));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success() && child_process::passed_exactly_one(&stdout),
        "native outcome {name} failed: {}\n{stdout}\n{}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    false
}

#[test]
fn native_common_outcome_completes_exact_workload() {
    if !in_child("native::native_common_outcome_completes_exact_workload") {
        return;
    }
    let (trace, probe) = crate::outcome::run(Fault::None).expect("native workload completed");
    assert_eq!(
        (
            trace.attempted,
            trace.completed,
            trace.results,
            trace.checkpoints,
            trace.checksum,
            trace.errors
        ),
        (64, 64, 64, 64, 6496, 0)
    );
    assert_eq!(probe.counts(), (OPERATIONS, OPERATIONS));
}

#[test]
fn native_common_outcome_rejects_corrupt_or_premature_completion() {
    if !in_child("native::native_common_outcome_rejects_corrupt_or_premature_completion") {
        return;
    }
    for fault in [
        Fault::WrongId,
        Fault::WrongValue,
        Fault::MissingDrop,
        Fault::PrematureCheckpoint,
        Fault::Duplicate,
    ] {
        assert!(
            crate::outcome::run(fault).is_err(),
            "fault {fault:?} accepted"
        );
    }
}
