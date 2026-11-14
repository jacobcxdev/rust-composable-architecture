#[rca_macros::reducer(
    state(empty), action(empty), body(generated),
    case(alert, policy = ephemeral, action = supplied(()),
        state_path = missing_state, action_path = missing_action,
        identity = missing_identity,
        bind(fn bind_alert(parent: MissingParent)),
    ),
)]
struct Feature {}

fn main() {}
