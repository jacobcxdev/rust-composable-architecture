use std::process::Command;
use std::time::Duration;

mod support;

// Catch missing dispatch, mixed cold/useful work, and false final-owner claims.
#[test]
fn finite_shared_cache_matrix_has_exact_direct_traces() {
    for mode in ["shared-reader-hit", "shared-cohort-hit"] {
        for (size, iterations) in [(1, 1), (64, 1), (1, 3), (64, 3), (1, 100), (64, 100)] {
            assert_record(mode, size, iterations, true);
        }
    }
    for mode in ["shared-reader-expired", "shared-cohort-expired"] {
        for iterations in [1, 3, 100] {
            assert_record(mode, 1, iterations, false);
        }
    }
}

fn assert_record(mode: &str, size: u64, iterations: u64, hit: bool) {
    let output = support::output(
        Command::new(env!("CARGO_BIN_EXE_rca-performance-profile")).args([
            mode.to_owned(),
            size.to_string(),
            iterations.to_string(),
        ]),
        Duration::from_secs(60),
    );
    assert!(
        output.status.success(),
        "{mode} {size} {iterations}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let record: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(record["schema"], "rca-shared-cache-performance-profile-v1");
    assert_eq!(record["execution"], "direct-ready");
    assert_eq!(record["workload"], mode);
    assert_eq!(record["size"], size);
    assert_eq!(record["iterations"], iterations);
    assert_eq!(record["cycles"], iterations);
    for field in [
        "roots_committed",
        "responses_committed",
        "causal_completed",
        "domain_clones",
        "provider_returned_values",
    ] {
        assert_eq!(record[field], 0, "{field}");
    }
    let useful = size * iterations;
    let bindings = if hit { 1 } else { iterations + 1 };
    let checksum = if hit {
        7 * useful
    } else {
        iterations * (iterations + 1) / 2
    };
    for (field, expected) in [
        ("cache_namespaces", 1),
        ("successful_lookups", useful + 1),
        ("seed_calls", bindings),
        ("initial_loads", bindings),
        ("initial_subscribes", bindings),
        ("user_loads", 0),
        ("user_subscribes", 0),
        ("save_calls", 0),
        ("values_created", bindings),
        ("values_dropped", bindings),
        ("subscriptions_created", bindings),
        ("subscriptions_dropped", bindings),
        ("read_calls", if hit { useful + 3 } else { iterations + 1 }),
        ("capture_operations", u64::from(hit)),
        ("reader_captures", u64::from(hit)),
        (
            "cohort_constructions",
            if mode.contains("cohort") {
                useful + 1
            } else {
                0
            },
        ),
        ("checksum", checksum),
        ("last_tag", if hit { 7 } else { iterations }),
    ] {
        assert_eq!(record[field], expected, "{mode}: {field}");
    }
    for field in [
        "owners_released",
        "value_expired",
        "subscription_expired",
        "producer_closed",
    ] {
        assert_eq!(record[field], true, "{field}");
    }
    let phases = &record["counters"];
    assert_eq!(phases["cold"]["seed_calls"], 1);
    assert_eq!(phases["cold"]["values_dropped"], u64::from(!hit));
    assert_eq!(phases["work"]["successful_lookups"], useful);
    assert_eq!(phases["work"]["read_calls"], useful);
    assert_eq!(
        phases["work"]["seed_calls"],
        if hit { 0 } else { iterations }
    );
    assert_eq!(
        phases["work"]["values_dropped"],
        if hit { 0 } else { iterations }
    );
    assert_eq!(phases["validation"]["read_calls"], u64::from(hit));
    assert_eq!(phases["teardown"]["read_calls"], u64::from(hit));
    assert_eq!(phases["teardown"]["values_dropped"], u64::from(hit));
    assert_eq!(
        record["features"]["deterministic"],
        cfg!(feature = "deterministic")
    );
    assert_eq!(
        record["features"]["allocations"],
        cfg!(feature = "allocations")
    );
    assert_allocations(&record);
}

fn assert_allocations(record: &serde_json::Value) {
    #[cfg(feature = "allocations")]
    {
        let allocation = &record["allocations"];
        assert_eq!(allocation["snapshots"].as_array().unwrap().len(), 6);
        assert!(allocation["phases"]["cold"].is_object());
        assert!(allocation["phases"].get("warmup").is_none());
        assert_eq!(allocation["executor_thread_local"]["source"], "none");
        assert_eq!(allocation["executor_thread_local"]["retained_blocks"], 0);
        assert_eq!(allocation["executor_thread_local"]["retained_bytes"], 0);
    }
    #[cfg(not(feature = "allocations"))]
    assert!(record["allocations"].is_null());
}

#[test]
fn invalid_shared_inputs_emit_no_success_record() {
    for args in [
        vec!["shared-reader-hit"],
        vec!["shared-reader-hit", "1"],
        vec!["shared-reader-hit", "0", "1"],
        vec!["shared-reader-hit", "2", "1"],
        vec!["shared-cohort-expired", "64", "1"],
        vec!["shared-reader-expired", "1", "0"],
        vec!["shared-reader-hit", "-1", "1"],
        vec!["shared-reader-hit", "+1", "1"],
        vec!["shared-reader-hit", "1", "1.5"],
        vec!["shared-reader-hit", "1", "1", "extra"],
        vec!["shared-unknown", "1", "1"],
        vec!["shared-reader-hit", "64", "18446744073709551615"],
        vec!["shared-reader-expired", "1", "18446744073709551615"],
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
