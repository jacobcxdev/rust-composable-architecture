#[rca_macros::reducer(
    #[allow(dead_code)]
    state(empty),
    action(empty),
    body(generated),
)]
struct UnsupportedAttribute {}

#[rca_macros::reducer(
    #[cfg(any())]
    state(empty),
    action(empty),
    body(generated),
)]
struct ConditionalState {}

#[rca_macros::reducer(
    state(empty),
    #[cfg_attr(all(), cfg(any()))]
    action(empty),
    body(generated),
)]
struct ConditionalAction {}

#[rca_macros::reducer(
    state(empty),
    action(empty),
    #[cfg(any())]
    body(generated),
)]
struct ConditionalBody {}

fn main() {}
