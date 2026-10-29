//! Checks composition, borrowing and shared transport boundaries at compilation.

#[test]
#[cfg_attr(miri, ignore = "trybuild launches rustc, which Miri isolation forbids")]
fn concurrent_boundaries_reject_non_send_values() {
    let tests = trybuild::TestCases::new();
    #[cfg(not(feature = "local"))]
    tests.compile_fail("tests/ui/non_send_state.rs");
    #[cfg(feature = "local")]
    tests.compile_fail("tests/ui/non_send_state_local.rs");
    #[cfg(not(feature = "local"))]
    tests.compile_fail("tests/ui/non_send_action.rs");
    #[cfg(feature = "local")]
    tests.compile_fail("tests/ui/non_send_action_local.rs");
    #[cfg(not(feature = "local"))]
    tests.compile_fail("tests/ui/non_send_selector.rs");
    #[cfg(feature = "local")]
    tests.compile_fail("tests/ui/non_send_selector_local.rs");
    #[cfg(not(feature = "local"))]
    tests.compile_fail("tests/ui/non_send_effect_future.rs");
    #[cfg(feature = "local")]
    tests.compile_fail("tests/ui/non_send_effect_future_local.rs");
    tests.compile_fail("tests/ui/borrowed_effect_capture.rs");
    tests.pass("tests/ui/move_only_send.rs");
    tests.pass("tests/ui/state_extraction_send.rs");
}

#[test]
#[cfg_attr(miri, ignore = "trybuild launches rustc, which Miri isolation forbids")]
fn local_reduction_keeps_transport_bounds_at_store_construction() {
    let tests = trybuild::TestCases::new();
    tests.pass("tests/ui/local_borrowed_reducer.rs");
    #[cfg(not(feature = "local"))]
    tests.compile_fail("tests/ui/non_send_reducer_store.rs");
    #[cfg(feature = "local")]
    tests.compile_fail("tests/ui/non_send_reducer_store_local.rs");
    tests.compile_fail("tests/ui/borrowed_reducer_store.rs");
}

#[test]
#[cfg_attr(miri, ignore = "trybuild launches rustc, which Miri isolation forbids")]
fn ergonomic_leaf_and_root_paths_keep_local_domains() {
    trybuild::TestCases::new().pass("tests/ui/ergonomic_local_reducer.rs");
}

#[test]
#[cfg_attr(miri, ignore = "trybuild launches rustc, which Miri isolation forbids")]
fn rc04_shared_scope_accepts_local_domains() {
    trybuild::TestCases::new().pass("tests/ui/rc04_local_shared_scope.rs");
}

#[test]
#[cfg_attr(miri, ignore = "trybuild launches rustc, which Miri isolation forbids")]
fn rc04_identified_and_lifecycle_scopes_accept_local_domains() {
    trybuild::TestCases::new().pass("tests/ui/rc04_local_identified_lifecycle.rs");
}

#[test]
#[cfg_attr(miri, ignore = "trybuild launches rustc, which Miri isolation forbids")]
fn rc04_composition_keeps_retained_and_hosted_boundaries() {
    let tests = trybuild::TestCases::new();
    #[cfg(not(feature = "local"))]
    tests.compile_fail("tests/ui/rc04_action_child_borrowed.rs");
    #[cfg(feature = "local")]
    tests.compile_fail("tests/ui/rc04_action_child_borrowed_local.rs");
    tests.compile_fail("tests/ui/rc04_action_parent_borrowed.rs");
    tests.compile_fail("tests/ui/rc04_mapper_borrowed.rs");
    #[cfg(not(feature = "local"))]
    tests.compile_fail("tests/ui/rc04_mapper_non_send.rs");
    #[cfg(feature = "local")]
    tests.compile_fail("tests/ui/rc04_mapper_non_send_local.rs");
    #[cfg(not(feature = "local"))]
    tests.compile_fail("tests/ui/rc04_mapper_non_sync.rs");
    #[cfg(feature = "local")]
    tests.compile_fail("tests/ui/rc04_mapper_non_sync_local.rs");
    #[cfg(not(feature = "local"))]
    tests.compile_fail("tests/ui/rc04_mapper_non_clone.rs");
    #[cfg(feature = "local")]
    tests.compile_fail("tests/ui/rc04_mapper_non_clone_local.rs");
    #[cfg(not(feature = "local"))]
    tests.compile_fail("tests/ui/rc04_id_borrowed.rs");
    #[cfg(feature = "local")]
    tests.compile_fail("tests/ui/rc04_id_borrowed_local.rs");
    #[cfg(not(feature = "local"))]
    tests.compile_fail("tests/ui/rc04_id_non_send.rs");
    #[cfg(feature = "local")]
    tests.compile_fail("tests/ui/rc04_id_non_send_local.rs");
    #[cfg(not(feature = "local"))]
    tests.compile_fail("tests/ui/rc04_id_non_sync.rs");
    #[cfg(feature = "local")]
    tests.compile_fail("tests/ui/rc04_id_non_sync_local.rs");
    #[cfg(not(feature = "local"))]
    tests.compile_fail("tests/ui/rc04_composed_store_non_send.rs");
    #[cfg(feature = "local")]
    tests.compile_fail("tests/ui/rc04_composed_store_non_send_local.rs");
    tests.compile_fail("tests/ui/rc04_composed_store_borrowed.rs");
}

#[test]
#[cfg_attr(miri, ignore = "trybuild launches rustc, which Miri isolation forbids")]
fn paths_accept_non_clone_actions() {
    let tests = trybuild::TestCases::new();
    tests.pass("tests/ui/path_action_clone.rs");
    tests.pass("tests/ui/if_some_non_clone_action.rs");
    tests.pass("tests/ui/if_case_identified_new_infers_id.rs");
    tests.compile_fail("tests/ui/if_case_path_mismatch.rs");
    tests.compile_fail("tests/ui/if_case_new_path_mismatch.rs");
}

#[test]
#[cfg_attr(miri, ignore = "trybuild launches rustc, which Miri isolation forbids")]
fn keyed_state_accepts_non_clone_state() {
    trybuild::TestCases::new().pass("tests/ui/keyed_non_clone_state.rs");
    trybuild::TestCases::new().pass("tests/ui/keyed_action_non_clone.rs");
    trybuild::TestCases::new().pass("tests/ui/for_each_non_clone_inference.rs");
    trybuild::TestCases::new().compile_fail("tests/ui/for_each_path_mismatch.rs");
}

#[test]
#[cfg_attr(miri, ignore = "trybuild launches rustc, which Miri isolation forbids")]
fn keyed_filtered_views_cannot_escape_or_outlive_mutation() {
    let tests = trybuild::TestCases::new();
    tests.pass("tests/ui/keyed_filtered_view_owned_projection.rs");
    tests.compile_fail("tests/ui/keyed_filtered_view_escape.rs");
    tests.compile_fail("tests/ui/keyed_filtered_view_mutation.rs");
}

#[test]
#[cfg_attr(miri, ignore = "trybuild launches rustc, which Miri isolation forbids")]
fn combine_rejects_mismatched_state() {
    #[cfg(not(feature = "local"))]
    trybuild::TestCases::new().compile_fail("tests/ui/combine_mismatched_state.rs");
    #[cfg(feature = "local")]
    trybuild::TestCases::new().compile_fail("tests/ui/combine_mismatched_state_local.rs");
}

#[test]
#[cfg_attr(miri, ignore = "trybuild launches rustc, which Miri isolation forbids")]
fn dependency_context_does_not_escape_override() {
    trybuild::TestCases::new().compile_fail("tests/ui/dependency_context_escape.rs");
}

#[test]
#[cfg_attr(miri, ignore = "trybuild launches rustc, which Miri isolation forbids")]
fn dependency_fixture_borrow_does_not_escape() {
    trybuild::TestCases::new().compile_fail("tests/ui/dependency_fixture_borrow_escape.rs");
}

#[test]
#[cfg_attr(miri, ignore = "trybuild launches rustc, which Miri isolation forbids")]
fn shared_scope_accepts_move_only_domains_and_rejects_borrow_escape() {
    let tests = trybuild::TestCases::new();
    tests.pass("tests/ui/shared_move_only.rs");
    tests.compile_fail("tests/ui/shared_overlapping_split.rs");
    tests.compile_fail("tests/ui/shared_private_borrow_escape.rs");
    tests.compile_fail("tests/ui/shared_settings_borrow_escape.rs");
}

#[test]
#[cfg_attr(miri, ignore = "trybuild launches rustc, which Miri isolation forbids")]
fn keyed_shared_accepts_move_only_domains_and_rejects_overlapping_split() {
    let tests = trybuild::TestCases::new();
    tests.pass("tests/ui/shared_keyed_move_only.rs");
    tests.compile_fail("tests/ui/shared_keyed_overlapping_split.rs");
}

#[test]
#[cfg_attr(miri, ignore = "trybuild launches rustc, which Miri isolation forbids")]
fn owned_shared_preserves_read_only_borrow_and_transport_boundaries() {
    let tests = trybuild::TestCases::new();
    tests.compile_fail("tests/ui/owned_shared_reader_write.rs");
    tests.compile_fail("tests/ui/owned_shared_await_borrow.rs");
    tests.compile_fail("tests/ui/owned_shared_local_send.rs");
    tests.compile_fail("tests/ui/owned_shared_update_reader_write.rs");
    tests.compile_fail("tests/ui/owned_shared_update_await_borrow.rs");
}

#[test]
#[cfg(feature = "diagnostics")]
#[cfg_attr(miri, ignore = "trybuild launches rustc, which Miri isolation forbids")]
fn diagnostic_change_cannot_escape_its_sink() {
    trybuild::TestCases::new().compile_fail("tests/ui/diagnostic_change_escape.rs");
}

#[test]
#[cfg(feature = "diagnostics")]
#[cfg_attr(miri, ignore = "trybuild launches rustc, which Miri isolation forbids")]
fn diagnostic_callbacks_can_borrow_local_captures() {
    trybuild::TestCases::new().pass("tests/ui/diagnostic_local_callbacks.rs");
}

#[test]
#[cfg_attr(miri, ignore = "compiler fixtures launch rustc")]
fn public_paths_exclude_implementation_and_duplicate_aliases() {
    let tests = trybuild::TestCases::new();
    tests.compile_fail("tests/ui/private_implementation_paths.rs");
    #[cfg(feature = "diagnostics")]
    tests.compile_fail("tests/ui/private_diagnostic_root_aliases.rs");
    #[cfg(feature = "test-util")]
    tests.compile_fail("tests/ui/private_testing_root_aliases.rs");
    #[cfg(feature = "local")]
    tests.compile_fail("tests/ui/private_local_shared_aliases.rs");
}
