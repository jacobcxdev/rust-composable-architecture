// Rejects a struct-only required-path helper on a state case.

use rca_macros::StatePaths;

#[derive(StatePaths)]
enum State {
    #[rca(required)]
    Child(u8),
}

fn main() {}
