#[rca_macros::reducer(
    state(generated(enum MissingCaseState { #[reducer(case = missing)] Item })),
    action(empty), body(generated),
)]
struct MissingCase {}

fn main() {}
