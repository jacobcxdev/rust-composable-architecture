#[rca_macros::reducer(
    state(generated(struct State { #[reducer(case = data)] data: usize })),
    action(generated(enum Action { #[reducer(case = data)] Data(_) })),
    body(generated), case(data, policy = automatic, action = empty),
)]
struct Feature {}
fn main() {}
