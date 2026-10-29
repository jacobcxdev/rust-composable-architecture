//! Checks the executable's exit status, stored bytes and restart behaviour.

use rca_example_persistent_counter::codec::CounterCodec;
use rca_persistence::Codec;
use std::path::Path;
use std::process::{Command, Output, Stdio};

/// Captures child diagnostics and attempts cleanup after polling failure or timeout.
fn output(command: &mut Command) -> Output {
    output_with_stdout(command, Stdio::piped())
}

/// Keeps the watchdog active when a test supplies a failing output destination.
fn output_with_stdout(command: &mut Command, stdout: Stdio) -> Output {
    let mut child = command
        .stdout(stdout)
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return child.wait_with_output().unwrap(),
            Ok(None) => {}
            Err(error) => {
                // Dropping Child does not terminate it. Cleanup remains best effort.
                let termination = child.kill();
                let result = child.wait_with_output();
                panic!(
                    "CLI polling failed: {error}; termination: {termination:?}; output: {result:?}"
                );
            }
        }
        if std::time::Instant::now() >= deadline {
            let termination = child.kill();
            let result = child.wait_with_output();
            panic!("CLI watchdog expired; termination: {termination:?}; output: {result:?}");
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}

/// Runs the real writer with one native path and one textual value.
fn run(path: &Path, value: &str) -> Output {
    output(
        Command::new(env!("CARGO_BIN_EXE_rca-example-persistent-counter"))
            .arg(path)
            .arg(value),
    )
}

/// Rejects crashes and success output, not only a non-zero exit status.
fn assert_error(result: &Output, message: &str) {
    assert_eq!(result.status.code(), Some(1), "{result:?}");
    assert!(result.stdout.is_empty(), "{result:?}");
    assert!(
        String::from_utf8_lossy(&result.stderr).contains(message),
        "{result:?}"
    );
}

// Reporting success before saving would leave no usable snapshot after restart.
#[test]
fn executable_restarts_from_saved_value_and_closes_successfully() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("counter-é-🦀");
    let first = run(&path, "7");
    assert!(first.status.success(), "{first:?}");
    assert_eq!(first.stdout, b"0 -> 7\n");
    assert!(first.stderr.is_empty(), "{first:?}");
    assert_eq!(
        CounterCodec.decode(&std::fs::read(&path).unwrap()).unwrap(),
        7
    );
    let second = run(&path, "12");
    assert!(second.status.success(), "{second:?}");
    assert!(second.stderr.is_empty(), "{second:?}");
    assert_eq!(String::from_utf8(second.stdout).unwrap(), "7 -> 12\n");
    assert_eq!(
        CounterCodec.decode(&std::fs::read(&path).unwrap()).unwrap(),
        12
    );
}

// An error exit must preserve malformed input, not silently reset it.
#[test]
fn executable_rejects_corrupt_storage_and_invalid_arguments() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("counter");
    std::fs::write(&path, b"RCACTR99unknown").unwrap();
    assert_error(&run(&path, "7"), "operation: Decode");
    assert_eq!(std::fs::read(&path).unwrap(), b"RCACTR99unknown");
    let absent = dir.path().join("absent");
    assert_error(&run(&absent, "-1"), "ParseIntError");
    assert!(!absent.exists());
    assert_error(
        &output(&mut Command::new(env!(
            "CARGO_BIN_EXE_rca-example-persistent-counter"
        ))),
        "usage: rca-example-persistent-counter PATH VALUE",
    );
}

// Invalid input must fail before opening or replacing an otherwise valid record.
#[test]
fn argument_boundaries_preserve_existing_storage() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("counter");
    let original = b"RCACTR01\x07\0\0\0\0\0\0\0";
    std::fs::write(&path, original).unwrap();
    for value in ["", "not a number", "18446744073709551616"] {
        assert_error(&run(&path, value), "ParseIntError");
        assert_eq!(std::fs::read(&path).unwrap(), original);
    }
    let missing =
        output(Command::new(env!("CARGO_BIN_EXE_rca-example-persistent-counter")).arg(&path));
    assert_error(&missing, "missing value");
    let extra = output(
        Command::new(env!("CARGO_BIN_EXE_rca-example-persistent-counter"))
            .arg(&path)
            .args(["8", "extra"]),
    );
    assert_error(&extra, "unexpected argument");
    assert_eq!(std::fs::read(&path).unwrap(), original);
}

// Native argument transport must not turn malformed Unicode into a numeric value.
#[cfg(any(unix, windows))]
#[test]
fn non_unicode_value_is_an_ordinary_error_without_writing() {
    #[cfg(unix)]
    let value = {
        use std::os::unix::ffi::OsStringExt as _;
        std::ffi::OsString::from_vec(vec![0xff])
    };
    #[cfg(windows)]
    let value = {
        use std::os::windows::ffi::OsStringExt as _;
        std::ffi::OsString::from_wide(&[0xd800])
    };
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("counter");
    let result = output(
        Command::new(env!("CARGO_BIN_EXE_rca-example-persistent-counter"))
            .arg(&path)
            .arg(value),
    );
    assert_error(&result, "value is not UTF-8");
    assert!(!path.exists());
}

// Native path bytes reach the filesystem without requiring Unicode conversion.
#[cfg(unix)]
#[test]
fn non_unicode_path_obeys_the_filesystem_boundary() {
    use std::io::Write as _;
    use std::os::unix::ffi::OsStringExt as _;

    let dir = tempfile::tempdir().unwrap();
    let path = dir
        .path()
        .join(std::ffi::OsString::from_vec(b"counter-\xff".to_vec()));
    let control = dir
        .path()
        .join(std::ffi::OsString::from_vec(b"control-\xff".to_vec()));
    let record = b"RCACTR01\0\0\0\0\0\0\0\0";
    let direct = std::fs::write(&control, record);
    let mut temporary = tempfile::NamedTempFile::new_in(dir.path()).unwrap();
    temporary.write_all(record).unwrap();
    let replacement = temporary.persist(&control);
    #[cfg(target_os = "macos")]
    {
        // Darwin's EILSEQ identifies rejected filename bytes, not arbitrary I/O failure.
        const ILLEGAL_BYTE_SEQUENCE: i32 = 92;
        if matches!(
            (&direct, &replacement),
            (Err(write), Err(replace))
                if write.raw_os_error() == Some(ILLEGAL_BYTE_SEQUENCE)
                    && replace.error.raw_os_error() == Some(ILLEGAL_BYTE_SEQUENCE)
        ) {
            drop(replacement);
            assert_error(&run(&path, "7"), "operation: Save");
            assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
            return;
        }
    }
    direct.expect("filesystem accepts direct native filename creation");
    replacement.expect("filesystem accepts native filename replacement");
    let first = run(&path, "0");
    assert!(first.status.success(), "{first:?}");
    assert_eq!(
        CounterCodec.decode(&std::fs::read(&path).unwrap()).unwrap(),
        0
    );
    let second = run(&path, "18446744073709551615");
    assert!(second.status.success(), "{second:?}");
    assert_eq!(second.stdout, b"0 -> 18446744073709551615\n");
    assert_eq!(
        CounterCodec.decode(&std::fs::read(&path).unwrap()).unwrap(),
        u64::MAX
    );
}

// Reporting failure must not panic or imply that an already committed save rolled back.
#[test]
fn stdout_failure_is_an_ordinary_error_after_the_save() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("counter");
    let (reader, writer) = std::io::pipe().unwrap();
    drop(reader);
    let result = output_with_stdout(
        Command::new(env!("CARGO_BIN_EXE_rca-example-persistent-counter"))
            .arg(&path)
            .arg("7"),
        Stdio::from(writer),
    );
    assert_error(&result, "Error:");
    assert_eq!(
        CounterCodec.decode(&std::fs::read(&path).unwrap()).unwrap(),
        7
    );
}

/// Checks existing and absent targets without normalising away directory syntax.
fn rejects_directory_spelling(suffix: &str) {
    for existing in [true, false] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("counter");
        let original = b"RCACTR01\x07\0\0\0\0\0\0\0";
        if existing {
            std::fs::write(&path, original).unwrap();
        }
        let mut spelling = path.as_os_str().to_os_string();
        spelling.push(suffix);
        let result = run(Path::new(&spelling), "8");
        assert_error(&result, "target spells a directory");
        if existing {
            assert_eq!(std::fs::read(&path).unwrap(), original);
        } else {
            assert!(!path.exists());
        }
    }
}

// Normalising away a separator can overwrite a file through a directory-only path.
#[test]
fn trailing_separator_never_creates_or_overwrites_a_file() {
    rejects_directory_spelling("/");
    #[cfg(windows)]
    rejects_directory_spelling("\\");
}

// Normalising terminal dot has the same unintended-write effect as a trailing slash.
#[test]
fn terminal_dot_never_creates_or_overwrites_a_file() {
    rejects_directory_spelling("/.");
    #[cfg(windows)]
    rejects_directory_spelling("\\.");
}
