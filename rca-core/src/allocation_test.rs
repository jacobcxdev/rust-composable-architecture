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

/// Maps stable harness names to measured cases, including their feature boundaries.
const CASES: &[(&str, fn())] = &[
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
    #[cfg(feature = "diagnostics")]
    (
        "diagnostics::diff::tests::rendered_lines_need_no_scratch_allocations",
        crate::diagnostics::rendered_lines_need_no_scratch_allocations,
    ),
];

/// Selects cases while retaining the supported subset of ordinary test-harness controls.
#[derive(Default)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "Mirrors named harness CLI switches; incompatible flags are rejected during parsing."
)]
struct Options {
    /// Selects the union of these names or substrings; an empty list selects all.
    filters: Vec<String>,
    /// Excludes matching cases after positive selection.
    skips: Vec<String>,
    /// Applies equality rather than substring matching to filters and exclusions.
    exact: bool,
    /// Prints selected names without invoking measurement functions.
    list: bool,
    /// Selects nothing because this roster has no ignored cases.
    ignored: bool,
    /// Accepts the ordinary harness flag without changing this non-ignored roster.
    include_ignored: bool,
    /// Stops dispatch after the first caught test panic.
    fail_fast: bool,
}

impl Options {
    /// Parses supported controls without silently treating unknown flags as filters.
    ///
    /// # Errors
    /// Rejects unknown flags, missing values, invalid thread counts and conflicting ignored controls.
    fn parse(args: &[String]) -> Result<Self, String> {
        let mut options = Self::default();
        let mut args = args.iter();
        while let Some(argument) = args.next() {
            match argument.as_str() {
                "--exact" => options.exact = true,
                "--list" => options.list = true,
                "--nocapture" => {}
                "--ignored" => options.ignored = true,
                "--include-ignored" => options.include_ignored = true,
                "--fail-fast" => options.fail_fast = true,
                "--skip" => {
                    let filter = args.next().ok_or("--skip requires a filter")?;
                    if filter.starts_with('-') {
                        return Err(
                            "--skip requires a filter; use --skip= for hyphen-prefixed filters"
                                .to_owned(),
                        );
                    }
                    options.skips.push(filter.clone());
                }
                "--test-threads" => Self::threads(
                    args.next()
                        .ok_or("--test-threads requires a positive integer")?,
                )?,
                "--" => {
                    options.filters.extend(args.cloned());
                    break;
                }
                value if value.starts_with("--skip=") => {
                    options.skips.push(value["--skip=".len()..].to_owned());
                }
                value if value.starts_with("--test-threads=") => {
                    Self::threads(&value["--test-threads=".len()..])?;
                }
                value if value.starts_with('-') => {
                    return Err(format!("unsupported option: {value}"));
                }
                _ => options.filters.push(argument.clone()),
            }
        }
        if options.ignored && options.include_ignored {
            return Err("--ignored and --include-ignored cannot be combined".to_owned());
        }
        Ok(options)
    }

    /// Accepts positive harness thread counts while allocation cases remain sequential.
    ///
    /// # Errors
    /// Rejects zero, negative, non-numeric and out-of-range values.
    fn threads(value: &str) -> Result<(), String> {
        match value.parse::<usize>() {
            Ok(threads) if threads > 0 => Ok(()),
            _ => Err("--test-threads requires a positive integer".to_owned()),
        }
    }

    /// Applies positive selection before exclusions, without invoking a case.
    fn matches(&self, name: &str) -> bool {
        let matches = |filter: &String| {
            if self.exact {
                name == filter
            } else {
                name.contains(filter)
            }
        };
        !self.ignored
            && (self.filters.is_empty() || self.filters.iter().any(matches))
            && !self.skips.iter().any(matches)
    }
}

/// Rejects child markers that could skip or repeat the intended measurement.
///
/// # Errors
/// Rejects listing, ambiguous selection, conflicting markers and mismatched case families.
fn validate_child(
    selected: &[&(&str, fn())],
    list: bool,
    keyed_child: bool,
    shared_child: Option<&std::ffi::OsStr>,
    scope_child: bool,
) -> Result<(), String> {
    let markers =
        usize::from(keyed_child) + usize::from(shared_child.is_some()) + usize::from(scope_child);
    if markers == 0 {
        return Ok(());
    }
    if list || selected.len() != 1 || markers != 1 {
        return Err("allocation child requires exactly one matching executable case".to_owned());
    }
    let name = selected[0].0;
    let is_keyed = name.starts_with("composition::keyed::allocation_tests::");
    let is_scope =
        name == "store::scope::tests::identified_warm_hit_does_not_allocate_identity_ownership";
    if keyed_child != is_keyed
        || scope_child != is_scope
        || shared_child.is_some_and(|marker| marker != std::ffi::OsStr::new(name))
    {
        return Err("allocation child marker does not match the selected case".to_owned());
    }
    Ok(())
}

/// Lists or sequentially runs selected cases and reports whether every case passed.
///
/// Each case owns its profiler. A case panic becomes a failed test, not an
/// application recovery policy. Abort-mode panics cannot be caught here.
///
/// # Errors
/// Returns invalid option or child-marker errors before invoking measurements.
pub(crate) fn run(
    args: &[String],
    keyed_child: bool,
    shared_child: Option<&std::ffi::OsStr>,
    scope_child: bool,
) -> Result<bool, String> {
    let options = Options::parse(args)?;
    let selected: Vec<_> = CASES
        .iter()
        .filter(|case| options.matches(case.0))
        .collect();
    validate_child(
        &selected,
        options.list,
        keyed_child,
        shared_child,
        scope_child,
    )?;
    if options.list {
        for case in &selected {
            println!("{}: test", case.0);
        }
        println!("\n{} tests", selected.len());
        return Ok(true);
    }

    let mut passed = 0;
    let mut failed = 0;
    for case in selected {
        // The unchanged case owns profiler creation and teardown inside this call.
        let result = std::panic::catch_unwind(case.1);
        if result.is_ok() {
            passed += 1;
            println!("test {} ... ok", case.0);
        } else {
            failed += 1;
            println!("test {} ... FAILED", case.0);
            if options.fail_fast {
                break;
            }
        }
    }
    println!(
        "\ntest result: {}. {passed} passed; {failed} failed",
        if failed == 0 { "ok" } else { "FAILED" }
    );
    Ok(failed == 0)
}

#[cfg(test)]
ordinary_test_items! {
mod runner_tests {
    //! Checks stable names and selection before any allocation measurement runs.

    use super::{CASES, Options, run};

    /// Supplies owned command-line arguments without changing the process environment.
    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    #[test]
    fn roster_preserves_exact_names_and_has_no_duplicates() {
        let expected = [
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
