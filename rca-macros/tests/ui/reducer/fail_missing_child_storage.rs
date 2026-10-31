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
    state(generated(struct State { #[reducer(child = child)] value: _ })),
    action(generated(enum Action { #[reducer(child = child)] Child(_) })),
    body(generated),
    child(child, reducer = Child, kind = required,
        state = StatePaths::value(), action = ActionPaths::child(),
        bind(fn bind_child(child: Child))),
)]
struct Feature {}

fn main() {}
