use rca_core::{Effect, ReduceContext, Reducer, ReducerExt};
use std::cell::Cell;
use std::rc::Rc;

struct Counter;
impl Reducer for Counter {
    type State = usize;
    type Action = ();
    fn reduce(&mut self, state: &mut usize, _: &(), _: &mut ReduceContext<'_>) -> Effect<()> {
        *state += 1;
        Effect::none()
    }
}

fn reducer<R: Reducer>(_: &R) {}
fn main() {
    let offset = 3;
    let calls = Rc::new(Cell::new(0));
    let wrapped = Counter.trace_changes(
        |state| *state + offset,
        |change| {
            calls.set(calls.get() + 1);
            assert!(change.after() > change.before());
        },
    );
    reducer(&wrapped);
    let borrowed = Counter.trace_changes(
        |state| *state + offset,
        |change| {
            assert_eq!(*change.after(), offset + 1);
        },
    );
    reducer(&borrowed);
}
