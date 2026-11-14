use rca_core::{FeatureContext, Task};
fn wrong(context: &mut FeatureContext<'_, usize, ()>) {
    context.task(Task::<String, ()>::run(|_| async {}));
}
fn main() {}
