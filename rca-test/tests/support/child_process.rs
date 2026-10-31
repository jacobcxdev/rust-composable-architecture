//! Captures direct panic-test children without a cross-package source include.
//!
//! This is the capture portion of `rca-tokio/tests/support/allocation_test.rs`.
//! The package-local copy keeps published crate test sources self-contained.

use std::io::{self, Read};
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

/// Retains bounded diagnostics while still draining each pipe to completion.
const OUTPUT_LIMIT: u64 = 1024 * 1024;

/// Drains both pipes, reaps the direct child, then joins both readers.
///
/// Children must not pass their pipe writers to surviving descendants. This is
/// not a process-tree supervisor. OS termination or reap failures remain errors;
/// no finite return time is guaranteed when the OS cannot terminate the child.
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

/// Retains one detection byte beyond the limit, then drains and rejects excess.
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

/// Keeps cleanup failures visible without skipping the reap attempt after a kill error.
fn stop_child(child: &mut Child, cause: &str) -> String {
    let killed = child.kill();
    let reaped = child.wait();
    format!("{cause}; kill={killed:?}; reap={reaped:?}")
}

#[cfg(test)]
mod tests {
    use super::{OUTPUT_LIMIT, capture, read_output};
    use std::io::{self, Write};
    use std::process::Command;
    use std::time::Duration;

    /// Selects this helper's fixture in either consuming integration-test executable.
    fn fixture(mode: &str) -> Command {
        let mut command = Command::new(std::env::current_exe().unwrap());
        let (_, module) = module_path!().split_once("::").unwrap();
        command
            .arg(format!("{module}::process_fixture"))
            .args(["--exact", "--nocapture"])
            .env("RCA_TEST_CAPTURE_FIXTURE", mode);
        command
    }

    #[test]
    fn process_fixture() {
        let Ok(mode) = std::env::var("RCA_TEST_CAPTURE_FIXTURE") else {
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
    fn read_output_drains_and_rejects_oversized_diagnostics() {
        let limit = usize::try_from(OUTPUT_LIMIT).unwrap();
        let accepted = read_output(io::Cursor::new(vec![b'x'; limit])).unwrap();
        assert_eq!(accepted.len(), limit);
        assert!(accepted.iter().all(|&byte| byte == b'x'));

        let bytes = vec![b'x'; limit + 8193];
        let mut input = io::Cursor::new(bytes);
        let result = read_output(&mut input);

        assert_eq!(
            input.position(),
            u64::try_from(input.get_ref().len()).unwrap()
        );
        let Err(error) = result else {
            panic!("oversized diagnostic output was accepted");
        };
        assert_eq!(error.to_string(), "child output exceeded the 1 MiB limit");
    }

    #[test]
    fn capture_drains_both_pipes_and_rejects_excess_output() {
        let output = capture(&mut fixture("large"), Duration::from_secs(5)).unwrap();
        assert!(output.status.success());
        assert!(
            output
                .stdout
                .split(|&byte| byte != b'x')
                .any(|run| run.len() == 512 * 1024)
        );
        let stderr = output.stderr.strip_suffix(b"\n").unwrap();
        assert_eq!(stderr.len(), 512 * 1024);
        assert!(stderr.iter().all(|&byte| byte == b'x'));

        let error = capture(&mut fixture("oversized"), Duration::from_secs(5)).unwrap_err();
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
