use rca_core::{ActionPath, Effect, Reduce, ReduceContext, Reducer, StatePath, scope_identified};

fn check(context: &mut ReduceContext<'_>) {
    let capture = std::rc::Rc::new(0);
    let mut reducer = scope_identified(
        Reduce::new(|_: &mut u64, _: &(), _| Effect::none()),
        StatePath::new(|s: &u64| s, |s: &mut u64| s),
        ActionPath::new(
            |a: &()| Some(a),
            move |a| {
                std::hint::black_box(&capture);
                a
            },
        ),
        |s: &u64| s,
        Reduce::new(|_: &mut u64, _: &(), _| Effect::none()),
    );
    let _ = Reducer::reduce(&mut reducer, &mut 0, &(), context);
}
fn main() {}
