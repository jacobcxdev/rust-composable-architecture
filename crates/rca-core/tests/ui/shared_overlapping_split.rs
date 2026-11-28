use rca_core::{ActionPath, Effect, ReduceContext, SharedReducer, scope_shared};
struct Child;
impl SharedReducer for Child {
    type State = u64;
    type Shared = u64;
    type Action = ();
    fn reduce(
        &mut self,
        _: &mut u64,
        _: &mut u64,
        _: &(),
        _: &mut ReduceContext<'_>,
    ) -> Effect<()> {
        Effect::none()
    }
}
fn main() {
    let _ = scope_shared(
        |state: &mut u64| (state, state),
        ActionPath::new(|action: &()| Some(action), |action| action),
        Child,
    );
}
