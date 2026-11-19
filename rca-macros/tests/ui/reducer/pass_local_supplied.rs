use rca::{Effect, ReduceContext, Reducer};
use std::{cell::Cell, rc::Rc};

struct State<'a>(&'a Rc<Cell<usize>>);
struct Action<'a>(&'a usize, Rc<()>);
struct Local<'a>(&'a Cell<usize>);

impl<'a> Reducer for Local<'a> {
    type State = State<'a>;
    type Action = Action<'a>;
    fn reduce(
        &mut self,
        state: &mut State<'a>,
        action: &Action<'a>,
        _: &mut ReduceContext<'_>,
    ) -> Effect<Action<'a>> {
        self.0.set(self.0.get() + 1);
        state.0.set(state.0.get() + *action.0);
        let _identity = &action.1;
        Effect::none()
    }
}

#[rca_macros::reducer(
    state(supplied(State<'a>)), action(supplied(Action<'a>)), body(supplied(body)),
)]
struct Feature<'a, R> {
    body: R,
    marker: core::marker::PhantomData<&'a ()>,
}

fn accepts_local<'a>(feature: Feature<'a, Local<'a>>, _: State<'a>, _: Action<'a>) {
    let _: &dyn Reducer<State = State<'a>, Action = Action<'a>> = &feature;
}

fn main() {
    let value = Rc::new(Cell::new(2));
    let calls = Cell::new(0);
    let amount = 3;
    accepts_local(
        Feature {
            body: Local(&calls),
            marker: core::marker::PhantomData,
        },
        State(&value),
        Action(&amount, Rc::new(())),
    );
}
