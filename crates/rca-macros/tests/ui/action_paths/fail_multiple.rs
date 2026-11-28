// Rejects a keyed case with multiple fields instead of one keyed payload.

use rca_macros::ActionPaths;

#[derive(ActionPaths)]
enum Action {
    #[rca(keyed)]
    Multiple(u8, u16),
}

fn main() {}
