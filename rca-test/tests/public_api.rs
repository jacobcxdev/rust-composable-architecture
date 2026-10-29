//! Guards source compatibility of public diagnostic values.

use rca_core::testing::RuntimeSnapshot;

#[test]
fn runtime_snapshot_remains_constructible_with_alpha_one_fields() {
    // The struct literal is the check: adding a required field breaks this consumer.
    let _snapshot = RuntimeSnapshot {
        commit_version: 0,
        live_task_ids: Vec::new(),
        cancellation_count: 0,
        timer_deadlines: Vec::new(),
        observation_count: 0,
    };
}
