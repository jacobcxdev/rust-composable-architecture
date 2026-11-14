use rca_core::{Effect, ReduceContext, SharedReducer};
struct Child;
impl SharedReducer for Child {
    type State = u64;
    type Shared = u64;
    type Action = ();
    fn reduce(
        &mut self,
        state: &mut u64,
        _: &mut u64,
        _: &(),
        _: &mut ReduceContext<'_>,
    ) -> Effect<()> {
        Effect::run(move |_| async move {
            *state += 1;
        })
    }
}
fn main() {}
