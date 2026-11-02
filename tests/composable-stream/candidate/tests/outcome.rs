#[path = "../../future_outcome.rs"]
mod future_outcome;

#[path = "../src/outcome.rs"]
mod outcome;

#[path = "../../outcome_native_tests.rs"]
mod native;

#[cfg(feature = "allocation-measurement")]
#[path = "../../outcome_allocations.rs"]
mod outcome_allocations;
