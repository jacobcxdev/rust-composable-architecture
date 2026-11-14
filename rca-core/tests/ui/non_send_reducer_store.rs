use std::{cell::Cell, rc::Rc};

use rca_core::{Effect, ReduceContext, Reducer, Runtime, Store};

struct Local(Rc<Cell<usize>>);

impl Reducer for Local {
    type State = usize;
    type Action = ();

    fn reduce(&mut self, state: &mut usize, _: &(), _: &mut ReduceContext<'_>) -> Effect<()> {
        *state += self.0.get();
        Effect::none()
    }
}

fn cross_actor_boundary(runtime: Runtime) {
    let _ = Store::new(0, Local(Rc::new(Cell::new(1))), runtime);
}

fn main() {}
