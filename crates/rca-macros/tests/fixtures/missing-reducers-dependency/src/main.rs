// These macros must report the missing RCA dependency.

use rca_macros::{ActionPaths, DependencyKey, Snapshot, StatePaths, reducers};

#[derive(StatePaths)]
struct State {
    value: u8,
}

#[derive(ActionPaths)]
enum Action {
    Tap,
}

#[derive(Snapshot)]
struct SnapshotState {
    value: u8,
}

#[derive(DependencyKey)]
#[dependency(value = u8, live = 0)]
struct ValueKey;

fn main() {
    let _ = reducers![()];
}
