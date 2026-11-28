#[rca_macros::reducer(
    state(supplied(usize)), action(supplied(usize)), body(supplied(body)),
    child(child, reducer = usize, kind = required, state = missing, action = missing, bind(fn first(child: usize))),
    child(child, reducer = usize, kind = required, state = missing, action = missing, bind(fn second(child: usize))),
)]
struct Feature<R> {
    body: R,
}
fn main() {}
