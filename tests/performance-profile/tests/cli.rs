use std::process::Command;
use std::time::Duration;

mod support;

#[test]
fn finite_width_and_depth_matrix() {
    for mode in [
        "future-chain",
        "run-chain",
        "merge-dense",
        "concatenate",
        "merge-sparse",
        "merge-empty",
        "map-deferred",
        "cancellation-depth",
        "timers-at",
        "timers-after",
    ] {
        let cases: &[(u64, u64)] = match mode {
            "map-deferred" => &[(0, 1), (1, 1), (16, 1), (16, 3)],
            "cancellation-depth" => &[(0, 1), (1, 1), (4, 1), (16, 1), (16, 3)],
            _ => &[(0, 1), (1, 1), (16, 1), (256, 1), (16, 3)],
        };
        for &(size, roots) in cases {
            for lane in [
                "native",
                #[cfg(feature = "deterministic")]
                "deterministic",
            ] {
                // Native timer qualification stays small; virtual time scales to 256.
                if lane == "native" && mode.starts_with("timers-") && size == 256 {
                    continue;
                }
                assert_shape_case(mode, size, roots, lane);
            }
        }
    }
}

fn assert_shape_case(mode: &str, size: u64, roots: u64, lane: &str) {
    let name = format!("{mode}-{lane}");
    let output = support::output(
        Command::new(env!("CARGO_BIN_EXE_rca-performance-profile")).args([
            &name,
            &size.to_string(),
            &roots.to_string(),
        ]),
        Duration::from_secs(60),
    );
    assert!(
        output.status.success(),
        "{name} {size} {roots}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let record: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let per_root = match mode {
        "merge-empty" => 0,
        "merge-sparse" => u64::from(size != 0),
        "map-deferred" | "cancellation-depth" => 1,
        _ => size,
    };
    let responses = per_root * roots;
    let reductions = responses + roots;
    let futures = if matches!(
        mode,
        "future-chain" | "merge-dense" | "concatenate" | "map-deferred"
    ) {
        responses
    } else {
        0
    };
    let runs = if mode == "run-chain" { responses } else { 0 };
    let maps = if mode == "map-deferred" {
        size * roots
    } else {
        0
    };
    let timers = if mode.starts_with("timers-") {
        responses
    } else {
        0
    };
    for (field, expected) in [
        ("roots_committed", roots),
        ("causal_completed", roots),
        ("responses_committed", responses),
        ("reducer_calls", reductions),
        ("final_state", reductions),
        ("checksum", reductions * (reductions + 1) / 2),
    ] {
        assert_eq!(record[field], expected, "{name} {size} {roots}: {field}");
    }
    for (field, expected) in [
        ("future_entries", futures),
        ("run_entries", runs),
        ("send_completions", runs),
        ("mapper_invocations", maps),
        ("captures_created", futures + runs + maps),
        ("captures_dropped", futures + runs + maps),
        ("timer_registrations", timers),
    ] {
        assert_eq!(
            record["work_facts"][field], expected,
            "{name} {size} {roots}: {field}"
        );
    }
    for field in [
        "state_extracted",
        "reducer_dropped",
        "host_task_dropped",
        "owners_released",
        "host_stopped",
    ] {
        assert_eq!(record[field], true, "{name} {size} {roots}: {field}");
    }
    assert_eq!(
        record["features"]["deterministic"],
        cfg!(feature = "deterministic")
    );
    assert_eq!(
        record["features"]["allocations"],
        cfg!(feature = "allocations")
    );
    #[cfg(feature = "allocations")]
    assert_eq!(
        record["allocations"]["executor_thread_local"]["retained_blocks"],
        1
    );
}

#[test]
fn effect_shapes_commit_exact_finite_work() {
    for (mode, size, responses, futures, runs, sends, maps, captures, timers) in [
        ("future-chain", "0", 0, 0, 0, 0, 0, 0, 0),
        ("future-chain", "2", 6, 6, 0, 0, 0, 6, 0),
        ("run-chain", "2", 6, 0, 6, 6, 0, 6, 0),
        ("merge-dense", "2", 6, 6, 0, 0, 0, 6, 0),
        ("concatenate", "2", 6, 6, 0, 0, 0, 6, 0),
        ("merge-sparse", "16", 3, 0, 0, 0, 0, 0, 0),
        ("merge-empty", "16", 0, 0, 0, 0, 0, 0, 0),
        ("map-deferred", "2", 3, 3, 0, 0, 6, 9, 0),
        ("cancellation-depth", "4", 3, 0, 0, 0, 0, 0, 0),
        ("timers-at", "2", 6, 0, 0, 0, 0, 0, 6),
        ("timers-after", "2", 6, 0, 0, 0, 0, 0, 6),
    ] {
        for lane in [
            "native",
            #[cfg(feature = "deterministic")]
            "deterministic",
        ] {
            let name = format!("{mode}-{lane}");
            let output = support::output(
                Command::new(env!("CARGO_BIN_EXE_rca-performance-profile"))
                    .args([&name, size, "3"]),
                Duration::from_secs(60),
            );
            assert!(
                output.status.success(),
                "{name}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            let record: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
            assert_eq!(record["responses_committed"], responses);
            assert_eq!(record["reducer_calls"], responses + 3);
            assert_eq!(record["checksum"], (responses + 3) * (responses + 4) / 2);
            for (field, expected) in [
                ("future_entries", futures),
                ("run_entries", runs),
                ("send_completions", sends),
                ("mapper_invocations", maps),
                ("captures_created", captures),
                ("captures_dropped", captures),
                ("timer_registrations", timers),
            ] {
                assert_eq!(record["work_facts"][field], expected, "{name}: {field}");
            }
            for field in [
                "state_extracted",
                "reducer_dropped",
                "host_task_dropped",
                "owners_released",
                "host_stopped",
            ] {
                assert_eq!(record[field], true, "{name}: {field}");
            }
        }
    }
}

#[test]
fn invalid_cli_exits_without_a_success_record() {
    for args in [
        vec![],
        vec!["not-a-workload", "0", "1"],
        vec!["store-native", "1", "1"],
        vec!["effects-deterministic", "0", "1"],
        vec!["store-native", "0", "18446744073709551615"],
        vec!["future-chain-native", "257", "1"],
        vec!["map-deferred-native", "17", "1"],
        vec!["cancellation-depth-native", "17", "1"],
        vec!["timers-at-native", "1", "0"],
    ] {
        let output = support::output(
            Command::new(env!("CARGO_BIN_EXE_rca-performance-profile")).args(args),
            Duration::from_secs(60),
        );
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        assert!(!output.stderr.is_empty());
    }
}

#[test]
fn valid_cli_emits_one_validated_record() {
    for (name, size, reductions, checksum) in [
        ("store-native", "0", 3, 6),
        ("effects-native", "2", 9, 45),
        #[cfg(feature = "deterministic")]
        ("store-deterministic", "0", 3, 6),
        #[cfg(feature = "deterministic")]
        ("effects-deterministic", "2", 9, 45),
    ] {
        assert_valid_record(name, size, reductions, checksum);
    }
}

#[cfg(not(feature = "deterministic"))]
#[test]
fn disabled_deterministic_names_fail_before_other_input_validation() {
    for name in [
        "store-deterministic",
        "effects-deterministic",
        "future-chain-deterministic",
        "run-chain-deterministic",
        "merge-dense-deterministic",
        "concatenate-deterministic",
        "merge-sparse-deterministic",
        "merge-empty-deterministic",
        "map-deferred-deterministic",
        "cancellation-depth-deterministic",
        "timers-at-deterministic",
        "timers-after-deterministic",
    ] {
        for args in [vec![name], vec![name, "0", "1"], vec![name, "2", "3"]] {
            let output = support::output(
                Command::new(env!("CARGO_BIN_EXE_rca-performance-profile")).args(args),
                Duration::from_secs(60),
            );
            assert!(!output.status.success());
            assert!(output.stdout.is_empty());
            assert_eq!(
                std::str::from_utf8(&output.stderr).unwrap().trim(),
                "deterministic workloads require the deterministic feature"
            );
        }
    }
}

fn assert_valid_record(name: &str, size: &str, reductions: u64, checksum: u64) {
    let output = support::output(
        Command::new(env!("CARGO_BIN_EXE_rca-performance-profile")).args([name, size, "3"]),
        Duration::from_secs(60),
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        std::str::from_utf8(&output.stdout).unwrap().lines().count(),
        1
    );
    let record: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(record["schema"], "rca-performance-profile-v2");
    assert_eq!(record["workload"], name);
    assert_eq!(
        record["feature_profile"],
        if cfg!(feature = "deterministic") {
            "deterministic-enabled"
        } else {
            "minimal-production"
        }
    );
    assert_eq!(
        record["features"]["deterministic"],
        cfg!(feature = "deterministic")
    );
    assert_eq!(
        record["features"]["allocations"],
        cfg!(feature = "allocations")
    );
    assert_eq!(record["roots_committed"], 3);
    assert_eq!(record["responses_committed"], reductions - 3);
    assert_eq!(record["reducer_calls"], reductions);
    assert_eq!(record["final_state"], reductions);
    assert_eq!(record["checksum"], checksum);
    assert_eq!(record["causal_completed"], 3);
    assert_eq!(record["state_extracted"], true);
    assert_eq!(record["reducer_dropped"], true);
    assert_eq!(record["host_task_dropped"], true);
    assert_eq!(record["host_stopped"], true);
    assert_eq!(record["owners_released"], true);
    #[cfg(feature = "allocations")]
    {
        assert_eq!(
            record["allocations"]["executor_thread_local"]["retained_blocks"],
            1
        );
        assert_eq!(
            record["allocations"]["snapshots"].as_array().unwrap().len(),
            5
        );
    }
    #[cfg(not(feature = "allocations"))]
    assert!(record["allocations"].is_null());
}
