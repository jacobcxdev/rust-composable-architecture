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
#[cfg(not(feature = "selected"))]
struct NotAReducer;

#[rca_macros::reducer(
    state(generated(struct State {
        #[reducer(child = active)] active: _,
        #[cfg(feature = "selected")] #[reducer(child = absent)] absent: _,
    })),
    action(generated(enum Action {
        #[reducer(child = active)] Active(_),
        #[cfg_attr(not(feature = "selected"), cfg(any()))] #[reducer(child = absent)] Absent(_),
    })),
    body(generated),
    child(active, reducer = Child, kind = required,
        state = StatePaths::active(), action = ActionPaths::active(),
        bind(fn bind_active(child: Child)),
    ),
    #[cfg_attr(not(feature = "selected"), cfg(any()))]
    child(absent, reducer = Child, kind = required,
        state = StatePaths::absent(), action = ActionPaths::absent(),
        bind(fn bind_absent(child: Child)),
    ),
)]
struct Feature<A, B> {
    #[reducer(child = active)]
    active: A,
    #[cfg_attr(not(feature = "selected"), cfg(any()))]
    #[reducer(child = absent)]
    absent: B,
    #[reducer(data)]
    marker: core::marker::PhantomData<B>,
}

fn main() {
    #[cfg(not(feature = "selected"))]
    let feature = Feature::<_, NotAReducer> {
        active: bind_active(Child),
        marker: core::marker::PhantomData,
    };
    #[cfg(feature = "selected")]
    let feature = Feature {
        active: bind_active(Child),
        absent: bind_absent(Child),
        marker: core::marker::PhantomData,
    };
    let _: &dyn Reducer<State = State, Action = Action> = &feature;
    let state = State {
        active: 4,
        #[cfg(feature = "selected")]
        absent: 9,
    };
    assert_eq!(*StatePaths::active().get(&state), 4);
}
