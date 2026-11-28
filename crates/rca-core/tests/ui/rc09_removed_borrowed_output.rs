use rca_core::{FeatureContext, Output, RemovedChild};

fn transfer<'turn, 'borrow>(
    removed: RemovedChild<'turn, u32>,
    context: &mut FeatureContext<'turn, (), ()>,
    output: &Output<&'borrow str>,
    text: &'borrow str,
) {
    removed
        .into_output(context, output, move |_| text, |_| {})
        .unwrap();
}
fn main() {}
