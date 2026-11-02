#[rca_macros::reducer(
    state(supplied(usize)), action(supplied(usize)), body(supplied(body)),
    child(child, reducer = usize, kind = required,
        state = missing, action = missing, bind(fn bind_child()),
    ),
)]
struct Feature<R> {
    body: R,
}
fn main() {}
