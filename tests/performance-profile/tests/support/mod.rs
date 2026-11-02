//! Bounded direct-child capture for CLI checks, outside workload measurement.
//!
//! Children must not leave descendants that retain their output pipes.

#[expect(
    dead_code,
    reason = "CLI hosts reuse capture, not the allocation-test selection helpers"
)]
mod child {
    include!("../../../../rca-core/src/allocation_test/child.rs");

    /// Captures bounded diagnostics and reaps the direct child before reader joins.
    pub(crate) fn output(command: &mut Command, timeout: Duration) -> Output {
        capture(command, timeout)
            .unwrap_or_else(|error| panic!("CLI child capture failed: {error}"))
    }
}

pub(crate) use child::output;
