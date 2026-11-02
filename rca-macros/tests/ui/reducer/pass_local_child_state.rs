use rca::{Effect, ReduceContext, Reducer};
use std::{cell::Cell, rc::Rc};

struct Child<'a>(&'a Cell<usize>);
impl<'a> Reducer for Child<'a> {
    type State = &'a Rc<Cell<usize>>;
    type Action = Vec<u8>;
    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &Self::Action,
        _: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        self.0.set(self.0.get() + 1);
        state.set(state.get() + action.len());
        Effect::none()
    }
}

#[rca_macros::reducer(
    state(generated(struct State<'a> {
        #[reducer(child = local)] value: _,
    })),
    action(supplied(Vec<u8>)),
    body(supplied(local)),
    child(local, reducer = Child<'a>, kind = required,
        state = StatePaths::value(),
        action = rca::ActionPath::new(|action: &Vec<u8>| Some(action), |action| action),
        bind(fn bind_local<'a>(child: Child<'a>)),
    ),
)]
struct Feature<'a, C> {
    local: C,
    marker: core::marker::PhantomData<&'a ()>,
}

fn main() {
    let calls = Cell::new(0);
    let value = Rc::new(Cell::new(3));
    let feature = Feature {
        local: bind_local(Child(&calls)),
        marker: core::marker::PhantomData,
    };
    let _: &dyn Reducer<State = State<'_>, Action = Vec<u8>> = &feature;
    let state = State { value: &value };
    assert!(std::ptr::eq(*StatePaths::value().get(&state), &value));
}
