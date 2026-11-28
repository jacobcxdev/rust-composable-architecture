//! Checks local removal authority without imposing Send on owned data.

#![cfg(feature = "local")]

#[test]
#[cfg_attr(miri, ignore = "compiler fixtures launch rustc")]
fn local_removal_keeps_turn_authority_but_accepts_non_send_data() {
    let tests = trybuild::TestCases::new();
    tests.pass("tests/ui/rc09_local_removed_rc.rs");
    tests.compile_fail("tests/ui/rc09_local_removed_escape.rs");
    tests.compile_fail("tests/ui/rc09_local_removed_shorten.rs");
}
