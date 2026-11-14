//! Checks typed task domains, retained values and transaction-gate compatibility.

#[test]
#[cfg_attr(miri, ignore = "compiler fixtures launch rustc")]
fn typed_tasks_keep_domain_and_retained_lifetime_boundaries() {
    let tests = trybuild::TestCases::new();
    tests.pass("tests/ui/rc05_send_only_domain.rs");
    tests.pass("tests/ui/rc05_owned_read_and_capture.rs");
    #[cfg(not(feature = "local"))]
    tests.compile_fail("tests/ui/rc05_wrong_state.rs");
    #[cfg(feature = "local")]
    tests.compile_fail("tests/ui/rc05_wrong_state_local.rs");
    tests.compile_fail("tests/ui/rc05_async_update.rs");
    tests.compile_fail("tests/ui/rc05_borrowed_capture.rs");
    #[cfg(not(feature = "local"))]
    tests.compile_fail("tests/ui/rc05_rc_task.rs");
    #[cfg(feature = "local")]
    tests.compile_fail("tests/ui/rc05_rc_task_local.rs");
    #[cfg(not(feature = "local"))]
    tests.compile_fail("tests/ui/rc05_rc_result.rs");
    #[cfg(feature = "local")]
    tests.compile_fail("tests/ui/rc05_rc_result_local.rs");
}

#[test]
#[cfg(feature = "test-util")]
#[cfg_attr(miri, ignore = "compiler fixtures launch rustc")]
fn typed_gate_rejects_the_wrong_state() {
    trybuild::TestCases::new().compile_fail("tests/ui/rc05_wrong_gate.rs");
}
