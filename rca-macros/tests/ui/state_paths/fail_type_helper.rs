// Rejects a state-path helper on the type rather than a field or case.

use rca_macros::StatePaths;

#[derive(StatePaths)]
#[rca(skip)]
struct State {
    child: u8,
}

fn main() {}
