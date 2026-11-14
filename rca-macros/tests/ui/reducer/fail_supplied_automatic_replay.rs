#[rca_macros::reducer(
    state(supplied(())), action(supplied(())), body(supplied(body)),
)]
struct Feature<R> {
    #[reducer(child = duplicate)]
    body: R,
}
fn main() {}
