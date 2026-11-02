//! Runs diagnostic allocation checks without a concurrent libtest receiver thread.

#[expect(
    dead_code,
    reason = "This host uses allocation fixtures; the original libtest target checks the remaining helpers."
)]
#[path = "../structural_diagnostics.rs"]
mod fixture;

/// Preserves both original measurement names and child markers.
const CASES: &[(&str, fn())] = &[
    (
        "structural_validation_reuses_scratch_across_captured_nodes",
        fixture::structural_validation_reuses_scratch_across_captured_nodes,
    ),
    (
        "structural_diff_cleanup_panic_releases_the_completed_report",
        fixture::structural_diff_cleanup_panic_releases_the_completed_report,
    ),
];

include!("../support/allocation_main.rs");
