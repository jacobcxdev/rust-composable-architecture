use std::{
    process::{Command, Output},
    time::Duration,
};

#[expect(
    dead_code,
    reason = "this fixture uses a subset of the shared core child helper"
)]
mod child_process {
    include!(concat!(env!("CARGO_MANIFEST_DIR"), "/../child_process.rs"));
}

fn run(args: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_composable-stream-upstream"));
    command.args(args);
    child_process::capture_bounded(&mut command, Duration::from_secs(60))
        .expect("ready fixture child completes within the watchdog")
}

#[test]
fn ready_future_mode_emits_verified_ledger() {
    let output = run(&["--ready-future"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("UTF-8 ledger");
    assert_eq!(stdout, "{\"schema\":\"composable-ready-future-v1\",\"engine\":\"composable\",\"operations_expected\":64,\"operations_attempted\":64,\"operations_completed\":64,\"results\":64,\"checksum\":6496,\"future_polls\":64,\"future_drops\":64,\"errors\":0,\"state_extracted\":true,\"host_stopped\":true,\"completion\":\"result-acknowledgement\"}\n");
}

#[test]
fn invalid_or_repeated_modes_emit_no_ledger() {
    for args in [
        vec!["--unknown"],
        vec!["--ready-future", "--ready-future"],
        vec!["--measure-ready-future-allocations", "--ready-future"],
        vec![
            "--measure-ready-future-allocations",
            "--measure-ready-future-allocations",
        ],
    ] {
        let output = run(&args);
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        assert!(String::from_utf8_lossy(&output.stderr)
            .contains("expected no arguments or --ready-future"));
    }
}

#[test]
fn allocation_mode_is_feature_gated_and_emits_only_its_ledger() {
    let output = run(&["--measure-ready-future-allocations"]);
    if cfg!(feature = "allocation-measurement") {
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8(output.stdout).expect("UTF-8 ledger");
        assert_eq!(stdout.lines().count(), 1);
        assert!(stdout.starts_with("{\"schema\":\"composable-ready-future-allocations-v1\""));
        assert!(stdout.contains("\"instrument\":\"dhat-0.3.3\""));
        assert!(stdout.contains("\"work\":{\"schema\":\"composable-ready-future-v1\""));
        assert!(stdout.contains("\"checksum\":6496"));
        assert!(stdout.contains("\"initial\":{\"operations\":20,"));
        assert!(stdout.contains("\"checked\":{\"operations\":44,"));
    } else {
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        assert!(String::from_utf8_lossy(&output.stderr).contains("requires allocation-measurement"));
    }
}
