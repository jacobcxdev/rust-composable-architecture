use rca::{Effect, ReduceContext, Reducer};

struct Counter;
impl Reducer for Counter {
    type State = usize;
    type Action = Vec<u8>;

    fn reduce(
        &mut self,
        state: &mut usize,
        action: &Vec<u8>,
        _: &mut ReduceContext<'_>,
    ) -> Effect<Vec<u8>> {
        *state += action.len();
        Effect::none()
    }
}

#[rca_macros::reducer(
    state(generated(struct ChildState { #[reducer(child = counter)] counter: _ })),
    action(generated(enum ChildAction { #[reducer(child = counter)] Counter(_) })),
    body(generated),
    child(counter, reducer = Counter, kind = required,
        state = ChildStatePaths::counter(), action = ChildActionPaths::counter(),
        bind(fn bind_counter(child: Counter)),
    ),
)]
struct ChildFeature<C> {
    #[reducer(child = counter)]
    counter: C,
}

#[rca_macros::reducer(
    state(generated(enum CaseState { #[reducer(case = item)] Item })),
    action(empty), body(generated),
    case(item, policy = ignored, action = empty),
)]
struct CaseFeature {}

fn main() {
    let child = ChildFeature {
        counter: bind_counter(Counter),
    };
    let _: &dyn Reducer<State = ChildState, Action = ChildAction> = &child;
    let _: &dyn Reducer<State = CaseState, Action = core::convert::Infallible> = &CaseFeature {};
    assert_eq!(
        ChildStatePaths::counter().get(&ChildState { counter: 7 }),
        &7
    );
    assert_eq!(CaseStatePaths::item().get(&CaseState::Item), Some(()));
}
