//! Checks deferred effects and the original local cause across optional test support.

use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use rca::{Effect, EffectError};

/// Deliberately has no `Send`, `Clone`, `Debug` or error-trait implementation.
struct OpaqueCause(Rc<u8>);

/// Counts destruction independently of whether the effect body starts.
struct DropCount(Arc<AtomicUsize>);

impl Drop for DropCount {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

#[test]
fn default_facade_defers_work_and_releases_unlaunched_owned_capture() {
    let calls = Arc::new(AtomicUsize::new(0));
    let drops = Arc::new(AtomicUsize::new(0));
    let operation_calls = Arc::clone(&calls);
    let owned = DropCount(Arc::clone(&drops));
    let effect: Effect<u8> = Effect::try_run(
        move |_| async move {
            let _owned = owned;
            operation_calls.fetch_add(1, Ordering::SeqCst);
            Err(EffectError::Failed(OpaqueCause(Rc::new(7))))
        },
        |cause, _| {
            assert_eq!(*cause.0, 7);
            std::future::ready(())
        },
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    drop(effect);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
}

#[cfg(feature = "test-util")]
/// Runs the effect through `TestStore` without imposing transport bounds on its cause.
mod launched {
    use super::{Effect, EffectError, OpaqueCause, Rc};
    use rca::{ReduceContext, Reducer, TestStore};

    #[derive(Debug, PartialEq)]
    enum Action {
        Start,
        Received(u8),
    }

    /// Extracts a transferable result before constructing the recovery future.
    struct Feature;

    impl Reducer for Feature {
        type State = u8;
        type Action = Action;

        fn reduce(
            &mut self,
            state: &mut u8,
            action: &Action,
            _: &mut ReduceContext<'_>,
        ) -> Effect<Action> {
            match action {
                Action::Start => Effect::try_run(
                    |_| async { Err(EffectError::Failed(OpaqueCause(Rc::new(7)))) },
                    |cause, sender| {
                        let value = *cause.0;
                        drop(cause);
                        async move {
                            sender.send(Action::Received(value)).await.unwrap();
                        }
                    },
                ),
                Action::Received(value) => {
                    *state = *value;
                    Effect::none()
                }
            }
        }
    }

    #[test]
    fn facade_routes_local_opaque_cause_without_blanket_send_or_clone() {
        let mut store = TestStore::new(0, Feature);
        futures_executor::block_on(async {
            drop(store.send(Action::Start).await.unwrap());
            drop(
                store
                    .receive_with(
                        |action| matches!(action, Action::Received(7)),
                        |state| *state = 7,
                    )
                    .await
                    .unwrap(),
            );
            store.finish().await.unwrap();
        });
    }
}
