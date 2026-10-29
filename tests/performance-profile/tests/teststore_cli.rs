use std::process::{Command, Output};
use std::time::Duration;

mod support;

/// Bounds parent diagnostics without adding files or work to the child's profiler.
fn capture(command: &mut Command) -> Output {
    support::output(command, Duration::from_secs(30))
}

fn child(args: &[&str]) -> Output {
    capture(Command::new(env!("CARGO_BIN_EXE_rca-performance-profile")).args(args))
}

#[test]
fn capture_completes_output_larger_than_a_pipe_buffer() {
    const CHILD: &str = "RCA_TESTSTORE_CAPTURE_CHILD";
    if std::env::var_os(CHILD).is_some() {
        println!("{}", "x".repeat(65536));
        eprintln!("{}", "y".repeat(65536));
        return;
    }
    let output = capture(
        Command::new(std::env::current_exe().unwrap())
            .args([
                "capture_completes_output_larger_than_a_pipe_buffer",
                "--exact",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(CHILD, "1"),
    );
    assert!(output.status.success());
    assert!(
        output
            .stdout
            .windows(65536)
            .any(|bytes| bytes.iter().all(|&byte| byte == b'x'))
    );
    let stderr = output.stderr.strip_suffix(b"\n").unwrap();
    assert_eq!(stderr.len(), 65536);
    assert!(stderr.iter().all(|&byte| byte == b'y'));
}

#[test]
fn capture_rejects_excess_output() {
    const CHILD: &str = "RCA_TESTSTORE_EXCESS_OUTPUT_CHILD";
    if std::env::var_os(CHILD).is_some() {
        use std::io::Write;
        let block = [b'x'; 8192];
        for _ in 0..129 {
            std::io::stdout().write_all(&block).unwrap();
        }
        return;
    }
    let result = std::panic::catch_unwind(|| {
        capture(
            Command::new(std::env::current_exe().unwrap())
                .args([
                    "capture_rejects_excess_output",
                    "--exact",
                    "--nocapture",
                    "--test-threads=1",
                ])
                .env(CHILD, "1"),
        )
    });
    let panic = result.expect_err("excess TestStore output must fail");
    let message = panic
        .downcast_ref::<String>()
        .expect("formatted diagnostic");
    assert_eq!(
        message,
        "CLI child capture failed: stdout capture failed: child output exceeded the 1 MiB limit"
    );
}

#[cfg(feature = "deterministic")]
#[test]
fn finite_owned_and_idle_matrix() {
    for (name, size, iterations, pairs) in [
        ("teststore-owned", "0", "1", 1),
        ("teststore-owned", "8", "1", 1),
        ("teststore-owned", "8", "16", 16),
        ("teststore-owned", "8", "256", 256),
        ("teststore-owned", "4096", "16", 16),
        ("teststore-owned", "65536", "16", 16),
        ("teststore-idle", "0", "1", 0),
        ("teststore-idle", "8", "1", 0),
        ("teststore-idle", "65536", "1", 0),
    ] {
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
        assert_eq!(record["schema"], "rca-teststore-performance-profile-v1");
        assert_eq!(record["workload"], name);
        assert_eq!(record["drive_contract"], "teststore-operation-ready-v1");
        for (field, value) in [
            ("roots", pairs),
            ("responses", pairs),
            ("last_sequence", pairs),
            ("sequence_checksum", pairs * (pairs + 1) / 2),
            ("reductions", 2 * pairs),
            ("matched_receives", pairs),
            ("expected_updates", 2 * pairs),
            ("boundaries", 2 * pairs),
            ("task_finishes", 2 * pairs),
            ("state_clones", 4 * pairs + 1),
            ("state_equalities", 2 * pairs),
            ("capture_calls", 4 * pairs + 2),
            ("revision_clones", 4 * pairs + 1),
            ("revision_equalities", 4 * pairs + 1),
            ("declared_sets", 2 * pairs),
            ("actions_created", 2 * pairs),
            ("actions_dropped", 2 * pairs),
            ("bytes_created", 6 * pairs + 2),
            ("bytes_dropped", 6 * pairs + 2),
            ("revisions_created", 10 * pairs + 3),
            ("revisions_dropped", 10 * pairs + 3),
            ("renderer_calls", 0),
        ] {
            assert_eq!(
                record["facts"][field], value,
                "{name} {size} {iterations}: {field}"
            );
        }
        assert_eq!(record["owners_released"], true);
        let length = size.parse::<u64>().unwrap();
        let byte_checksum: u64 = (0..length).map(|index| (pairs + index) % 251).sum();
        assert_eq!(record["facts"]["byte_len"], length);
        assert_eq!(record["facts"]["byte_checksum"], byte_checksum);
        assert_eq!(record["facts"]["bytes_verified"], true);
        assert_eq!(record["features"]["deterministic"], true);
        assert_eq!(
            record["features"]["allocations"],
            cfg!(feature = "allocations")
        );
        #[cfg(feature = "allocations")]
        {
            assert_eq!(
                record["allocations"]["executor_thread_local"]["source"],
                "none"
            );
            assert_eq!(
                record["allocations"]["executor_thread_local"]["retained_blocks"],
                0
            );
            assert_eq!(
                record["allocations"]["executor_thread_local"]["retained_bytes"],
                0
            );
        }
        #[cfg(not(feature = "allocations"))]
        assert!(record["allocations"].is_null());
    }
}

#[test]
#[cfg(feature = "deterministic")]
fn finite_mismatch_matrix_retains_failure_then_disposes_all_owners() {
    for size in ["8", "4096", "65536"] {
        let output = child(&["teststore-mismatch", size, "1"]);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        #[cfg(not(feature = "allocations"))]
        assert!(output.stderr.is_empty());
        #[cfg(feature = "allocations")]
        {
            let stderr = std::str::from_utf8(&output.stderr).unwrap();
            let lines: Vec<_> = stderr.lines().collect();
            assert_eq!(lines.len(), 4, "unexpected diagnostic: {stderr}");
            assert!(lines[0].starts_with("dhat: Total: "));
            assert!(lines[1].starts_with("dhat: At t-gmax: "));
            assert_eq!(lines[2], "dhat: At t-end:  0 bytes in 0 blocks");
            assert_eq!(
                lines[3],
                "dhat: The data has been saved to the memory buffer"
            );
        }
        assert_eq!(
            std::str::from_utf8(&output.stdout).unwrap().lines().count(),
            1
        );
        let record: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(record["schema"], "rca-teststore-failure-profile-v1");
        assert_eq!(record["outcome"], "expected-state-mismatch");
        assert_eq!(record["phase_semantics"]["work"], "failure-return");
        for (field, value) in [
            ("roots", 0),
            ("responses", 1),
            ("last_sequence", 1),
            ("sequence_checksum", 0),
            ("reductions", 1),
            ("matched_receives", 0),
            ("expected_updates", 1),
            ("boundaries", 0),
            ("task_finishes", 0),
            ("state_clones", 3),
            ("state_equalities", 1),
            ("capture_calls", 3),
            ("revision_clones", 2),
            ("revision_equalities", 2),
            ("declared_sets", 1),
            ("actions_created", 1),
            ("actions_dropped", 1),
            ("bytes_created", 6),
            ("bytes_dropped", 6),
            ("revisions_created", 6),
            ("revisions_dropped", 6),
            ("renderer_calls", 2),
        ] {
            assert_eq!(record["facts"][field], value, "{size}: {field}");
        }
        for field in [
            "panic_retained_at_failure",
            "store_retained_at_failure",
            "sticky_incomplete",
            "expected_drop_unwind",
            "owners_released",
        ] {
            assert_eq!(record[field], true, "{size}: {field}");
        }
        assert_eq!(record["failure_live"]["bytes"], 3);
        assert_eq!(record["failure_live"]["revisions"], 1);
        assert_eq!(record["effect_responses"], 0);
        assert_eq!(record["commits"], 1);
        let length = size.parse::<u64>().unwrap();
        assert_eq!(record["facts"]["byte_len"], length);
        let checksum: u64 = (0..length).map(|j| (1 + j) % 251).sum();
        assert_eq!(record["facts"]["byte_checksum"], checksum);
        assert_eq!(record["facts"]["bytes_verified"], true);
        assert_eq!(record["features"]["deterministic"], true);
        assert_eq!(
            record["features"]["allocations"],
            cfg!(feature = "allocations")
        );
        #[cfg(feature = "allocations")]
        {
            let allocations = &record["allocations"];
            assert!(allocations["phases"]["work"].is_object());
            assert!(allocations["phases"]["failure"].is_null());
            assert_eq!(allocations["snapshots"].as_array().unwrap().len(), 5);
            assert!(allocations["snapshots"][2]["live_bytes"].as_u64().unwrap() > 0);
            assert_eq!(allocations["snapshots"][4]["live_blocks"], 0);
            assert_eq!(allocations["snapshots"][4]["live_bytes"], 0);
            assert_eq!(allocations["executor_thread_local"]["source"], "none");
        }
        #[cfg(not(feature = "allocations"))]
        assert!(record["allocations"].is_null());
    }
}

#[test]
fn invalid_names_and_dimensions_emit_no_success() {
    for args in [
        vec!["teststore-owned"],
        vec!["teststore-owned", "1", "1"],
        vec!["teststore-owned", "8", "0"],
        vec!["teststore-owned", "8", "257"],
        vec!["teststore-owned", "+8", "1"],
        vec!["teststore-owned", "8", "1", "extra"],
        vec!["teststore-idle", "8", "2"],
        vec!["teststore-mismatch", "0", "1"],
        vec!["teststore-mismatch", "8", "2"],
        vec!["teststore-unknown", "8", "1"],
        vec!["teststore-owned", "18446744073709551615", "1"],
    ] {
        let output = child(&args);
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        assert!(!output.stderr.is_empty());
        #[cfg(not(feature = "deterministic"))]
        assert_eq!(
            std::str::from_utf8(&output.stderr).unwrap().trim(),
            "teststore workloads require the deterministic feature"
        );
    }
}

#[cfg(not(feature = "deterministic"))]
#[test]
fn disabled_names_reject_before_dimension_validation_or_measurement() {
    for name in ["teststore-owned", "teststore-idle", "teststore-mismatch"] {
        for args in [vec![name], vec![name, "0", "1"], vec![name, "65536", "1"]] {
            let output = child(&args);
            assert!(!output.status.success());
            assert!(output.stdout.is_empty());
            assert_eq!(
                std::str::from_utf8(&output.stderr).unwrap().trim(),
                "teststore workloads require the deterministic feature"
            );
        }
    }
}
