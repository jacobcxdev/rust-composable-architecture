//! Checks finite native work, protocol errors and incompatible feature graphs.

#![cfg(feature = "task-fold")]

use std::fs::{File, OpenOptions};
use std::io::{Seek, Write};
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

mod support;

/// Keeps the measured 64-bit owner budget, without promising a stable Rust ABI.
#[cfg(target_pointer_width = "64")]
#[test]
fn task_owners_preserve_the_baseline_footprint() {
    // Fail during this small host's compilation, before expensive profile builds.
    const {
        assert!(size_of::<rca_core::Task<u64, ()>>() == 48);
        assert!(align_of::<rca_core::Task<u64, ()>>() == 8);
        assert!(size_of::<rca_core::local::Task<u64, ()>>() == 48);
        assert!(align_of::<rca_core::local::Task<u64, ()>>() == 8);
    }
}

/// Owns the small controller input until the supervised process exits.
struct Input(PathBuf);

impl Drop for Input {
    fn drop(&mut self) {
        if let Err(error) = std::fs::remove_file(&self.0) {
            let path = self.0.display();
            assert!(std::thread::panicking(), "cannot remove {path}: {error}");
            let _ = writeln!(std::io::stderr(), "cannot remove {path}: {error}");
        }
    }
}

/// Supplies finite input through a file, reusing the existing child supervisor.
fn output(args: &[&str], commands: &str) -> Output {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let path = std::env::temp_dir().join(format!(
        "rca-task-fold-{}-{}.input",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let mut file: File = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&path)
        .unwrap();
    let _input = Input(path);
    file.write_all(commands.as_bytes()).unwrap();
    file.rewind().unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_task_fold_cost"));
    command.args(args);
    let result =
        support::output_with_input(&mut command, Duration::from_secs(30), Stdio::from(file));
    drop(command);
    result
}

#[cfg(not(any(
    feature = "deterministic",
    feature = "diagnostics",
    feature = "allocations",
    feature = "persistence",
    feature = "local"
)))]
mod production_graph {
    use super::output;

    /// Requires the complete protocol and successful cleanup before accepting a record.
    fn record(args: &[&str]) -> serde_json::Value {
        let result = output(args, "GO\nREPORT\nEXIT\n");
        assert!(
            result.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert_eq!(result.stderr, [] as [u8; 0]);
        let text = String::from_utf8(result.stdout).unwrap();
        let lines: Vec<_> = text.lines().collect();
        assert_eq!(lines.len(), 3);
        assert_eq!(&lines[..2], &["READY", "DONE"]);
        let row: serde_json::Value = serde_json::from_str(lines[2]).unwrap();
        let (schema, clock) = if cfg!(feature = "task-fold-controlled") {
            ("rca-task-fold-controlled-cost-v1", "paused-checked")
        } else {
            ("rca-task-fold-cost-v1", "live")
        };
        assert_eq!(row["schema"], schema);
        assert_eq!(row["details"]["clock"], clock);
        assert_eq!(row["workload"], args[0]);
        assert_eq!(row["size"], args[1].parse::<u64>().unwrap());
        assert_eq!(row["iterations"], args[2].parse::<u64>().unwrap());
        assert_eq!(row["workers"], 1);
        assert_eq!(row["cleanup_complete"], true);
        assert_eq!(row["latencies_ns"], serde_json::json!([]));
        assert!(row["elapsed_ns"].as_u64().unwrap() > 0);
        assert_eq!(row["details"]["allocator"], "system");
        assert_eq!(row["details"]["runtime"], "Tokio current-thread");
        assert_eq!(
            row["details"]["task_groups_enabled"],
            cfg!(feature = "task-fold-groups")
        );
        row
    }

    #[test]
    fn old_api_task_names_complete_literal_work_on_both_carriers() {
        for carrier in ["shared", "local"] {
            for kind in ["unnamed", "named"] {
                let name = format!("task-{kind}-{carrier}");
                let row = record(&[&name, "1", "3", "1"]);
                assert_eq!(row["operations"], 3);
                assert_eq!(row["checksum"], 3);
            }
        }
    }

    #[cfg(feature = "task-fold-groups")]
    #[test]
    fn nested_task_names_complete_the_same_literal_body() {
        for carrier in ["shared", "local"] {
            for depth in ["2", "3"] {
                let name = format!("task-named-{carrier}");
                let row = record(&[&name, depth, "3", "1"]);
                assert_eq!(row["operations"], 3);
                assert_eq!(row["checksum"], 3);
            }
        }
    }

    #[test]
    fn folds_cover_empty_sparse_dense_and_churn_with_literal_answers() {
        for carrier in ["shared", "local"] {
            for (stem, size, iterations, operations, checksum) in [
                ("root-empty", "1", "3", 3, 0),
                ("subtree-empty", "1", "3", 3, 0),
                ("root", "1", "3", 3, 18),
                ("subtree", "1", "3", 3, 6),
                ("root-sparse", "16", "3", 3, 18),
                ("subtree-sparse", "16", "3", 3, 6),
                ("root-sparse", "256", "2", 2, 192),
                ("subtree-sparse", "256", "2", 2, 64),
                ("root-sparse", "1024", "2", 2, 768),
                ("subtree-sparse", "1024", "2", 2, 256),
                ("root", "256", "2", 2, 3072),
                ("subtree", "256", "2", 2, 1024),
                ("root-churn", "3", "2", 8, 68),
                ("subtree-churn", "3", "2", 8, 20),
            ] {
                let name = format!("fold-{stem}-{carrier}");
                let row = record(&[&name, size, iterations, "1"]);
                assert_eq!(row["operations"], operations, "{name}/{size}");
                assert_eq!(row["checksum"], checksum, "{name}/{size}");
            }
        }
    }

    #[test]
    fn bad_protocol_never_completes_successfully() {
        for commands in ["", "BAD\n", "GO\nBAD\n", "GO\nREPORT\nBAD\n"] {
            let result = output(&["fold-root-empty-shared", "1", "1", "1"], commands);
            assert!(!result.status.success());
            assert_ne!(result.stderr, [] as [u8; 0]);
        }
    }

    #[cfg(not(feature = "task-fold-groups"))]
    #[test]
    fn common_api_build_rejects_nested_names_before_ready() {
        for name in ["task-named-shared", "task-named-local"] {
            for depth in ["2", "3"] {
                let result = output(&[name, depth, "1", "1"], "");
                assert!(!result.status.success());
                assert_eq!(result.stdout, [] as [u8; 0]);
            }
        }
    }
}

#[test]
fn invalid_dimensions_never_emit_ready() {
    for args in [
        vec![],
        vec!["unknown", "1", "1", "1"],
        vec!["task-unnamed-shared", "2", "1", "1"],
        vec!["task-named-local", "4", "1", "1"],
        vec!["task-named-local", "1", "1", "2"],
        vec!["fold-root-empty-shared", "2", "1", "1"],
        vec!["fold-root-local", "2", "1", "1"],
        vec!["fold-subtree-sparse-local", "0", "1", "1"],
        vec!["fold-root-shared", "+1", "1", "1"],
        vec!["fold-root-shared", "1", "0", "1"],
        vec!["fold-root-shared", "1", "18446744073709551616", "1"],
        vec!["fold-root-churn-shared", "3", "9223372036854775807", "1"],
        vec!["fold-root-shared", "1", "1", "1", "extra"],
    ] {
        let result = output(&args, "");
        assert!(!result.status.success(), "{args:?}");
        assert_eq!(result.stdout, [] as [u8; 0], "{args:?}");
        assert_ne!(result.stderr, [] as [u8; 0], "{args:?}");
    }
}

#[cfg(any(
    feature = "deterministic",
    feature = "diagnostics",
    feature = "allocations",
    feature = "persistence",
    feature = "local"
))]
#[test]
fn incompatible_feature_graph_fails_before_ready() {
    let result = output(&["task-unnamed-shared", "1", "1", "1"], "");
    assert!(!result.status.success());
    assert_eq!(result.stdout, [] as [u8; 0]);
    assert_ne!(result.stderr, [] as [u8; 0]);
}
