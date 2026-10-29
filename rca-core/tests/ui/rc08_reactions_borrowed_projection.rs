use rca_core::{Feature, FeatureContext, InitialPolicy, RootMount, Task};
struct Root;
impl Feature for Root {
    type State = String;
    type Action = ();
    fn reduce(&mut self, _: &mut String, _: &(), _: &mut FeatureContext<'_, String, ()>) {}
}
fn main() {
    let _ = RootMount::new(Root).on_change(
        |state| state.as_str(),
        InitialPolicy::Skip,
        |_, _| Task::run(|_| async {}),
    );
}
