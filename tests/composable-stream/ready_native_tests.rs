use super::{run, Fault};

#[test]
fn ready_future_native_workload_has_exact_results_and_ownership() {
    if crate::child_process::run_in_child(
        "ready::tests::ready_future_native_workload_has_exact_results_and_ownership",
    ) {
        return;
    }

    let (trace, _) = run(Fault::None).expect("valid native ready-future workload");
    assert_eq!(
        (
            trace.attempted,
            trace.completed,
            trace.results,
            trace.checksum,
            trace.errors
        ),
        (64, 64, 64, 6496, 0)
    );
}

#[test]
fn ready_future_native_wrong_identity_is_rejected() {
    if crate::child_process::run_in_child(
        "ready::tests::ready_future_native_wrong_identity_is_rejected",
    ) {
        return;
    }

    run(Fault::WrongId).expect_err("wrong native output identity");
}

#[test]
fn ready_future_native_wrong_value_is_rejected() {
    if crate::child_process::run_in_child(
        "ready::tests::ready_future_native_wrong_value_is_rejected",
    ) {
        return;
    }

    run(Fault::WrongValue).expect_err("wrong native output value");
}

#[test]
fn ready_future_native_duplicate_result_is_rejected() {
    if crate::child_process::run_in_child(
        "ready::tests::ready_future_native_duplicate_result_is_rejected",
    ) {
        return;
    }

    run(Fault::Duplicate).expect_err("duplicate native reducer output");
}
