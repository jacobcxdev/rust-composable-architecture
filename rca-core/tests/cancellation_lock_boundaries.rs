//! Qualifies successful synchronous commits around explicit contention failure.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, mpsc};

use rca_core::Shared;

// Running a Busy callback or losing either successful increment breaks this.
#[test]
fn explicit_retry_retains_both_increments_and_publishes_only_successful_accesses() {
    let shared = Shared::new(0);
    let publications = Arc::new(AtomicUsize::new(0));
    let rejected_mutations = AtomicUsize::new(0);
    let rejected_publications = AtomicUsize::new(0);
    let worker_value = shared.clone();
    let worker_publications = Arc::clone(&publications);
    let busy = std::thread::scope(|scope| {
        // These endpoints drop before the scope joins on either side's panic.
        let (entered, entry) = mpsc::channel();
        let (release, released) = mpsc::channel();
        let worker = scope.spawn(move || {
            worker_value
                .try_update(
                    |value| {
                        *value += 1;
                        entered.send(()).expect("main waits for contention");
                        released.recv().expect("main releases contention");
                        Ok::<_, ()>(())
                    },
                    || {
                        worker_publications.fetch_add(1, Ordering::SeqCst);
                    },
                )
                .unwrap();
        });
        entry.recv().expect("worker enters contention");
        let busy = shared.try_update(
            |value| {
                rejected_mutations.fetch_add(1, Ordering::SeqCst);
                *value += 1;
                Ok::<_, ()>(())
            },
            || {
                rejected_publications.fetch_add(1, Ordering::SeqCst);
            },
        );

        // Release and join before assertions; unwind also drops the endpoints.
        release.send(()).expect("worker waits for release");
        worker.join().expect("worker completes its update");
        busy
    });

    let busy = busy.unwrap_err();
    assert!(busy.access_error().unwrap().is_busy());
    assert!(!busy.is_committed());
    assert_eq!(rejected_mutations.load(Ordering::SeqCst), 0);
    assert_eq!(rejected_publications.load(Ordering::SeqCst), 0);
    assert_eq!(publications.load(Ordering::SeqCst), 1);
    assert_eq!(shared.read(|value| *value).unwrap(), 1);

    shared
        .try_update(
            |value| {
                *value += 1;
                Ok::<_, ()>(())
            },
            || {
                publications.fetch_add(1, Ordering::SeqCst);
            },
        )
        .unwrap();
    assert_eq!(shared.read(|value| *value).unwrap(), 2);
    assert_eq!(publications.load(Ordering::SeqCst), 2);
}
