// Package-local counterpart of rca-core/src/allocation_test/child.rs.
// Allocation and panic integration tests must also work from crate archives.

use std::io::{self, Read};
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

/// A deadlock watchdog, not a runtime or performance threshold.
const CHILD_TIMEOUT: Duration = Duration::from_secs(60);
/// Retains bounded diagnostics while still draining each pipe to completion.
const OUTPUT_LIMIT: u64 = 1024 * 1024;

/// Runs one selected test in a separate process.
pub(crate) fn run_in_child(test_name: &str) -> bool {
    const CHILD_ENV: &str = "RCA_ALLOCATION_TEST_CHILD";
    if std::env::var_os(CHILD_ENV).as_deref() == Some(std::ffi::OsStr::new(test_name)) {
        return false;
    }

    let mut command =
        Command::new(std::env::current_exe().expect("allocation test executable must exist"));
    command
        .args([test_name, "--exact", "--nocapture", "--test-threads=1"])
        .env(CHILD_ENV, test_name);
    let output = capture(&mut command, CHILD_TIMEOUT)
        .unwrap_or_else(|error| panic!("isolated allocation test {test_name}: {error}"));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success() && passed_one_test(&stdout),
        "isolated allocation test {test_name} did not pass exactly once: {}\n{stdout}\n{}",
        output.status,
        String::from_utf8_lossy(&output.stderr),
    );
    true
}

/// Checks the final harness summary, excluding broad, ignored or spoofed selections.
fn passed_one_test(stdout: &str) -> bool {
    stdout
        .lines()
        .rev()
        .find(|line| line.starts_with("test result:"))
        .is_some_and(|line| {
            line == "test result: ok. 1 passed; 0 failed"
                || line.starts_with("test result: ok. 1 passed; 0 failed; 0 ignored;")
        })
}

/// Drains both pipes, reaps the direct child, then joins both readers.
///
/// Children own their pipe writers and must not pass them to surviving
/// descendants. This helper does not supervise a process tree.
pub(crate) fn capture(command: &mut Command, timeout: Duration) -> Result<Output, String> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("child launch failed: {error}"))?;
    let stdout = child.stdout.take().expect("stdout was configured as piped");
    let stderr = child.stderr.take().expect("stderr was configured as piped");
    let stdout = thread::Builder::new()
        .spawn(move || read_output(stdout))
        .map_err(|error| stop_child(&mut child, &format!("stdout reader failed: {error}")))?;
    let stderr = match thread::Builder::new().spawn(move || read_output(stderr)) {
        Ok(reader) => reader,
        Err(error) => {
            let failure = stop_child(&mut child, &format!("stderr reader failed: {error}"));
            let _ = stdout.join();
            return Err(failure);
        }
    };
    let status = wait_for_exit(&mut child, timeout);
    // Join both readers before propagating either result.
    let stdout = stdout.join();
    let stderr = stderr.join();
    let stdout = stdout
        .expect("stdout reader must not panic")
        .map_err(|error| format!("stdout capture failed: {error}"))?;
    let stderr = stderr
        .expect("stderr reader must not panic")
        .map_err(|error| format!("stderr capture failed: {error}"))?;
    let status = status.map_err(|error| {
        format!(
            "{error}\n{}\n{}",
            String::from_utf8_lossy(&stdout),
            String::from_utf8_lossy(&stderr),
        )
    })?;
    Ok(Output {
        status,
        stdout,
        stderr,
    })
}

/// Drains excess output rather than blocking a child on a full pipe.
fn read_output(mut pipe: impl Read) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    pipe.by_ref()
        .take(OUTPUT_LIMIT + 1)
        .read_to_end(&mut bytes)?;
    if u64::try_from(bytes.len()).expect("bounded output length fits u64") > OUTPUT_LIMIT {
        io::copy(&mut pipe, &mut io::sink())?;
        return Err(io::Error::other("child output exceeded the 1 MiB limit"));
    }
    Ok(bytes)
}

/// Reaps normal exits and attempts termination and reaping after any wait failure.
fn wait_for_exit(child: &mut Child, timeout: Duration) -> Result<ExitStatus, String> {
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status),
            Ok(None) => {}
            Err(error) => {
                return Err(stop_child(child, &format!("child status failed: {error}")));
            }
        }
        if started.elapsed() >= timeout {
            return Err(stop_child(child, "child timed out"));
        }
        thread::sleep(Duration::from_millis(10));
    }
}

/// Keeps cleanup failures visible without skipping the reap attempt.
fn stop_child(child: &mut Child, cause: &str) -> String {
    let killed = child.kill();
    let reaped = child.wait();
    format!("{cause}; kill={killed:?}; reap={reaped:?}")
}

#[cfg(test)]
mod tests {
    use super::{CHILD_TIMEOUT, capture, passed_one_test};
    use std::io::{self, Write};
    use std::process::Command;
    use std::time::Duration;

    /// Selects only this helper's child fixture in each consuming test executable.
    fn fixture(mode: &str) -> Command {
        let mut command = Command::new(std::env::current_exe().unwrap());
        let (_, module) = module_path!().split_once("::").unwrap();
        command
            .arg(format!("{module}::process_fixture"))
            .args(["--exact", "--nocapture"])
            .env("RCA_TOKIO_CAPTURE_FIXTURE", mode);
        command
    }

    #[test]
    fn process_fixture() {
        let Ok(mode) = std::env::var("RCA_TOKIO_CAPTURE_FIXTURE") else {
            return;
        };
        match mode.as_str() {
            "large" | "oversized" => {
                let mut stdout = io::stdout().lock();
                let mut stderr = io::stderr().lock();
                // 64 chunks produce 512 KiB per pipe; 129 exceed the one-MiB limit.
                let chunks = if mode == "large" { 64 } else { 129 };
                let chunk = [b'x'; 8192];
                for _ in 0..chunks {
                    stdout.write_all(&chunk).unwrap();
                    stderr.write_all(&chunk).unwrap();
                }
                writeln!(stdout).unwrap();
                writeln!(stderr).unwrap();
            }
            "hang" => loop {
                std::thread::park();
            },
            _ => panic!("unknown capture fixture mode: {mode}"),
        }
    }

    #[test]
    fn rejects_broad_or_spoofed_child_success() {
        for stdout in [
            "test result: ok. 11 passed; 0 failed; 0 ignored;",
            "test result: ok. 1 passed; 0 failed; 1 ignored;",
            "test result: ok. 1 passed; 0 failed; 0 ignored;\ntest result: ok. 0 passed; 0 failed",
            "fixture printed 1 passed; 0 failed",
        ] {
            assert!(
                !passed_one_test(stdout),
                "accepted invalid child summary: {stdout}"
            );
        }
        assert!(passed_one_test(
            "test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s"
        ));
    }

    #[test]
    fn capture_drains_both_pipes_and_rejects_excess_output() {
        let output = capture(&mut fixture("large"), CHILD_TIMEOUT).unwrap();
        assert!(output.status.success());
        assert!(passed_one_test(&String::from_utf8_lossy(&output.stdout)));
        assert!(
            output
                .stdout
                .split(|&byte| byte != b'x')
                .any(|run| run.len() == 512 * 1024)
        );
        let stderr = output.stderr.strip_suffix(b"\n").unwrap();
        assert_eq!(stderr.len(), 512 * 1024);
        assert!(stderr.iter().all(|&byte| byte == b'x'));

        let error = capture(&mut fixture("oversized"), CHILD_TIMEOUT).unwrap_err();
        assert!(
            error.contains("child output exceeded the 1 MiB limit"),
            "{error}"
        );
    }

    #[test]
    fn capture_timeout_reaps_before_joining_readers() {
        let error = capture(&mut fixture("hang"), Duration::from_secs(2)).unwrap_err();
        assert!(error.contains("child timed out"), "{error}");
        assert!(error.contains("reap=Ok("), "{error}");
    }
}
