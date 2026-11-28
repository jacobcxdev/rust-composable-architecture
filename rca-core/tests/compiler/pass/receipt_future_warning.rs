//@ error-in-other-file: 1 warning emitted
// Converting the receipt does not cancel the admitted action or poll its future.
// `receipt_future_handled.rs` shows awaiting and explicit non-cancelling discard.
pub fn ignored(store: &rca_core::Store<String, ()>) {
    store.try_send(()).unwrap().into_future();
    //~^ WARN: unused `ActionReceiptFuture` that must be used
}

fn main() {}
