// Keeps a generic named Fn from shadowing the generated closure trait.

use rca_macros::ActionPaths;

#[derive(ActionPaths)]
enum Action<Fn> {
    Child(Fn),
}

fn main() {
    let path = ActionPaths::<u8>::child();
    let action = path.embed(9);
    assert!(matches!(path.extract(&action), Some(9)));
}
