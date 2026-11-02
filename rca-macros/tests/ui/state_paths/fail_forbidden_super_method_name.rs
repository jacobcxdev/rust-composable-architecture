// Rejects a case name that would produce the forbidden method name `super`.

use rca_macros::StatePaths;

#[derive(StatePaths)]
enum State {
    Super(u8),
}

fn main() {}
