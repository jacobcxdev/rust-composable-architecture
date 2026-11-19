mod feature {
    #[rca_macros::reducer(
        state(generated(pub struct State { private: String })),
        action(supplied(())), body(generated),
    )]
    pub struct Feature {}
}
fn main() {
    let _ = feature::StatePaths::private();
}
