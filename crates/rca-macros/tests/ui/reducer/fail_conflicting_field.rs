#[rca_macros::reducer(
    state(supplied(())), action(supplied(())), body(generated),
)]
struct Feature<R> {
    #[reducer(reduce, data)]
    body: R,
}
fn main() {}
