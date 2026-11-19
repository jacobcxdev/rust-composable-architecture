// Rejects a generated namespace that would replace an existing item.

use rca_macros::ActionPaths;

struct ActionPaths;

#[derive(ActionPaths)]
enum Action {
    Child(u8),
}

fn main() {}
