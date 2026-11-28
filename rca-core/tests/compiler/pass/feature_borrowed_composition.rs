//@ check-pass
// Static composition keeps borrowed, non-Send and non-Clone domains and selectors.
use std::{cell::Cell, marker::PhantomData};

use rca_core::{
    ActionPath, Effect, Feature, FeatureContext, ReduceContext, Reducer, StatePath, combine, scope,
};

/// Returns no work for any state and action domain.
struct Leaf<S, A>(PhantomData<fn(S, A)>);

impl<S, A> Reducer for Leaf<S, A> {
    type State = S;
    type Action = A;

    fn reduce(&mut self, _: &mut S, _: &A, _: &mut ReduceContext<'_>) -> Effect<A> {
        Effect::none()
    }
}

/// Borrows a local counter without implementing Clone, Send or Sync.
struct Selector<'a>(&'a Cell<usize>);

impl Selector<'_> {
    /// Records a projection while retaining the whole selector capture.
    fn record(&self) {
        self.0.set(self.0.get() + 1);
    }
}

/// Requires the composed reducer to implement Feature without an adapter.
fn accepts_feature<F: Feature>(_: &F) {}

/// Keeps state unrestricted and applies only the existing action lifetime bound.
pub fn borrowed_composition<S, A: 'static>(
    state: &mut S,
    action: &A,
    observations: &Cell<usize>,
    reducer_context: &mut ReduceContext<'_>,
    feature_context: &mut FeatureContext<'_, S, A>,
) {
    let read = Selector(observations);
    let write = Selector(observations);
    let path = StatePath::new(
        move |state: &S| {
            read.record();
            state
        },
        move |state: &mut S| {
            write.record();
            state
        },
    );
    let actions = ActionPath::new(|action: &A| Some(action), |action| action);
    let mut feature = combine((scope(path, actions, Leaf(PhantomData)),));

    accepts_feature(&feature);
    // Both traits are in scope. Neither method requires disambiguation.
    let _: Effect<A> = feature.reduce(state, action, reducer_context);
    feature.update(state, action, feature_context);
}

/// Direct Feature implementations remain available on separate types.
pub struct DirectFeature;

impl Feature for DirectFeature {
    type State = usize;
    type Action = ();

    fn update(&mut self, state: &mut usize, _: &(), _: &mut FeatureContext<'_, usize, ()>) {
        *state += 1;
    }
}

fn main() {}
