// A local reducer already implements the local Feature trait.
use rca_core::ReduceContext;
use rca_core::local::{Effect, Feature, FeatureContext, Reducer};

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
