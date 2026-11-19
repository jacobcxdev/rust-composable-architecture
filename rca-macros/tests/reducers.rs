//! Checks reducer-list order and exact missing-dependency diagnostics.
//!
//! Nested Cargo commands remain in the build job's process group. The caller owns
//! timeout and cancellation for Cargo and its descendants. Only the diagnostic
//! oracle retains stderr, bounded to 1 MiB; stdout streams to the build job.

use std::cell::RefCell;
use std::io::{self, Read};
use std::path::Path;
use std::process::{Command, Stdio};

use rca::{Effect, ReduceContext, Reducer};
use rca_macros::reducers;

/// Supplies the shared state shape required by both reducer inputs.
struct State(Vec<&'static str>);

/// Keeps the reducer input type independent of the construction-order log.
struct Action;

/// Supplies the first distinct reducer type in the generated tuple.
struct First;

/// Supplies the second distinct reducer type in the generated tuple.
struct Second;

impl Reducer for First {
    type State = State;
    type Action = Action;

    fn reduce(
        &mut self,
        state: &mut Self::State,
        _action: &Self::Action,
        _context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        state.0.push("first");
        Effect::none()
    }
}

impl Reducer for Second {
    type State = State;
    type Action = Action;

    fn reduce(
        &mut self,
        state: &mut Self::State,
        _action: &Self::Action,
        _context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        state.0.push("second");
        Effect::none()
    }
}

#[test]
fn expands_to_a_reducer_tuple_in_source_order() {
    let calls = RefCell::new(Vec::new());
    let _: rca::Combine<_> = reducers![
        {
            calls.borrow_mut().push("first");
            First
        },
        {
            calls.borrow_mut().push("second");
            Second
        },
    ];
    assert_eq!(calls.into_inner(), ["first", "second"]);
}

#[test]
fn reducers_qualification() {
    reducers_missing_dependency_diagnostic_is_macro_neutral();
    let tests = trybuild::TestCases::new();
    tests.compile_fail("tests/ui/reducers/fail_*.rs");
}

/// Requires each macro invocation to identify the absent runtime dependency.
fn reducers_missing_dependency_diagnostic_is_macro_neutral() {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/missing-reducers-dependency/Cargo.toml");
    let target = std::path::PathBuf::from(
        std::env::var_os("CARGO_TARGET_DIR").unwrap_or_else(|| "target".into()),
    )
    .join("reducers-missing-dependency-fixture");
    let mut child = Command::new(env!("CARGO"))
        .args(["check", "--manifest-path"])
        .arg(manifest)
        .arg("--target-dir")
        .arg(target)
        .args(["--color", "never"])
        .env("CARGO_BUILD_JOBS", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::piped())
        .spawn()
        .expect("missing-dependency Cargo check starts");
    let stderr = child.stderr.take().expect("stderr was configured as piped");
    let diagnostics = read_diagnostics(stderr);
    // The reader is dropped before waiting. Reap before reporting either failure.
    let status = child.wait();
    let diagnostics = diagnostics.unwrap_or_else(|error| {
        panic!("missing-dependency diagnostic capture failed: {error}; child wait: {status:?}")
    });
    let status = status.expect("missing-dependency Cargo check exits");

    assert!(
        !status.success(),
        "missing dependency fixture unexpectedly compiled: {status}"
    );
    let stderr = String::from_utf8_lossy(&diagnostics);
    let diagnostic =
        "RCA macros require `rust-composable-architecture` or `rca-core` in Cargo dependencies";
    for invocation in [
        "#[derive(StatePaths)]",
        "#[derive(ActionPaths)]",
        "#[derive(Snapshot)]",
        "#[derive(DependencyKey)]",
        "reducers![()]",
    ] {
        let mut matching_error = false;
        assert!(
            stderr.lines().any(|line| {
                if line.starts_with("error") || line.starts_with("warning") {
                    matching_error = line.strip_prefix("error: ") == Some(diagnostic);
                }
                matching_error && line.contains(invocation)
            }),
            "{invocation} did not report the macro-neutral diagnostic:\n{stderr}"
        );
    }
}

/// Returns at most 1 MiB of diagnostics and drains excess before rejecting it.
///
/// The build-job owner supplies the process deadline; this reader does not add one.
fn read_diagnostics(mut reader: impl Read) -> io::Result<Vec<u8>> {
    const LIMIT: u64 = 1024 * 1024;
    let mut bytes = Vec::new();
    let exceeded = {
        let mut bounded = reader.by_ref().take(LIMIT + 1);
        bounded.read_to_end(&mut bytes)?;
        bounded.limit() == 0
    };
    if exceeded {
        io::copy(&mut reader, &mut io::sink())?;
        return Err(io::Error::other(
            "compiler diagnostics exceeded the 1 MiB limit",
        ));
    }
    Ok(bytes)
}

#[test]
fn diagnostic_capture_preserves_empty_small_and_exact_limit_inputs() {
    for bytes in [
        Vec::new(),
        b"error: expected diagnostic\n".to_vec(),
        vec![b'x'; 1024 * 1024],
    ] {
        assert_eq!(read_diagnostics(bytes.as_slice()).unwrap(), bytes);
    }
}

#[test]
fn diagnostic_capture_drains_and_rejects_excess_input() {
    let bytes = vec![b'x'; 1024 * 1024 + 17];
    let mut input = bytes.as_slice();
    let error = read_diagnostics(&mut input).expect_err("oversized diagnostics must fail");
    assert!(input.is_empty(), "all excess bytes must be drained");
    assert_eq!(error.kind(), io::ErrorKind::Other);
    assert_eq!(
        error.to_string(),
        "compiler diagnostics exceeded the 1 MiB limit"
    );
}
