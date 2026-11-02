//! Lists the measured controls for the synchronous allocation host.

/// Retains original names, child markers and feature boundaries.
pub(crate) const CASES: &[(&str, fn())] = &[
    (
        "dependencies::tests::allocation_failure_cleanup_and_retention_controls",
        crate::dependencies::tests::allocation_failure_cleanup_and_retention_controls,
    ),
    #[cfg(feature = "diagnostics")]
    (
        "diagnostic_workloads::tests::observed_seed_emits_complete_exact_work_record",
        crate::diagnostic_workloads::tests::observed_seed_emits_complete_exact_work_record,
    ),
    #[cfg(feature = "diagnostics")]
    (
        "diagnostic_workloads::tests::explicit_disabled_seed_has_no_diagnostic_work_or_callback_owners",
        crate::diagnostic_workloads::tests::explicit_disabled_seed_has_no_diagnostic_work_or_callback_owners,
    ),
    #[cfg(feature = "persistence")]
    (
        "document::controls::missing_and_present_clean_load",
        crate::document::controls::missing_and_present_clean_load,
    ),
    #[cfg(feature = "persistence")]
    (
        "document::controls::single_edit_bursts_have_one_save_and_no_omitted_receipts",
        crate::document::controls::single_edit_bursts_have_one_save_and_no_omitted_receipts,
    ),
    #[cfg(feature = "persistence")]
    (
        "document::controls::load_decode_failures_require_explicit_retry",
        crate::document::controls::load_decode_failures_require_explicit_retry,
    ),
    #[cfg(feature = "persistence")]
    (
        "document::controls::encode_failure_latest_close_retry_fences_edits",
        crate::document::controls::encode_failure_latest_close_retry_fences_edits,
    ),
    #[cfg(feature = "persistence")]
    (
        "document::controls::save_failure_old_barrier_and_latest_flush_retry",
        crate::document::controls::save_failure_old_barrier_and_latest_flush_retry,
    ),
    #[cfg(feature = "persistence")]
    (
        "document::controls::receipt_ownership_does_not_cancel_physical_save",
        crate::document::controls::receipt_ownership_does_not_cancel_physical_save,
    ),
    #[cfg(feature = "persistence")]
    (
        "document::controls::captured_barrier_precedes_newer_acknowledgement",
        crate::document::controls::captured_barrier_precedes_newer_acknowledgement,
    ),
    #[cfg(feature = "persistence")]
    (
        "document::controls::cancellation_abort_external_join_then_new_document",
        crate::document::controls::cancellation_abort_external_join_then_new_document,
    ),
    #[cfg(feature = "persistence")]
    (
        "document::controls::abort_and_shutdown_are_not_flush",
        crate::document::controls::abort_and_shutdown_are_not_flush,
    ),
    #[cfg(feature = "persistence")]
    (
        "document::controls::broken_wait_byte_task_and_owner_gates_release_every_guard",
        crate::document::controls::broken_wait_byte_task_and_owner_gates_release_every_guard,
    ),
    #[cfg(feature = "persistence")]
    (
        "document::controls::broken_outcome_intermediate_and_capture_gates_release_owners",
        crate::document::controls::broken_outcome_intermediate_and_capture_gates_release_owners,
    ),
    #[cfg(all(feature = "persistence", feature = "deterministic"))]
    (
        "document_rebind::controls::both_orders_preserve_binding_sibling_and_independent_debt",
        crate::document_rebind::controls::both_orders_preserve_binding_sibling_and_independent_debt,
    ),
    #[cfg(all(feature = "persistence", feature = "deterministic"))]
    (
        "document_rebind::controls::rejects_oracle_faults_and_releases_both_gates",
        crate::document_rebind::controls::rejects_oracle_faults_and_releases_both_gates,
    ),
    (
        "feature_transactions::tests::rejects_skipped_work_and_retained_owner",
        crate::feature_transactions::tests::rejects_skipped_work_and_retained_owner,
    ),
    (
        "feature_transactions::tests::root_and_mounted_dispatch_complete_one_and_sixty_four_increments",
        crate::feature_transactions::tests::root_and_mounted_dispatch_complete_one_and_sixty_four_increments,
    ),
    #[cfg(feature = "deterministic")]
    (
        "host::tests::ready_releases_owned_waker_on_failure",
        crate::host::tests::ready_releases_owned_waker_on_failure,
    ),
    #[cfg(feature = "deterministic")]
    (
        "host::tests::ready_rejects_retained_waker_on_failure",
        crate::host::tests::ready_rejects_retained_waker_on_failure,
    ),
    #[cfg(feature = "deterministic")]
    (
        "host::tests::ready_allocation_teardown_is_observed",
        crate::host::tests::ready_allocation_teardown_is_observed,
    ),
    #[cfg(feature = "deterministic")]
    (
        "host::tests::ready_rejects_retained_unrelated_owner",
        crate::host::tests::ready_rejects_retained_unrelated_owner,
    ),
    (
        "issues::tests::retained_owned_issue_rejects_cache_none",
        crate::issues::tests::retained_owned_issue_rejects_cache_none,
    ),
    (
        "issues::tests::clean_owned_issue_qualifies_cache_none",
        crate::issues::tests::clean_owned_issue_qualifies_cache_none,
    ),
    (
        "keyed_scaling::tests::keyed_scaling_real_trace_preserves_order_and_exact_lifetimes",
        crate::keyed_scaling::tests::keyed_scaling_real_trace_preserves_order_and_exact_lifetimes,
    ),
    (
        "keyed_scaling::tests::keyed_scaling_large_widths_retain_exact_final_rows",
        crate::keyed_scaling::tests::keyed_scaling_large_widths_retain_exact_final_rows,
    ),
    (
        "keyed_scaling::tests::keyed_scaling_replacement_wraps_with_exact_generations",
        crate::keyed_scaling::tests::keyed_scaling_replacement_wraps_with_exact_generations,
    ),
    #[cfg(feature = "deterministic")]
    (
        "keyed_scaling::tests::keyed_scaling_deterministic_matches_native_trace",
        crate::keyed_scaling::tests::keyed_scaling_deterministic_matches_native_trace,
    ),
    (
        "lifecycle::tests::exact_work_rejects_skipped_action_and_retained_owner",
        crate::lifecycle::tests::exact_work_rejects_skipped_action_and_retained_owner,
    ),
    (
        "lifecycle::tests::native_dispatch_validates_real_work_and_terminal_release",
        crate::lifecycle::tests::native_dispatch_validates_real_work_and_terminal_release,
    ),
    (
        "tests::tiny_real_workloads_have_identical_exact_outcomes",
        crate::tests::tiny_real_workloads_have_identical_exact_outcomes,
    ),
    (
        "tests::allocation_child_rejects_empty_selection",
        crate::tests::allocation_child_rejects_empty_selection,
    ),
    (
        "measurement::tests::early_error_preserves_caller_file_and_releases_session",
        crate::measurement::tests::early_error_preserves_caller_file_and_releases_session,
    ),
    (
        "measurement::tests::unwind_preserves_caller_file_and_releases_session",
        crate::measurement::tests::unwind_preserves_caller_file_and_releases_session,
    ),
    (
        "measurement::tests::attributes_known_allocations_and_drops",
        crate::measurement::tests::attributes_known_allocations_and_drops,
    ),
    (
        "measurement::tests::attributes_known_allocations_across_warmup_boundary",
        crate::measurement::tests::attributes_known_allocations_across_warmup_boundary,
    ),
    (
        "measurement::tests::attributes_known_allocations_across_cold_boundary",
        crate::measurement::tests::attributes_known_allocations_across_cold_boundary,
    ),
    (
        "measurement::tests::rejects_unexplained_live_allocations",
        crate::measurement::tests::rejects_unexplained_live_allocations,
    ),
    (
        "measurement::tests::empty_measurement_does_not_allocate",
        crate::measurement::tests::empty_measurement_does_not_allocate,
    ),
    (
        "measurement::tests::rejects_cache_sized_unrelated_owner_after_clean_session",
        crate::measurement::tests::rejects_cache_sized_unrelated_owner_after_clean_session,
    ),
    (
        "measurement::tests::rejects_cold_cache_sized_unrelated_owner",
        crate::measurement::tests::rejects_cold_cache_sized_unrelated_owner,
    ),
    (
        "measurement::tests::file_safe_child_rejects_empty_selection",
        crate::measurement::tests::file_safe_child_rejects_empty_selection,
    ),
    #[cfg(all(feature = "deterministic", feature = "local"))]
    (
        "mixed_pool::tests::capacity_one_pool_completes_only_the_captured_work",
        crate::mixed_pool::tests::capacity_one_pool_completes_only_the_captured_work,
    ),
    #[cfg(all(feature = "deterministic", feature = "local"))]
    (
        "mixed_pool::tests::rejects_skipped_admission_and_retained_concrete_work",
        crate::mixed_pool::tests::rejects_skipped_admission_and_retained_concrete_work,
    ),
    #[cfg(all(feature = "deterministic", feature = "local"))]
    (
        "mixed_service::tests::one_session_completes_exact_mounted_work",
        crate::mixed_service::tests::one_session_completes_exact_mounted_work,
    ),
    #[cfg(all(feature = "deterministic", feature = "local"))]
    (
        "mixed_service::tests::sixty_four_reused_sessions_change_only_delivery_work",
        crate::mixed_service::tests::sixty_four_reused_sessions_change_only_delivery_work,
    ),
    #[cfg(all(feature = "deterministic", feature = "local"))]
    (
        "mixed_service::tests::rejects_skipped_update_and_retained_owner",
        crate::mixed_service::tests::rejects_skipped_update_and_retained_owner,
    ),
    (
        "observation_controls::real_observation_owner_retention_is_rejected_in_fresh_child",
        crate::observation_controls::real_observation_owner_retention_is_rejected_in_fresh_child,
    ),
    #[cfg(feature = "deterministic")]
    (
        "owned_snapshot::tests::rejects_skipped_capture_and_retained_authority",
        crate::owned_snapshot::tests::rejects_skipped_capture_and_retained_authority,
    ),
    #[cfg(feature = "deterministic")]
    (
        "owned_snapshot::tests::owned_history_captures_private_mutation_without_retaining_authority",
        crate::owned_snapshot::tests::owned_history_captures_private_mutation_without_retaining_authority,
    ),
    (
        "stack_retirement::tests::stack_retirement_preserves_prefix_and_rejects_retired_authority",
        crate::stack_retirement::tests::stack_retirement_preserves_prefix_and_rejects_retired_authority,
    ),
    #[cfg(feature = "deterministic")]
    (
        "teststore::tests::mismatch_guard_rejection_preserves_error_owners_and_hook",
        crate::teststore::tests::mismatch_guard_rejection_preserves_error_owners_and_hook,
    ),
    #[cfg(feature = "deterministic")]
    (
        "teststore::tests::mismatch_validation_unwind_preserves_payload_owners_and_hook",
        crate::teststore::tests::mismatch_validation_unwind_preserves_payload_owners_and_hook,
    ),
    #[cfg(feature = "deterministic")]
    (
        "teststore::tests::success_and_idle_keep_renderer_lazy_and_balance_owned_values",
        crate::teststore::tests::success_and_idle_keep_renderer_lazy_and_balance_owned_values,
    ),
    #[cfg(feature = "deterministic")]
    (
        "teststore::tests::allocation_endpoint_rejects_retained_task_weak_and_probe_owners",
        crate::teststore::tests::allocation_endpoint_rejects_retained_task_weak_and_probe_owners,
    ),
    #[cfg(feature = "deterministic")]
    (
        "teststore::tests::mismatch_cleanup_restores_child_panic_hook",
        crate::teststore::tests::mismatch_cleanup_restores_child_panic_hook,
    ),
    #[cfg(feature = "deterministic")]
    (
        "teststore::tests::failure_allocation_endpoint_rejects_retained_panic_payload",
        crate::teststore::tests::failure_allocation_endpoint_rejects_retained_panic_payload,
    ),
    (
        "topology::tests::retirement_only_removes_descendants_and_preserves_the_keyed_entry",
        crate::topology::tests::retirement_only_removes_descendants_and_preserves_the_keyed_entry,
    ),
    (
        "topology::tests::retirement_only_rejects_skipped_removal_and_each_retained_owner",
        crate::topology::tests::retirement_only_rejects_skipped_removal_and_each_retained_owner,
    ),
    (
        "topology::tests::topology_controls_reject_skipped_work_and_each_retained_owner",
        crate::topology::tests::topology_controls_reject_skipped_work_and_each_retained_owner,
    ),
    (
        "topology::tests::six_topologies_complete_exact_leaf_work_and_release_owners",
        crate::topology::tests::six_topologies_complete_exact_leaf_work_and_release_owners,
    ),
    #[cfg(feature = "deterministic")]
    (
        "topology::tests::deterministic_topology_uses_the_same_exact_work_contract",
        crate::topology::tests::deterministic_topology_uses_the_same_exact_work_contract,
    ),
];

#[cfg(test)]
mod tests {
    use super::CASES;

    #[test]
    fn allocation_roster_preserves_feature_counts_and_unique_names() {
        let expected = 28
            + 2 * usize::from(cfg!(feature = "diagnostics"))
            + 11 * usize::from(cfg!(feature = "persistence"))
            + 14 * usize::from(cfg!(feature = "deterministic"))
            + 5 * usize::from(cfg!(all(feature = "deterministic", feature = "local")))
            + 2 * usize::from(cfg!(all(
                feature = "persistence",
                feature = "deterministic"
            )));
        assert_eq!(CASES.len(), expected);
        for (index, case) in CASES.iter().enumerate() {
            assert!(!case.0.is_empty());
            assert!(!CASES[..index].iter().any(|previous| previous.0 == case.0));
        }
    }
}
