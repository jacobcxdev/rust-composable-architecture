// Rejects a case name that would produce the forbidden method name `crate`.

use rca_macros::StatePaths;

#[derive(StatePaths)]
enum State {
    Crate(u8),
}

fn main() {}
