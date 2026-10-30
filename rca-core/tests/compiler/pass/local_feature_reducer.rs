//@ check-pass
// Local reducers also implement Feature without extra state or action bounds.
use std::cell::Cell;

use rca_core::ReduceContext;
use rca_core::local::{Effect, Feature, FeatureContext, Reduce, Reducer};

/// Requires the local reducer to implement Feature without an adapter.
fn accepts_feature<F: Feature>(_: &F) {}

/// Retains borrowed, non-Send captures and unrestricted state and action domains.
pub fn borrowed_reducer<S, A>(
    state: &mut S,
    action: &A,
    calls: &Cell<usize>,
    reducer_context: &mut ReduceContext<'_>,
    feature_context: &mut FeatureContext<'_, S, A>,
) {
    let mut feature = Reduce::new(|_: &mut S, _: &A, _: &mut ReduceContext<'_>| {
        calls.set(calls.get() + 1);
        Effect::none()
    });

    accepts_feature(&feature);
    // Both traits are in scope. Neither method requires disambiguation.
    let _: Effect<A> = feature.reduce(state, action, reducer_context);
    feature.update(state, action, feature_context);
}

/// Direct Feature implementations remain available on separate local types.
pub struct DirectFeature;

impl Feature for DirectFeature {
    type State = usize;
    type Action = ();

    fn update(&mut self, state: &mut usize, _: &(), _: &mut FeatureContext<'_, usize, ()>) {
        *state += 1;
    }
}

fn main() {}
