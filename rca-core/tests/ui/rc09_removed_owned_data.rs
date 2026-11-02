#![expect(
    dead_code,
    reason = "uncalled functions check turn-bound type signatures"
)]
use rca_core::{FeatureContext, Output, RemovedChild};
use std::cell::Cell;
use std::rc::Rc;

struct Owned(Cell<u32>);
fn own(removed: RemovedChild<'_, Owned>) -> Owned {
    removed.into_inner()
}
fn transfer<'turn>(
    removed: RemovedChild<'turn, Owned>,
    context: &mut FeatureContext<'turn, (), ()>,
    output: &Output<Box<Cell<u32>>>,
) {
    let synchronous_only = Rc::new(());
    removed
        .into_output(
            context,
            output,
            move |owned| {
                drop(synchronous_only);
                Box::new(owned.0)
            },
            |result| {
                drop(result);
            },
        )
        .unwrap();
}
fn main() {}
