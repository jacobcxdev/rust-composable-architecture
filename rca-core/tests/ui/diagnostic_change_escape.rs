use rca_core::diagnostics::Change;
use rca_core::{Effect, ReduceContext, Reducer, ReducerExt};
use std::sync::{Arc, Mutex};

struct Counter;
impl Reducer for Counter {
    type State = usize;
    type Action = ();
    fn reduce(&mut self, _: &mut usize, _: &(), _: &mut ReduceContext<'_>) -> Effect<()> {
        Effect::none()
    }
}

fn main() {
    let escaped: Arc<Mutex<Vec<Change<'static, (), usize>>>> = Arc::new(Mutex::new(Vec::new()));
    let _ = Counter.trace_changes(
        |state| *state,
        move |change| {
            escaped.lock().unwrap().push(change);
        },
    );
}
