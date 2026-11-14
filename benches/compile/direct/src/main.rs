//! Minimal Store consumer for compile-time and binary-size comparison.

use rca_core::{Effect, ReduceContext, Reducer};

/// Scalar state changed by one owned string action.
struct State {
    /// Total payload length committed by the reducer.
    count: usize,
}

/// Owned payload keeps the fixture distinct from a zero-sized action.
struct Action(
    /// Text whose length contributes to the state.
    String,
);

/// Effect-free baseline without composition or dependency lookup.
struct DirectReducer;

impl Reducer for DirectReducer {
    type State = State;
    type Action = Action;

    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &Self::Action,
        _context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        state.count = state.count.wrapping_add(action.0.len());
        Effect::none()
    }
}

fn main() {
    rca_compile_fixture_support::exercise(
        State { count: 0 },
        DirectReducer,
        Action(String::from("direct")),
        |state| assert_eq!(state.count, 6),
    );
}
