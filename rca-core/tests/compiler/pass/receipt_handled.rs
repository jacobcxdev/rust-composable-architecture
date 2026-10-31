//@ check-pass
// Positive counterpart to `receipt_warning.rs`: wait for commit and causal work.
pub async fn waited(store: &rca_core::Store<String, ()>) {
    store.try_send(()).unwrap().await.unwrap().finish().await;
}

// Explicit receipt discard leaves the admitted action and causal work running.
pub fn detached(store: &rca_core::Store<String, ()>) {
    drop(store.try_send(()).unwrap());
}

fn main() {}
