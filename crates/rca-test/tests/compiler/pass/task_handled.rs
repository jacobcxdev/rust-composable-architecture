//@ check-pass
// Positive counterpart to task_warning.rs: verify causal completion explicitly.
pub async fn waited(store: &mut rca_test::TestStore<String, ()>) {
    store.send(()).await.unwrap().finish().await.unwrap();
}

// Explicit discard leaves causal work running; it does not cancel the task.
pub async fn detached(store: &mut rca_test::TestStore<String, ()>) {
    drop(store.send(()).await.unwrap());
}

fn main() {}
