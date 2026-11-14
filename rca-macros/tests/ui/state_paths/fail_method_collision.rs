// Rejects distinct case names that become the same path constructor.

use rca_macros::StatePaths;

#[derive(StatePaths)]
enum State {
    ChildState(u8),
    Child_State(u8),
}

fn main() {}
