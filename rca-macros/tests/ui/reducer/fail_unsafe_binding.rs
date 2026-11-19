use rca::{Effect, ReduceContext, Reducer};

struct Child;
impl Reducer for Child {
    type State = usize;
    type Action = usize;
    fn reduce(&mut self, _: &mut usize, _: &usize, _: &mut ReduceContext<'_>) -> Effect<usize> {
        Effect::none()
    }
}

#[rca_macros::reducer(
    state(supplied(usize)), action(supplied(usize)), body(supplied(body)),
    child(counter, reducer = Child, kind = required,
        state = rca::StatePath::new(|state: &usize| state, |state: &mut usize| state),
        action = rca::ActionPath::new(|action: &usize| Some(action), |action| action),
        bind(unsafe fn bind_child(child: Child)),
    ),
)]
struct Feature<R> {
    body: R,
}

fn main() {}
