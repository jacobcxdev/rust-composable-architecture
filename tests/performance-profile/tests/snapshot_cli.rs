use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

// Far above these one-record fixtures; limits each parent read independently.
const OUTPUT_LIMIT: u64 = 65_536;

/// Owns a new capture directory until its child and evidence checks are complete.
struct CaptureDirectory {
    path: PathBuf,
}

impl CaptureDirectory {
    fn new(label: &str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "rca-snapshot-{label}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self { path }
    }
}

impl Drop for CaptureDirectory {
    fn drop(&mut self) {
        // These fixtures create files and, for one error control, an empty directory.
        // Refuse deeper trees instead of recursively deleting unexpected contents.
        for entry in fs::read_dir(&self.path).unwrap() {
            let entry = entry.unwrap();
            if entry.file_type().unwrap().is_dir() {
                fs::remove_dir(entry.path()).unwrap();
            } else {
                fs::remove_file(entry.path()).unwrap();
            }
        }
        fs::remove_dir(&self.path).unwrap();
    }
}

fn child(args: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_rca-performance-profile"));
    command.args(args);
    let (directory, result) = capture(&mut command);
    result.unwrap_or_else(|error| panic!("{error}: {}", directory.path.display()))
}

fn read_output(path: &Path) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    File::open(path)
        .and_then(|file| file.take(OUTPUT_LIMIT + 1).read_to_end(&mut bytes))
        .map_err(|error| format!("{}: {error}", path.display()))?;
    if bytes.len() > 65_536 {
        return Err(format!("{} output exceeds 65536 bytes", path.display()));
    }
    Ok(bytes)
}

fn check_output_sizes(paths: &[&Path]) -> Result<(), String> {
    for path in paths {
        let size = fs::metadata(path)
            .map_err(|error| format!("{} metadata: {error}", path.display()))?
            .len();
        if size > OUTPUT_LIMIT {
            return Err(format!(
                "{} output exceeds 65536 bytes: {size}",
                path.display()
            ));
        }
    }
    Ok(())
}

/// Preserves the snapshot host's stricter live-file and terminal-read output limits.
fn capture(command: &mut Command) -> (CaptureDirectory, Result<Output, String>) {
    let owned_directory = CaptureDirectory::new("cli");
    let directory = &owned_directory.path;
    eprintln!("Snapshot child evidence: {}", directory.display());
    let stdout = directory.join("stdout");
    let stderr = directory.join("stderr");
    fs::write(directory.join("command"), format!("{command:?}\n")).unwrap();
    let mut process = command
        .stdout(Stdio::from(File::create(&stdout).unwrap()))
        .stderr(Stdio::from(File::create(&stderr).unwrap()))
        .spawn()
        .unwrap();
    // No fallible operation after spawn may abandon this owned child.
    let mut failure = fs::write(directory.join("owned-pid"), process.id().to_string())
        .err()
        .map(|error| format!("owned-pid: {error}"));
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut timed_out = false;
    let mut stop = "terminal";
    let mut kill = None;
    let status = loop {
        match process.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) => {}
            Err(error) => failure = Some(format!("try_wait: {error}")),
        }
        // Observation can overshoot between polls; this is not an OS disk quota.
        if failure.is_none() {
            failure = check_output_sizes(&[&stdout, &stderr]).err();
        }
        if failure.is_none() && Instant::now() >= deadline {
            timed_out = true;
            failure = Some("snapshot child deadline".into());
        }
        if failure.is_some() {
            stop = "live";
            kill = Some(process.kill().map_err(|error| error.to_string()));
            break process.wait();
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let result = (|| {
        let status = status.as_ref().map_err(|error| format!("reap: {error}"))?;
        if let Some(error) = &failure {
            return Err(error.clone());
        }
        check_output_sizes(&[&stdout, &stderr])?;
        Ok(Output {
            status: *status,
            stdout: read_output(&stdout)?,
            stderr: read_output(&stderr)?,
        })
    })();
    let cleanup = if status.is_ok() {
        "reaped"
    } else {
        "reap-failed"
    };
    let evidence = format!(
        "actual={status:?}; deadline={timed_out}; stop={stop}; kill={kill:?}; cleanup={cleanup}; result={:?}\n",
        result.as_ref().err()
    );
    let terminal = fs::write(directory.join("status"), &evidence);
    if result.is_err() || terminal.is_err() {
        // Keep failure evidence in the test log before scoped file cleanup.
        eprintln!("Snapshot capture outcome: {evidence}");
    }
    let result = terminal
        .map_err(|error| format!("status evidence: {error}"))
        .and(result);
    (owned_directory, result)
}

fn fixture_command(mode: &str) -> Command {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command.args(["--exact", "capture_fixture", "--ignored", "--nocapture"]);
    command.env("RCA_SNAPSHOT_CAPTURE_FIXTURE", mode);
    command
}

#[test]
#[ignore = "finite child fixture, invoked only by capture controls"]
fn capture_fixture() {
    let mode = std::env::var("RCA_SNAPSHOT_CAPTURE_FIXTURE").unwrap();
    if mode == "small" {
        println!("small stdout");
        eprintln!("small stderr");
        return;
    }
    let bytes = vec![b'x'; 65_537];
    if mode.ends_with("stdout") {
        std::io::stdout().write_all(&bytes).unwrap();
        std::io::stdout().flush().unwrap();
    } else {
        std::io::stderr().write_all(&bytes).unwrap();
        std::io::stderr().flush().unwrap();
    }
    // Independently finite even if the parent's live-size check is broken.
    if mode.starts_with("live") {
        std::thread::sleep(Duration::from_secs(1));
    }
}

#[test]
fn capture_small_output() {
    let (owned_directory, output) = capture(&mut fixture_command("small"));
    let directory = &owned_directory.path;
    let output = output.unwrap();
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("small stdout"));
    assert!(String::from_utf8_lossy(&output.stderr).contains("small stderr"));
    assert!(
        fs::read_to_string(directory.join("status"))
            .unwrap()
            .contains("cleanup=reaped")
    );
    let path = directory.clone();
    drop(owned_directory);
    assert!(!path.exists(), "completed snapshot capture must be removed");
}

#[test]
fn capture_directory_cleans_files_after_unwind() {
    let mut path = None;
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let directory = CaptureDirectory::new("unwind");
        path = Some(directory.path.clone());
        fs::write(directory.path.join("stdout"), b"captured output").unwrap();
        fs::create_dir(directory.path.join("owned-pid")).unwrap();
        panic!("intentional snapshot cleanup unwind");
    }));
    let panic = result.expect_err("the fixture must unwind with its caller");
    assert_eq!(
        panic.downcast_ref::<&str>(),
        Some(&"intentional snapshot cleanup unwind")
    );
    assert!(!path.unwrap().exists());
}

#[test]
fn capture_live_output_limit() {
    let mut accepted = Vec::new();
    for stream in ["stdout", "stderr"] {
        let (owned_directory, output) = capture(&mut fixture_command(&format!("live-{stream}")));
        let directory = &owned_directory.path;
        let Err(error) = output else {
            accepted.push(stream);
            continue;
        };
        assert!(error.contains("output exceeds 65536 bytes"), "{error}");
        let status = fs::read_to_string(directory.join("status")).unwrap();
        assert!(status.contains("stop=live"), "{status}");
        assert!(status.contains("deadline=false"), "{status}");
        assert!(status.contains("kill=Some(Ok(()))"), "{status}");
        assert!(status.contains("cleanup=reaped"), "{status}");
        assert!(fs::metadata(directory.join(stream)).unwrap().len() > 65_536);
    }
    assert!(
        accepted.is_empty(),
        "oversized streams accepted: {accepted:?}"
    );
}

fn terminal_wait(process: &mut Child, directory: &Path) -> Result<ExitStatus, String> {
    let mut failure = fs::write(directory.join("owned-pid"), process.id().to_string())
        .err()
        .map(|error| format!("owned-pid: {error}"));
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut timed_out = false;
    let mut kill = None;
    let actual = loop {
        match process.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) => {}
            Err(error) => failure = Some(format!("try_wait: {error}")),
        }
        if failure.is_none() && Instant::now() >= deadline {
            timed_out = true;
            failure = Some("terminal fixture deadline".into());
        }
        if failure.is_some() {
            kill = Some(process.kill().map_err(|error| error.to_string()));
            break process.wait();
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let cleanup = if actual.is_ok() {
        "reaped"
    } else {
        "reap-failed"
    };
    let evidence = format!(
        "actual={actual:?}; deadline={timed_out}; kill={kill:?}; cleanup={cleanup}; failure={failure:?}\n"
    );
    // Keep actual terminal/cleanup evidence visible even if the status write fails.
    eprintln!("Terminal fixture outcome: {evidence}");
    let recorded = fs::write(directory.join("status"), evidence);
    let status = actual.map_err(|error| format!("reap: {error}; kill={kill:?}"))?;
    recorded.map_err(|error| format!("status evidence: {error}"))?;
    if let Some(error) = failure {
        return Err(format!("{error}; kill={kill:?}"));
    }
    Ok(status)
}

#[test]
fn capture_terminal_evidence_failure_reaps() {
    let owned_directory = CaptureDirectory::new("terminal-failure");
    let directory = &owned_directory.path;
    eprintln!("Snapshot child evidence: {}", directory.display());
    // A directory at the evidence filename deterministically rejects the write.
    fs::create_dir(directory.join("owned-pid")).unwrap();
    let mut command = fixture_command("live-stderr");
    fs::write(directory.join("command"), format!("{command:?}\n")).unwrap();
    let mut process = command
        .stdout(File::create(directory.join("stdout")).unwrap())
        .stderr(File::create(directory.join("stderr")).unwrap())
        .spawn()
        .unwrap();
    // Keep the actual owned handle outside the old path's unwind, for RED safety.
    let attempted = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        terminal_wait(&mut process, directory)
    }));
    let observed = process.try_wait();
    let needed_fallback = !matches!(observed, Ok(Some(_)));
    let kill = if needed_fallback {
        Some(process.kill())
    } else {
        None
    };
    let actual = process.wait();
    // Record independent containment before any assertion can unwind the test.
    let containment = format!("actual={actual:?}; fallback={needed_fallback}; kill={kill:?}\n");
    eprintln!("Terminal fixture independent containment: {containment}");
    fs::write(directory.join("containment"), containment).unwrap();
    assert!(actual.is_ok(), "{actual:?}");
    let result = attempted.expect("post-spawn evidence failure must return after owned reap");
    let error = result.unwrap_err();
    assert!(error.contains("owned-pid"), "{error}");
    assert!(
        !needed_fallback,
        "terminal helper left its owned child live"
    );
    let status = fs::read_to_string(directory.join("status")).unwrap();
    assert!(status.contains("cleanup=reaped"), "{status}");
    assert!(status.contains("actual=Ok("), "{status}");
    assert!(status.contains("deadline=false"), "{status}");
}

#[test]
fn capture_terminal_output_limit() {
    let mut accepted = Vec::new();
    for stream in ["stdout", "stderr"] {
        // Wait for a finite fixture's normal exit before exercising the final read.
        let owned_directory = CaptureDirectory::new("terminal");
        let directory = &owned_directory.path;
        eprintln!("Snapshot child evidence: {}", directory.display());
        let mut command = fixture_command(&format!("terminal-{stream}"));
        fs::write(directory.join("command"), format!("{command:?}\n")).unwrap();
        let mut process = command
            .stdout(File::create(directory.join("stdout")).unwrap())
            .stderr(File::create(directory.join("stderr")).unwrap())
            .spawn()
            .unwrap();
        let status = terminal_wait(&mut process, directory).unwrap();
        assert!(status.success());
        let error = check_output_sizes(&[&directory.join("stdout"), &directory.join("stderr")])
            .unwrap_err();
        assert!(error.contains("output exceeds 65536 bytes"), "{error}");
        let Err(error) = read_output(&directory.join(stream)) else {
            accepted.push(stream);
            continue;
        };
        assert!(error.contains("output exceeds 65536 bytes"), "{error}");
        let boundary = directory.join("boundary");
        fs::write(&boundary, vec![b'x'; 65_536]).unwrap();
        assert_eq!(read_output(&boundary).unwrap().len(), 65_536);
    }
    assert!(
        accepted.is_empty(),
        "oversized streams accepted: {accepted:?}"
    );
}

#[cfg(feature = "deterministic")]
fn record(name: &str, size: &str, iterations: &str) -> serde_json::Value {
    let output = child(&[name, size, iterations]);
    assert!(
        output.status.success(),
        "{name} {size} {iterations}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        std::str::from_utf8(&output.stdout).unwrap().lines().count(),
        1
    );
    let record: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(record["workload"], name);
    assert_eq!(record["size"], size.parse::<u64>().unwrap());
    assert_eq!(record["iterations"], iterations.parse::<u64>().unwrap());
    assert_eq!(record["drive_contract"], "direct-controlled-poll-v1");
    assert_eq!(record["features"]["deterministic"], true);
    assert_eq!(
        record["features"]["allocations"],
        cfg!(feature = "allocations")
    );
    assert_eq!(record["core_features"], "diagnostics,test-util");
    assert_eq!(record["owners_released"], true);
    for field in [
        "roots",
        "responses",
        "tasks",
        "redactor_calls",
        "writer_calls",
    ] {
        assert_eq!(record["facts"][field], 0, "{field}");
    }
    #[cfg(feature = "allocations")]
    {
        let cache = &record["allocations"]["executor_thread_local"];
        assert_eq!(cache["source"], "none");
        assert_eq!(cache["retained_blocks"], 0);
        assert_eq!(cache["retained_bytes"], 0);
        assert_eq!(
            record["allocations"]["snapshots"].as_array().unwrap().len(),
            5
        );
        assert_eq!(record["allocations"]["snapshots"][4]["live_blocks"], 0);
        assert_eq!(record["allocations"]["snapshots"][4]["live_bytes"], 0);
    }
    #[cfg(not(feature = "allocations"))]
    assert!(record["allocations"].is_null());
    record
}

#[cfg(feature = "deterministic")]
#[test]
fn finite_byte_matrix() {
    // Literal digests for bytes j mod 251, independently summed before implementation.
    for (size, iterations, digest) in [
        ("0", "1", 0),
        ("8", "1", 28),
        ("8", "256", 7168),
        ("4096", "16", 8_082_560),
        ("65536", "16", 131_026_800),
    ] {
        let record = record("snapshot-bytes", size, iterations);
        assert_eq!(record["schema"], "rca-snapshot-bytes-profile-v1");
        let facts = &record["facts"];
        assert_eq!(facts["comparisons"], iterations.parse::<u64>().unwrap());
        assert_eq!(facts["reserved_cases"], iterations.parse::<u64>().unwrap());
        assert_eq!(facts["byte_len"], size.parse::<u64>().unwrap());
        assert_eq!(facts["byte_digest"], digest);
        assert_eq!(facts["independent_buffers"], true);
        assert_eq!(facts["bytes_verified"], true);
        assert_eq!(facts["capture_calls"], 0);
    }
}

#[cfg(feature = "deterministic")]
#[test]
fn finite_ordered_matrix() {
    for (cases, batches, outer, polls, digest) in [
        ("0", "1", 1, 0, 0),
        ("1", "1", 2, 2, 0),
        ("16", "1", 17, 152, 120),
        ("64", "1", 65, 2144, 2016),
        ("16", "16", 272, 2432, 1920),
    ] {
        let record = record("snapshot-ordered", cases, batches);
        assert_eq!(record["schema"], "rca-ordered-capture-profile-v1");
        let count = cases.parse::<u64>().unwrap() * batches.parse::<u64>().unwrap();
        let facts = &record["facts"];
        for (field, value) in [
            ("outer_polls", outer),
            ("timeout_polls", outer),
            ("case_polls", polls),
            ("factory_entries", count),
            ("factories_created", count),
            ("factories_dropped", count),
            ("futures_created", count),
            ("futures_dropped", count),
            ("values_created", count),
            ("values_dropped", count),
            ("wakes", count),
            ("finish_calls", batches.parse().unwrap()),
            ("finish_owners_dropped", batches.parse().unwrap()),
            ("byte_digest", digest),
        ] {
            assert_eq!(facts[field], value, "{cases} {batches}: {field}");
        }
        assert_eq!(facts["bytes_verified"], true);
        assert_eq!(facts["byte_len"], 8);
    }
}

#[test]
fn invalid_dimensions_and_names_emit_no_record() {
    for args in [
        vec!["snapshot-bytes"],
        vec!["snapshot-bytes", "1", "1"],
        vec!["snapshot-bytes", "8", "0"],
        vec!["snapshot-bytes", "8", "257"],
        vec!["snapshot-bytes", "+8", "1"],
        vec!["snapshot-bytes", "8", "1", "extra"],
        vec!["snapshot-ordered", "2", "1"],
        vec!["snapshot-ordered", "16", "17"],
        vec!["snapshot-ordered", "64", "0"],
        vec!["snapshot-unknown", "0", "1"],
        vec!["snapshot-bytes", "18446744073709551615", "1"],
    ] {
        let output = child(&args);
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        assert!(!output.stderr.is_empty());
    }
}

#[cfg(not(feature = "deterministic"))]
#[test]
fn disabled_names_reject_before_measurement() {
    for name in ["snapshot-bytes", "snapshot-ordered"] {
        for args in [vec![name], vec![name, "0", "1"], vec![name, "65536", "1"]] {
            let output = child(&args);
            assert!(!output.status.success());
            assert!(output.stdout.is_empty());
            assert_eq!(
                std::str::from_utf8(&output.stderr).unwrap().trim(),
                "snapshot workloads require the deterministic feature"
            );
        }
    }
}
