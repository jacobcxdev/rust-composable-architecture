use rca_core::{ActionPath, Effect, Reduce, ReduceContext, Reducer, StatePath, scope_identified};

fn check(context: &mut ReduceContext<'_>) {
    let mut state = std::rc::Rc::new(0_u64);
    let mut reducer = scope_identified(
        Reduce::new(|_: &mut std::rc::Rc<u64>, _: &(), _| Effect::none()),
        StatePath::new(|s: &std::rc::Rc<u64>| s, |s: &mut std::rc::Rc<u64>| s),
        ActionPath::new(|a: &()| Some(a), |a| a),
        |s: &std::rc::Rc<u64>| s,
        Reduce::new(|_: &mut std::rc::Rc<u64>, _: &(), _| Effect::none()),
    );
    let _ = Reducer::reduce(&mut reducer, &mut state, &(), context);
}
fn main() {}
