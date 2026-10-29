// Rejects an optional-path helper on a state case with no payload.

use rca_macros::StatePaths;

#[derive(StatePaths)]
enum State {
    #[rca(optional)]
    Unit,
}

fn main() {}
