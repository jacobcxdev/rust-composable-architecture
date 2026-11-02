#[rca_macros::reducer(
    state(generated(struct State { #[cfg(any())] absent: MissingType, present: usize })),
    action(supplied(())), body(generated),
)]
struct Feature {}
fn main() {
    let _ = StatePaths::absent();
}
