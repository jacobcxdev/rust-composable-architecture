use rca_core::testing::TransactionGate;
use rca_core::{Effect, Reduce, Runtime, Store};
fn wrong(runtime: Runtime) {
    let reducer = Reduce::new(|_: &mut usize, _: &(), _| Effect::none());
    let _ = Store::builder(0usize, reducer, runtime)
        .transaction_gate(TransactionGate::<String, ()>::new());
}
fn main() {}
