//! Runs observation allocation controls without a concurrent libtest receiver thread.

#[expect(
    dead_code,
    reason = "Allocation controls use only the observed exercise from the shared adapter."
)]
#[path = "../../candidate.rs"]
mod adapter;
#[path = "../../allocations.rs"]
mod allocations;

include!("../../allocation_main.rs");
