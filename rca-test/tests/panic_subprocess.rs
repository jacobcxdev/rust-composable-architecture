//! Checks deterministic panic shutdown in isolated processes and rejects false evidence.

#[path = "support/child_process.rs"]
mod child_process;

use std::process::{Command, Output};
use std::time::Duration;

use rca_test::DeterministicRuntime;
use rca_test::conformance::{PanicScenario, assert_panic_output, run_panic_fixture};

/// Selects the exact child scenario without changing the parent environment.
const CHILD_ENV: &str = "RCA_DETERMINISTIC_PANIC_CHILD";

// Each malformed public Output violates exactly one validator requirement.
// These checks must reject false-positive panic/teardown evidence, not merely
// confirm that the real subprocess fixtures happen to satisfy the validator.
#[cfg(any(unix, windows))]
#[test]
fn panic_output_validator_rejects_incomplete_or_unsafe_evidence() {
    #[cfg(unix)]
    use std::os::unix::process::ExitStatusExt;
    #[cfg(windows)]
    use std::os::windows::process::ExitStatusExt;
    use std::process::ExitStatus;

    // Unix wait status encodes exit code 1 in the high byte; Windows uses 1.
    #[cfg(unix)]
    let failure = ExitStatus::from_raw(1 << 8);
    #[cfg(windows)]
    let failure = ExitStatus::from_raw(1);
    for (scenario, original, wrong_original) in [
        (
            PanicScenario::Reducer,
            "intentional reducer panic",
            "intentional selector panic",
        ),
        (
            PanicScenario::Selector,
            "intentional selector panic",
            "intentional effect panic",
        ),
        (
            PanicScenario::Effect,
            "intentional effect panic",
            "intentional reducer panic",
        ),
    ] {
        let valid_stderr = format!("{original}\nRCA_PANIC_STATE_DROPPED\n");
        let valid = Output {
            status: failure,
            stdout: Vec::new(),
            stderr: valid_stderr.as_bytes().to_vec(),
        };
        assert_panic_output(scenario, &valid);

        for (status, stderr, expected) in [
            (
                ExitStatus::default(),
                valid_stderr.clone(),
                "unexpectedly succeeded",
            ),
            (
                failure,
                "RCA_PANIC_STATE_DROPPED\n".to_owned(),
                "omitted original panic",
            ),
            (
                failure,
                format!("{wrong_original}\nRCA_PANIC_STATE_DROPPED\n"),
                "omitted original panic",
            ),
            (
                failure,
                format!("{original}\n"),
                "omitted Store drop marker",
            ),
            (
                failure,
                format!("{valid_stderr}RCA_PANIC_LATER_REDUCED\n"),
                "reduced action after panic",
            ),
        ] {
            let output = Output {
                status,
                // Correct stdout must not substitute for missing stderr evidence.
                stdout: valid.stderr.clone(),
                stderr: stderr.into_bytes(),
            };
            let panic = std::panic::catch_unwind(|| assert_panic_output(scenario, &output))
                .expect_err("the public validator must reject malformed panic evidence");
            let message = panic
                .downcast_ref::<&str>()
                .copied()
                .or_else(|| panic.downcast_ref::<String>().map(String::as_str));
            assert!(
                message.is_some_and(|message| message.contains(expected)),
                "wrong rejection for {scenario:?}: {message:?}"
            );
        }
    }
}

#[test]
fn deterministic_reducer_panic() {
    run_parent_or_child(PanicScenario::Reducer);
}

#[test]
fn deterministic_selector_panic() {
    run_parent_or_child(PanicScenario::Selector);
}

#[test]
fn deterministic_effect_panic() {
    run_parent_or_child(PanicScenario::Effect);
}

/// Runs only the selected fixture in the child and validates its original panic.
fn run_parent_or_child(scenario: PanicScenario) {
    if std::env::var(CHILD_ENV).as_deref() == Ok(scenario.name()) {
        deterministic_child(scenario);
        panic!("panic child returned without propagating failure");
    }

    let output = run_child(scenario);
    assert_eq!(
        output.status.code(),
        Some(101),
        "panic child did not unwind normally: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_panic_output(scenario, &output);
}

/// Drives the real runtime conformance fixture inside the isolated child process.
fn deterministic_child(scenario: PanicScenario) {
    let scheduler = DeterministicRuntime::new();
    let runtime = scheduler.runtime();
    futures_executor::block_on(scheduler.run(run_panic_fixture(runtime, scenario)))
        .expect("panic child scheduler unexpectedly stalled");
}

/// Uses a five-second deadlock watchdog, not a performance measurement.
fn run_child(scenario: PanicScenario) -> Output {
    let test_name = format!("deterministic_{}_panic", scenario.name());
    let mut command = Command::new(std::env::current_exe().expect("test executable path"));
    command
        .args(["--exact", &test_name, "--nocapture"])
        .env(CHILD_ENV, scenario.name());
    child_process::capture(&mut command, Duration::from_secs(5))
        .unwrap_or_else(|error| panic!("panic child capture failed: {error}"))
}
