//! Runs command allocation checks without a concurrent libtest receiver thread.

#[expect(
    dead_code,
    unused_imports,
    clippy::wildcard_imports,
    reason = "This host uses allocation fixtures; the original libtest target checks the remaining helpers."
)]
#[path = "../command_port.rs"]
mod fixture;

/// Preserves the original measurement name and child marker.
const CASES: &[(&str, fn())] = &[(
    "command_port_phase_costs",
    fixture::command_port_phase_costs,
)];

include!("../support/allocation_main.rs");
