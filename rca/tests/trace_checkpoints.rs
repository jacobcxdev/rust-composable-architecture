//! Checks checkpoint behaviour in debug and release builds without strengthening state bounds.

#![cfg(feature = "diagnostics")]

use std::cell::Cell;
use std::rc::Rc;

use rca::diagnostics::{CheckpointOutcome, checkpoint, checkpoint_disabled};

#[test]
fn facade_checkpoint_preserves_local_error_identity_and_observation_order() {
    let order = std::cell::RefCell::new(Vec::new());
    let mut state = Rc::new(Cell::new(0));
    let error = Rc::new("private typed error");
    let result = checkpoint(
        &mut state,
        "outer.operation",
        |state| {
            order.borrow_mut().push("capture");
            state.get()
        },
        |state| {
            order.borrow_mut().push("operation");
            state.set(1);
            Err::<(), _>(Rc::clone(&error))
        },
        |event| {
            order.borrow_mut().push("sink");
            assert_eq!(event.label(), "outer.operation");
            assert_eq!(event.outcome(), CheckpointOutcome::Error);
            assert!(event.is_changed());
        },
    );
    assert!(Rc::ptr_eq(&result.unwrap_err(), &error));
    assert_eq!(state.get(), 1);
    #[cfg(debug_assertions)]
    assert_eq!(*order.borrow(), ["capture", "operation", "capture", "sink"]);
    #[cfg(not(debug_assertions))]
    assert_eq!(*order.borrow(), ["operation"]);
}

#[test]
fn facade_disabled_checkpoint_runs_operation_without_state_bounds() {
    let mut state = Rc::new(Cell::new(0));
    let result = checkpoint_disabled(&mut state, |state| {
        state.set(state.get() + 1);
        Ok::<_, ()>(Rc::clone(state))
    });
    assert!(Rc::ptr_eq(&result.unwrap(), &state));
    assert_eq!(state.get(), 1);
}

#[cfg(feature = "test-util")]
#[test]
fn facade_disabled_trace_keeps_base_effect_and_single_reduction() {
    use rca::{Effect, ReduceContext, Reducer, ReducerExt, TestStore};

    /// Emits a second action so disabled tracing must preserve both reduction and effects.
    struct Counter;

    impl Reducer for Counter {
        type State = usize;
        type Action = usize;

        fn reduce(
            &mut self,
            state: &mut usize,
            action: &usize,
            _: &mut ReduceContext<'_>,
        ) -> Effect<usize> {
            *state += action;
            if *action == 1 {
                Effect::send(2)
            } else {
                Effect::none()
            }
        }
    }

    let reducer: Counter = Counter.trace_changes_disabled();
    futures_executor::block_on(async {
        let mut store = TestStore::new(0, reducer);
        let sent = store.send_with(1, |state| *state = 1).await.unwrap();
        let received = store
            .receive_with(|action| *action == 2, |state| *state = 3)
            .await
            .unwrap();
        received.finish().await.unwrap();
        sent.finish().await.unwrap();
        store.finish().await.unwrap();
    });
}
