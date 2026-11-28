//@ check-pass
// Positive counterpart to `receipt_future_warning.rs`: poll the receipt to commit.
pub async fn waited(store: &rca_core::Store<String, ()>) {
    store
        .try_send(())
        .unwrap()
        .into_future()
        .await
        .unwrap()
        .finish()
        .await;
}

// Explicit future discard does not cancel the admitted action or causal work.
pub fn detached(store: &rca_core::Store<String, ()>) {
    drop(store.try_send(()).unwrap().into_future());
}

fn main() {}
