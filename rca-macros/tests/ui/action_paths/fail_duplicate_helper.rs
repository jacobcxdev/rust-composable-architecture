// Rejects a repeated action-path helper even when both flags agree.

use rca_macros::ActionPaths;

#[derive(ActionPaths)]
enum Action {
    #[rca(skip)]
    #[rca(skip)]
    Parent,
}

fn main() {}
