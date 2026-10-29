//@ check-pass
use std::cell::Cell;

use rca_core::{DependencyKey, Effect, Reduce};
use rca_test::TestStore;

struct Value;

impl DependencyKey for Value {
    type Value = u32;

    fn live_value() -> u32 {
        panic!("the factory supplies this value")
    }
}

pub fn configure() {
    let captured = Cell::new(1);
    let reducer = Reduce::new(|_: &mut (), _: &(), _| Effect::none());
    // A consumed factory can own Send-only data; no shared invocation is possible.
    let builder = TestStore::builder((), reducer)
        .with_clock_dependency::<Value>(move |_| captured.replace(2));
    fn owned_send<T: Send>(value: T) -> T {
        value
    }
    drop(owned_send(builder));
}

fn main() {}
