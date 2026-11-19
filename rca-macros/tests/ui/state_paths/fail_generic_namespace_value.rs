use rca_macros::{ActionPaths, StatePaths};
use std::marker::PhantomData;

#[derive(StatePaths)]
struct State<T> {
    value: T,
}

#[derive(ActionPaths)]
enum Action<T> {
    Value(T),
}

fn destructure(state: StatePaths<u8>, action: ActionPaths<u8>) {
    let StatePaths(_) = state;
    let ActionPaths(_) = action;
}

fn main() {
    let _ = StatePaths::<u8>(PhantomData);
    let _ = ActionPaths::<u8>(PhantomData);
}
