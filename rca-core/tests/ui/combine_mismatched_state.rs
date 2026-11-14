// Combined reducers must agree on the state passed to the store.

use rca_core::{
    Effect, MonotonicInstant, ReduceContext, Reducer, Runtime, RuntimeDriver, RuntimeFuture, Store,
    combine,
};

#[derive(Debug)]
struct Driver;

impl RuntimeDriver for Driver {
    fn spawn(&self, _task: RuntimeFuture) {}

    fn now(&self) -> MonotonicInstant {
        MonotonicInstant::ZERO
    }

    fn sleep_until(&self, _deadline: MonotonicInstant) -> RuntimeFuture {
        Box::pin(async {})
    }

    fn yield_now(&self) -> RuntimeFuture {
        Box::pin(async {})
    }
}

struct ByteReducer;

impl Reducer for ByteReducer {
    type State = u8;
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

struct WordReducer;

impl Reducer for WordReducer {
    type State = u16;
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
    let _store = Store::new(
        0_u8,
        combine((ByteReducer, WordReducer)),
        Runtime::new(Driver),
    );
}
