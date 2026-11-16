//! Checks finite controlled Store work and rejects unavailable profiling graphs.

use std::process::Command;
use std::time::Duration;

/// Supervises each CLI process with a fixed timeout.
mod support;

/// Runs the actual executable with the existing bounded child supervisor.
fn output(args: &[&str]) -> std::process::Output {
    support::output(
        Command::new(env!("CARGO_BIN_EXE_controlled_cost")).args(args),
        Duration::from_secs(30),
    )
}

#[cfg(not(any(
    feature = "deterministic",
    feature = "diagnostics",
    feature = "allocations",
    feature = "persistence",
    feature = "local"
)))]
/// Checks the production graph independently of feature-rejection cases.
mod production_graph {
    use super::output;

    /// Requires real work, literal accounting and complete physical owner release.
    fn record(args: &[&str]) -> serde_json::Value {
        let result = output(args);
        assert!(
            result.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert_eq!(result.stderr, [] as [u8; 0]);
        let record: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
        assert_eq!(record["schema"], "rca-controlled-cost-v1");
        assert_eq!(record["runtime"], "manual");
        assert_eq!(record["boundary"], "profile_controlled_work");
        assert_eq!(record["scenario"], args[0]);
        assert_eq!(record["payload_bytes"], 65_536);
        assert_eq!(record["driver_empty"], true);
        assert_eq!(record["owners_released"], true);
        assert_eq!(record["runtime_abandonments"], 0);
        record
    }

    #[test]
    fn finite_schedule_matrix_has_literal_work_and_poll_counts() {
        // These literals catch skipped/extra actor turns, lost actions and wrong ordering.
        for (scenario, actors, clients, commits, checksum, order) in [
            ("admission-empty-ab", 3, 9, 2, 3, 12),
            ("admission-empty-ba", 3, 9, 2, 3, 21),
            ("admission-full-ab", 4, 10, 2, 3, 12),
            ("admission-full-ba", 4, 10, 2, 3, 21),
            ("graceful-idle", 2, 3, 0, 0, 0),
            ("graceful-queued", 2, 9, 2, 3, 12),
            ("abortive-unpolled", 1, 1, 0, 0, 0),
            ("abortive-idle", 2, 1, 0, 0, 0),
            ("abortive-query", 3, 3, 0, 0, 0),
        ] {
            let row = record(&[scenario, "1", "0"]);
            for (field, expected) in [
                ("iterations", 1),
                ("actor_polls", actors),
                ("client_polls", clients),
                ("commits", commits),
                ("checksum", checksum),
                ("order_checksum", order),
                ("state_drops", 1),
                ("reducer_drops", 1),
                ("actor_drops", 1),
                ("spawns", 1),
                ("sensitivity", 0),
                ("sensitivity_checksum", 0),
            ] {
                assert_eq!(row[field], expected, "{scenario}: {field}");
            }
        }
    }

    #[test]
    fn repeated_fixtures_do_not_inherit_actors_or_cleanup() {
        // Later fixtures must repeat the full lifecycle, rather than reusing a Store.
        for (scenario, actors, clients, commits, checksum, order) in [
            ("admission-empty-ab", 9, 27, 6, 9, 36),
            ("admission-empty-ba", 9, 27, 6, 9, 63),
            ("admission-full-ab", 12, 30, 6, 9, 36),
            ("admission-full-ba", 12, 30, 6, 9, 63),
            ("graceful-idle", 6, 9, 0, 0, 0),
            ("graceful-queued", 6, 27, 6, 9, 36),
            ("abortive-unpolled", 3, 3, 0, 0, 0),
            ("abortive-idle", 6, 3, 0, 0, 0),
            ("abortive-query", 9, 9, 0, 0, 0),
        ] {
            let row = record(&[scenario, "3", "0"]);
            for (field, expected) in [
                ("iterations", 3),
                ("actor_polls", actors),
                ("client_polls", clients),
                ("commits", commits),
                ("checksum", checksum),
                ("order_checksum", order),
                ("state_drops", 3),
                ("reducer_drops", 3),
                ("actor_drops", 3),
                ("spawns", 3),
            ] {
                assert_eq!(row[field], expected, "{scenario}: {field}");
            }
        }
    }

    #[test]
    fn sensitivity_adds_checked_arithmetic_without_changing_store_work() {
        for scenario in [
            "admission-empty-ab",
            "admission-full-ab",
            "graceful-queued",
            "abortive-query",
        ] {
            let ordinary = record(&[scenario, "3", "0"]);
            let sensitive = record(&[scenario, "3", "64"]);
            assert_eq!(sensitive["sensitivity"], 64);
            assert_eq!(sensitive["sensitivity_checksum"], 6240);
            for field in [
                "iterations",
                "actor_polls",
                "client_polls",
                "commits",
                "checksum",
                "order_checksum",
                "state_drops",
                "reducer_drops",
                "actor_drops",
                "spawns",
            ] {
                assert_eq!(ordinary[field], sensitive[field], "{scenario}: {field}");
            }
        }
    }

    #[test]
    fn maximum_iteration_bound_completes_every_fixture() {
        let row = record(&["graceful-idle", "256", "64"]);
        for (field, expected) in [
            ("iterations", 256),
            ("actor_polls", 512),
            ("client_polls", 768),
            ("commits", 0),
            ("state_drops", 256),
            ("reducer_drops", 256),
            ("actor_drops", 256),
            ("spawns", 256),
            ("sensitivity_checksum", 532_480),
        ] {
            assert_eq!(row[field], expected, "{field}");
        }
    }
}

#[test]
fn invalid_arguments_never_emit_a_success_record() {
    for args in [
        vec![],
        vec!["unknown", "1", "0"],
        vec!["graceful-idle", "0", "0"],
        vec!["graceful-idle", "257", "0"],
        vec!["graceful-idle", "+1", "0"],
        vec!["graceful-idle", " 1", "0"],
        vec!["graceful-idle", "18446744073709551616", "0"],
        vec!["graceful-idle", "1", "1"],
        vec!["graceful-idle", "1", "65"],
        vec!["graceful-idle", "1", "0", "extra"],
    ] {
        let result = output(&args);
        assert!(!result.status.success(), "{args:?}");
        assert!(result.stdout.is_empty(), "{args:?}");
        assert!(!result.stderr.is_empty(), "{args:?}");
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
fn nonproduction_feature_graph_fails_before_work() {
    let result = output(&["graceful-idle", "1", "0"]);
    assert!(!result.status.success());
    assert_eq!(result.stdout, [] as [u8; 0]);
    assert_ne!(result.stderr, [] as [u8; 0]);
}
