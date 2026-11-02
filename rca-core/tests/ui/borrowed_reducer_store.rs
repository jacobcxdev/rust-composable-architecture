use rca_core::{Effect, ReduceContext, Reducer, Runtime, Store};

struct Borrowed<'a>(&'a usize);

impl Reducer for Borrowed<'_> {
    type State = usize;
    type Action = ();

    fn reduce(&mut self, state: &mut usize, _: &(), _: &mut ReduceContext<'_>) -> Effect<()> {
        *state += *self.0;
        Effect::none()
    }
}

fn cross_actor_boundary(runtime: Runtime) {
    let value = 1;
    let _ = Store::new(0, Borrowed(&value), runtime);
}

fn main() {}
