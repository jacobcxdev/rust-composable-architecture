use std::{cell::Cell, rc::Rc};

use rca_core::{ActionPath, Effect, ReduceContext, Reducer, SharedReducer, StatePath, combine};

struct Local<'a>(&'a Cell<usize>);

impl<'a> Reducer for Local<'a> {
    type State = &'a Rc<Cell<usize>>;
    type Action = &'a usize;

    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &Self::Action,
        _: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        self.0.set(self.0.get() + 1);
        state.set(state.get() + **action);
        Effect::none()
    }
}

impl<'a> SharedReducer for Local<'a> {
    type State = &'a Rc<Cell<usize>>;
    type Shared = &'a Cell<usize>;
    type Action = &'a usize;

    fn reduce(
        &mut self,
        state: &mut Self::State,
        shared: &mut Self::Shared,
        action: &Self::Action,
        _: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        state.set(state.get() + **action);
        shared.set(shared.get() + state.get());
        Effect::none()
    }
}

fn reduce_locally<'a>(
    calls: &'a Cell<usize>,
    value: &'a Rc<Cell<usize>>,
    amount: &'a usize,
    context: &mut ReduceContext<'_>,
) {
    let mut state = value;
    let mut reducer = combine((Local(calls),));
    let _ = reducer.reduce(&mut state, &amount, context);
    let mut shared = calls;
    let _ = SharedReducer::reduce(&mut Local(calls), &mut state, &mut shared, &amount, context);
}

fn main() {
    struct State<'a> {
        value: &'a Rc<Cell<usize>>,
    }
    struct Action<'a> {
        value: &'a Rc<Cell<usize>>,
    }
    let value = Rc::new(Cell::new(0));
    let calls = Cell::new(0);
    let state_path = StatePath::new(
        |state: &State<'_>| &state.value,
        |state: &mut State<'_>| &mut state.value,
    );
    let action_path = ActionPath::new(
        |action: &Action<'_>| Some(&action.value),
        |value| {
            calls.set(calls.get() + 1);
            Action { value }
        },
    );
    let mut state = State { value: &value };
    state_path.get_mut(&mut state).set(3);
    assert_eq!(state_path.get(&state).get(), 3);
    let action = action_path.embed(&value);
    assert!(Rc::ptr_eq(action_path.extract(&action).unwrap(), &value));
    assert_eq!(calls.get(), 1);
    let _ = reduce_locally;
}
