//@ normalize-stderr-test: "rca_core::Feature" -> "Feature"
//@ normalize-stderr-test: "rca_core::Reducer" -> "Reducer"
// Enabling Local adds same-named traits, so rustc qualifies the shared names.
// A reducer already implements Feature through the library's blanket implementation.
use rca_core::{Effect, Feature, FeatureContext, ReduceContext, Reducer};

struct Counter;

impl Reducer for Counter {
    type State = usize;
    type Action = ();

    fn reduce(&mut self, state: &mut usize, _: &(), _: &mut ReduceContext<'_>) -> Effect<()> {
        *state += 1;
        Effect::none()
    }
}

impl Feature for Counter {
    //~^ ERROR: conflicting implementations of trait
    type State = usize;
    type Action = ();

    fn update(&mut self, state: &mut usize, _: &(), _: &mut FeatureContext<'_, usize, ()>) {
        *state += 1;
    }
}

fn main() {}
