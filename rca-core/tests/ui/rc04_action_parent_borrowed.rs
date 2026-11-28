use rca_core::{ActionPath, Effect, Reduce, ReduceContext, Reducer, StatePath, scope_identified};

fn embed<'s>(_: ()) -> &'s str {
    "owned output"
}

fn check<'s>(action: &'s str, context: &mut ReduceContext<'_>) {
    let mut reducer = scope_identified(
        Reduce::new(|_: &mut u64, _: &&'s str, _| Effect::none()),
        StatePath::new(|s: &u64| s, |s: &mut u64| s),
        ActionPath::new(|_: &&'s str| None::<&()>, embed),
        |s: &u64| s,
        Reduce::new(|_: &mut u64, _: &(), _| Effect::none()),
    );
    let _ = Reducer::reduce(&mut reducer, &mut 0, &action, context);
}
fn main() {}
