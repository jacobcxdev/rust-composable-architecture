//! Literal output checks and rejection controls for both physical-release orders.
//!
//! Each fault runs through the workload and its cleanup. The original rejection
//! must survive, and cleanup must release both schedules' owners.
use super::*;

fn config() -> Config {
    Config::parse(
        ["document-keyed-rebind-deterministic", "2", "1"]
            .into_iter()
            .map(str::to_owned),
    )
    .unwrap()
}

/// Runs DHAT measurements in an isolated child; ordinary controls stay in process.
fn child(name: &str) -> bool {
    #[cfg(feature = "allocations")]
    {
        crate::tests::in_child(name)
    }
    #[cfg(not(feature = "allocations"))]
    {
        let _ = name;
        true
    }
}

// Catches silently accepting a different workload or unbounded dimensions.
#[test]
fn rejects_invalid_dimensions_before_construction() {
    for args in [
        vec![],
        vec!["document-keyed-rebind-native", "2", "1"],
        vec!["document-keyed-rebind-deterministic", "0", "1"],
        vec!["document-keyed-rebind-deterministic", "2", "0"],
        vec!["document-keyed-rebind-deterministic", "8", "1"],
        vec!["document-keyed-rebind-deterministic", "2", "2"],
        vec!["document-keyed-rebind-deterministic", "2"],
        vec!["document-keyed-rebind-deterministic", "2", "1", "extra"],
        vec![
            "document-keyed-rebind-deterministic",
            "18446744073709551616",
            "1",
        ],
    ] {
        assert!(Config::parse(args.into_iter().map(str::to_owned)).is_err());
    }
}

// Catches missing replacement, extra loads/saves, stale publication or conflated joins.
#[cfg_attr(not(feature = "allocations"), test)]
pub(crate) fn both_orders_preserve_binding_sibling_and_independent_debt() {
    if !child(
        "document_rebind::controls::both_orders_preserve_binding_sibling_and_independent_debt",
    ) {
        return;
    }
    let json: serde_json::Value = serde_json::from_str(&run(config()).unwrap()).unwrap();
    assert_eq!(json["schedules"], 2);
    assert_eq!(json["cold_loads"], 4);
    assert_eq!(json["work_loads"], 6);
    assert_eq!(json["loads"], 10);
    assert_eq!(json["saves"], 0);
    assert_eq!(json["cancellations"], 2);
    assert_eq!(json["fresh_value"], 42);
    assert_eq!(json["fresh_revision"], 2);
    assert_eq!(json["sibling_value"], 23);
    assert_eq!(json["sibling_revision"], 3);
    assert_eq!(json["retained_old_value"], 7);
    assert_eq!(json["physical_loads_created"], 10);
    assert_eq!(json["physical_loads_released"], 10);
    assert_eq!(json["deliveries_created"], 10);
    assert_eq!(json["deliveries_dropped"], 10);
    assert_eq!(json["producer_joins_created"], 2);
    assert_eq!(json["producer_joins_dropped"], 2);
    assert_eq!(json["pending_physical"], 0);
    assert_eq!(json["owners_released"], true);
    assert_eq!(json["host_task_dropped"], true);
    #[cfg(feature = "allocations")]
    {
        assert!(json["allocations"]["phases"]["construction"].is_object());
        assert!(json["allocations"]["phases"]["cold"].is_object());
        assert!(json["allocations"]["phases"]["work"].is_object());
        assert!(json["allocations"]["phases"]["validation"].is_object());
        assert!(json["allocations"]["phases"]["teardown"].is_object());
        assert_eq!(
            json["allocations"]["executor_thread_local"]["retained_blocks"],
            0
        );
    }
    #[cfg(not(feature = "allocations"))]
    assert!(json["allocations"].is_null());
}

// Each mutation exercises the actual workload; successful cleanup is independently required.
#[cfg_attr(not(feature = "allocations"), test)]
pub(crate) fn rejects_oracle_faults_and_releases_both_gates() {
    if !child("document_rebind::controls::rejects_oracle_faults_and_releases_both_gates") {
        return;
    }
    for (fault, expected) in [
        (Control::WrongFreshValue, "rebind fresh value invalid"),
        (
            Control::ChangeSibling,
            "rebind sibling changed without its own action",
        ),
        (
            Control::ReleaseBoth,
            "rebind close finished before independent release",
        ),
        (Control::RetainBackend, "rebind backend owner retained"),
    ] {
        for schedule in 0..2 {
            let mut controls = [Control::None; 2];
            controls[schedule] = fault;
            let error = run_inner(config(), controls).unwrap_err();
            assert_eq!(error.original, expected, "schedule {schedule}");
            assert_eq!(error.cleanup, None, "schedule {schedule}: {error:?}");
        }
    }
}
