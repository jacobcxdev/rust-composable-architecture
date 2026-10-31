// Rejects a required-state-path helper on an action case.

use rca_macros::ActionPaths;

#[derive(ActionPaths)]
enum Action {
    #[rca(required)]
    Parent,
}

fn main() {}
