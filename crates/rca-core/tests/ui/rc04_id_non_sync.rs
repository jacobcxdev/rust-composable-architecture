use rca_core::{ActionPath, Effect, Reduce, ReduceContext, Reducer, StatePath, scope_identified};
use std::{
    cell::Cell,
    hash::{Hash, Hasher},
};

#[derive(Clone)]
struct Id(Cell<u64>);
impl PartialEq for Id {
    fn eq(&self, other: &Self) -> bool {
        self.0.get() == other.0.get()
    }
}
impl Eq for Id {}
impl Hash for Id {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.0.get().hash(state);
    }
}

fn check(context: &mut ReduceContext<'_>) {
    fn send<T: Send>() {}
    send::<Id>();
    let mut state = Id(Cell::new(0));
    let mut reducer = scope_identified(
        Reduce::new(|_: &mut Id, _: &(), _| Effect::none()),
        StatePath::new(|s: &Id| s, |s: &mut Id| s),
        ActionPath::new(|a: &()| Some(a), |a| a),
        |s: &Id| s,
        Reduce::new(|_: &mut Id, _: &(), _| Effect::none()),
    );
    let _ = Reducer::reduce(&mut reducer, &mut state, &(), context);
}
fn main() {}
