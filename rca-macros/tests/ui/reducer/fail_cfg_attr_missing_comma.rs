#[rca_macros::reducer(
    state(empty),
    action(empty),
    body(generated),
    #[cfg_attr(true)]
    child(child, reducer = (), kind = required,
        state = state_path, action = action_path,
        bind(fn bind_child(child: ())),
    ),
)]
struct Feature {}

fn main() {}
