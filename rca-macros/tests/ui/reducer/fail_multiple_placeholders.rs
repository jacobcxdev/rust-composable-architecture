#[rca_macros::reducer(
    state(generated(struct State { #[reducer(child = child)] value: (_, _) })),
    action(supplied(())), body(supplied(body)),
    child(child, reducer = usize, kind = required, state = missing, action = missing, bind(fn bind_child(child: usize))),
)]
struct Feature<R> {
    body: R,
}
fn main() {}
