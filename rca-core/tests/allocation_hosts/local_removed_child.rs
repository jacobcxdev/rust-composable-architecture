//! Runs Local removal checks without a concurrent libtest receiver thread.

#[expect(
    dead_code,
    unused_imports,
    reason = "This host uses allocation fixtures; the original libtest target checks the remaining helpers."
)]
#[path = "../local_removed_child.rs"]
mod fixture;

/// Preserves the original measurement name and child marker.
const CASES: &[(&str, fn())] = &[(
    "local_plain_removal_work_costs",
    fixture::local_plain_removal_work_costs,
)];

include!("../support/allocation_main.rs");
