//! Checks that local operations own requests without crossing threads.

#![cfg(feature = "local")]

#[test]
#[cfg_attr(miri, ignore = "compiler fixtures launch rustc")]
fn local_operation_frontend_keeps_requests_on_the_owner_thread() {
    let tests = trybuild::TestCases::new();
    tests.pass("tests/ui/rc10_local_operation_rc.rs");
    tests.compile_fail("tests/ui/rc10_local_operation_send.rs");
}
