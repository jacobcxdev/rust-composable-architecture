// Rejects an empty state-path helper rather than silently choosing a path kind.

use rca_macros::StatePaths;

struct Child;

#[derive(StatePaths)]
struct State {
    #[rca()]
    child: Child,
}

fn main() {}
