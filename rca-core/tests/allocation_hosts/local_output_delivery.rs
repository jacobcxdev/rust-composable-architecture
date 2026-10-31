//! Runs Local output checks without a concurrent libtest receiver thread.

#[expect(
    dead_code,
    reason = "This host uses allocation fixtures; the original libtest target checks the remaining helpers."
)]
#[path = "../local_output_delivery.rs"]
mod fixture;

/// Preserves the original measurement name and child marker.
const CASES: &[(&str, fn())] = &[(
    "local_output_phase_costs",
    fixture::local_output_phase_costs,
)];

include!("../support/allocation_main.rs");
