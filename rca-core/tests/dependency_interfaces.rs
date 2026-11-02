//! Checks independent provider registration and an interface-only consumer.
//!
//! Nested Cargo commands use a separate target directory and the locked, offline
//! fixture graph. The surrounding test job supplies the process timeout.

use std::path::PathBuf;
use std::process::Command;

// A dependency on providers, an orphan registration design, or a required live implementation breaks this.
#[test]
#[cfg_attr(
    miri,
    ignore = "isolated consumers launch Cargo, which Miri isolation forbids"
)]
fn separate_interface_provider_and_provider_free_consumer_compile_and_run() {
    let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/modular-dependencies/Cargo.toml");
    let target = std::env::var_os("CARGO_TARGET_DIR")
        .map_or_else(
            || PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../target"),
            PathBuf::from,
        )
        .join("dependency-interfaces");
    assert!(
        target.is_absolute(),
        "dependency interface tests require an absolute CARGO_TARGET_DIR; unset it to use Cargo's default"
    );

    let tree = Command::new(env!("CARGO"))
        .args(["tree", "--locked", "--offline", "--manifest-path"])
        .arg(&fixture)
        .args(["-p", "modular-consumer", "--prefix", "none"])
        .env("RUSTC_WRAPPER", "")
        .env("RUSTC_WORKSPACE_WRAPPER", "")
        .env("CARGO_TARGET_DIR", &target)
        .output()
        .expect("fixture dependency graph must run");
    assert!(
        tree.status.success(),
        "{}",
        String::from_utf8_lossy(&tree.stderr)
    );
    let dependencies = String::from_utf8_lossy(&tree.stdout);
    assert!(dependencies.contains("modular-interface"));
    assert!(
        !dependencies.contains("modular-provider"),
        "consumer must not link providers: {dependencies}"
    );

    let tests = Command::new(env!("CARGO"))
        .args(["test", "--locked", "--offline", "--manifest-path"])
        .arg(&fixture)
        .args(["-p", "modular-consumer", "-p", "modular-provider"])
        .args(["--lib", "--", "--format=pretty", "--color=never"])
        .env("RUSTC_WRAPPER", "")
        .env("RUSTC_WORKSPACE_WRAPPER", "")
        .env("CARGO_TARGET_DIR", &target)
        .output()
        .expect("fixture tests must run");
    assert!(
        tests.status.success(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&tests.stdout),
        String::from_utf8_lossy(&tests.stderr)
    );
    let stdout = String::from_utf8_lossy(&tests.stdout);
    for required in [
        "interface_only_consumer_runs_a_real_reducer_with_owned_test_defaults",
        "independently_owned_provider_can_register_foreign_interface_defaults",
    ] {
        let passed = format!("test {required} ... ok");
        assert!(
            stdout.lines().any(|line| line == passed),
            "required fixture test did not pass: {required}\n{stdout}"
        );
    }
}
