// A shared store cannot retain a selector that captures Rc ownership.

use std::fmt;
use std::rc::Rc;

use rca_core::{
    Effect, MonotonicInstant, ReduceContext, Reducer, Runtime, RuntimeDriver, RuntimeFuture, Store,
};

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

struct Feature;

impl Reducer for Feature {
    type State = usize;
    type Action = ();

    fn reduce(
        &mut self,
        _state: &mut Self::State,
        _action: &Self::Action,
        _context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        Effect::none()
    }
}

fn main() {
    let store = Store::new(0, Feature, Runtime::new(Driver));
    let capture = Rc::new(());
    let _observation = store.observe(move |state| {
        let _keep_capture = &capture;
        *state
    });
}
