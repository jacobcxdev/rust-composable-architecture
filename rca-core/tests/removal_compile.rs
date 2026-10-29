//! Turn authority cannot escape, while plain owned data remains unconstrained.

#[test]
#[cfg_attr(miri, ignore = "compiler fixtures launch rustc")]
fn removed_child_keeps_turn_and_owned_output_boundaries() {
    let tests = trybuild::TestCases::new();
    tests.pass("tests/ui/rc09_removed_owned_data.rs");
    #[cfg(not(feature = "local"))]
    tests.compile_fail("tests/ui/rc09_removed_escape.rs");
    #[cfg(feature = "local")]
    tests.compile_fail("tests/ui/rc09_removed_escape_local.rs");
    #[cfg(not(feature = "local"))]
    tests.compile_fail("tests/ui/rc09_removed_error_escape.rs");
    #[cfg(feature = "local")]
    tests.compile_fail("tests/ui/rc09_removed_error_escape_local.rs");
    #[cfg(not(feature = "local"))]
    tests.compile_fail("tests/ui/rc09_removed_shorten.rs");
    #[cfg(feature = "local")]
    tests.compile_fail("tests/ui/rc09_removed_shorten_local.rs");
    #[cfg(not(feature = "local"))]
    tests.compile_fail("tests/ui/rc09_removed_rc_receive.rs");
    #[cfg(feature = "local")]
    tests.compile_fail("tests/ui/rc09_removed_rc_receive_local.rs");
    tests.compile_fail("tests/ui/rc09_removed_borrowed_output.rs");
}
