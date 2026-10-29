// Rejects an empty action-path helper rather than ignoring it.

use rca_macros::ActionPaths;

#[derive(ActionPaths)]
enum Action {
    #[rca()]
    Parent,
}

fn main() {}
