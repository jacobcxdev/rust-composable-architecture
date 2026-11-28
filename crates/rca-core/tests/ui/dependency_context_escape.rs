// A temporary dependency override cannot escape inside deferred work.

use rca_core::{DependencyKey, Effect, ReduceContext, Reducer};

struct ValueKey;

impl DependencyKey for ValueKey {
    type Value = usize;

    fn live_value() -> Self::Value {
        0
    }
}

struct Feature;

impl Reducer for Feature {
    type State = ();
    type Action = ();

    fn reduce(
        &mut self,
        _state: &mut Self::State,
        _action: &Self::Action,
        context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        let _escaped = context.with_dependency_override::<ValueKey, _>(1, |context| async move {
            std::hint::black_box(context.dependency::<ValueKey>());
        });
        Effect::none()
    }
}

fn main() {}
