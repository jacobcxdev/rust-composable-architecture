//! Runs reaction allocation checks without a concurrent libtest receiver thread.

#[path = "../changed_reaction_costs.rs"]
mod fixture;

/// Preserves the original measurement name.
const CASES: &[(&str, fn())] = &[(
    "children_have_exact_reaction_work",
    fixture::children_have_exact_reaction_work,
)];

include!("../support/allocation_main.rs");
