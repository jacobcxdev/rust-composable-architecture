//! Preserves local owned observations, typed terminal outcomes and exactly-once cleanup.

use std::cell::Cell;
use std::rc::Rc;

use rca::{ExternalObservation, ObservationError, ObservationProducer};

/// Keeps observation values local and move-only without `Debug` or error-trait bounds.
struct LocalValue(Rc<u8>);

#[test]
fn default_facade_preserves_local_move_only_values_and_failure() {
    let shutdowns = Rc::new(Cell::new(0));
    let mut observation = ExternalObservation::new({
        let shutdowns = Rc::clone(&shutdowns);
        move |producer: ObservationProducer<LocalValue, LocalValue>| {
            assert!(producer.publish(LocalValue(Rc::new(2))).is_ok());
            assert!(producer.fail(LocalValue(Rc::new(3))));
            (LocalValue(Rc::new(1)), move || {
                shutdowns.set(shutdowns.get() + 1);
            })
        }
    });
    futures_executor::block_on(async {
        assert_eq!(*observation.next().await.unwrap().unwrap().0, 1);
        assert_eq!(*observation.next().await.unwrap().unwrap().0, 2);
        let error: ObservationError<LocalValue> = match observation.next().await {
            Err(error) => error,
            Ok(_) => panic!("expected original producer failure"),
        };
        assert!(!error.is_cancelled());
        assert_eq!(*error.into_failure().unwrap().0, 3);
        assert!(observation.next().await.unwrap().is_none());
    });
    assert_eq!(shutdowns.get(), 1);
    drop(observation);
    assert_eq!(shutdowns.get(), 1);
}

#[test]
fn default_facade_cancel_before_activation_has_typed_terminal_outcome() {
    let mut observation = ExternalObservation::<u8, ()>::new(|_| -> (u8, fn()) {
        panic!("cancelled subscription must not activate")
    });
    observation.cancel();
    futures_executor::block_on(async {
        let error = observation.next().await.unwrap_err();
        assert!(error.is_cancelled());
        assert!(error.into_failure().is_none());
        assert_eq!(observation.next().await.unwrap(), None);
    });
}
