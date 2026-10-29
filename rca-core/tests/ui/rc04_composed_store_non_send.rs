use rca_core::{
    ActionPath, Effect, Reduce, ReduceContext, Reducer, Runtime, StatePath, Store, scope_identified,
};
use std::{cell::Cell, rc::Rc};

fn check(runtime: Runtime, context: &mut ReduceContext<'_>) {
    let capture = Rc::new(Cell::new(0));
    let mut reducer = scope_identified(
        Reduce::new(|_: &mut u64, _: &(), _| Effect::none()),
        StatePath::new(
            move |s: &u64| {
                capture.set(capture.get() + 1);
                s
            },
            |s: &mut u64| s,
        ),
        ActionPath::new(|a: &()| Some(a), |a| a),
        |s: &u64| s,
        Reduce::new(|_: &mut u64, _: &(), _| Effect::none()),
    );
    let _ = Reducer::reduce(&mut reducer, &mut 0, &(), context);
    let _ = Store::new(0, reducer, runtime);
}
fn main() {}
