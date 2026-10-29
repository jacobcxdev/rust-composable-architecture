#[rca_macros::reducer(state(empty), action(empty), body(supplied(body)))]
struct Feature<R> {
    #[reducer()]
    body: R,
}

fn main() {}
