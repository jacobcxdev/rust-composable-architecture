use rca_core::{ActionPath, Effect, Reduce, ReduceContext, Reducer, StatePath, scope_identified};

fn identity<'a, 's>(state: &'a &'s str) -> &'a &'s str {
    state
}

fn check<'s>(id: &'s str, context: &mut ReduceContext<'_>) {
    let mut state = id;
    let mut reducer = scope_identified(
        Reduce::new(|_: &mut &'s str, _: &(), _| Effect::none()),
        StatePath::new(|s: &&'s str| s, |s: &mut &'s str| s),
        ActionPath::new(|a: &()| Some(a), |a| a),
        identity,
        Reduce::new(|_: &mut &'s str, _: &(), _| Effect::none()),
    );
    let _ = Reducer::reduce(&mut reducer, &mut state, &(), context);
}
fn main() {}
