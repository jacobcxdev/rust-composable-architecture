use rca_core::{FeatureContext, Output, RemovedChild};
use std::rc::Rc;

fn transfer<'turn>(
    removed: RemovedChild<'turn, u32>,
    context: &mut FeatureContext<'turn, (), ()>,
    output: &Output<u32>,
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
fn main() {}
