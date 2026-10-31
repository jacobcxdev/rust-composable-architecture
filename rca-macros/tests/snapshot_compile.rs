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
    for (name, supports_extra) in [
        ("snapshot-direct-core", true),
        ("snapshot-facade", true),
        ("snapshot-renamed-core", true),
        ("snapshot-renamed-facade", true),
        ("snapshot-keyword-core", true),
        ("snapshot-keyword-facade", true),
        ("snapshot-crate-self", true),
        ("snapshot-manual", false),
        ("snapshot-hosted", false),
        ("snapshot-diagnostics", false),
    ] {
        for extra in [false, true] {
            if extra && !supports_extra {
                continue;
            }

            let manifest = manifest_root.join(format!("tests/fixtures/{name}/Cargo.toml"));
            let mut command = std::process::Command::new(env!("CARGO"));
            command
                .args(["test", "--lib", "--manifest-path"])
                .arg(manifest)
                .arg("--target-dir")
                .arg(&target)
                .env("CARGO_BUILD_JOBS", "1");
            if extra {
                command.args(["--features", "extra"]);
            }
            let status = command
                .stdin(std::process::Stdio::null())
                .status()
                .unwrap_or_else(|error| {
                    panic!("{name}, extra={extra}: Cargo test could not complete: {error}")
                });
            assert!(status.success(), "{name}, extra={extra} failed: {status}");
        }
    }
}
