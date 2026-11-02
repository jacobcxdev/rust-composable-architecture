//! Unsupported implicit history and the independent manual escape hatch.

use std::path::PathBuf;
use std::process::Command;

#[test]
#[cfg_attr(miri, ignore = "trybuild launches rustc, which Miri isolation forbids")]
fn owned_snapshot_requires_explicit_capture_policy() {
    let tests = trybuild::TestCases::new();
    tests.pass("tests/ui/rc07_owned_snapshot_manual.rs");
    tests.compile_fail("tests/ui/rc07_owned_snapshot_clone.rs");
    tests.compile_fail("tests/ui/rc07_owned_snapshot_reference.rs");
    tests.compile_fail("tests/ui/rc07_owned_snapshot_handle.rs");
}

#[test]
#[cfg_attr(
    miri,
    ignore = "isolated consumers launch Cargo, which Miri isolation forbids"
)]
fn owned_snapshot_core_facade_and_hosted_bounds_compile_in_isolation() {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/rc07-owned-snapshot/Cargo.toml");
    let target = std::env::var_os("CARGO_TARGET_DIR")
        .map_or_else(
            || PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../target"),
            PathBuf::from,
        )
        .join("rc07-owned-snapshot-consumer");
    for (operation, feature, expected_error) in [
        ("run", None, None),
        ("check", Some("hosted"), None),
        (
            "check",
            Some("no-clone"),
            Some("the trait bound `hosted::Value: Clone` is not satisfied"),
        ),
        (
            "check",
            Some("no-debug"),
            Some("`hosted::Value` doesn't implement `Debug`"),
        ),
        (
            "check",
            Some("no-equality"),
            Some("can't compare `hosted::Value` with `hosted::Value`"),
        ),
        (
            "check",
            Some("no-send"),
            Some("`Rc<usize>` cannot be sent between threads safely"),
        ),
    ] {
        let mut command = Command::new(env!("CARGO"));
        command
            .args([
                operation,
                "--locked",
                "--offline",
                "--no-default-features",
                "--manifest-path",
            ])
            .arg(&manifest)
            .env("RUSTC_WRAPPER", "")
            .env("RUSTC_WORKSPACE_WRAPPER", "")
            .env("CARGO_TARGET_DIR", &target);
        if let Some(feature) = feature {
            command.args(["--features", feature]);
        }
        let output = command.output().expect("isolated consumer Cargo must run");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        println!(
            "consumer feature={feature:?} status={}\n{stdout}\n{stderr}",
            output.status
        );
        if let Some(required) = expected_error {
            assert!(
                !output.status.success(),
                "consumer unexpectedly accepted {feature:?}"
            );
            assert!(
                stderr.contains(required),
                "missing intended {required} boundary: {stderr}"
            );
            assert!(
                stderr.contains("Value"),
                "missing captured value in diagnostic: {stderr}"
            );
            assert!(
                stderr.contains("projected"),
                "failure must be at hosted consumer: {stderr}"
            );
            assert_eq!(
                stderr.matches("error[E0277]").count(),
                1,
                "expected one isolated consumer error: {stderr}"
            );
        } else {
            assert!(output.status.success(), "{stdout}\n{stderr}");
        }
    }
}
