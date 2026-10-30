//! Runs only allocation controls with the shared synchronous dispatcher.

#![expect(
    dead_code,
    unused_imports,
    reason = "This host uses workload support without its ordinary semantic test registrations."
)]

#[path = "../../future_outcome.rs"]
mod future_outcome;
#[path = "../../outcome_native_tests.rs"]
mod native;
#[path = "outcome.rs"]
mod outcome;
#[path = "../../outcome_allocations.rs"]
mod outcome_allocations;

include!("../../outcome_allocation_main.rs");
