// Rejects a case name that would produce the forbidden method name `crate`.

use rca_macros::ActionPaths;

#[derive(ActionPaths)]
enum Action {
    Crate(u8),
}

fn main() {}
