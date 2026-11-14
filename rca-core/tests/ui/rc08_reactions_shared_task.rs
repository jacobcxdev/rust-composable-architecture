use rca_core::{Feature, FeatureContext, InitialPolicy, RootMount, Task};
use std::rc::Rc;
struct Root;
impl Feature for Root {
    type State = i32;
    type Action = ();
    fn update(&mut self, _: &mut i32, _: &(), _: &mut FeatureContext<'_, i32, ()>) {}
}
fn main() {
    let _ = RootMount::new(Root).on_change(
        |state| Rc::new(*state),
        InitialPolicy::Skip,
        |change, _| {
            Task::run(move |_| async move {
                drop(change);
            })
        },
    );
}
