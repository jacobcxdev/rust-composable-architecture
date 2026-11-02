use std::process::Command;
use std::time::Duration;

mod support;

// Missing any step of the real editor-to-document chain, repeating the equal
// edit, changing the sibling, or retaining an owner must reject this outcome.
#[test]
#[cfg(all(feature = "persistence", feature = "deterministic"))]
fn ordinary_save_has_exact_joint_work_and_releases_owners() {
    let output = support::output(
        Command::new(env!("CARGO_BIN_EXE_rca-performance-profile")).args([
            "joint-document-deterministic",
            "2",
            "1",
        ]),
        Duration::from_secs(60),
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let record: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        record["schema"],
        "rca-joint-document-performance-profile-v1"
    );
    assert_eq!(record["workload"], "joint-document-deterministic");
    assert_eq!(record["private_values"], serde_json::json!([8, 20]));
    assert_eq!(record["shared_values"], serde_json::json!([8, 20]));
    assert_eq!(record["backend_values"], serde_json::json!([8, 20]));
    assert_eq!(record["observed"], serde_json::json!([[7, 20], [8, 20]]));
    for (field, expected) in [
        ("loads", 2),
        ("saves", 1),
        ("writes", 1),
        ("dependency_lookups", 3),
        ("cache_hits", 1),
        ("cache_hit_seed_calls", 0),
        ("changed_edits", 1),
        ("equal_edits", 1),
        ("unrelated_actions", 1),
        ("on_change_reactions", 1),
        ("timer_actions", 1),
        ("save_success_actions", 1),
        ("issues", 1),
        ("physical_created", 3),
        ("physical_completed", 3),
        ("delivery_dropped", 3),
        ("backend_closes", 2),
        ("selector_calls", 3),
        ("parent_reductions", 9),
        ("child_reductions", 8),
        ("document_reductions", 10),
        ("pending_physical", 0),
    ] {
        assert_eq!(record[field], expected, "{field}");
    }
    for field in ["owners_released", "host_task_dropped", "causal_finished"] {
        assert_eq!(record[field], true, "{field}");
    }
    #[cfg(feature = "allocations")]
    for phase in ["construction", "cold", "work", "validation", "teardown"] {
        assert!(
            record["allocations"]["phases"][phase].is_object(),
            "{phase}"
        );
    }
    #[cfg(not(feature = "allocations"))]
    assert!(record["allocations"].is_null());
}

// The only supported fixture is a single ordinary-save cycle with two rows.
#[test]
fn unsupported_joint_dimensions_emit_no_success_record() {
    for args in [
        vec!["joint-document-deterministic"],
        vec!["joint-document-deterministic", "1", "1"],
        vec!["joint-document-deterministic", "2", "0"],
        vec!["joint-document-deterministic", "2", "2"],
        vec!["joint-document-deterministic", "2", "1", "extra"],
        vec!["joint-document-native", "2", "1"],
    ] {
        let output = support::output(
            Command::new(env!("CARGO_BIN_EXE_rca-performance-profile")).args(args),
            Duration::from_secs(60),
        );
        assert_eq!(output.status.code(), Some(1));
        assert!(output.stdout.is_empty());
        assert!(!output.stderr.is_empty());
    }
    if !cfg!(all(feature = "persistence", feature = "deterministic")) {
        let output = support::output(
            Command::new(env!("CARGO_BIN_EXE_rca-performance-profile")).args([
                "joint-document-deterministic",
                "2",
                "1",
            ]),
            Duration::from_secs(60),
        );
        assert_eq!(output.status.code(), Some(1));
        assert!(output.stdout.is_empty());
        assert!(
            String::from_utf8_lossy(&output.stderr)
                .contains("persistence and deterministic features")
        );
    }
}
