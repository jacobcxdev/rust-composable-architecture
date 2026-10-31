// Rejects a helper on an action payload field instead of its case.

use rca_macros::ActionPaths;

#[derive(ActionPaths)]
enum Action {
    Child(#[rca(keyed)] u8),
}

fn main() {}
