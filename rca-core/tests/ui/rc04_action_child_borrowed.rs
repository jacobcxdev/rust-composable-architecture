use rca_core::{ActionPath, Effect, Reduce, ReduceContext, Reducer, StatePath, scope_identified};

fn extract<'a, 's>(_: &'a ()) -> Option<&'a &'s str> {
    None
}
fn embed(_: &str) {}

fn check<'s>(action: &'s str, context: &mut ReduceContext<'_>) {
    let mut child = Reduce::new(|_: &mut u64, _: &&'s str, _| Effect::none());
    let _ = Reducer::reduce(&mut child, &mut 0, &action, context);
    let mut reducer = scope_identified(
        Reduce::new(|_: &mut u64, _: &(), _| Effect::none()),
        StatePath::new(|s: &u64| s, |s: &mut u64| s),
        ActionPath::new(extract, embed),
        |s: &u64| s,
        child,
    );
    let _ = Reducer::reduce(&mut reducer, &mut 0, &(), context);
}
fn main() {}
