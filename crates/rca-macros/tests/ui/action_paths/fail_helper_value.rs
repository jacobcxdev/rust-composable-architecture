// Rejects a helper value: action-path options are flags, not assignments.

use rca_macros::ActionPaths;

#[derive(ActionPaths)]
enum Action {
    #[rca(skip = true)]
    Parent,
}

fn main() {}
