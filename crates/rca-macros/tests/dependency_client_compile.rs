//! Checks renamed consumers and dependency-client compiler contracts.
//!
//! The first two consumers run their unit tests; the all-surfaces consumer runs.
//! Trybuild then executes positive fixtures and compares negative diagnostics.
//! Nested Cargo commands inherit the build job's output streams and process group.
//! The caller owns timeout and cancellation for Cargo and its descendants.

#[test]
fn dependency_client_compile_matrix() {
    for name in [
        "renamed-dependency-clients",
        "renamed-core-dependency-clients",
        "d39-renamed-all-surfaces",
    ] {
        let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join(format!("tests/fixtures/{name}/Cargo.toml"));
        let target = std::path::PathBuf::from(
            std::env::var_os("CARGO_TARGET_DIR").unwrap_or_else(|| "target".into()),
        )
        .join("dependency-client-fixture");
        let status = std::process::Command::new(env!("CARGO"))
            .args([
                if name == "d39-renamed-all-surfaces" {
                    "run"
                } else {
                    "test"
                },
                "--manifest-path",
            ])
            .arg(manifest)
            .arg("--target-dir")
            .arg(target)
            .env("CARGO_BUILD_JOBS", "1")
            .stdin(std::process::Stdio::null())
            .status()
            .unwrap_or_else(|error| panic!("{name} Cargo consumer could not complete: {error}"));
        assert!(status.success(), "{name} failed: {status}");
    }
    let tests = trybuild::TestCases::new();
    tests.pass("tests/ui/dependency_client/pass_*.rs");
    tests.compile_fail("tests/ui/dependency_client/fail_*.rs");
}
