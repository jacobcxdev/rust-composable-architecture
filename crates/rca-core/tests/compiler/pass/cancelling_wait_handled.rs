//@ check-pass
// Positive counterpart to `cancelling_wait_warning.rs`: await all causal work.
pub async fn waited(store: &rca_core::Store<String, ()>) {
    let task = store.send(()).await.unwrap();
    task.finish_cancelling_on_drop().await;
}

// Explicitly dropping this guard requests cancellation, unlike dropping the task.
pub async fn cancelled(store: &rca_core::Store<String, ()>) {
    let task = store.send(()).await.unwrap();
    drop(task.finish_cancelling_on_drop());
}

fn main() {}
