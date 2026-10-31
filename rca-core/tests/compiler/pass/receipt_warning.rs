//@ error-in-other-file: 1 warning emitted
// Admission has already occurred; ignoring its receipt does not cancel it.
// `receipt_handled.rs` shows waiting and explicit non-cancelling discard.
pub fn ignored(store: &rca_core::Store<String, ()>) {
    store.try_send(()).unwrap();
    //~^ WARN: unused `ActionReceipt` that must be used
}

fn main() {}
