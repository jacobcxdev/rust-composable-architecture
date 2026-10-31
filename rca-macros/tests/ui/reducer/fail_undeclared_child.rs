#[rca_macros::reducer(
    state(generated(struct State { #[reducer(child = missing)] value: _ })),
    action(supplied(())), body(generated),
)]
struct Feature {}
fn main() {}
