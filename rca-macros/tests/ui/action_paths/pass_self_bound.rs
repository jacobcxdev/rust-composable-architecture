// Keeps Self bounds tied to the original state or action type.

use rca_macros::{ActionPaths, StatePaths};

trait Marker {}

#[derive(ActionPaths)]
enum Action<T>
where
    Self: Marker,
{
    Child(T),
}

impl<T> Marker for Action<T> {}

#[derive(StatePaths)]
struct State<T>
where
    Self: Marker,
{
    child: T,
}

impl<T> Marker for State<T> {}

fn main() {
    let action_path = ActionPaths::<u8>::child();
    let action = action_path.embed(4);
    assert!(matches!(action_path.extract(&action), Some(4)));

    let state_path = StatePaths::<u8>::child();
    assert_eq!(*state_path.get(&State { child: 6 }), 6);
}
