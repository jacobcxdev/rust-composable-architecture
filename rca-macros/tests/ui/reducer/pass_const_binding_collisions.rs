#![allow(non_upper_case_globals)]

use rca::{Effect, ReduceContext, Reducer};

struct Body;
impl Reducer for Body {
    type State = usize;
    type Action = usize;
    fn reduce(
        &mut self,
        value: &mut usize,
        amount: &usize,
        _: &mut ReduceContext<'_>,
    ) -> Effect<usize> {
        *value += *amount;
        Effect::none()
    }
}

#[rca_macros::reducer(state(supplied(usize)), action(supplied(usize)), body(supplied(body)))]
struct Feature<R, const state: usize, const action: usize, const context: usize> {
    body: R,
}

fn main() {
    let feature = Feature::<_, 1, 2, 3> { body: Body };
    let _: &dyn Reducer<State = usize, Action = usize> = &feature;
}
