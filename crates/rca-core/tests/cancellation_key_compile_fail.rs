//! Checks the actual effect transport boundary for synchronous lock guards.

#[test]
#[cfg_attr(miri, ignore = "trybuild launches rustc, which Miri isolation forbids")]
fn effect_transport_rejects_held_guards_but_accepts_owned_results() {
    let tests = trybuild::TestCases::new();
    #[cfg(not(feature = "local"))]
    tests.compile_fail("tests/ui/cancellation_mutex_guard_await.rs");
    #[cfg(feature = "local")]
    tests.compile_fail("tests/ui/cancellation_mutex_guard_await_local.rs");
    tests.pass("tests/ui/cancellation_mutex_guard_released.rs");
}
