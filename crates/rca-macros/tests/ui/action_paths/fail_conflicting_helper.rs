// Rejects a case that requests both a keyed path and no generated path.

use rca_macros::ActionPaths;

#[derive(ActionPaths)]
enum Action {
    #[rca(keyed, skip)]
    Row(u8),
}

fn main() {}
