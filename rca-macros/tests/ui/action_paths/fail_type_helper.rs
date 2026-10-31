// Rejects an action-path helper on the type instead of a case.

use rca_macros::ActionPaths;

#[derive(ActionPaths)]
#[rca(skip)]
enum Action {
    Parent,
}

fn main() {}
