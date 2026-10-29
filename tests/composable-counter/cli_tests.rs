use std::process::Command;

#[expect(
    dead_code,
    reason = "CLI checks reuse bounded capture, not allocation-test selection"
)]
mod child {
    include!("../../rca-core/src/allocation_test/child.rs");

    /// Captures the direct fixture process without unbounded output or waits.
    pub(super) fn output(command: &mut Command) -> Output {
        capture(command, CHILD_TIMEOUT).expect("fixture completed within capture limits")
    }
}

#[test]
fn allocation_mode_requires_instrumentation() {
    let output = child::output(Command::new(BINARY).arg("--measure-allocations"));
    assert_eq!(
        output.status.success(),
        cfg!(feature = "allocation-measurement")
    );
    if cfg!(feature = "allocation-measurement") {
        let value: serde_json::Value = serde_json::from_slice(&output.stdout).expect("valid JSON");
        assert_eq!(value["schema"], "composable-counter-allocations-v1");
        assert_eq!(value["instrument"], "dhat-0.3.3");
        assert_eq!(value["engine"], value["work"]["engine"]);
        assert_eq!(value["work"]["final"], 10_020);
        for name in ["A", "B", "C", "D", "E"] {
            for field in [
                "allocated_blocks",
                "allocated_bytes",
                "live_blocks",
                "live_bytes",
            ] {
                assert!(value["snapshots"][name][field].is_u64());
            }
        }
        for (name, operations) in [
            ("construction", 1),
            ("warmup", 20),
            ("checked", 10_000),
            ("teardown", 1),
            ("setup", 1),
        ] {
            assert_eq!(value["phases"][name]["operations"], operations);
            assert!(value["phases"][name]["allocated_blocks"].is_u64());
            assert!(value["phases"][name]["allocated_bytes"].is_u64());
        }
    } else {
        assert!(output.stdout.is_empty());
    }
}

#[test]
fn allocation_mode_rejects_other_modes_and_repetition() {
    for other in ["--measure", "--measure-lifecycle", "--measure-allocations"] {
        let output = child::output(Command::new(BINARY).args(["--measure-allocations", other]));
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
    }
}
