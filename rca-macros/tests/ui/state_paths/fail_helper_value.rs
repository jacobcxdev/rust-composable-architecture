// Rejects a helper value: state-path options are flags, not assignments.

use rca_macros::StatePaths;

#[derive(StatePaths)]
struct State {
    #[rca(optional = true)]
    child: Option<u8>,
}

fn main() {}
