//! Checks ordinary reducer composition without optional features or workspace feature unification.
//!
//! The included compiler fixtures are type-checked, not executed.
#![expect(
    dead_code,
    reason = "Compiler-only fixture functions are not executed."
)]

#[path = "../../../ui/rc04_local_identified_lifecycle.rs"]
mod identified_lifecycle;
#[path = "../../../ui/rc04_local_shared_scope.rs"]
mod shared;

fn main() {}
