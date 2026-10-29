//! Owned projection and profile transfer boundaries for transaction reactions.

#[test]
#[cfg_attr(miri, ignore = "compiler fixtures launch rustc")]
fn changed_reactions_keep_only_owned_projection_and_profile_bounds() {
    let tests = trybuild::TestCases::new();
    tests.pass("tests/ui/rc08_reactions_shared.rs");
    #[cfg(feature = "local")]
    tests.pass("tests/ui/rc08_reactions_local.rs");
    tests.compile_fail("tests/ui/rc08_reactions_borrowed_projection.rs");
    #[cfg(not(feature = "local"))]
    tests.compile_fail("tests/ui/rc08_reactions_shared_callback.rs");
    #[cfg(feature = "local")]
    tests.compile_fail("tests/ui/rc08_reactions_shared_callback_local.rs");
    #[cfg(all(not(feature = "local"), not(feature = "diagnostics")))]
    tests.compile_fail("tests/ui/rc08_reactions_shared_task.rs");
    #[cfg(all(feature = "local", not(feature = "diagnostics")))]
    tests.compile_fail("tests/ui/rc08_reactions_shared_task_local.rs");
    #[cfg(all(not(feature = "local"), feature = "diagnostics"))]
    tests.compile_fail("tests/ui/rc08_reactions_shared_task_diagnostics.rs");
    #[cfg(all(feature = "local", feature = "diagnostics"))]
    tests.compile_fail("tests/ui/rc08_reactions_shared_task_local_diagnostics.rs");
}
