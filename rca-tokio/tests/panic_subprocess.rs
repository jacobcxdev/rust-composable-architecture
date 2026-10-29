//! Checks isolated panic shutdown on current-thread and multi-thread Tokio hosts.

#[expect(
    dead_code,
    reason = "the panic host needs bounded capture, not the allocation-test entry point"
)]
#[path = "support/allocation_test.rs"]
mod allocation_test;

use std::process::{Command, Output};
use std::time::Duration;

use rca_test::conformance::{PanicScenario, assert_panic_output, run_panic_fixture};
use rca_tokio::TokioRuntime;

/// Selects one child scenario without changing the parent process environment.
const CHILD_ENV: &str = "RCA_TOKIO_PANIC_CHILD";
/// Distinguishes completed post-fault assertions from a second assertion panic.
const POSTCHECKS_COMPLETE: &str = "RCA_TOKIO_PANIC_POSTCHECKS_COMPLETE";

/// Requires both unwind evidence and completed assertions from the isolated task.
fn assert_tokio_panic_output(scenario: PanicScenario, output: &Output) {
    assert_panic_output(scenario, output);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.lines().any(|line| line == POSTCHECKS_COMPLETE),
        "Tokio panic postchecks did not complete: {stderr}"
    );
}

#[cfg(any(unix, windows))]
#[test]
fn tokio_panic_validator_rejects_incomplete_postchecks() {
    #[cfg(unix)]
    use std::os::unix::process::ExitStatusExt;
    #[cfg(windows)]
    use std::os::windows::process::ExitStatusExt;
    use std::process::ExitStatus;

    #[cfg(unix)]
    let failure = ExitStatus::from_raw(1 << 8);
    #[cfg(windows)]
    let failure = ExitStatus::from_raw(1);
    for scenario in [
        PanicScenario::Reducer,
        PanicScenario::Selector,
        PanicScenario::Effect,
    ] {
        let mut output = Output {
            status: failure,
            stdout: Vec::new(),
            stderr: format!(
                "intentional {} panic\nRCA_PANIC_STATE_DROPPED\npostcondition assertion failed\n",
                scenario.name(),
            )
            .into_bytes(),
        };
        let panic = std::panic::catch_unwind(|| assert_tokio_panic_output(scenario, &output))
            .expect_err("incomplete Tokio postchecks must fail the parent validator");
        let message = panic
            .downcast_ref::<&str>()
            .copied()
            .or_else(|| panic.downcast_ref::<String>().map(String::as_str));
        assert!(message.is_some_and(|message| message.contains("postchecks did not complete")));

        output.stderr = format!(
            "intentional {} panic\nRCA_PANIC_STATE_DROPPED\n{POSTCHECKS_COMPLETE}\n",
            scenario.name(),
        )
        .into_bytes();
        assert_tokio_panic_output(scenario, &output);
    }
}

#[test]
fn tokio_current_reducer_panic() {
    run_parent_or_child("current", PanicScenario::Reducer);
}

#[test]
fn tokio_current_selector_panic() {
    run_parent_or_child("current", PanicScenario::Selector);
}

#[test]
fn tokio_current_effect_panic() {
    run_parent_or_child("current", PanicScenario::Effect);
}

#[test]
fn tokio_multi_reducer_panic() {
    run_parent_or_child("multi", PanicScenario::Reducer);
}

#[test]
fn tokio_multi_selector_panic() {
    run_parent_or_child("multi", PanicScenario::Selector);
}

#[test]
fn tokio_multi_effect_panic() {
    run_parent_or_child("multi", PanicScenario::Effect);
}

/// Runs the selected child or checks its unwind and completed postcondition evidence.
fn run_parent_or_child(flavour: &'static str, scenario: PanicScenario) {
    let mode = format!("{flavour}-{}", scenario.name());
    if std::env::var(CHILD_ENV).as_deref() == Ok(mode.as_str()) {
        tokio_child(flavour, scenario);
        panic!("panic child returned after Store failure");
    }

    let output = run_child(flavour, scenario, &mode);
    assert_tokio_panic_output(scenario, &output);
}

/// Owns one native host and emits the marker only after all scenario assertions pass.
fn tokio_child(flavour: &str, scenario: PanicScenario) {
    let host = if flavour == "current" {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("current-thread Tokio runtime")
    } else {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .enable_all()
            .build()
            .expect("multi-thread Tokio runtime")
    };
    host.block_on(async move {
        let runtime = TokioRuntime::current().expect("active Tokio runtime");
        run_panic_fixture(runtime, scenario).await;
        eprintln!("{POSTCHECKS_COMPLETE}");
    });
}

/// Runs one exact scenario while draining its panic output under a watchdog.
fn run_child(flavour: &str, scenario: PanicScenario, mode: &str) -> Output {
    let test_name = format!("tokio_{flavour}_{}_panic", scenario.name());
    let mut command = Command::new(std::env::current_exe().expect("test executable path"));
    command
        .args(["--exact", &test_name, "--nocapture"])
        .env(CHILD_ENV, mode);
    allocation_test::capture(&mut command, Duration::from_secs(5))
        .unwrap_or_else(|error| panic!("panic child capture failed: {error}"))
}
