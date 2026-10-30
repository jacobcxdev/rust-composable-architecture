//! Dispatches isolated allocation measurements without the ordinary parallel test harness.
//!
//! This test-only host preserves stable case names and runs selected cases sequentially.
//! Child markers must identify exactly one executable case before profiler code runs.

/// Shares child capture with consumers without registering duplicate fixture tests.
mod child {
    include!("allocation_test/child.rs");

    #[cfg(test)]
    ordinary_test_items! {
        mod tests;
    }
}

pub(crate) use child::run_in_child;

/// Preserves callers whose dedicated marker uses presence rather than a test name.
///
/// Returns true after the parent checks the child result, or false inside that child.
///
/// # Panics
/// Panics if the selected child fails to run exactly one successful test.
pub(crate) fn run_in_marked_child(test_name: &str, child_env: &str) -> bool {
    if std::env::var_os(child_env).is_some() {
        return false;
    }
    child::run_test_child(test_name, child_env, "1", &[]);
    true
}

/// Maps actor marker presence to the exact case required by the shared dispatcher.
///
/// # Errors
/// Rejects multiple actor markers or an actor marker combined with a shared marker.
pub(crate) fn actor_child_marker(
    shared_child: Option<&std::ffi::OsStr>,
    timer_child: bool,
    direct_future_child: bool,
) -> Result<Option<&std::ffi::OsStr>, String> {
    match (shared_child, timer_child, direct_future_child) {
        (None, true, false) => Ok(Some(std::ffi::OsStr::new(
            "store::actor::tests::timer_command_avoids_discarded_reply_allocation",
        ))),
        (None, false, true) => Ok(Some(std::ffi::OsStr::new(
            "store::actor::tests::direct_future_launch_avoids_erased_sender_allocation",
        ))),
        (marker, false, false) => Ok(marker),
        _ => Err("allocation child markers must not conflict".to_owned()),
    }
}

/// Maps stable harness names to measured cases, including their feature boundaries.
const CASES: &[(&str, fn())] = &[
    (
        "effect::timer::tests::timer_index_storage_costs",
        crate::effect::timer::allocation_tests::timer_index_storage_costs,
    ),
    #[cfg(feature = "local")]
    (
        "observation::wait::tests::wait_storage_allocates_one_box_and_poll_cancel_allocate_nothing",
        crate::observation::wait::allocation_tests::wait_storage_allocates_one_box_and_poll_cancel_allocate_nothing,
    ),
    #[cfg(all(not(loom), feature = "local"))]
    (
        "store::command::reply_contract_tests::terminal_callbacks_allocate_one_existing_payload_box",
        crate::store::command::allocation_tests::terminal_callbacks_allocate_one_existing_payload_box,
    ),
    #[cfg(all(not(loom), feature = "test-util"))]
    (
        "store::task_context::terminal_receipt_tests::terminal_receipt_first_access_is_allocation_free",
        crate::store::task_context::terminal_receipt_tests::terminal_receipt_first_access_is_allocation_free,
    ),
    #[cfg(not(loom))]
    (
        "store::scoped::tests::distinct_projection_publication_costs",
        crate::store::distinct_projection_publication_costs,
    ),
    #[cfg(not(loom))]
    (
        "store::scoped::tests::scoped_observation_adapter_storage_costs",
        crate::store::scoped_observation_adapter_storage_costs,
    ),
    (
        "store::transaction::tests::retiring_delayed_suffixes_prunes_waits_with_constant_allocation_budget",
        crate::store::retiring_delayed_suffixes_prunes_waits_with_constant_allocation_budget,
    ),
    (
        "store::actor::tests::timer_command_avoids_discarded_reply_allocation",
        crate::store::timer_command_avoids_discarded_reply_allocation,
    ),
    (
        "store::actor::tests::direct_future_launch_avoids_erased_sender_allocation",
        crate::store::direct_future_launch_avoids_erased_sender_allocation,
    ),
    #[cfg(feature = "local")]
    (
        "store::carriers::allocation_tests::effect_registration_costs",
        crate::store::carriers::allocation_tests::effect_registration_costs,
    ),
    (
        "store::scope::output_origin_costs::output_origin_costs",
        crate::store::scope::output_origin_costs,
    ),
    (
        "store::mount::collection_costs::keyed_collection_costs",
        crate::store::keyed_collection_costs,
    ),
    (
        "store::mounted::allocation_tests::route_and_roster_costs",
        crate::store::route_and_roster_costs,
    ),
    (
        "store::scope::allocation_tests::authority_capture_clone_and_drop_costs",
        crate::store::scope::authority_capture_clone_and_drop_costs,
    ),
    (
        "store::mount::allocation_tests::mount_declaration_and_acquisition_costs",
        crate::store::mount_declaration_and_acquisition_costs,
    ),
    (
        "store::mounted::allocation_tests::mounted_storage_and_work_costs",
        crate::store::mounted_storage_and_work_costs,
    ),
    (
        "composition::keyed::allocation_tests::cleared_shrink_releases_backing_storage_but_retains_lineage",
        crate::composition::cleared_shrink_releases_backing_storage_but_retains_lineage,
    ),
    (
        "composition::keyed::allocation_tests::unused_empty_collections_do_not_allocate",
        crate::composition::unused_empty_collections_do_not_allocate,
    ),
    (
        "composition::keyed::allocation_tests::warmed_keyed_reconciliation_does_not_allocate",
        crate::composition::warmed_keyed_reconciliation_does_not_allocate,
    ),
    (
        "composition::keyed::allocation_tests::reserved_keyed_collection_operations_do_not_allocate",
        crate::composition::reserved_keyed_collection_operations_do_not_allocate,
    ),
    (
        "composition::keyed::allocation_tests::warmed_alternating_store_and_parent_contexts_do_not_allocate",
        crate::composition::warmed_alternating_store_and_parent_contexts_do_not_allocate,
    ),
    (
        "store::scope::tests::empty_registry_reuses_lifecycle_root_without_allocation",
        crate::store::scope::empty_registry_reuses_lifecycle_root_without_allocation,
    ),
    (
        "store::scope::tests::node_identity_allocates_once_per_visited_slot_and_not_on_warm_reuse",
        crate::store::scope::node_identity_allocates_once_per_visited_slot_and_not_on_warm_reuse,
    ),
    (
        "store::scope::tests::identified_warm_hit_does_not_allocate_identity_ownership",
        crate::store::scope::identified_warm_hit_does_not_allocate_identity_ownership,
    ),
    (
        "reducer::tests::closure_reducer_and_identity_paths_do_not_allocate",
        crate::reducer::closure_reducer_and_identity_paths_do_not_allocate,
    ),
    (
        "effect::allocation_tests::scope_tagging_non_empty_effects_does_not_allocate",
        crate::effect::scope_tagging_non_empty_effects_does_not_allocate,
    ),
    (
        "effect::registry::tests::actor_task_registration_allocates_only_completion",
        crate::effect::registry::actor_task_registration_allocates_only_completion,
    ),
    (
        "effect::registry::tests::paired_harvest_allocates_only_result_buffer",
        crate::effect::registry::paired_harvest_allocates_only_result_buffer,
    ),
    (
        "composition::scope::allocation_tests::scope_none_paths_do_not_allocate_after_warm_up",
        crate::composition::scope_none_paths_do_not_allocate_after_warm_up,
    ),
    (
        "composition::shared::allocation_tests::warmed_shared_empty_effect_matches_ordinary_scope_allocations",
        crate::composition::warmed_shared_empty_effect_matches_ordinary_scope_allocations,
    ),
    (
        "composition::shared::allocation_tests::warmed_shared_keyed_paths_do_not_allocate_or_remount",
        crate::composition::warmed_shared_keyed_paths_do_not_allocate_or_remount,
    ),
    (
        "composition::dependency::allocation_tests::dependency_overlays_do_not_allocate_during_reduction",
        crate::composition::dependency_overlays_do_not_allocate_during_reduction,
    ),
    (
        "composition::lifecycle::allocation_tests::optional_lifecycle_hot_paths_do_not_allocate_after_warm_up",
        crate::composition::optional_lifecycle_hot_paths_do_not_allocate_after_warm_up,
    ),
    (
        "composition::lifecycle::allocation_tests::case_lifecycle_hot_paths_do_not_allocate_after_warm_up",
        crate::composition::case_lifecycle_hot_paths_do_not_allocate_after_warm_up,
    ),
    (
        "composition::stack::state_tests::pop_does_not_allocate",
        crate::composition::pop_does_not_allocate,
    ),
    (
        "composition::combine::tests::combine_all_none_widths_do_not_allocate",
        crate::composition::combine_all_none_widths_do_not_allocate,
    ),
    (
        "composition::combine::tests::vector_all_none_reduction_does_not_allocate",
        crate::composition::vector_all_none_reduction_does_not_allocate,
    ),
    (
        "composition::keyed::operations::tests::moves_bound_permutation_allocations",
        crate::composition::moves_bound_permutation_allocations,
    ),
    (
        "composition::keyed::operations::tests::merge_result_capacity_tracks_conflicts_instead_of_all_inputs",
        crate::composition::merge_result_capacity_tracks_conflicts_instead_of_all_inputs,
    ),
    (
        "composition::keyed::operations::tests::sort_reuses_completed_scratch_storage",
        crate::composition::sort_reuses_completed_scratch_storage,
    ),
    (
        "composition::path::tests::keyed_path_operations_do_not_allocate",
        crate::composition::keyed_path_operations_do_not_allocate,
    ),
    (
        "live_cache_hits_allocate_only_the_requested_identity",
        crate::sharing::cache::allocation_tests::live_cache_hits_allocate_only_the_requested_identity,
    ),
    (
        "shared_key_error_debug_formats_without_allocating",
        crate::sharing::cache::allocation_tests::shared_key_error_debug_formats_without_allocating,
    ),
    (
        "cold_publication_keeps_its_required_construction_allocations",
        crate::sharing::cache::allocation_tests::cold_publication_keeps_its_required_construction_allocations,
    ),
    (
        "losing_publication_does_not_clone_winner_metadata",
        crate::sharing::cache::allocation_tests::losing_publication_does_not_clone_winner_metadata,
    ),
    #[cfg(feature = "diagnostics")]
    (
        "diagnostics::diff::tests::rendered_lines_need_no_scratch_allocations",
        crate::diagnostics::rendered_lines_need_no_scratch_allocations,
    ),
];

include!("allocation_test/runner.rs");

#[cfg(test)]
ordinary_test_items! {
mod runner_tests {
    //! Checks stable names and selection before any allocation measurement runs.

    use super::{CASES, Options, actor_child_marker, run};

    /// Supplies owned command-line arguments without changing the process environment.
    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    #[test]
    fn roster_preserves_exact_names_and_has_no_duplicates() {
        let expected = [
            "effect::timer::tests::timer_index_storage_costs",
            #[cfg(feature = "local")]
            "observation::wait::tests::wait_storage_allocates_one_box_and_poll_cancel_allocate_nothing",
            #[cfg(all(not(loom), feature = "local"))]
            "store::command::reply_contract_tests::terminal_callbacks_allocate_one_existing_payload_box",
            #[cfg(all(not(loom), feature = "test-util"))]
            "store::task_context::terminal_receipt_tests::terminal_receipt_first_access_is_allocation_free",
            #[cfg(not(loom))]
            "store::scoped::tests::distinct_projection_publication_costs",
            #[cfg(not(loom))]
            "store::scoped::tests::scoped_observation_adapter_storage_costs",
            "store::transaction::tests::retiring_delayed_suffixes_prunes_waits_with_constant_allocation_budget",
            "store::actor::tests::timer_command_avoids_discarded_reply_allocation",
            "store::actor::tests::direct_future_launch_avoids_erased_sender_allocation",
            #[cfg(feature = "local")]
            "store::carriers::allocation_tests::effect_registration_costs",
            "store::scope::output_origin_costs::output_origin_costs",
            "store::mount::collection_costs::keyed_collection_costs",
            "store::mounted::allocation_tests::route_and_roster_costs",
            "store::scope::allocation_tests::authority_capture_clone_and_drop_costs",
            "store::mount::allocation_tests::mount_declaration_and_acquisition_costs",
            "store::mounted::allocation_tests::mounted_storage_and_work_costs",
            "composition::keyed::allocation_tests::cleared_shrink_releases_backing_storage_but_retains_lineage",
            "composition::keyed::allocation_tests::unused_empty_collections_do_not_allocate",
            "composition::keyed::allocation_tests::warmed_keyed_reconciliation_does_not_allocate",
            "composition::keyed::allocation_tests::reserved_keyed_collection_operations_do_not_allocate",
            "composition::keyed::allocation_tests::warmed_alternating_store_and_parent_contexts_do_not_allocate",
            "store::scope::tests::empty_registry_reuses_lifecycle_root_without_allocation",
            "store::scope::tests::node_identity_allocates_once_per_visited_slot_and_not_on_warm_reuse",
            "store::scope::tests::identified_warm_hit_does_not_allocate_identity_ownership",
            "reducer::tests::closure_reducer_and_identity_paths_do_not_allocate",
            "effect::allocation_tests::scope_tagging_non_empty_effects_does_not_allocate",
            "effect::registry::tests::actor_task_registration_allocates_only_completion",
            "effect::registry::tests::paired_harvest_allocates_only_result_buffer",
            "composition::scope::allocation_tests::scope_none_paths_do_not_allocate_after_warm_up",
            "composition::shared::allocation_tests::warmed_shared_empty_effect_matches_ordinary_scope_allocations",
            "composition::shared::allocation_tests::warmed_shared_keyed_paths_do_not_allocate_or_remount",
            "composition::dependency::allocation_tests::dependency_overlays_do_not_allocate_during_reduction",
            "composition::lifecycle::allocation_tests::optional_lifecycle_hot_paths_do_not_allocate_after_warm_up",
            "composition::lifecycle::allocation_tests::case_lifecycle_hot_paths_do_not_allocate_after_warm_up",
            "composition::stack::state_tests::pop_does_not_allocate",
            "composition::combine::tests::combine_all_none_widths_do_not_allocate",
            "composition::combine::tests::vector_all_none_reduction_does_not_allocate",
            "composition::keyed::operations::tests::moves_bound_permutation_allocations",
            "composition::keyed::operations::tests::merge_result_capacity_tracks_conflicts_instead_of_all_inputs",
            "composition::keyed::operations::tests::sort_reuses_completed_scratch_storage",
            "composition::path::tests::keyed_path_operations_do_not_allocate",
            "live_cache_hits_allocate_only_the_requested_identity",
            "shared_key_error_debug_formats_without_allocating",
            "cold_publication_keeps_its_required_construction_allocations",
            "losing_publication_does_not_clone_winner_metadata",
            #[cfg(feature = "diagnostics")]
            "diagnostics::diff::tests::rendered_lines_need_no_scratch_allocations",
        ];
        assert_eq!(
            CASES.iter().map(|case| case.0).collect::<Vec<_>>(),
            expected
        );
        for (index, case) in CASES.iter().enumerate() {
            assert!(!CASES[..index].iter().any(|earlier| earlier.0 == case.0));
        }
    }

    #[test]
    fn substring_filters_form_a_union_and_skip_excludes_matches() {
        let options = Options::parse(&args(&[
            "moves_bound",
            "unused_empty",
            "--skip",
            "unused_empty",
        ]))
        .unwrap();
        let selected: Vec<_> = CASES
            .iter()
            .filter(|case| options.matches(case.0))
            .map(|case| case.0)
            .collect();
        assert_eq!(
            selected,
            ["composition::keyed::operations::tests::moves_bound_permutation_allocations"]
        );
    }

    #[test]
    fn exact_filters_do_not_accept_substrings() {
        let options = Options::parse(&args(&["moves_bound", "--exact"])).unwrap();
        assert!(!CASES.iter().any(|case| options.matches(case.0)));
        let options = Options::parse(&args(&[
            "composition::keyed::operations::tests::moves_bound_permutation_allocations",
            "--exact",
        ]))
        .unwrap();
        assert_eq!(
            CASES.iter().filter(|case| options.matches(case.0)).count(),
            1
        );
    }

    #[test]
    fn invalid_options_are_errors_not_filters() {
        for arguments in [
            vec!["--unknown"],
            vec!["--skip"],
            vec!["--skip", "--unknown"],
            vec!["--test-threads"],
            vec!["--test-threads=0"],
            vec!["--test-threads=no"],
            vec!["--test-threads=-1"],
            vec!["--ignored", "--include-ignored"],
        ] {
            assert!(Options::parse(&args(&arguments)).is_err());
        }
    }

    #[test]
    fn literal_filters_and_inline_options_keep_their_meaning() {
        let options = Options::parse(&args(&[
            "--exact",
            "--skip=-skip",
            "--test-threads=3",
            "--",
            "-keep",
            "-skip",
        ]))
        .unwrap();
        assert!(options.matches("-keep"));
        assert!(!options.matches("-skip"));
        assert!(!options.matches("prefix-keep"));
        assert!(!options.matches("other"));
    }

    #[test]
    fn accepted_controls_preserve_nonignored_roster_selection() {
        let options = Options::parse(&args(&[
            "--nocapture",
            "--test-threads",
            "2",
            "--include-ignored",
            "--fail-fast",
            "--list",
        ]))
        .unwrap();
        assert!(options.list);
        assert!(options.fail_fast);
        assert!(CASES.iter().all(|case| options.matches(case.0)));
        let options = Options::parse(&args(&["--ignored"])).unwrap();
        assert!(!CASES.iter().any(|case| options.matches(case.0)));
    }

    #[test]
    fn zero_match_runner_succeeds_without_dispatch() {
        assert_eq!(
            run(
                &args(&["does-not-name-any-allocation-case"]),
                false,
                None,
                false
            ),
            Ok(true)
        );
    }

    #[test]
    fn list_runner_never_dispatches_measurements() {
        assert_eq!(run(&args(&["--list"]), false, None, false), Ok(true));
    }

    #[test]
    fn actor_markers_select_their_exact_registered_cases() {
        for (timer, direct_future, name) in [
            (
                true,
                false,
                "store::actor::tests::timer_command_avoids_discarded_reply_allocation",
            ),
            (
                false,
                true,
                "store::actor::tests::direct_future_launch_avoids_erased_sender_allocation",
            ),
        ] {
            let marker = actor_child_marker(None, timer, direct_future).unwrap();
            assert_eq!(marker, Some(std::ffi::OsStr::new(name)));
            let options = Options::parse(&args(&[name, "--exact"])).unwrap();
            let selected: Vec<_> = CASES
                .iter()
                .filter(|case| options.matches(case.0))
                .collect();
            assert_eq!(
                super::validate_child(&selected, false, false, marker, false),
                Ok(())
            );
        }
        assert_eq!(actor_child_marker(None, false, false), Ok(None));
        let shared = Some(std::ffi::OsStr::new("shared case"));
        assert_eq!(actor_child_marker(shared, false, false), Ok(shared));
    }

    #[test]
    fn actor_markers_reject_conflicting_markers() {
        assert!(actor_child_marker(None, true, true).is_err());
        for (timer, direct_future) in [(true, false), (false, true), (true, true)] {
            assert!(actor_child_marker(Some(std::ffi::OsStr::new("")), timer, direct_future).is_err());
        }
        for (timer, direct_future, name) in [
            (
                true,
                false,
                "store::actor::tests::timer_command_avoids_discarded_reply_allocation",
            ),
            (
                false,
                true,
                "store::actor::tests::direct_future_launch_avoids_erased_sender_allocation",
            ),
        ] {
            let marker = actor_child_marker(None, timer, direct_future).unwrap();
            for (keyed, scope) in [(true, false), (false, true), (true, true)] {
                assert!(run(&args(&[name, "--exact"]), keyed, marker, scope).is_err());
            }
        }
    }

    #[test]
    fn actor_markers_reject_missing_multiple_wrong_and_listed_cases() {
        for (timer, direct_future, name, other) in [
            (
                true,
                false,
                "store::actor::tests::timer_command_avoids_discarded_reply_allocation",
                "store::actor::tests::direct_future_launch_avoids_erased_sender_allocation",
            ),
            (
                false,
                true,
                "store::actor::tests::direct_future_launch_avoids_erased_sender_allocation",
                "store::actor::tests::timer_command_avoids_discarded_reply_allocation",
            ),
        ] {
            let marker = actor_child_marker(None, timer, direct_future).unwrap();
            for arguments in [
                vec!["not-a-case"],
                vec![],
                vec![other, "--exact"],
                vec![name, "--exact", "--list"],
                vec![name, "--exact", "--skip", name],
                vec![name, "--exact", "--ignored"],
            ] {
                assert!(run(&args(&arguments), false, marker, false).is_err());
            }
        }
    }

    #[test]
    fn child_selection_rejects_zero_multiple_wrong_family_and_list() {
        for arguments in [
            vec!["not-a-case"],
            vec![],
            vec![
                "store::scope::tests::empty_registry_reuses_lifecycle_root_without_allocation",
                "--exact",
            ],
            vec![
                "composition::keyed::allocation_tests::unused_empty_collections_do_not_allocate",
                "--exact",
                "--list",
            ],
        ] {
            run(&args(&arguments), true, None, false)
                .expect_err("invalid child selection must be rejected");
        }
        run(
            &args(&["not-a-case"]),
            false,
            Some(std::ffi::OsStr::new("not-a-case")),
            false,
        )
        .expect_err("unknown child case must be rejected");
        run(
            &args(&["--list"]),
            true,
            Some(std::ffi::OsStr::new("not-a-case")),
            false,
        )
        .expect_err("child list selection must be rejected");
    }

    #[test]
    fn scope_child_accepts_only_its_exact_case_without_dispatch() {
        let options = Options::parse(&args(&[
            "store::scope::tests::identified_warm_hit_does_not_allocate_identity_ownership",
            "--exact",
        ]))
        .unwrap();
        let selected: Vec<_> = CASES
            .iter()
            .filter(|case| options.matches(case.0))
            .collect();
        super::validate_child(&selected, false, false, None, true)
            .expect("exact scope child is valid");
        assert!(super::validate_child(&selected, false, true, None, false).is_err());
        assert!(
            super::validate_child(
                &selected,
                false,
                false,
                Some(std::ffi::OsStr::new(
                    "store::scope::tests::identified_warm_hit_does_not_allocate_identity_ownership"
                )),
                false,
            )
            .is_err()
        );
    }

    #[test]
    fn scope_child_rejects_wrong_zero_multiple_and_list_selections() {
        for arguments in [
            vec!["does-not-name-any-allocation-case"],
            vec![],
            vec![
                "store::scope::tests::empty_registry_reuses_lifecycle_root_without_allocation",
                "--exact",
            ],
            vec![
                "composition::keyed::allocation_tests::unused_empty_collections_do_not_allocate",
                "--exact",
            ],
            vec![
                "store::scope::tests::identified_warm_hit_does_not_allocate_identity_ownership",
                "--exact",
                "--list",
            ],
        ] {
            run(&args(&arguments), false, None, true)
                .expect_err("invalid scope child selection must be rejected");
        }
    }

    #[test]
    fn conflicting_child_markers_reject_before_dispatch() {
        let arguments = args(&[
            "store::scope::tests::identified_warm_hit_does_not_allocate_identity_ownership",
            "--exact",
        ]);
        run(&arguments, true, None, true).expect_err("conflicting child markers must be rejected");
        run(&arguments, false, Some(std::ffi::OsStr::new("scope")), true)
            .expect_err("conflicting shared and scope markers must be rejected");
        run(&arguments, true, Some(std::ffi::OsStr::new("scope")), true)
            .expect_err("conflicting keyed, shared and scope markers must be rejected");
    }
}
}
