//@ check-pass
// Positive counterpart to `task_warning.rs`: await completion when it matters.
pub async fn waited(store: &architecture::Store<String, ()>) {
    store.send(()).await.unwrap().finish().await;
}

// Explicit discard leaves admitted causal work running; it is not cancellation.
pub async fn detached(store: &architecture::Store<String, ()>) {
    drop(store.send(()).await.unwrap());
}

fn main() {}
