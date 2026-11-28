//! Bounded direct-child capture for CLI checks, outside workload measurement.
//!
//! Children must not leave descendants that retain their output pipes.

#[expect(
    dead_code,
    reason = "CLI hosts reuse capture, not the allocation-test selection helpers"
)]
mod child {
    include!("../../../../crates/rca-core/src/allocation_test/child.rs");

    /// Captures bounded diagnostics and reaps the direct child before reader joins.
    pub(crate) fn output(command: &mut Command, timeout: Duration) -> Output {
        capture(command, timeout)
            .unwrap_or_else(|error| panic!("CLI child capture failed: {error}"))
    }

    /// Retains explicit finite stdin for controller-driven CLI processes.
    pub(crate) fn output_with_input(
        command: &mut Command,
        timeout: Duration,
        input: Stdio,
    ) -> Output {
        capture_with_input(command, timeout, input)
            .unwrap_or_else(|error| panic!("CLI child capture failed: {error}"))
    }
}

#[allow(
    unused_imports,
    reason = "each CLI test host selects either null-input or controller-input capture"
)]
pub(crate) use child::{output, output_with_input};
