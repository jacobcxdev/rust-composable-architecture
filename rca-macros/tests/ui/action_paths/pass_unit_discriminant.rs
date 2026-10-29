// Accepts an action enum containing an explicitly numbered unit case.

use rca_macros::ActionPaths;

#[repr(u8)]
#[derive(ActionPaths)]
enum Action {
    Child(u8),
    Ignored = 1,
}

fn main() {
    let path = ActionPaths::child();
    let action = path.embed(5);
    assert!(matches!(path.extract(&action), Some(5)));
}
