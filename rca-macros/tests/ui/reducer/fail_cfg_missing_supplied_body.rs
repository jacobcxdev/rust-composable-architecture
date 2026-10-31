#[rca_macros::reducer(
    state(supplied(())), action(supplied(())), body(supplied(body)),
)]
struct Feature {
    #[cfg(false)]
    body: MissingAdapter,
}

fn main() {}
