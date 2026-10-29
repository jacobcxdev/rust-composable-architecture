use std::process::Command;
use std::time::Duration;

mod support;

/// Reads one independently checked collection record after bounded child execution.
fn record(mode: &str, size: &str, iterations: &str) -> serde_json::Value {
    let output = support::output(
        Command::new(env!("CARGO_BIN_EXE_rca-performance-profile")).args([mode, size, iterations]),
        Duration::from_secs(60),
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn collection_modes_validate_independent_order_counts_and_ownership() {
    for (mode, inserting, control, comparisons) in [
        ("collection-insert-head", true, false, [0, 0, 0, 0]),
        ("collection-insert-middle", true, false, [0, 0, 0, 0]),
        ("collection-insert-tail", true, false, [0, 0, 0, 0]),
        ("collection-sort-sorted", false, false, [0, 0, 32, 5120]),
        ("collection-sort-reversed", false, false, [0, 0, 32, 5120]),
        ("collection-sort-ties", false, false, [0, 0, 47, 8108]),
        ("collection-insert-control", true, true, [0, 0, 0, 0]),
        ("collection-sort-control", false, true, [0, 0, 0, 0]),
    ] {
        for (index, size) in ["0", "1", "16", "1024"].into_iter().enumerate() {
            let actual = record(mode, size, "1");
            let created = if inserting {
                [1, 2, 17, 1025][index]
            } else {
                [0, 1, 16, 1024][index]
            };
            let pairs = if inserting && !control {
                [1, 2, 17, 1025][index]
            } else {
                [0, 1, 16, 1024][index]
            };
            assert_eq!(actual["schema"], "rca-collection-performance-profile-v1");
            assert_eq!(actual["workload"], mode);
            assert_eq!(
                actual["kind"],
                if control {
                    "driver-control"
                } else {
                    "operation"
                }
            );
            assert_eq!(actual["driver_visits"], 1);
            assert_eq!(actual["operations"], u64::from(!control));
            assert_eq!(actual["comparisons"], comparisons[index]);
            assert_eq!(actual["validated_pairs"], pairs);
            assert_eq!(actual["values_created"], created);
            assert_eq!(actual["values_dropped"], created);
            assert_eq!(actual["work_drops"], 0);
            assert_eq!(actual["prebuilt_fixtures"], 1);
            assert_eq!(actual["owners_released"], true);
            assert_eq!(
                actual["features"]["allocations"],
                cfg!(feature = "allocations")
            );
            if cfg!(feature = "allocations") {
                assert_eq!(
                    actual["allocations"]["snapshots"].as_array().unwrap().len(),
                    5
                );
                assert_eq!(
                    actual["allocations"]["executor_thread_local"]["retained_blocks"],
                    0
                );
                assert_eq!(
                    actual["allocations"]["executor_thread_local"]["retained_bytes"],
                    0
                );
            } else {
                assert!(actual["allocations"].is_null());
            }
        }
    }
}

#[test]
fn prebuilt_large_cases_complete_without_resetting_inside_work() {
    let inserted = record("collection-insert-middle", "1024", "256");
    assert_eq!(inserted["operations"], 256);
    assert_eq!(inserted["validated_pairs"], 262_400);
    assert_eq!(inserted["values_created"], 262_400);
    assert_eq!(inserted["values_dropped"], 262_400);
    let sorted = record("collection-sort-ties", "1024", "256");
    assert_eq!(sorted["operations"], 256);
    assert_eq!(sorted["comparisons"], 2_075_648);
    assert_eq!(sorted["validated_pairs"], 262_144);
    assert_eq!(sorted["values_created"], 262_144);
    assert_eq!(sorted["values_dropped"], 262_144);
}

#[test]
fn invalid_collection_dimensions_emit_no_success_record() {
    for args in [
        vec!["collection-unknown", "16", "1"],
        vec!["collection-insert-head"],
        vec!["collection-insert-head", "16"],
        vec!["collection-insert-head", "2", "1"],
        vec!["collection-insert-head", "16", "0"],
        vec!["collection-insert-head", "16", "2"],
        vec!["collection-insert-head", "16", "257"],
        vec!["collection-insert-head", "-1", "1"],
        vec!["collection-insert-head", "+1", "1"],
        vec!["collection-insert-head", "18446744073709551616", "1"],
        vec!["collection-insert-head", "16", "1", "extra"],
    ] {
        let output = support::output(
            Command::new(env!("CARGO_BIN_EXE_rca-performance-profile")).args(args),
            Duration::from_secs(60),
        );
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
    }
}
