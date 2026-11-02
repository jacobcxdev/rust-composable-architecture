#[rca_macros::reducer(
    state(generated(struct State { #[reducer(case = info)] info: usize })),
    action(generated(enum Action { #[reducer(case = info)] Info(_) })),
    body(generated),
    case(info, policy = ignored, action = empty, bind(fn bind_info(parent: usize))),
)]
struct Feature {}
fn main() {}
