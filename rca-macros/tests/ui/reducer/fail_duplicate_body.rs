#[rca_macros::reducer(
    state(supplied(())), action(supplied(())), body(supplied(body)), body(generated),
)]
struct Feature<R> {
    body: R,
}
fn main() {}
