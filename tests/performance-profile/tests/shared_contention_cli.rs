use std::process::Command;
use std::time::Duration;

mod support;

// A missing dispatch, split publication or falsely expired owner must not pass.
// Bounded child capture is separate from the workload's own native thread joins.
#[test]
fn concurrent_cache_cells_preserve_exact_candidates_and_owners() {
    for (mode, hit) in [("shared-race-cold", false), ("shared-race-hit", true)] {
        for callers in [1, 2, 4, 8] {
            let output = support::output(
                Command::new(env!("CARGO_BIN_EXE_rca-performance-profile"))
                    .args([mode.to_owned(), callers.to_string()]),
                Duration::from_secs(60),
            );
            assert!(
                output.status.success(),
                "{mode}/{callers}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            let record: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
            assert_eq!(record["schema"], "rca-shared-contention-profile-v1");
            assert_eq!(record["callers"], callers);
            assert_eq!(record["workload"], mode);
            let calls = if hit { 64 * callers } else { callers };
            let candidates = if hit { 1 } else { callers };
            let work = &record["counters"]["work"];
            for field in ["successful_lookups", "cohort_constructions"] {
                assert_eq!(work[field], calls, "{field}");
            }
            assert_eq!(work["read_calls"], 0);
            assert_eq!(record["counters"]["validation"]["read_calls"], 2 * calls);
            assert_eq!(record["counters"]["teardown"]["read_calls"], 1);
            assert_eq!(record["returned_cohorts_retained_after_work"], calls);
            assert_eq!(record["cold_cohorts_retained_after_work"], u64::from(hit));
            assert_eq!(record["read_phase"], "serial-validation");
            for field in [
                "seed_calls",
                "initial_loads",
                "initial_subscribes",
                "values_created",
                "subscriptions_created",
            ] {
                assert_eq!(work[field], if hit { 0 } else { callers }, "{field}");
                assert_eq!(record["totals"][field], candidates, "{field}");
            }
            for field in ["values_dropped", "subscriptions_dropped"] {
                assert_eq!(work[field], if hit { 0 } else { callers - 1 }, "{field}");
                assert_eq!(record["totals"][field], candidates, "{field}");
            }
            for field in [
                "user_loads",
                "user_subscribes",
                "save_calls",
                "provider_returned_values",
                "reader_captures",
                "capture_operations",
            ] {
                assert_eq!(record["totals"][field], 0, "{field}");
            }
            for field in [
                "owners_released",
                "all_candidate_values_expired",
                "all_candidate_subscriptions_expired",
                "all_producers_closed",
                "mutation_visible",
            ] {
                assert_eq!(record[field], true, "{field}");
            }
            let winner = record["winner_tag"].as_u64().unwrap();
            if hit {
                assert_eq!(winner, 7);
                assert_eq!(record["checksum"], 448 * callers);
            } else {
                assert!((1..=callers).contains(&winner));
                assert_eq!(record["checksum"], winner * callers);
            }
            assert_eq!(record["live_candidates_after_work"], 1);
            assert_eq!(
                record["features"]["allocations"],
                cfg!(feature = "allocations")
            );
            #[cfg(feature = "allocations")]
            {
                assert_eq!(
                    record["allocations"]["snapshots"].as_array().unwrap().len(),
                    6
                );
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
}

#[test]
fn invalid_concurrent_inputs_emit_no_success_record() {
    for args in [
        vec!["shared-race-cold"],
        vec!["shared-race-cold", "0"],
        vec!["shared-race-cold", "3"],
        vec!["shared-race-hit", "16"],
        vec!["shared-race-hit", "-1"],
        vec!["shared-race-hit", "+1"],
        vec!["shared-race-hit", "1.5"],
        vec!["shared-race-hit", ""],
        vec!["shared-race-cold", "1", "extra"],
        vec!["shared-race-unknown", "1"],
        vec!["shared-race-cold", "18446744073709551616"],
    ] {
        let output = support::output(
            Command::new(env!("CARGO_BIN_EXE_rca-performance-profile")).args(args),
            Duration::from_secs(60),
        );
        assert_eq!(output.status.code(), Some(1));
        assert!(output.stdout.is_empty());
        assert!(!output.stderr.is_empty());
    }
}
