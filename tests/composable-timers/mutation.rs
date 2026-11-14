//! Runs timer-policy negative controls in isolated test processes.
//!
//! Children must not pass stderr writers to surviving descendant processes.

use std::io::{self, Read};
use std::process::{Command, ExitStatus, Output, Stdio};
use std::time::{Duration, Instant};

/// Runs one mutation and requires its specific assertion diagnostic.
pub(crate) fn reject(test: &str, variable: &str, mode: &str, diagnostic: &str) {
    let output = capture(test, variable, mode, Duration::from_secs(10));
    assert!(!output.status.success(), "mutation was not rejected");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains(diagnostic), "wrong failure: {stderr}");
}

/// Stops and reaps the child if process inspection or thread creation fails.
struct Child(std::process::Child);

impl Drop for Child {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Drains diagnostics while the child runs, then joins before reporting failure.
fn capture(test: &str, variable: &str, mode: &str, timeout: Duration) -> Output {
    let child = Child(
        Command::new(std::env::current_exe().unwrap())
            .args(["--exact", test, "--ignored", "--nocapture"])
            .env(variable, mode)
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    std::thread::scope(move |scope| {
        // This local owner drops before scoped readers join during unwinding.
        let mut child = child;
        let stderr = child.0.stderr.take().unwrap();
        let reader = scope.spawn(move || read_output(stderr));
        let status = wait(&mut child, timeout);
        let bytes = reader.join();
        let status = status.unwrap_or_else(|failure| panic!("{failure}"));
        Output {
            status,
            stdout: Vec::new(),
            stderr: bytes.unwrap().unwrap(),
        }
    })
}

/// Keeps timeout and status errors distinct from valid negative controls.
fn wait(child: &mut Child, timeout: Duration) -> Result<ExitStatus, String> {
    let deadline = Instant::now() + timeout;
    loop {
        let failure = match child.0.try_wait() {
            Ok(Some(status)) => return Ok(status),
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
                continue;
            }
            Ok(None) => "mutation subprocess timed out, not a valid negative control".to_owned(),
            Err(error) => format!("mutation subprocess status failed: {error}"),
        };
        let killed = child.0.kill();
        let reaped = child.0.wait();
        return Err(format!("{failure}; kill={killed:?}; reap={reaped:?}"));
    }
}

/// Retains at most one MiB plus one detection byte, then drains excess input.
fn read_output(mut pipe: impl Read) -> io::Result<Vec<u8>> {
    // Match the existing allocation runners without changing their shared inputs.
    const OUTPUT_LIMIT: u64 = 1024 * 1024;
    let mut bytes = Vec::new();
    pipe.by_ref()
        .take(OUTPUT_LIMIT + 1)
        .read_to_end(&mut bytes)?;
    if u64::try_from(bytes.len()).unwrap() > OUTPUT_LIMIT {
        io::copy(&mut pipe, &mut io::sink())?;
        return Err(io::Error::other(
            "mutation diagnostics exceeded the 1 MiB limit",
        ));
    }
    Ok(bytes)
}

#[test]
fn oversized_diagnostics_fail_after_draining_the_pipe() {
    let mut pipe = io::Cursor::new(vec![b'x'; 1024 * 1024 + 17]);
    let error = read_output(&mut pipe).unwrap_err();
    assert_eq!(pipe.position(), 1024 * 1024 + 17);
    assert_eq!(
        error.to_string(),
        "mutation diagnostics exceeded the 1 MiB limit"
    );
}

#[test]
fn permitted_diagnostics_remain_exact() {
    let input = vec![b'x'; 1024 * 1024];
    assert_eq!(read_output(input.as_slice()).unwrap(), input);
}

#[test]
#[should_panic(expected = "mutation was not rejected")]
fn successful_child_is_not_a_negative_control() {
    reject(
        "mutation::control_child",
        "RCA_TIMER_RUNNER_CONTROL",
        "success",
        "failure",
    );
}

#[test]
#[should_panic(expected = "wrong failure")]
fn unrelated_failure_is_not_a_negative_control() {
    reject(
        "mutation::control_child",
        "RCA_TIMER_RUNNER_CONTROL",
        "failure",
        "different diagnostic",
    );
}

#[test]
#[should_panic(expected = "mutation subprocess timed out")]
fn timeout_is_not_a_negative_control() {
    capture(
        "mutation::control_child",
        "RCA_TIMER_RUNNER_CONTROL",
        "park",
        Duration::ZERO,
    );
}

#[test]
#[should_panic(expected = "mutation diagnostics exceeded the 1 MiB limit")]
fn oversized_child_output_does_not_block_exit() {
    capture(
        "mutation::control_child",
        "RCA_TIMER_RUNNER_CONTROL",
        "large",
        Duration::from_secs(10),
    );
}

#[test]
#[ignore = "invoked by bounded process-runner controls"]
fn control_child() {
    match std::env::var("RCA_TIMER_RUNNER_CONTROL").unwrap().as_str() {
        "success" => {}
        "failure" => panic!("intentional control failure"),
        "park" => loop {
            std::thread::park();
        },
        "large" => {
            use std::io::Write;
            let mut stderr = io::stderr().lock();
            for _ in 0..256 {
                stderr.write_all(&[b'x'; 8192]).unwrap();
            }
        }
        other => panic!("unknown runner control: {other}"),
    }
}
