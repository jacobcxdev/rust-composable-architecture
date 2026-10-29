use rca_core::local::{FeatureContext, Output, RemovedChild};
use std::rc::Rc;

fn transfer<'turn>(
    removed: RemovedChild<'turn, Rc<u32>>,
    context: &mut FeatureContext<'turn, (), Rc<()>>,
    output: &Output<Rc<u32>>,
) {
    let retained = Rc::new(());
    removed
        .into_output(
            context,
            output,
            |value| value,
            move |_| {
                drop(retained);
            },
        )
        .unwrap();
}
fn owned_data(removed: RemovedChild<'_, Rc<u32>>) -> Rc<u32> {
    removed.into_inner()
}
fn main() {}
