// Shared store construction rejects state that cannot cross threads.

use std::rc::Rc;

use rca_core::{Effect, ReduceContext, Reducer, Runtime, Store};

struct Bad;

impl Reducer for Bad {
    type State = Rc<()>;
    type Action = ();

    fn reduce(
        &mut self,
        _state: &mut Self::State,
        _action: &Self::Action,
        _context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        Effect::none()
    }
}

fn cross_actor_boundary(runtime: Runtime) {
    let _ = Store::new(Rc::new(()), Bad, runtime);
}

fn main() {}
