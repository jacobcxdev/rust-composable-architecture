//! Runs only allocation controls with the shared synchronous dispatcher.

#![expect(
    dead_code,
    unused_imports,
    reason = "This host uses workload support without its ordinary semantic test registrations."
)]

#[path = "../../common.rs"]
mod common;
#[path = "../../legacy_ready.rs"]
mod ready;
#[path = "../../ready_allocations.rs"]
mod ready_allocations;
#[path = "../../ready_future.rs"]
mod ready_future;

include!("../../ready_allocation_main.rs");
