use std::process::Command;

#[expect(
    dead_code,
    reason = "only bounded capture is needed from the shared allocation child support"
)]
mod subprocess {
    include!("../../rca-core/src/allocation_test/child.rs");

    /// Captures one direct child with bounded output and a fifteen-second deadline.
    ///
    /// Probes own their pipe writers and do not pass them to surviving descendants.
    pub(super) fn output(command: &mut Command) -> Output {
        capture(command, Duration::from_secs(15)).expect("mounted allocation driver must complete")
    }
}

/// Runs one fresh probe process while draining both pipes to avoid pipe stalls.
///
/// The child has a fifteen-second deadline. The owning test command also needs
/// the external deadline specified in the fixture README.
fn run(count: usize) -> serde_json::Value {
    let output = subprocess::output(
        Command::new(env!("CARGO_BIN_EXE_mounted-allocations")).arg(count.to_string()),
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn native_mounted_cardinalities_and_terminal_release() {
    for count in [0, 2, 10, 1000] {
        let result = run(count);
        assert_eq!(result["schema"], "mounted-keyed-allocations-v1");
        assert_eq!(result["accounting"], "semantic-boundary-not-quiescent");
        assert_eq!(result["comparison_qualified"], false);
        assert_eq!(result["engine"], "rca");
        assert_eq!(result["cardinality"], count);
        assert_eq!(result["outputs"], if count == 0 { 0 } else { 6 });
        assert_eq!(result["reductions"], if count == 0 { 2 } else { 18 });
        assert_eq!(
            result["children_dropped"],
            if count == 0 { 0 } else { count + 2 }
        );
        assert_eq!(result["root_released"], true);
        assert_eq!(result["policy_work_skipped"], count == 0);
        let names = result["snapshots"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row["name"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            [
                "baseline",
                "populated_unmounted",
                "store_constructed",
                "mounted",
                "work_completed",
                "removed",
                "reinserted",
                "cleared",
                "extracted_host_stopped",
                "released"
            ]
        );
        let expected = if count == 0 {
            serde_json::json!([])
        } else {
            serde_json::json!([[1, 10], [2, 100], [1, 14], [1, 7], [2, 110], [1, 7]])
        };
        assert_eq!(result["trace"], expected);
        assert_eq!(
            result["verified_child_rows"],
            if count == 0 {
                serde_json::json!([])
            } else {
                serde_json::json!([[1, 10, 3, 1, 7], [2, 120, 10, 2, 110]])
            }
        );
        for (index, snapshot) in result["snapshots"].as_array().unwrap().iter().enumerate() {
            assert_eq!(
                snapshot["outputs"],
                if count == 0 {
                    0
                } else {
                    [0, 0, 0, 0, 5, 5, 6, 6, 6, 6][index]
                }
            );
            assert_eq!(
                snapshot["children_dropped"],
                if count == 0 {
                    0
                } else {
                    [0, 0, 0, 0, 1, 2, 2, count + 2, count + 2, count + 2][index]
                }
            );
            assert_eq!(snapshot["roots_dropped"], usize::from(index == 9));
            for field in [
                "total_blocks",
                "total_bytes",
                "curr_blocks",
                "curr_bytes",
                "max_blocks",
                "max_bytes",
            ] {
                assert!(snapshot[field].is_u64(), "missing {field}");
            }
        }
    }
}
