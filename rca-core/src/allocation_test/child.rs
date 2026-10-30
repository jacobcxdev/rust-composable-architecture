use std::io::{self, Read};
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

/// A deadlock watchdog, not a runtime or performance threshold.
const CHILD_TIMEOUT: Duration = Duration::from_secs(60);
/// Retain bounded diagnostics while still draining both pipes to completion.
const OUTPUT_LIMIT: u64 = 1024 * 1024;

/// Runs one selected test in a separate process.
pub(crate) fn run_in_child(test_name: &str) -> bool {
    run_in_child_with_env(test_name, &[])
}

/// Sets child-only configuration before any runtime or backtrace state is cached.
pub(crate) fn run_in_child_with_env(test_name: &str, environment: &[(&str, &str)]) -> bool {
    const CHILD_ENV: &str = "RCA_ALLOCATION_TEST_CHILD";
    if std::env::var_os(CHILD_ENV).as_deref() == Some(std::ffi::OsStr::new(test_name)) {
        return false;
    }
    run_test_child(test_name, CHILD_ENV, test_name, environment);
    true
}

/// Requires the selected test to execute, not merely an empty successful harness.
pub(super) fn run_test_child(
    test_name: &str,
    child_env: &str,
    marker: &str,
    environment: &[(&str, &str)],
) {
    let mut command =
        Command::new(std::env::current_exe().expect("allocation test executable must exist"));
    command
        .args([test_name, "--exact", "--nocapture", "--test-threads=1"])
        .envs(environment.iter().copied())
        .env(child_env, marker);
    let output = capture(&mut command, CHILD_TIMEOUT)
        .unwrap_or_else(|error| panic!("isolated allocation test {test_name}: {error}"));
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    print!("{stdout}");
    eprint!("{stderr}");
    assert!(
        output.status.success() && passed_one_test(&stdout),
        "isolated allocation test {test_name} did not pass exactly once: {}\n{stdout}\n{stderr}",
        output.status,
    );
}

/// Checks the final harness summary, excluding ignored or accidentally broad selections.
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

/// Drains each pipe while the direct child runs, then joins readers after reaping it.
///
/// These test cases own their pipe writers; they must not pass them to surviving
/// descendants. This helper is not a process-tree supervisor.
fn capture(command: &mut Command, timeout: Duration) -> Result<Output, String> {
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

/// Reaps normal exits and attempts both termination and reaping on failure.
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
