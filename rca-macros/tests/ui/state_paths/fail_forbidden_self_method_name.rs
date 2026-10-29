// Rejects a case name that would produce the forbidden method name `self`.

use rca_macros::StatePaths;

#[derive(StatePaths)]
enum State {
    Self_(u8),
}

fn main() {}
