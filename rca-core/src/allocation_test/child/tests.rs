//! Exercises real child ownership, bounded diagnostics and exact harness summaries.

use super::*;
use std::io::Write as _;

/// Selects the fixture alone, so its child summary proves one actual execution.
const FIXTURE: &str = "allocation_test::child::tests::process_fixture";
/// Chooses child behaviour without changing environment state in the parent.
const FIXTURE_MODE: &str = "RCA_ALLOCATION_WATCHDOG_FIXTURE";

/// Creates an exact-selection command whose mode belongs only to that child.
fn fixture(mode: &str) -> Command {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args([FIXTURE, "--exact", "--nocapture"])
        .env(FIXTURE_MODE, mode);
    command
}

#[test]
fn process_fixture() {
    let Ok(mode) = std::env::var(FIXTURE_MODE) else {
        return;
    };
    match mode.as_str() {
        "success" => {
            println!("fixture stdout");
            eprintln!("fixture stderr");
        }
        "failure" => panic!("fixture failure"),
        "large" | "oversized" => {
            let mut stdout = io::stdout().lock();
            let mut stderr = io::stderr().lock();
            // 64 chunks produce 512 KiB per pipe; 129 exceed each 1 MiB limit.
            let chunks = if mode == "large" { 64 } else { 129 };
            let stdout_chunk = [b'x'; 8192];
            let stderr_chunk = [b'y'; 8192];
            for _ in 0..chunks {
                stdout.write_all(&stdout_chunk).unwrap();
                stderr.write_all(&stderr_chunk).unwrap();
            }
            writeln!(stdout).unwrap();
            writeln!(stderr).unwrap();
        }
        "hang" => {
            println!("fixture entered timeout");
            loop {
                thread::park();
            }
        }
        _ => panic!("unknown fixture mode: {mode}"),
    }
}

#[test]
fn accepts_success_and_rejects_failure_and_empty_selection() {
    let output = capture(&mut fixture("success"), CHILD_TIMEOUT).unwrap();
    assert!(output.status.success());
    assert!(passed_one_test(&String::from_utf8_lossy(&output.stdout)));
    assert!(String::from_utf8_lossy(&output.stderr).contains("fixture stderr"));

    let output = capture(&mut fixture("failure"), CHILD_TIMEOUT).unwrap();
    assert!(!output.status.success());
    assert!(!passed_one_test(&String::from_utf8_lossy(&output.stdout)));
    assert!(String::from_utf8_lossy(&output.stderr).contains("fixture failure"));

    let mut empty = Command::new(std::env::current_exe().unwrap());
    empty.args(["no_such_allocation_test", "--exact"]);
    let output = capture(&mut empty, CHILD_TIMEOUT).unwrap();
    assert!(output.status.success());
    assert!(!passed_one_test(&String::from_utf8_lossy(&output.stdout)));
}

#[test]
fn checks_the_final_exact_summary_instead_of_a_substring() {
    for summary in [
        "test result: ok. 11 passed; 0 failed; 0 ignored;",
        "test result: ok. 1 passed; 0 failed; 1 ignored;",
        "test result: ok. 1 passed; 0 failed\ntest result: ok. 0 passed; 0 failed",
        "1 passed; 0 failed",
    ] {
        assert!(!passed_one_test(summary), "{summary}");
    }
    assert!(passed_one_test("test result: ok. 1 passed; 0 failed"));
}

#[test]
fn launcher_requires_one_passing_test() {
    run_test_child(FIXTURE, FIXTURE_MODE, "success", &[]);
    std::panic::catch_unwind(|| run_test_child(FIXTURE, FIXTURE_MODE, "failure", &[]))
        .expect_err("a failed child test must fail its parent");
    std::panic::catch_unwind(|| {
        run_test_child("no_such_allocation_test", FIXTURE_MODE, "success", &[]);
    })
    .expect_err("an empty child selection must fail its parent");
    let mut missing = Command::new("/no/such/rca-allocation-test");
    let error = capture(&mut missing, CHILD_TIMEOUT).unwrap_err();
    assert!(error.contains("child launch failed"), "{error}");
}

#[test]
fn drains_large_pipes_and_rejects_excess_output() {
    let output = capture(&mut fixture("large"), CHILD_TIMEOUT).unwrap();
    assert!(output.status.success());
    assert!(passed_one_test(&String::from_utf8_lossy(&output.stdout)));
    assert!(
        output
            .stdout
            .split(|&byte| byte != b'x')
            .any(|run| run.len() == 512 * 1024)
    );
    assert_eq!(output.stderr.last(), Some(&b'\n'));
    let stderr = &output.stderr[..output.stderr.len() - 1];
    assert_eq!(stderr.len(), 512 * 1024);
    assert!(stderr.iter().all(|&byte| byte == b'y'));
    let error = capture(&mut fixture("oversized"), CHILD_TIMEOUT).unwrap_err();
    assert!(
        error.contains("child output exceeded the 1 MiB limit"),
        "{error}"
    );
}

#[test]
fn timeout_kills_and_reaps_the_owned_process() {
    let mut child = fixture("hang")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let error = wait_for_exit(&mut child, Duration::from_millis(100)).unwrap_err();
    assert!(error.contains("child timed out"), "{error}");
    assert!(error.contains("reap=Ok("), "{error}");
    assert!(
        !child
            .try_wait()
            .unwrap()
            .expect("child was reaped")
            .success()
    );
}

#[test]
fn timeout_reaps_before_joining_readers() {
    let error = capture(&mut fixture("hang"), Duration::from_secs(2)).unwrap_err();
    assert!(error.contains("child timed out"), "{error}");
    assert!(error.contains("reap=Ok("), "{error}");
}
