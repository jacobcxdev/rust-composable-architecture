// Rejects an unknown helper option rather than silently ignoring it.

use rca_macros::ActionPaths;

#[derive(ActionPaths)]
enum Action {
    #[rca(unknown)]
    Parent,
}

fn main() {}
