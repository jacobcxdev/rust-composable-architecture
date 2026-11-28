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
    state(generated(struct MissingReducerState { #[reducer(child = counter)] counter: _ })),
    action(generated(enum MissingReducerAction { #[reducer(child = counter)] Counter(_) })),
    body(generated),
    child(counter, kind = required,
        state = MissingReducerStatePaths::counter(), action = MissingReducerActionPaths::counter(),
        bind(fn bind_missing_reducer(child: Counter)),
    ),
)]
struct MissingReducer<C> {
    #[reducer(child = counter)]
    counter: C,
}

#[rca_macros::reducer(
    state(generated(struct MissingKindState { #[reducer(child = counter)] counter: _ })),
    action(generated(enum MissingKindAction { #[reducer(child = counter)] Counter(_) })),
    body(generated),
    child(counter, reducer = Counter,
        state = MissingKindStatePaths::counter(), action = MissingKindActionPaths::counter(),
        bind(fn bind_missing_kind(child: Counter)),
    ),
)]
struct MissingKind<C> {
    #[reducer(child = counter)]
    counter: C,
}

#[rca_macros::reducer(
    state(generated(struct MissingStateState { #[reducer(child = counter)] counter: _ })),
    action(generated(enum MissingStateAction { #[reducer(child = counter)] Counter(_) })),
    body(generated),
    child(counter, reducer = Counter, kind = required,
        action = MissingStateActionPaths::counter(),
        bind(fn bind_missing_state(child: Counter)),
    ),
)]
struct MissingState<C> {
    #[reducer(child = counter)]
    counter: C,
}

#[rca_macros::reducer(
    state(generated(struct MissingActionState { #[reducer(child = counter)] counter: _ })),
    action(generated(enum MissingActionAction { #[reducer(child = counter)] Counter(_) })),
    body(generated),
    child(counter, reducer = Counter, kind = required,
        state = MissingActionStatePaths::counter(),
        bind(fn bind_missing_action(child: Counter)),
    ),
)]
struct MissingAction<C> {
    #[reducer(child = counter)]
    counter: C,
}

#[rca_macros::reducer(
    state(generated(struct MissingBindState { #[reducer(child = counter)] counter: _ })),
    action(generated(enum MissingBindAction { #[reducer(child = counter)] Counter(_) })),
    body(generated),
    child(counter, reducer = Counter, kind = required,
        state = MissingBindStatePaths::counter(), action = MissingBindActionPaths::counter(),
    ),
)]
struct MissingBind<C> {
    #[reducer(child = counter)]
    counter: C,
}

#[rca_macros::reducer(
    state(generated(enum MissingPolicyState { #[reducer(case = item)] Item })),
    action(empty), body(generated),
    case(item, action = empty),
)]
struct MissingPolicy {}

#[rca_macros::reducer(
    state(generated(enum MissingCaseActionState { #[reducer(case = item)] Item })),
    action(empty), body(generated),
    case(item, policy = ignored),
)]
struct MissingCaseAction {}

fn main() {}
