#![cfg(loom)]
//! Explores small admission, cancellation and value-transfer reference models.
//!
//! These models do not call RCA. They check their stated ordering rules, not the
//! production implementation. Production-backed mailbox models live in its unit tests.

use loom::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use loom::sync::{Arc, Mutex};
use loom::thread;

#[test]
fn lifecycle_close_and_admission_linearize() {
    const OPEN: usize = 0;
    const ADMITTED: usize = 1;
    const CLOSED: usize = 2;

    loom::model(|| {
        let state = Arc::new(AtomicUsize::new(OPEN));
        let accepted = Arc::new(AtomicUsize::new(0));
        let terminal = Arc::new(AtomicUsize::new(0));

        let admission_state = Arc::clone(&state);
        let admission_accepted = Arc::clone(&accepted);
        let admission_terminal = Arc::clone(&terminal);
        let admission = thread::spawn(move || {
            if admission_state
                .compare_exchange(OPEN, ADMITTED, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                admission_accepted.fetch_add(1, Ordering::Release);
                thread::yield_now();
                admission_terminal.fetch_add(1, Ordering::Release);
            }
        });

        let close_state = Arc::clone(&state);
        let close = thread::spawn(move || {
            close_state.swap(CLOSED, Ordering::AcqRel);
        });

        admission.join().expect("admission thread");
        close.join().expect("close thread");

        assert_eq!(state.load(Ordering::Acquire), CLOSED);
        assert!(accepted.load(Ordering::Acquire) <= 1);
        assert_eq!(
            terminal.load(Ordering::Acquire),
            accepted.load(Ordering::Acquire)
        );
    });
}

#[test]
fn lineage_cancel_and_validation_have_one_winner() {
    const LIVE: usize = 0;
    const VALIDATED: usize = 1;
    const CANCELLED: usize = 2;

    loom::model(|| {
        let state = Arc::new(AtomicUsize::new(LIVE));
        let committed = Arc::new(AtomicBool::new(false));

        let validation_state = Arc::clone(&state);
        let validation_committed = Arc::clone(&committed);
        let validation = thread::spawn(move || {
            if validation_state
                .compare_exchange(LIVE, VALIDATED, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                validation_committed.store(true, Ordering::Release);
            }
        });

        let cancellation_state = Arc::clone(&state);
        let cancellation = thread::spawn(move || {
            let _ = cancellation_state.compare_exchange(
                LIVE,
                CANCELLED,
                Ordering::AcqRel,
                Ordering::Acquire,
            );
        });

        validation.join().expect("validation thread");
        cancellation.join().expect("cancellation thread");

        let terminal = state.load(Ordering::Acquire);
        assert!(matches!(terminal, VALIDATED | CANCELLED));
        assert_eq!(committed.load(Ordering::Acquire), terminal == VALIDATED);
    });
}

#[test]
fn observation_in_flight_take_preserves_newest_value() {
    loom::model(|| {
        let slot = Arc::new(Mutex::new(None::<(usize, usize)>));
        let in_flight = Arc::new(Mutex::new(None::<(usize, usize)>));
        let delivered = Arc::new(Mutex::new(None::<(usize, usize)>));
        let version = Arc::new(AtomicUsize::new(0));

        let publish_slot = Arc::clone(&slot);
        let publish_version = Arc::clone(&version);
        let publisher = thread::spawn(move || {
            *publish_slot.lock().expect("observation slot") = Some((1, 10));
            publish_version.store(1, Ordering::Release);
            thread::yield_now();
            *publish_slot.lock().expect("observation slot") = Some((2, 20));
            publish_version.store(2, Ordering::Release);
        });

        let take_slot = Arc::clone(&slot);
        let take_in_flight = Arc::clone(&in_flight);
        let take_delivered = Arc::clone(&delivered);
        let taker = thread::spawn(move || {
            *take_in_flight.lock().expect("in-flight value") =
                take_slot.lock().expect("observation slot").take();
            thread::yield_now();
            *take_delivered.lock().expect("delivered value") =
                take_in_flight.lock().expect("in-flight value").take();
        });

        publisher.join().expect("publisher thread");
        taker.join().expect("taker thread");

        let delivered = *delivered.lock().expect("delivered value");
        let slot = *slot.lock().expect("observation slot");
        assert!(matches!(
            (delivered, slot),
            (None | Some((1, 10)), Some((2, 20))) | (Some((2, 20)), None)
        ));
        assert!(in_flight.lock().expect("in-flight value").is_none());
        assert_eq!(version.load(Ordering::Acquire), 2);
    });
}
