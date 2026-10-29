use rca::{Effect, ReduceContext, Reducer};

struct Child;
impl Reducer for Child {
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
    state(generated(struct State {
        #[cfg(not(feature = "selected"))] #[reducer(child = active)] active: _,
        #[cfg(any())] #[reducer(child = absent)] absent: _,
    })),
    action(generated(enum Action {
        #[cfg(not(feature = "selected"))] #[reducer(child = active)] Active(_),
        #[cfg_attr(not(feature = "selected"), cfg(any()))] #[reducer(child = absent)] Absent(_),
    })),
    body(generated),
    #[cfg(not(feature = "selected"))]
    child(active, reducer = Child, kind = required,
        state = StatePaths::active(), action = ActionPaths::active(),
        bind(fn bind_active(child: Child)),
    ),
    #[cfg_attr(not(feature = "selected"), cfg(any()))]
    child(absent, reducer = MissingReducer, kind = required,
        state = StatePaths::absent(), action = ActionPaths::absent(),
        bind(fn bind_absent(child: MissingReducer)),
    ),
)]
struct Feature<C> {
    #[cfg(not(feature = "selected"))]
    #[reducer(child = active)]
    active: C,
    #[cfg(any())]
    #[reducer(child = absent)]
    absent: MissingAdapter,
}

fn main() {
    let feature = Feature {
        active: bind_active(Child),
    };
    let _: &dyn Reducer<State = State, Action = Action> = &feature;
    let mut state = State { active: 2 };
    *StatePaths::active().get_mut(&mut state) = 4;
    assert_eq!(state.active, 4);
    let action = ActionPaths::active().embed(vec![1, 2, 3]);
    assert_eq!(ActionPaths::active().extract(&action).unwrap(), &[1, 2, 3]);
}
