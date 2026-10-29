mod collections;
mod dependencies;
#[cfg(feature = "diagnostics")]
mod diagnostic_workloads;
#[cfg(feature = "persistence")]
mod document;
#[cfg(all(feature = "persistence", feature = "deterministic"))]
mod document_rebind;
mod feature_transactions;
#[cfg(feature = "deterministic")]
mod finite_drive;
mod host;
mod issues;
#[cfg(all(feature = "persistence", feature = "deterministic"))]
mod joint_document;
mod keyed;
#[cfg(test)]
mod keyed_controls;
mod keyed_scaling;
mod lifecycle;
mod measurement;
#[cfg(all(feature = "deterministic", feature = "local"))]
mod mixed_pool;
#[cfg(all(feature = "deterministic", feature = "local"))]
mod mixed_service;
mod observation;
#[cfg(test)]
mod observation_controls;
#[cfg(feature = "deterministic")]
mod owned_snapshot;
mod shared_cache;
#[cfg(feature = "deterministic")]
mod snapshot_workloads;
mod stack_retirement;
#[cfg(feature = "deterministic")]
mod teststore;
mod topology;
mod workloads;

use workloads::{Adapter, Config, profile_useful_work};

#[cfg(feature = "allocations")]
#[global_allocator]
static ALLOCATOR: dhat::Alloc = dhat::Alloc;

/// Returns one record only after useful work, state and teardown checks pass.
fn run(config: Config) -> Result<String, &'static str> {
    let cache = if config.native {
        measurement::ExecutorCache::Native
    } else {
        measurement::ExecutorCache::Deterministic
    };
    let mut measurement = measurement::Measurement::new(Some(cache));
    let mut adapter = Adapter::new(config)?;
    measurement.construction_finished();
    profile_useful_work(&mut adapter, config)?;
    measurement.work_finished();
    let checked = adapter.validate_state(config)?;
    measurement.validation_finished();
    let outcome = adapter.teardown(checked)?;
    let allocations = measurement.finish()?;
    outcome.validate(config)?;
    let allocations = allocations.map_or_else(|| "null".to_owned(), |record| record.json());
    let work_facts = outcome.facts.json();
    let feature_profile = if cfg!(feature = "deterministic") {
        "deterministic-enabled"
    } else {
        "minimal-production"
    };
    Ok(format!(
        "{{\"schema\":\"rca-performance-profile-v2\",\"feature_profile\":\"{}\",\"features\":{{\"deterministic\":{},\"allocations\":{}}},\"workload\":\"{}\",\"size\":{},\"iterations\":{},\"warmup_roots\":0,\"roots_committed\":{},\"responses_committed\":{},\"reducer_calls\":{},\"causal_completed\":{},\"final_state\":{},\"checksum\":{},\"state_extracted\":{},\"reducer_dropped\":{},\"host_task_dropped\":{},\"owners_released\":{},\"host_stopped\":{},\"work_facts\":{},\"allocations\":{}}}",
        feature_profile,
        cfg!(feature = "deterministic"),
        cfg!(feature = "allocations"),
        config.name,
        config.size,
        config.iterations,
        outcome.roots,
        outcome.responses,
        outcome.reductions,
        outcome.causal_completed,
        outcome.final_state,
        outcome.checksum,
        outcome.state_extracted,
        outcome.reducer_dropped,
        outcome.host_task_dropped,
        outcome.owners_released,
        outcome.host_stopped,
        work_facts,
        allocations
    ))
}

#[expect(
    clippy::too_many_lines,
    reason = "one ordered dispatcher keeps workload prefixes and feature rejection together"
)]
/// Routes specific prefixes before their broader family names.
fn dispatch(args: impl Iterator<Item = String>) -> Result<String, &'static str> {
    let mut args = args.peekable();
    if args
        .peek()
        .is_some_and(|name| name.starts_with("topology-"))
    {
        topology::run(args)
    } else if args
        .peek()
        .is_some_and(|name| name.starts_with("mixed-service-"))
    {
        #[cfg(all(feature = "deterministic", feature = "local"))]
        {
            mixed_service::run(args)
        }
        #[cfg(not(all(feature = "deterministic", feature = "local")))]
        {
            Err("mixed service workloads require deterministic and local features")
        }
    } else if args
        .peek()
        .is_some_and(|name| name.starts_with("mixed-pool-"))
    {
        #[cfg(all(feature = "deterministic", feature = "local"))]
        {
            mixed_pool::run(args)
        }
        #[cfg(not(all(feature = "deterministic", feature = "local")))]
        {
            Err("mixed pool workloads require deterministic and local features")
        }
    } else if args.peek().is_some_and(|name| name == "owned-snapshot") {
        #[cfg(feature = "deterministic")]
        {
            owned_snapshot::run(args)
        }
        #[cfg(not(feature = "deterministic"))]
        {
            Err("owned snapshot workload requires the deterministic feature")
        }
    } else if args.peek().is_some_and(|name| name.starts_with("feature-")) {
        feature_transactions::Config::parse(args).and_then(feature_transactions::run)
    } else if args
        .peek()
        .is_some_and(|name| name.starts_with("lifecycle-"))
    {
        lifecycle::Config::parse(args).and_then(lifecycle::run)
    } else if args
        .peek()
        .is_some_and(|name| name.starts_with("collection-"))
    {
        collections::Config::parse(args).and_then(collections::run)
    } else if args
        .peek()
        .is_some_and(|name| name.starts_with("joint-document-"))
    {
        #[cfg(all(feature = "persistence", feature = "deterministic"))]
        {
            joint_document::Config::parse(args).and_then(joint_document::run)
        }
        #[cfg(not(all(feature = "persistence", feature = "deterministic")))]
        {
            Err("joint document workloads require persistence and deterministic features")
        }
    } else if args
        .peek()
        .is_some_and(|name| name.starts_with("snapshot-"))
    {
        #[cfg(feature = "deterministic")]
        {
            snapshot_workloads::Config::parse(args).and_then(snapshot_workloads::run)
        }
        #[cfg(not(feature = "deterministic"))]
        {
            Err("snapshot workloads require the deterministic feature")
        }
    } else if args
        .peek()
        .is_some_and(|name| name.starts_with("diagnostic-"))
    {
        #[cfg(feature = "diagnostics")]
        {
            diagnostic_workloads::Config::parse(args).and_then(diagnostic_workloads::run)
        }
        #[cfg(not(feature = "diagnostics"))]
        {
            Err("diagnostic workloads require the diagnostics feature")
        }
    } else if args
        .peek()
        .is_some_and(|name| name.starts_with("teststore-"))
    {
        #[cfg(feature = "deterministic")]
        {
            teststore::Config::parse(args).and_then(teststore::run)
        }
        #[cfg(not(feature = "deterministic"))]
        {
            Err("teststore workloads require the deterministic feature")
        }
    } else if args
        .peek()
        .is_some_and(|name| name.starts_with("document-keyed-rebind-"))
    {
        #[cfg(all(feature = "persistence", feature = "deterministic"))]
        {
            document_rebind::Config::parse(args).and_then(document_rebind::run)
        }
        #[cfg(not(all(feature = "persistence", feature = "deterministic")))]
        {
            Err("document rebind workloads require persistence and deterministic features")
        }
    } else if args
        .peek()
        .is_some_and(|name| name.starts_with("document-"))
    {
        #[cfg(feature = "persistence")]
        {
            document::Config::parse(args).and_then(document::run)
        }
        #[cfg(not(feature = "persistence"))]
        {
            Err("document workloads require the persistence feature")
        }
    } else if args
        .peek()
        .is_some_and(|name| name.starts_with("dependency-"))
    {
        dependencies::Config::parse(args).and_then(dependencies::run)
    } else if args.peek().is_some_and(|name| name.starts_with("issue-")) {
        issues::Config::parse(args).and_then(issues::run)
    } else if args.peek().is_some_and(|name| name.starts_with("observe-")) {
        observation::Config::parse(args).and_then(observation::run)
    } else if args
        .peek()
        .is_some_and(|name| name.starts_with("stack-retirement-"))
    {
        stack_retirement::Config::parse(args).and_then(stack_retirement::run)
    } else if args
        .peek()
        .is_some_and(|name| name.starts_with("keyed-scale-"))
    {
        keyed_scaling::Config::parse(args).and_then(keyed_scaling::run)
    } else if args.peek().is_some_and(|name| name.starts_with("keyed-")) {
        keyed::Config::parse(args).and_then(keyed::run)
    } else if args
        .peek()
        .is_some_and(|name| name.starts_with("shared-race-"))
    {
        shared_cache::contention::Config::parse(args).and_then(shared_cache::contention::run)
    } else if args.peek().is_some_and(|name| name.starts_with("shared-")) {
        shared_cache::Config::parse(args).and_then(shared_cache::run)
    } else {
        Config::parse(args).and_then(run)
    }
}

fn main() {
    match dispatch(std::env::args().skip(1)) {
        Ok(record) => println!("{record}"),
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use workloads::{Outcome, WorkFacts};

    /// Reuses bounded child execution for allocation and lock-reentry controls.
    pub(crate) mod allocation_child {
        include!("../../../rca-core/src/allocation_test/child.rs");

        /// Keeps the file-safety fixture's directory setup outside shared process support.
        #[cfg(feature = "allocations")]
        pub(crate) fn output(command: &mut Command) -> Output {
            capture(command, CHILD_TIMEOUT)
                .unwrap_or_else(|error| panic!("isolated cleanup test: {error}"))
        }

        /// Requires actual test execution after the caller checks its sentinel file.
        #[cfg(feature = "allocations")]
        pub(crate) fn require_one_test(output: &Output) {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            assert!(
                output.status.success() && passed_one_test(&stdout),
                "isolated cleanup test did not pass exactly once: {}\n{stdout}\n{stderr}",
                output.status,
            );
        }
    }

    /// Returns true only inside the selected bounded, isolated test.
    pub(crate) fn in_child(name: &str) -> bool {
        !allocation_child::run_in_child(name)
    }

    #[cfg(feature = "allocations")]
    #[test]
    fn allocation_child_rejects_empty_selection() {
        let result = std::panic::catch_unwind(|| in_child("missing::allocation_test"));
        let panic = result.expect_err("empty allocation selection must fail");
        let message = panic
            .downcast_ref::<String>()
            .expect("formatted diagnostic");
        assert!(message.starts_with(
            "isolated allocation test missing::allocation_test did not pass exactly once:"
        ));
    }

    fn config(name: &str, size: &str, iterations: &str) -> Config {
        Config::parse([name, size, iterations].into_iter().map(str::to_owned)).unwrap()
    }

    #[test]
    fn rejects_invalid_inputs_including_overflow() {
        for args in [
            vec![],
            vec!["store-native", "0"],
            vec!["unknown", "0", "1"],
            vec!["store-native", "1", "1"],
            vec!["effects-native", "0", "1"],
            vec!["store-native", "0", "0"],
            vec!["store-native", "-1", "1"],
            vec!["store-native", "+0", "1"],
            vec!["store-native", "0", "1.5"],
            vec!["store-native", "0", "1", "extra"],
            vec!["effects-native", "18446744073709551615", "2"],
            vec!["store-native", "0", "18446744073709551615"],
        ] {
            assert!(Config::parse(args.into_iter().map(str::to_owned)).is_err());
        }
    }

    #[test]
    fn rejects_missing_work_wrong_checksum_and_unreleased_owners() {
        let config = config("effects-native", "2", "3");
        let valid = Outcome {
            roots: 3,
            responses: 6,
            reductions: 9,
            causal_completed: 3,
            final_state: 9,
            checksum: 45,
            reducer_dropped: true,
            host_task_dropped: true,
            owners_released: true,
            state_extracted: true,
            host_stopped: true,
            facts: WorkFacts::default(),
        };
        valid.validate(config).unwrap();
        for broken in [
            Outcome { roots: 2, ..valid },
            Outcome {
                responses: 5,
                ..valid
            },
            Outcome {
                reductions: 8,
                ..valid
            },
            Outcome {
                causal_completed: 2,
                ..valid
            },
            Outcome {
                final_state: 8,
                ..valid
            },
            Outcome {
                checksum: 44,
                ..valid
            },
            Outcome {
                reducer_dropped: false,
                ..valid
            },
            Outcome {
                host_task_dropped: false,
                ..valid
            },
            Outcome {
                owners_released: false,
                ..valid
            },
            Outcome {
                state_extracted: false,
                ..valid
            },
            Outcome {
                host_stopped: false,
                ..valid
            },
        ] {
            assert!(
                broken.validate(config).is_err(),
                "accepted corrupt outcome: {broken:?}"
            );
        }
    }

    #[test]
    fn tiny_real_workloads_have_identical_exact_outcomes() {
        #[cfg(feature = "allocations")]
        if !in_child("tests::tiny_real_workloads_have_identical_exact_outcomes") {
            return;
        }
        for (name, size, roots, responses, reductions, checksum) in [
            ("store-native", "0", 3, 0, 3, 6),
            #[cfg(feature = "deterministic")]
            ("store-deterministic", "0", 3, 0, 3, 6),
            ("effects-native", "2", 3, 6, 9, 45),
            #[cfg(feature = "deterministic")]
            ("effects-deterministic", "2", 3, 6, 9, 45),
        ] {
            let output = run(config(name, size, "3")).unwrap();
            let record: serde_json::Value = serde_json::from_str(&output).unwrap();
            assert_eq!(record["workload"], name);
            assert_eq!(record["roots_committed"], roots);
            assert_eq!(record["responses_committed"], responses);
            assert_eq!(record["reducer_calls"], reductions);
            assert_eq!(record["final_state"], reductions);
            assert_eq!(record["checksum"], checksum);
            assert_eq!(record["causal_completed"], roots);
            assert_eq!(record["owners_released"], true);
            assert_eq!(record["host_stopped"], true);
        }
    }
}
