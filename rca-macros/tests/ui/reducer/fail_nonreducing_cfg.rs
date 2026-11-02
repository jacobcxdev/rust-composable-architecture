#[rca_macros::reducer(
    state(empty), action(empty), body(generated),
    #[cfg]
    case(alert, policy = ignored, action = empty),
)]
struct Ignored {}

#[rca_macros::reducer(
    state(empty), action(empty), body(generated),
    #[cfg_attr]
    case(alert, policy = data, action = empty),
)]
struct Data {}

fn main() {}
