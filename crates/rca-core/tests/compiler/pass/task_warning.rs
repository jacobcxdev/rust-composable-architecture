//@ error-in-other-file: 1 warning emitted
// Ignoring a committed action's task does not cancel its causal work.
// `task_handled.rs` shows the awaited and intentionally discarded alternatives.
pub async fn ignored(store: &rca_core::Store<String, ()>) {
    store.send(()).await.unwrap();
    //~^ WARN: unused `ActionTask` that must be used
}

fn main() {}
