#![deny(warnings)]

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
        #[r#cfg_attr(false, r#cfg(false))] #[r#reducer(r#child = active)] active: _,
        #[r#cfg_attr(true, r#cfg(false))] #[r#reducer(r#child = absent)] absent: _,
    })),
    action(generated(enum Action {
        #[r#cfg_attr(false, r#cfg(false))] #[r#reducer(r#child = active)] Active(_),
        #[r#cfg_attr(true, r#cfg_attr(true, r#cfg(false)))] #[r#reducer(r#child = absent)] Absent(_),
    })),
    body(generated),
    #[r#cfg_attr(false, r#cfg(false))]
    child(active, reducer = Child, kind = required,
        state = StatePaths::active(), action = ActionPaths::active(),
        bind(fn bind_active(child: Child)),
    ),
    #[r#cfg_attr(true, r#cfg(false))]
    child(absent, reducer = MissingReducer, kind = required,
        state = StatePaths::absent(), action = ActionPaths::absent(),
        bind(fn bind_absent(child: MissingReducer)),
    ),
)]
struct Feature<C> {
    #[r#cfg_attr(false, r#cfg(false))]
    #[r#reducer(r#child = active)]
    active: C,
    #[r#cfg_attr(true, r#cfg(false))]
    #[r#reducer(r#child = absent)]
    absent: MissingAdapter,
}

#[rca_macros::reducer(
    state(supplied(usize)), action(supplied(Vec<u8>)), body(supplied(body)),
)]
struct ConditionalSupplied {
    #[cfg(false)]
    body: MissingAdapter,
    #[cfg(true)]
    body: Child,
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

    let supplied = ConditionalSupplied { body: Child };
    let _: &dyn Reducer<State = usize, Action = Vec<u8>> = &supplied;
}
