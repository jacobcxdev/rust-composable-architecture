use rca_core::{Feature, FeatureContext, InitialPolicy, RootMount, Task};
use std::rc::Rc;
struct Root;
impl Feature for Root {
    type State = i32;
    type Action = ();
    fn reduce(&mut self, _: &mut i32, _: &(), _: &mut FeatureContext<'_, i32, ()>) {}
}
fn main() {
    let capture = Rc::new(1);
    let _ = RootMount::new(Root).on_change(
        move |state| *state + *capture,
        InitialPolicy::Skip,
        |_, _| Task::run(|_| async {}),
    );
}
