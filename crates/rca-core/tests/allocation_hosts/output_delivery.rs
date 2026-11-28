//! Runs output allocation checks without a concurrent libtest receiver thread.

#[expect(
    dead_code,
    unused_imports,
    reason = "This host uses allocation fixtures; the original libtest target checks the remaining helpers."
)]
#[path = "../output_delivery.rs"]
mod fixture;

/// Preserves the original measurement name and child marker.
const CASES: &[(&str, fn())] = &[("output_phase_costs", fixture::output_phase_costs)];

include!("../support/allocation_main.rs");
