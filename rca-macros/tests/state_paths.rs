//! Checks state-path consumers, renamed dependencies and rejection diagnostics.
//!
//! Nested Cargo commands inherit the build job's output streams and process group.
//! The caller owns timeout and cancellation for Cargo and its descendants.

use std::path::Path;
use std::process::Command;

#[test]
fn state_paths_qualification() {
    renamed_generic_state_paths_fixture_compiles();
    direct_umbrella_state_paths_fixture_compiles();
    exact_underscore_alias_state_paths_fixture_compiles();
    renamed_keyword_alias_fixture_compiles();
    crate_self_state_paths_fixture_compiles();
    state_paths_compile_matrix();
}

fn renamed_generic_state_paths_fixture_compiles() {
    fixture_compiles("renamed-state-paths");
}

fn direct_umbrella_state_paths_fixture_compiles() {
    fixture_compiles("direct-umbrella-state-paths");
}

fn exact_underscore_alias_state_paths_fixture_compiles() {
    fixture_compiles("exact-underscore-alias-state-paths");
}

fn renamed_keyword_alias_fixture_compiles() {
    fixture_compiles("renamed-keyword-alias");
}

fn crate_self_state_paths_fixture_compiles() {
    fixture_compiles("crate-self-state-paths");
}

fn fixture_compiles(name: &str) {
    let manifest =
        Path::new(env!("CARGO_MANIFEST_DIR")).join(format!("tests/fixtures/{name}/Cargo.toml"));
    let target = std::path::PathBuf::from(
        std::env::var_os("CARGO_TARGET_DIR").unwrap_or_else(|| "target".into()),
    )
    .join("state-path-fixture");
    let status = Command::new(env!("CARGO"))
        .args(["check", "--manifest-path"])
        .arg(manifest)
        .arg("--target-dir")
        .arg(target)
        .env("CARGO_BUILD_JOBS", "1")
        .stdin(std::process::Stdio::null())
        .status()
        .unwrap_or_else(|error| {
            panic!("{name} StatePaths Cargo check could not complete: {error}")
        });

    assert!(
        status.success(),
        "{name} StatePaths fixture failed: {status}"
    );
}

fn state_paths_compile_matrix() {
    let tests = trybuild::TestCases::new();
    tests.pass("tests/ui/state_paths/pass_*.rs");
    tests.compile_fail("tests/ui/state_paths/fail_*.rs");
}
