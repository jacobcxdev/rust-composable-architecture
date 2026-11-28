// Rejects unions rather than treating their fields as action cases.

use rca_macros::ActionPaths;

#[derive(ActionPaths)]
union Action {
    child: u8,
}

fn main() {}
