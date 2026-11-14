// Rejects a helper attached to one field of a multi-field state case.

use rca_macros::StatePaths;

#[derive(StatePaths)]
enum State {
    Multiple(#[rca(skip)] u8, u8),
}

fn main() {}
