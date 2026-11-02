#[cfg(not(test))]
#[path = "../../../future_outcome.rs"]
mod future_outcome;
#[cfg(not(test))]
#[path = "../outcome.rs"]
mod outcome;
#[cfg(not(test))]
#[path = "../../../outcome_allocations.rs"]
mod outcome_allocations;

#[cfg(not(test))]
fn main() {
    outcome_allocations::main("rca");
}
