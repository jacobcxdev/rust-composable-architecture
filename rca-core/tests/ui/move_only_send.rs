// Shared store operations remain Send for move-only state and actions.

use std::fmt;

use rca_core::{
    Effect, MonotonicInstant, ReduceContext, Reducer, Runtime, RuntimeDriver, RuntimeFuture, Store,
};

struct MoveOnlyState(String);
struct MoveOnlyAction(String);
struct Feature;

impl Reducer for Feature {
    type State = MoveOnlyState;
    type Action = MoveOnlyAction;

    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &Self::Action,
        _context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        state.0.push_str(&action.0);
        Effect::none()
    }
}

#[derive(Clone)]
struct Driver;

impl fmt::Debug for Driver {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Driver")
    }
}

impl RuntimeDriver for Driver {
    fn spawn(&self, _task: RuntimeFuture) {}

    fn now(&self) -> MonotonicInstant {
        MonotonicInstant::ZERO
    }

    fn sleep_until(&self, _deadline: MonotonicInstant) -> RuntimeFuture {
        Box::pin(std::future::pending())
    }

    fn yield_now(&self) -> RuntimeFuture {
        Box::pin(std::future::ready(()))
    }
}

fn assert_send<T: Send>(_: T) {}
fn assert_send_sync<T: Send + Sync>() {}

fn main() {
    assert_send_sync::<Store<MoveOnlyState, MoveOnlyAction>>();
    let store = Store::new(MoveOnlyState(String::new()), Feature, Runtime::new(Driver));

    assert_send(store.send(MoveOnlyAction(String::from("action"))));
    assert_send(store.read(|state| state.0.len()));
    assert_send(store.observe(|state| state.0.len()));
    assert_send(store.shutdown());
    assert_send(store.closed());
}
