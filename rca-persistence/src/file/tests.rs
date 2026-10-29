use std::sync::mpsc;
use std::task::Poll;
use std::time::Duration;

use super::FileStage as Stage;
use super::*;

#[global_allocator]
static ALLOCATOR: dhat::Alloc = dhat::Alloc;

#[test]
fn offload_future_storage_avoids_inner_allocation() {
    const TEST: &str = "file::tests::offload_future_storage_avoids_inner_allocation";
    const CHILD_ENV: &str = "RCA_PERSISTENCE_ALLOCATION_TEST";
    if std::env::var_os(CHILD_ENV).as_deref() != Some(std::ffi::OsStr::new(TEST)) {
        let process = Child(
            std::process::Command::new(std::env::current_exe().unwrap())
                .args([TEST, "--exact", "--nocapture", "--test-threads=1"])
                .env(CHILD_ENV, TEST)
                .env("RUST_LIB_BACKTRACE", "0")
                .env("RUST_BACKTRACE", "0")
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .unwrap(),
        );
        let output = capture_child(process);
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success() && stdout.contains("1 passed; 0 failed"),
            "allocation probe failed: {}\n{stdout}\n{}",
            output.status,
            String::from_utf8_lossy(&output.stderr),
        );
        print!("{stdout}");
        return;
    }

    measure_file_backend_allocations();
}

/// Records construction allocations and whether dropping the result releases them.
fn footprint<F>(construct: impl FnOnce() -> F) -> (usize, u64, u64, bool) {
    let before = dhat::HeapStats::get();
    let future = std::hint::black_box(construct());
    let after = dhat::HeapStats::get();
    let size = std::mem::size_of_val(&future);
    drop(future);
    let released = dhat::HeapStats::get();
    (
        size,
        after.total_blocks - before.total_blocks,
        after.total_bytes - before.total_bytes,
        released.curr_blocks == before.curr_blocks && released.curr_bytes == before.curr_bytes,
    )
}

/// Measures construction, admission and cleanup in the isolated single-test process.
fn measure_file_backend_allocations() {
    fn public_contract<F: Future + Send + Unpin + 'static>(future: F) -> F {
        future
    }

    let host = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    futures_executor::block_on(std::future::ready(()));
    let directory = tempfile::tempdir().unwrap();
    let client = FileBackend::new(
        directory.path().join("probe"),
        FileOptions {
            maximum_bytes: 1024,
            sync: SyncMode::File,
        },
        host.handle().clone(),
    )
    .unwrap();
    let _profiler = dhat::Profiler::builder().testing().build();
    let initial = dhat::HeapStats::get();
    let clone = footprint(|| client.clone());
    let load = footprint(|| public_contract(client.load()));
    let save = footprint(|| public_contract(client.save(Vec::new())));
    let temporary = footprint(|| tempfile::NamedTempFile::new_in(directory.path()).unwrap());
    let replacement = footprint(|| NativeReplacement::new(&client.session.storage).unwrap());
    // Some native mutexes allocate their owned state on first lock, not construction.
    let admission = footprint(|| drop(client.session.state.lock().unwrap()));
    // Rust reads and caches backtrace environment settings on the first capture.
    let capture = footprint(Backtrace::capture);
    host.shutdown_background();
    // Tokio's test-util feature first locks its clock during blocking-task admission.
    // Measure that host setup separately from the file operation's task allocation.
    let host_admission = footprint(|| {
        let failure = futures_executor::block_on(client.host.spawn_blocking(|| ())).unwrap_err();
        assert!(failure.is_cancelled());
        failure
    });
    let rejected_load = footprint(|| {
        let failure = futures_executor::block_on(client.load()).unwrap_err();
        assert_eq!(failure.stage, FileStage::Join);
        failure
    });
    let rejected_save = footprint(|| {
        let failure = futures_executor::block_on(client.save(Vec::new())).unwrap_err();
        assert_eq!(failure.stage, FileStage::Join);
        failure
    });
    drop(client);
    let released = dhat::HeapStats::get();
    assert_eq!(released.curr_blocks, initial.curr_blocks);
    assert_eq!(released.curr_bytes, initial.curr_bytes);
    println!(
        "clone={clone:?} load={load:?} save={save:?} temporary={temporary:?} replacement={replacement:?} admission={admission:?} capture={capture:?} host_admission={host_admission:?} rejected_load={rejected_load:?} rejected_save={rejected_save:?} load_result_size={} save_result_size={}",
        std::mem::size_of::<Result<Option<Vec<u8>>, FileError>>(),
        std::mem::size_of::<Result<(), FileError>>(),
    );
    assert_eq!(
        clone.1, 0,
        "cloning retains the existing session without allocating"
    );
    assert_eq!(
        replacement.1, temporary.1,
        "replacement borrows the target without another allocation"
    );
    assert_eq!(load.0, 2 * std::mem::size_of::<usize>());
    assert_eq!(save.0, 2 * std::mem::size_of::<usize>());
    assert_eq!(load.1, 1, "only the owned outer future allocates");
    assert_eq!(
        save.1, 1,
        "empty bytes and the shared path need no allocation"
    );
    assert_eq!(
        rejected_save.1,
        save.1 + 1,
        "admission adds one Tokio task allocation, not an inner future box"
    );
    assert_eq!(rejected_load.1, load.1 + 1);
    for footprint in [
        clone,
        load,
        save,
        temporary,
        replacement,
        capture,
        rejected_load,
        rejected_save,
    ] {
        assert!(
            footprint.3,
            "the operation releases all allocations: {footprint:?}"
        );
    }

    crate::document_transitions::tests::measure_transition_allocations();
    crate::document::tests::measure_failure_allocations();
    crate::shared_key::tests::measure_error_allocations();
    crate::shared_document::tests::measure_session_allocations();
}

// Removing the one-byte probe bound would drain attacker-controlled input.
#[test]
fn reads_stop_after_one_byte_beyond_the_limit() {
    struct Counted<'a>(&'a mut usize);
    impl Read for Counted<'_> {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            *self.0 += buf.len();
            buf.fill(1);
            Ok(buf.len())
        }
    }
    let mut count = 0;
    let error = read_bounded(Counted(&mut count), 4).unwrap_err();
    assert_eq!(count, 5);
    assert_eq!(error.stage, Stage::Load);
    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
}

// A read error must not turn a partial snapshot into a successful load or lose its cause.
#[test]
fn bounded_reads_preserve_io_errors_before_and_after_partial_bytes() {
    struct FailingRead<'a> {
        prefix: &'a [u8],
        calls: &'a mut usize,
        error: Option<io::Error>,
    }
    impl Read for FailingRead<'_> {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            *self.calls += 1;
            if self.prefix.is_empty() {
                Err(self
                    .error
                    .take()
                    .expect("read must stop at the first error"))
            } else {
                self.prefix.read(buf)
            }
        }
    }

    for (prefix, expected_calls) in [(b"".as_slice(), 1), (b"ab".as_slice(), 2)] {
        let mut calls = 0;
        let reader = FailingRead {
            prefix,
            calls: &mut calls,
            error: Some(io::Error::new(io::ErrorKind::PermissionDenied, fmt::Error)),
        };
        let failure = read_bounded(reader, 4).unwrap_err();
        assert_eq!(calls, expected_calls);
        assert_eq!(failure.stage, FileStage::Load);
        assert_eq!(failure.kind(), io::ErrorKind::PermissionDenied);
        let source = failure
            .source()
            .unwrap()
            .downcast_ref::<io::Error>()
            .unwrap();
        assert_eq!(source.kind(), io::ErrorKind::PermissionDenied);
        assert!(source.get_ref().unwrap().is::<fmt::Error>());
    }
}

// Truncating the target or decoding an absent file as zero breaks this contract.
#[test]
fn snapshots_round_trip_and_replace_complete_values() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("counter");
    let storage = CounterStorage::new(&path, SyncMode::File).unwrap();
    assert_eq!(storage.load().unwrap(), None);
    storage.save(7).unwrap();
    assert_eq!(fs::read(&path).unwrap(), b"RCACTR01\x07\0\0\0\0\0\0\0");
    storage.save(u64::MAX).unwrap();
    let reopened = CounterStorage::new(&path, SyncMode::File).unwrap();
    assert_eq!(reopened.load().unwrap(), Some(u64::MAX));
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
}

// The private counter codec rejects malformed records; the byte adapter does not decode them.
#[test]
fn counter_fixture_rejects_malformed_bytes_without_mutation() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("counter");
    let storage = CounterStorage::new(&path, SyncMode::File).unwrap();
    for bytes in [
        b"".as_slice(),
        b"RCACTR01\x07",
        b"RCACTR02\x07\0\0\0\0\0\0\0",
        b"RCACTR01\x07\0\0\0\0\0\0\0extra",
    ] {
        fs::write(&path, bytes).unwrap();
        let error = storage.load().unwrap_err();
        assert_eq!(error.stage, Stage::Load);
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert_eq!(fs::read(&path).unwrap(), bytes);
    }
}

// Following a symlink or opening a directory would violate the regular-file boundary.
#[test]
fn non_file_targets_are_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("counter");
    fs::create_dir(&path).unwrap();
    let storage = CounterStorage::new(&path, SyncMode::File).unwrap();
    for (error, stage) in [
        (storage.load().unwrap_err(), Stage::Load),
        (storage.save(8).unwrap_err(), Stage::Prepare),
    ] {
        assert_eq!(error.stage, stage);
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    }
    assert!(path.is_dir());
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);

    #[cfg(unix)]
    {
        let regular = dir.path().join("regular");
        let missing = dir.path().join("missing");
        fs::write(&regular, counter_bytes(7)).unwrap();

        for (name, referent) in [
            ("directory-link", &path),
            ("file-link", &regular),
            ("dangling-link", &missing),
        ] {
            let link = dir.path().join(name);
            std::os::unix::fs::symlink(referent, &link).unwrap();
            let storage = CounterStorage::new(&link, SyncMode::File).unwrap();
            for (error, stage) in [
                (storage.load().unwrap_err(), Stage::Load),
                (storage.save(8).unwrap_err(), Stage::Prepare),
            ] {
                assert_eq!(error.stage, stage);
                assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
            }
            assert!(fs::symlink_metadata(&link).unwrap().is_symlink());
            assert_eq!(fs::read_link(&link).unwrap(), *referent);
        }

        assert!(path.is_dir());
        assert_eq!(fs::read(&regular).unwrap(), counter_bytes(7));
        assert!(!missing.exists());
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 5);
    }
}

/// Fails a chosen stage while keeping real file writes and replacement cleanup.
struct Fault<'a> {
    native: NativeReplacement<'a>,
    stage: Option<Stage>,
    zero: bool,
}

impl Write for Fault<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.stage == Some(Stage::Write) {
            return Err(io::Error::other("injected write failure"));
        }
        if self.zero {
            return Ok(0);
        }
        // Real short writes exercise write_all rather than replacing its behaviour.
        self.native.write(&buf[..buf.len().min(3)])
    }

    fn flush(&mut self) -> io::Result<()> {
        self.native.flush()
    }
}

impl Replacement for Fault<'_> {
    fn sync_file(&mut self) -> io::Result<()> {
        if self.stage == Some(Stage::SyncFile) {
            return Err(io::Error::other("injected file sync failure"));
        }
        self.native.sync_file()
    }

    fn replace(self) -> io::Result<impl SyncDirectory> {
        if self.stage == Some(Stage::Replace) {
            return Err(io::Error::other("injected replacement failure"));
        }
        Ok(FaultDirectory {
            native: self.native.replace()?,
            fail: self.stage == Some(Stage::SyncDirectory),
        })
    }
}

/// Preserves directory-stage fault injection after the native replacement is consumed.
struct FaultDirectory<Directory> {
    native: Directory,
    fail: bool,
}

impl<Directory: SyncDirectory> SyncDirectory for FaultDirectory<Directory> {
    fn sync_directory(self) -> io::Result<()> {
        if self.fail {
            return Err(io::Error::other("injected directory sync failure"));
        }
        self.native.sync_directory()
    }
}

// Ignoring a write/sync error would acknowledge an incomplete or unsynced snapshot.
#[test]
fn failures_keep_commit_boundary_and_clean_owned_temporary_files() {
    for stage in [
        Stage::Write,
        Stage::SyncFile,
        Stage::Replace,
        Stage::SyncDirectory,
    ] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("counter");
        let storage = CounterStorage::new(&path, SyncMode::File).unwrap();
        storage.save(7).unwrap();
        let unrelated = dir.path().join("unrelated");
        fs::write(&unrelated, b"keep").unwrap();
        let target = Fault {
            native: NativeReplacement::new(&storage.0).unwrap(),
            stage: Some(stage),
            zero: false,
        };
        let failure = replace(b"RCACTR01\x08\0\0\0\0\0\0\0", target).unwrap_err();
        assert_eq!(failure.stage, stage);
        assert_eq!(failure.kind(), io::ErrorKind::Other);
        let source = std::error::Error::source(&failure)
            .unwrap()
            .downcast_ref::<io::Error>()
            .unwrap();
        assert!(source.to_string().starts_with("injected"));
        assert_eq!(
            storage.load().unwrap(),
            Some(if stage == Stage::SyncDirectory { 8 } else { 7 })
        );
        assert_eq!(fs::read(&unrelated).unwrap(), b"keep");
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 2);
    }
}

// Replacing write_all with one write loses all bytes after the first short write.
#[test]
fn short_writes_complete_and_zero_writes_fail_before_replacement() {
    for zero in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let storage = CounterStorage::new(dir.path().join("counter"), SyncMode::File).unwrap();
        storage.save(7).unwrap();
        let target = Fault {
            native: NativeReplacement::new(&storage.0).unwrap(),
            stage: None,
            zero,
        };
        let result = replace(b"RCACTR01\x08\0\0\0\0\0\0\0", target);
        if zero {
            let error = result.unwrap_err();
            assert_eq!(error.stage, Stage::Write);
            assert_eq!(error.kind(), io::ErrorKind::WriteZero);
            assert_eq!(storage.load().unwrap(), Some(7));
        } else {
            result.unwrap();
            assert_eq!(storage.load().unwrap(), Some(8));
        }
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    }
}

// This private writer operation must not commit or consume the pending replacement.
#[test]
fn native_flush_preserves_uncommitted_replacement_for_further_writes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("counter");
    let storage = CounterStorage::new(&path, SyncMode::File).unwrap();
    storage.save(7).unwrap();
    let mut target = NativeReplacement::new(&storage.0).unwrap();
    target.write_all(b"RCACTR01").unwrap();
    for _ in 0..2 {
        target.flush().unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"RCACTR01\x07\0\0\0\0\0\0\0");
    }
    target.write_all(b"\x08\0\0\0\0\0\0\0").unwrap();
    target.sync_file().unwrap();
    target.replace().unwrap().sync_directory().unwrap();

    assert_eq!(fs::read(&path).unwrap(), b"RCACTR01\x08\0\0\0\0\0\0\0");
    assert_eq!(storage.load().unwrap(), Some(8));
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
}

// Native persist failure must release the owned temporary file, not the target.
#[test]
fn native_replacement_failure_keeps_target_directory() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("counter");
    let storage = CounterStorage::new(&path, SyncMode::File).unwrap();
    let target = NativeReplacement::new(&storage.0).unwrap();
    fs::create_dir(&path).unwrap();
    let failure = replace(b"RCACTR01\x08\0\0\0\0\0\0\0", target).unwrap_err();
    assert_eq!(failure.stage, Stage::Replace);
    assert!(path.is_dir());
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
}

#[cfg(unix)]
#[test]
fn directory_synced_save_round_trips_on_native_host() {
    let dir = tempfile::tempdir().unwrap();
    let storage =
        CounterStorage::new(dir.path().join("counter"), SyncMode::FileAndDirectory).unwrap();
    storage.save(12).unwrap();
    assert_eq!(storage.load().unwrap(), Some(12));
}

/// Stops and reaps an owned subprocess, including when an assertion unwinds.
struct Child(std::process::Child);

impl Drop for Child {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Runs the file worker in a fresh process without changing this test process's directory.
fn child(mode: &str, dir: &Path) -> Child {
    let mut command = std::process::Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", "file::tests::subprocess_worker", "--nocapture"])
        .env("RCA_FILE_TEST_MODE", mode)
        .env("RCA_FILE_TEST_DIR", dir)
        .stdout(std::process::Stdio::null());
    let capture = match mode {
        "backtrace-disabled" => Some("0"),
        "backtrace-enabled" => Some("1"),
        _ => None,
    };
    if let Some(capture) = capture {
        command
            .env("RUST_LIB_BACKTRACE", capture)
            .env("RUST_BACKTRACE", "0");
    }
    Child(command.spawn().unwrap())
}

/// Bounds subprocess completion; the owner kills and reaps it if the watchdog expires.
fn wait(child: &mut Child) -> std::process::ExitStatus {
    wait_result(child).unwrap_or_else(|failure| panic!("{failure}"))
}

/// Keeps timeout diagnostics available before the caller propagates failure.
fn wait_result(child: &mut Child) -> Result<std::process::ExitStatus, String> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    loop {
        let failure = match child.0.try_wait() {
            Ok(Some(status)) => return Ok(status),
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(5));
                continue;
            }
            Ok(None) => "child watchdog expired".to_owned(),
            Err(error) => format!("child status failed: {error}"),
        };
        let killed = child.0.kill();
        let reaped = child.0.wait();
        return Err(format!("{failure}; kill={killed:?}; reap={reaped:?}"));
    }
}

/// Drains both pipes while the child runs, including before a watchdog failure.
///
/// This package-local test helper follows the core allocation runner's pattern.
/// Its children must not pass pipe writers to surviving descendant processes.
fn capture_child(process: Child) -> std::process::Output {
    std::thread::scope(move |scope| {
        // This local owner drops before scoped readers join if thread creation panics.
        let mut process = process;
        let stdout = process.0.stdout.take().unwrap();
        let stderr = process.0.stderr.take().unwrap();
        let stdout = scope.spawn(move || read_child_output(stdout));
        let stderr = scope.spawn(move || read_child_output(stderr));
        let status = wait_result(&mut process);

        // Join both before propagating either reader result.
        let stdout = stdout.join();
        let stderr = stderr.join();
        let stdout = stdout.unwrap().unwrap();
        let stderr = stderr.unwrap().unwrap();
        let status = status.unwrap_or_else(|failure| {
            panic!(
                "{failure}\n{}\n{}",
                String::from_utf8_lossy(&stdout),
                String::from_utf8_lossy(&stderr),
            )
        });
        std::process::Output {
            status,
            stdout,
            stderr,
        }
    })
}

/// Bounds retained diagnostics without blocking a writer that exceeds the limit.
fn read_child_output(mut pipe: impl Read) -> io::Result<Vec<u8>> {
    // Match the core runner's per-stream diagnostic limit, outside allocation windows.
    const OUTPUT_LIMIT: u64 = 1024 * 1024;
    let mut bytes = Vec::new();
    pipe.by_ref()
        .take(OUTPUT_LIMIT + 1)
        .read_to_end(&mut bytes)?;
    if u64::try_from(bytes.len()).unwrap() > OUTPUT_LIMIT {
        io::copy(&mut pipe, &mut io::sink())?;
        return Err(io::Error::other("child output exceeded the 1 MiB limit"));
    }
    Ok(bytes)
}

#[test]
fn child_capture_drains_both_streams_before_waiting_for_exit() {
    let process = Child(
        std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "file::tests::subprocess_worker", "--nocapture"])
            .env("RCA_FILE_TEST_MODE", "large-output")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let output = capture_child(process);
    assert!(output.status.success());
    assert!(
        output
            .stdout
            .split(|&byte| byte != b'x')
            .any(|run| run.len() == 512 * 1024)
    );
    assert_eq!(output.stderr, vec![b'y'; 512 * 1024]);
}

// Truncating the target before replacement would destroy the old snapshot on death.
#[test]
fn killed_writers_leave_complete_old_or_new_snapshots() {
    for mode in ["before", "after"] {
        let dir = tempfile::tempdir().unwrap();
        let storage = CounterStorage::new(dir.path().join("counter"), SyncMode::File).unwrap();
        storage.save(7).unwrap();
        let mut writer = child(mode, dir.path());
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        while !dir.path().join("ready").exists() {
            assert!(
                writer.0.try_wait().unwrap().is_none(),
                "writer exited before boundary"
            );
            assert!(
                std::time::Instant::now() < deadline,
                "writer boundary watchdog expired"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        writer.0.kill().unwrap();
        assert!(!wait(&mut writer).success());
        let mut reader = child(
            if mode == "before" {
                "read-old"
            } else {
                "read-new"
            },
            dir.path(),
        );
        assert!(wait(&mut reader).success());
        // A killed pre-replace writer leaves one owned orphan. Loading never adopts it.
        assert_eq!(
            fs::read_dir(dir.path()).unwrap().count(),
            if mode == "before" { 3 } else { 2 }
        );
    }
}

// Changing cwd after construction must not redirect a relative target.
#[test]
fn relative_paths_remain_bound_to_original_directory() {
    let dir = tempfile::tempdir().unwrap();
    let mut process = child("relative", dir.path());
    assert!(wait(&mut process).success());
    assert_eq!(
        CounterStorage::new(dir.path().join("counter"), SyncMode::File)
            .unwrap()
            .load()
            .unwrap(),
        Some(9)
    );
}

// Backtrace settings are process-wide and cached, so each mode needs a fresh child.
#[test]
fn file_errors_capture_backtraces_only_when_requested() {
    let dir = tempfile::tempdir().unwrap();
    for mode in ["backtrace-disabled", "backtrace-enabled"] {
        let mut process = child(mode, dir.path());
        assert!(wait(&mut process).success(), "backtrace mode: {mode}");
    }
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
}

/// Checks construction, blocking-worker and join errors under the child's capture policy.
fn check_error_backtraces(enabled: bool, dir: &Path) {
    crate::document::tests::check_failure_backtraces(enabled);
    crate::shared_key::tests::check_error_backtraces(enabled);
    crate::shared_document::tests::check_error_backtraces(enabled);

    let host = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let invalid = FileBackend::new(
        dir.join("value"),
        FileOptions {
            maximum_bytes: 0,
            sync: SyncMode::File,
        },
        host.handle().clone(),
    )
    .unwrap_err();
    let options = FileOptions {
        maximum_bytes: 16,
        sync: SyncMode::File,
    };
    let native = FileBackend::new(dir, options, host.handle().clone()).unwrap();
    let loaded = host.block_on(native.load()).unwrap_err();
    let backend = FileBackend::new(dir.join("value"), options, host.handle().clone()).unwrap();
    host.shutdown_background();
    let joined = futures_executor::block_on(backend.load()).unwrap_err();
    assert_eq!(invalid.stage, Stage::Prepare);
    assert_eq!(loaded.stage, Stage::Load);
    assert_eq!(joined.stage, Stage::Join);

    for failure in [invalid, loaded, joined] {
        let summary = format!(
            "file {:?} failed: {}",
            failure.stage,
            failure.source().unwrap()
        );
        let displayed = failure.to_string();
        crate::document::tests::assert_display_rejects_writes(&failure, &summary, enabled);
        if enabled {
            assert_eq!(failure.backtrace.status(), BacktraceStatus::Captured);
            assert_eq!(displayed, format!("{summary}\n{}", failure.backtrace));
        } else {
            assert_eq!(failure.backtrace.status(), BacktraceStatus::Disabled);
            assert_eq!(displayed, summary);
        }
    }
}

/// Parks a worker at a known replacement boundary until its parent kills it.
struct Boundary<'a> {
    native: NativeReplacement<'a>,
    dir: PathBuf,
    before: bool,
}

impl Boundary<'_> {
    fn stop(dir: &Path) {
        fs::write(dir.join("ready"), b"ready").unwrap();
        loop {
            std::thread::park();
        }
    }
}

impl Write for Boundary<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.native.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.native.flush()
    }
}

impl Replacement for Boundary<'_> {
    fn sync_file(&mut self) -> io::Result<()> {
        self.native.sync_file()?;
        if self.before {
            Self::stop(&self.dir);
        }
        Ok(())
    }

    fn replace(self) -> io::Result<impl SyncDirectory> {
        let Self {
            native,
            dir,
            before,
        } = self;
        let directory = native.replace()?;
        if !before {
            Self::stop(&dir);
        }
        Ok(directory)
    }
}

#[test]
fn subprocess_worker() {
    let Ok(mode) = std::env::var("RCA_FILE_TEST_MODE") else {
        return;
    };
    if mode == "large-output" {
        let mut stdout = io::stdout().lock();
        let mut stderr = io::stderr().lock();
        for _ in 0..64 {
            stdout.write_all(&[b'x'; 8192]).unwrap();
            stderr.write_all(&[b'y'; 8192]).unwrap();
        }
        return;
    }
    let dir = PathBuf::from(std::env::var_os("RCA_FILE_TEST_DIR").unwrap());
    if matches!(mode.as_str(), "backtrace-disabled" | "backtrace-enabled") {
        check_error_backtraces(mode == "backtrace-enabled", &dir);
        return;
    }
    if mode == "relative" {
        std::env::set_current_dir(&dir).unwrap();
        let storage = CounterStorage::new("counter", SyncMode::File).unwrap();
        let other = tempfile::tempdir().unwrap();
        std::env::set_current_dir(other.path()).unwrap();
        storage.save(9).unwrap();
        assert!(!other.path().join("counter").exists());
        return;
    }
    let storage = CounterStorage::new(dir.join("counter"), SyncMode::File).unwrap();
    if mode.starts_with("read-") {
        assert_eq!(
            storage.load().unwrap(),
            Some(if mode == "read-old" { 7 } else { 8 })
        );
        return;
    }
    let target = Boundary {
        native: NativeReplacement::new(&storage.0).unwrap(),
        dir,
        before: mode == "before",
    };
    replace(b"RCACTR01\x08\0\0\0\0\0\0\0", target).unwrap();
}

/// Adds a private counter codec so tests can distinguish complete old and new snapshots.
#[derive(Clone)]
struct CounterStorage(FileStorage);

impl CounterStorage {
    fn new(path: impl AsRef<Path>, sync: SyncMode) -> Result<Self, FileError> {
        FileStorage::new(
            path,
            FileOptions {
                maximum_bytes: 16,
                sync,
            },
        )
        .map(Self)
    }

    fn load(&self) -> Result<Option<u64>, FileError> {
        self.0
            .load()?
            .map(|bytes| {
                if bytes.len() != 16 || &bytes[..8] != b"RCACTR01" {
                    return Err(failed(
                        Stage::Load,
                        io::Error::new(io::ErrorKind::InvalidData, "invalid counter"),
                    ));
                }
                Ok(u64::from_le_bytes(bytes[8..].try_into().unwrap()))
            })
            .transpose()
    }

    fn save(&self, value: u64) -> Result<(), FileError> {
        self.0.save(&counter_bytes(value))
    }
}

fn counter_bytes(value: u64) -> Vec<u8> {
    let mut bytes = b"RCACTR01".to_vec();
    bytes.extend_from_slice(&value.to_le_bytes());
    bytes
}

fn bind_backend(storage: &CounterStorage, host: tokio::runtime::Handle) -> FileBackend {
    FileBackend::new(&storage.0.path, storage.0.options, host).unwrap()
}

/// Uses a separate test watchdog without changing the backend's completion contract.
fn bounded<T>(host: &tokio::runtime::Runtime, future: impl Future<Output = T>) -> T {
    host.block_on(async {
        tokio::time::timeout(Duration::from_secs(10), future)
            .await
            .expect("adapter operation must complete within the test watchdog")
    })
}

// An early close would let the abandoned old write overwrite a replacement writer.
#[test]
fn close_waits_for_cancelled_write_and_fences_every_clone() {
    let dir = tempfile::tempdir().unwrap();
    let storage = CounterStorage::new(dir.path().join("counter"), SyncMode::File).unwrap();
    storage.save(7).unwrap();
    let host = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();
    let client = bind_backend(&storage, host.handle().clone());
    let sibling = client.clone();
    let dormant = sibling.save(counter_bytes(99));
    let (started, start) = mpsc::sync_channel(1);
    let (release, released) = mpsc::sync_channel(1);
    let writer = storage.clone();
    let mut operation = client.offload(move |_| {
        started.send(()).unwrap();
        released.recv_timeout(Duration::from_secs(10)).unwrap();
        writer.save(8)
    });
    host.block_on(std::future::poll_fn(|cx| {
        assert!(operation.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    }));
    start.recv_timeout(Duration::from_secs(10)).unwrap();
    drop(operation);
    let mut close = Box::pin(client.close());
    host.block_on(std::future::poll_fn(|cx| {
        assert!(
            close.as_mut().poll(cx).is_pending(),
            "close must wait for the admitted write"
        );
        Poll::Ready(())
    }));
    drop(close);
    host.block_on(async {
        assert_eq!(
            dormant.await.unwrap_err().kind(),
            std::io::ErrorKind::BrokenPipe
        );
        assert_eq!(
            sibling.load().await.unwrap_err().kind(),
            std::io::ErrorKind::BrokenPipe
        );
        assert_eq!(
            client.save(counter_bytes(99)).await.unwrap_err().kind(),
            std::io::ErrorKind::BrokenPipe
        );
    });
    assert_eq!(storage.load().unwrap(), Some(7));
    let mut first = Box::pin(client.close());
    let mut second = Box::pin(sibling.close());
    host.block_on(std::future::poll_fn(|cx| {
        assert!(first.as_mut().poll(cx).is_pending());
        assert!(second.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    }));
    release.send(()).unwrap();
    bounded(&host, async {
        first.await;
        second.await;
    });
    assert_eq!(storage.load().unwrap(), Some(8));
    let replacement = bind_backend(&storage, host.handle().clone());
    bounded(&host, async {
        replacement.save(counter_bytes(12)).await.unwrap();
        replacement.close().await;
        client.close().await;
    });
    assert_eq!(storage.load().unwrap(), Some(12));
}

// Finishing one admitted job must not close the client while another still owns I/O.
#[test]
fn close_waits_for_every_admitted_job() {
    let dir = tempfile::tempdir().unwrap();
    let storage = CounterStorage::new(dir.path().join("counter"), SyncMode::File).unwrap();
    let host = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();
    let client = bind_backend(&storage, host.handle().clone());
    let (release_first, first_gate) = mpsc::sync_channel(1);
    let (release_second, second_gate) = mpsc::sync_channel(1);
    let mut first = client.offload(move |_| {
        first_gate.recv_timeout(Duration::from_secs(10)).unwrap();
        Ok(1)
    });
    let mut second = client.offload(move |_| {
        second_gate.recv_timeout(Duration::from_secs(10)).unwrap();
        Ok(2)
    });
    host.block_on(std::future::poll_fn(|cx| {
        assert!(first.as_mut().poll(cx).is_pending());
        assert!(second.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    }));
    let mut close = Box::pin(client.close());
    host.block_on(std::future::poll_fn(|cx| {
        assert!(close.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    }));
    release_first.send(()).unwrap();
    assert_eq!(bounded(&host, first).unwrap(), 1);
    host.block_on(std::future::poll_fn(|cx| {
        assert!(
            close.as_mut().poll(cx).is_pending(),
            "second job still owns its admission"
        );
        Poll::Ready(())
    }));
    drop(second);
    release_second.send(()).unwrap();
    bounded(&host, close);
}

// Admission must happen on poll, while constructing an unused close must not fence.
#[test]
fn unpolled_close_and_operations_have_no_lifecycle_effect() {
    let dir = tempfile::tempdir().unwrap();
    let storage = CounterStorage::new(dir.path().join("counter"), SyncMode::File).unwrap();
    let host = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();
    let client = bind_backend(&storage, host.handle().clone());
    drop(client.close());
    host.block_on(client.save(counter_bytes(7))).unwrap();
    let dormant_load = client.load();
    let dormant_save = client.save(counter_bytes(99));
    bounded(&host, client.close());
    host.block_on(async {
        for failure in [
            dormant_load.await.unwrap_err(),
            dormant_save.await.unwrap_err(),
        ] {
            assert_eq!(failure.stage, Stage::Admission);
            assert_eq!(failure.kind(), io::ErrorKind::BrokenPipe);
        }
    });
    assert_eq!(storage.load().unwrap(), Some(7));
}

// Completing the final write must broadcast to independent registered waiters.
#[test]
fn completion_wakes_all_close_waiters_without_stopping_shared_host() {
    let dir = tempfile::tempdir().unwrap();
    let storage = CounterStorage::new(dir.path().join("counter"), SyncMode::File).unwrap();
    let host = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();
    let client = bind_backend(&storage, host.handle().clone());
    let (release, released) = mpsc::sync_channel(1);
    let mut operation = client.offload(move |_| {
        released.recv_timeout(Duration::from_secs(10)).unwrap();
        Ok(())
    });
    host.block_on(std::future::poll_fn(|cx| {
        assert!(operation.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    }));
    let (registered, registrations) = async_channel::bounded(2);
    let (completed, completions) = async_channel::bounded(2);
    let (resume, resumed) = async_channel::bounded(1);
    let unrelated = host.spawn(async move {
        resumed.recv().await.unwrap();
        42
    });
    for id in [1, 2] {
        let client = client.clone();
        let registered = registered.clone();
        let completed = completed.clone();
        host.spawn(async move {
            let mut close = Box::pin(client.close());
            std::future::poll_fn(|cx| {
                assert!(close.as_mut().poll(cx).is_pending());
                Poll::Ready(())
            })
            .await;
            registered.send(()).await.unwrap();
            close.await;
            completed.send(id).await.unwrap();
        });
    }
    bounded(&host, async {
        for _ in 0..2 {
            registrations.recv().await.unwrap();
        }
        drop(operation);
        release.send(()).unwrap();
        let mut ids = Vec::new();
        for _ in 0..2 {
            ids.push(
                tokio::time::timeout(Duration::from_secs(10), completions.recv())
                    .await
                    .unwrap()
                    .unwrap(),
            );
        }
        ids.sort_unstable();
        assert_eq!(ids, [1, 2]);
        resume.send(()).await.unwrap();
        assert_eq!(unrelated.await.unwrap(), 42);
    });
}

// Errors and unwinding must release ownership without turning close into a save result.
#[test]
fn failed_and_panicking_operations_still_allow_close() {
    let dir = tempfile::tempdir().unwrap();
    let storage = CounterStorage::new(dir.path().join("counter"), SyncMode::File).unwrap();
    let host = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();
    let client = bind_backend(&storage, host.handle().clone());
    host.block_on(async {
        let failure = failed(
            Stage::Write,
            io::Error::from(io::ErrorKind::PermissionDenied),
        );
        let error = client
            .offload::<()>(move |_| Err(failure))
            .await
            .unwrap_err();
        assert_eq!(error.stage, Stage::Write);
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        let panic = client
            .offload::<()>(|_| panic!("controlled operation panic"))
            .await
            .unwrap_err();
        assert_eq!(panic.stage, Stage::Join);
        assert_eq!(panic.kind(), io::ErrorKind::Other);
        assert!(
            std::error::Error::source(&panic)
                .unwrap()
                .downcast_ref::<tokio::task::JoinError>()
                .unwrap()
                .is_panic()
        );
        tokio::time::timeout(Duration::from_secs(10), client.close())
            .await
            .unwrap();
    });
}

// Losing the host must surface a join error, never a successful save acknowledgement.
#[test]
fn stopped_host_reports_join_failure_without_writing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("counter");
    let storage = CounterStorage::new(&path, SyncMode::File).unwrap();
    let host = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let client = bind_backend(&storage, host.handle().clone());
    drop(host);
    let failure = futures_executor::block_on(client.save(counter_bytes(8))).unwrap_err();
    assert_eq!(failure.stage, Stage::Join);
    assert_eq!(failure.kind(), io::ErrorKind::Interrupted);
    assert!(
        std::error::Error::source(&failure)
            .unwrap()
            .downcast_ref::<tokio::task::JoinError>()
            .unwrap()
            .is_cancelled()
    );
    assert!(!path.exists());
    // The stopped runtime drops the queued closure without entering its body.
    let waiter = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();
    bounded(&waiter, client.close());
}

// Dropping a queued operation future must not release its external ownership early.
#[test]
fn dropped_queued_operation_keeps_admission_until_external_work_finishes() {
    let dir = tempfile::tempdir().unwrap();
    let storage = CounterStorage::new(dir.path().join("counter"), SyncMode::File).unwrap();
    let host = tokio::runtime::Builder::new_current_thread()
        .max_blocking_threads(1)
        .enable_time()
        .build()
        .unwrap();
    let client = bind_backend(&storage, host.handle().clone());
    let (started, start) = mpsc::sync_channel(1);
    let (release, released) = mpsc::sync_channel(1);
    let (finished, finish) = mpsc::sync_channel(1);
    host.spawn_blocking(move || {
        started.send(()).unwrap();
        released.recv_timeout(Duration::from_secs(10)).unwrap();
        finished.send(()).unwrap();
    });
    start.recv_timeout(Duration::from_secs(10)).unwrap();
    let mut operation = Box::pin(client.save(counter_bytes(8)));
    let mut close = Box::pin(client.close());
    host.block_on(std::future::poll_fn(|cx| {
        assert!(operation.as_mut().poll(cx).is_pending());
        assert!(close.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    }));
    drop(operation);
    // Runtime shutdown is not cancellation: queued blocking work can still run.
    host.shutdown_background();
    let waiter = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();
    waiter.block_on(std::future::poll_fn(|cx| {
        assert!(
            close.as_mut().poll(cx).is_pending(),
            "queued closure still owns admission"
        );
        Poll::Ready(())
    }));
    release.send(()).unwrap();
    bounded(&waiter, close);
    finish.recv_timeout(Duration::from_secs(10)).unwrap();
    assert_eq!(storage.load().unwrap(), Some(8));
}

// Dropping the future must not be mistaken for stopping an already running write.
#[test]
fn cancelled_await_does_not_cancel_started_blocking_write() {
    let dir = tempfile::tempdir().unwrap();
    let storage = CounterStorage::new(dir.path().join("counter"), SyncMode::File).unwrap();
    storage.save(7).unwrap();
    let host = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let (started, start) = mpsc::sync_channel(1);
    let (release, released) = mpsc::sync_channel(1);
    let (finished, finish) = mpsc::sync_channel(1);
    let writer = storage.clone();
    let client = FileBackend::new(
        dir.path().join("counter"),
        FileOptions {
            maximum_bytes: 1024,
            sync: SyncMode::File,
        },
        host.handle().clone(),
    )
    .unwrap();
    let mut operation = client.offload(move |_| {
        started.send(()).unwrap();
        released.recv_timeout(Duration::from_secs(10)).unwrap();
        writer.save(8)?;
        finished.send(()).unwrap();
        Ok(())
    });
    host.block_on(std::future::poll_fn(|cx| {
        assert!(operation.as_mut().poll(cx).is_pending());
        Poll::Ready(())
    }));
    start.recv_timeout(Duration::from_secs(10)).unwrap();
    drop(operation);
    assert_eq!(storage.load().unwrap(), Some(7));
    release.send(()).unwrap();
    finish.recv_timeout(Duration::from_secs(10)).unwrap();
    drop(host);
    assert_eq!(storage.load().unwrap(), Some(8));
}
