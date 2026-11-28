// Requires an enum because action paths select and embed enum cases.

use rca_macros::ActionPaths;

#[derive(ActionPaths)]
struct Action {
    child: u8,
}

fn main() {}
