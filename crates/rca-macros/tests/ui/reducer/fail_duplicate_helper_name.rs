#[rca_macros::reducer(
    state(supplied(usize)), action(supplied(usize)), body(supplied(body)),
    child(first, reducer = usize, kind = required, state = missing, action = missing, bind(fn same(child: usize))),
    child(second, reducer = usize, kind = required, state = missing, action = missing, bind(fn same(child: usize))),
)]
struct Feature<R> {
    body: R,
}
fn main() {}
