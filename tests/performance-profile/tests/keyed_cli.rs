use std::process::Command;
use std::time::Duration;

mod support;

#[test]
fn keyed_modes_commit_exact_low_high_lifecycle_traces() {
    for mode in ["keyed-steady", "keyed-retire"] {
        for (size, cycles) in [(2, 1), (2, 3), (256, 1), (256, 3)] {
            assert_keyed_record(mode, "native", size, cycles);
            #[cfg(feature = "deterministic")]
            assert_keyed_record(mode, "deterministic", size, cycles);
        }
    }
}

fn assert_keyed_record(mode: &str, lane: &str, size: u64, cycles: u64) {
    let output = support::output(
        Command::new(env!("CARGO_BIN_EXE_rca-performance-profile")).args([
            format!("{mode}-{lane}"),
            size.to_string(),
            cycles.to_string(),
        ]),
        Duration::from_secs(60),
    );
    assert!(
        output.status.success(),
        "{mode}-{lane} {size} {cycles}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let record: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(record["schema"], "rca-keyed-performance-profile-v1");
    let steady = mode == "keyed-steady";
    assert_keyed_counters(&record, steady, size, cycles);
    for field in [
        "state_extracted",
        "reducers_dropped",
        "host_task_dropped",
        "owners_released",
        "host_stopped",
    ] {
        assert_eq!(record[field], true, "{mode}: {field}");
    }
    assert_keyed_projection(&record, steady, size, cycles);
    assert_eq!(
        record["features"]["deterministic"],
        cfg!(feature = "deterministic")
    );
    assert_eq!(
        record["features"]["allocations"],
        cfg!(feature = "allocations")
    );
    #[cfg(feature = "allocations")]
    {
        assert_eq!(
            record["allocations"]["executor_thread_local"]["retained_blocks"],
            u64::from(lane == "native")
        );
        if lane == "deterministic" {
            assert_eq!(
                record["allocations"]["executor_thread_local"]["retained_bytes"],
                0
            );
            assert_eq!(
                record["allocations"]["executor_thread_local"]["source"],
                "none"
            );
        }
        assert_eq!(
            record["allocations"]["snapshots"].as_array().unwrap().len(),
            6
        );
        assert!(record["allocations"]["phases"]["warmup"].is_object());
        if !steady {
            for field in ["allocated_blocks", "allocated_bytes"] {
                assert_eq!(record["allocations"]["phases"]["warmup"][field], 0);
            }
        }
    }
    #[cfg(not(feature = "allocations"))]
    assert!(record["allocations"].is_null());
}

/// Checks ordered rows and literal digests, including retired-key replacement generations.
fn assert_keyed_projection(record: &serde_json::Value, steady: bool, size: u64, cycles: u64) {
    let digest = match (steady, size, cycles) {
        (true, 2, 1) => 9_261_973_701_899_529_337_u64,
        (true, 2, 3) => 4_665_284_664_013_999_823,
        (true, 256, 1) => 4_641_790_918_752_592_958,
        (true, 256, 3) => 7_825_103_406_451_428_164,
        (false, 2, 1) => 11_188_594_931_870_369_311,
        (false, 2, 3) => 15_795_368_403_956_676_847,
        (false, 256, 1) => 13_258_112_303_726_856_714,
        (false, 256, 3) => 17_864_885_775_813_164_250,
        _ => panic!("case outside finite qualification matrix"),
    };
    assert_eq!(record["projection_digest"], digest);
    let rows = record["final_projection"].as_array().unwrap();
    assert_eq!(rows.len() as u64, size);
    for (index, row) in rows.iter().enumerate() {
        let id = if steady {
            index as u64
        } else if index == rows.len() - 1 {
            0
        } else {
            size - 1 - index as u64
        };
        assert_eq!(
            row,
            &serde_json::json!([
                id,
                if !steady && id == 0 { cycles } else { 0 },
                if id == 0 {
                    if steady { cycles * 3 } else { 1 }
                } else if id == 1 && !steady {
                    cycles
                } else {
                    0
                }
            ])
        );
    }
}

fn assert_keyed_counters(record: &serde_json::Value, steady: bool, size: u64, cycles: u64) {
    for (field, expected) in [
        ("warmup_roots", u64::from(steady)),
        ("warmup_parent_reductions", u64::from(steady)),
        ("warmup_causal_completed", u64::from(steady)),
        ("warmup_reads", u64::from(steady)),
        ("roots_committed", cycles * if steady { 3 } else { 5 }),
        ("responses_committed", cycles * if steady { 2 } else { 1 }),
        ("parent_reductions", cycles * if steady { 5 } else { 6 }),
        ("leaf_reductions", cycles * 4),
        ("causal_completed", cycles * if steady { 3 } else { 5 }),
        ("tasks_started", if steady { 0 } else { cycles * 2 }),
        ("tasks_retired", if steady { 0 } else { cycles }),
        ("tasks_completed", if steady { 0 } else { cycles }),
        ("captures_created", if steady { 0 } else { cycles * 2 }),
        ("captures_dropped", if steady { 0 } else { cycles * 2 }),
        ("states_created", size + if steady { 0 } else { cycles }),
        ("states_dropped", size + if steady { 0 } else { cycles }),
        ("stale_output_attempts", if steady { 0 } else { cycles }),
        ("stale_outputs_committed", 0),
        ("accepted_outputs", cycles * if steady { 2 } else { 1 }),
        ("scope_registrations", cycles * if steady { 1 } else { 3 }),
        ("root_send_attempts", cycles * if steady { 1 } else { 2 }),
        ("scoped_send_attempts", cycles * if steady { 2 } else { 4 }),
        ("scoped_read_attempts", if steady { 0 } else { cycles * 2 }),
        ("callback_attempts", if steady { 0 } else { cycles }),
        ("weak_upgrades", if steady { 0 } else { cycles * 2 }),
        ("weak_read_attempts", if steady { 0 } else { cycles }),
        ("reader_registrations", if steady { 0 } else { cycles * 2 }),
        ("reader_next_calls", if steady { 0 } else { cycles * 4 }),
        ("readers_closed", if steady { 0 } else { cycles }),
        (
            "effect_sender_attempts",
            if steady { 0 } else { cycles * 2 },
        ),
        ("root_read_calls", cycles + 1),
    ] {
        assert_eq!(record[field], expected, "{field}");
    }
}

#[test]
fn keyed_invalid_inputs_emit_no_success_record() {
    for args in [
        ["keyed-steady-native", "0", "1"],
        ["keyed-retire-native", "16", "1"],
        ["keyed-steady-native", "2", "0"],
        ["keyed-retire-native", "256", "18446744073709551615"],
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

#[cfg(not(feature = "deterministic"))]
#[test]
fn keyed_disabled_lane_rejects_before_size_validation() {
    for name in ["keyed-steady-deterministic", "keyed-retire-deterministic"] {
        let output = support::output(
            Command::new(env!("CARGO_BIN_EXE_rca-performance-profile")).arg(name),
            Duration::from_secs(60),
        );
        assert_eq!(
            String::from_utf8_lossy(&output.stderr).trim(),
            "deterministic workloads require the deterministic feature"
        );
        assert!(output.stdout.is_empty());
        assert_eq!(output.status.code(), Some(1));
    }
}
