//! Checks snapshot diagnostics and executes consumers across dependency boundaries.
//!
//! Nested Cargo commands inherit the build job's output streams and process group.
//! The caller owns timeout and cancellation for Cargo and its descendants.

#[test]
fn owned_snapshot_compile_matrix() {
    let tests = trybuild::TestCases::new();
    tests.pass("tests/ui/snapshot/pass_*.rs");
    tests.compile_fail("tests/ui/snapshot/fail_*.rs");
}

#[test]
fn snapshot_dependencies_execute_across_features() {
    let manifest_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let target = std::env::var_os("CARGO_TARGET_DIR")
        .map_or_else(|| manifest_root.join("../target"), std::path::PathBuf::from)
        .join("snapshot-fixture");
    let extra: &[&str] = &["", "extra"];
    for (name, configurations) in [
        ("snapshot-direct-core", extra),
        ("snapshot-facade", extra),
        ("snapshot-renamed-core", extra),
        ("snapshot-renamed-facade", extra),
        ("snapshot-keyword-core", extra),
        ("snapshot-keyword-facade", extra),
        ("snapshot-crate-self", extra),
        ("snapshot-manual", &[""]),
        ("snapshot-hosted", &[""]),
        ("snapshot-diagnostics", &[""]),
        ("snapshot-mixed-roots", &["short", "core", "short,core"]),
    ] {
        for &features in configurations {
            let manifest = manifest_root.join(format!("tests/fixtures/{name}/Cargo.toml"));
            let mut command = std::process::Command::new(env!("CARGO"));
            command
                .args(["test", "--lib", "--manifest-path"])
                .arg(manifest)
                .arg("--target-dir")
                .arg(&target)
                .env("CARGO_BUILD_JOBS", "1");
            if !features.is_empty() {
                command.args(["--features", features]);
            }
            let status = command
                .stdin(std::process::Stdio::null())
                .status()
                .unwrap_or_else(|error| {
                    panic!("{name}, features={features:?}: Cargo test could not complete: {error}")
                });
            assert!(
                status.success(),
                "{name}, features={features:?} failed: {status}"
            );
        }
    }
}
