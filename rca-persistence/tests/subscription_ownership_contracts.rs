//! Checks original payload ownership across delivery, cancellation and provider completion.
//!
//! Heap addresses and drop counts distinguish moves from replacement or early cleanup.
//! A provider channel controls completion independently of subscription cancellation.

use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll, Waker};

use futures_executor::block_on;
use rca_persistence::{SharedUpdate, SharedUpdates};

#[derive(Debug)]
/// Tracks one move-only allocation without exposing it to framework cloning.
struct DropProbe {
    /// Gives the payload a stable address while its enclosing value moves.
    value: Box<u32>,
    /// Records destruction independently of the payload and subscription lifetimes.
    drops: Arc<AtomicUsize>,
}

impl DropProbe {
    /// Creates a distinct owned payload with an externally retained destruction counter.
    fn new(value: u32, drops: &Arc<AtomicUsize>) -> Self {
        Self {
            value: Box::new(value),
            drops: Arc::clone(drops),
        }
    }

    /// Returns an identity token that the tests compare but never dereference.
    fn identity(&self) -> *const u32 {
        std::ptr::from_ref(self.value.as_ref())
    }
}

impl Drop for DropProbe {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}

// Catches terminal overwrite, premature payload destruction, or replacing an owned cause.
#[test]
fn first_failure_preserves_initial_latest_and_rejected_payload_ownership() {
    let initial_drops = Arc::new(AtomicUsize::new(0));
    let latest_drops = Arc::new(AtomicUsize::new(0));
    let first_drops = Arc::new(AtomicUsize::new(0));
    let later_drops = Arc::new(AtomicUsize::new(0));
    let rejected_drops = Arc::new(AtomicUsize::new(0));
    let initial = DropProbe::new(7, &initial_drops);
    let latest = DropProbe::new(8, &latest_drops);
    let first = DropProbe::new(91, &first_drops);
    let initial_identity = initial.identity();
    let latest_identity = latest.identity();
    let first_identity = first.identity();
    let mut endpoint = None;
    let (mut updates, join) = SharedUpdates::activate(|producer| {
        producer.publish(SharedUpdate::Present(latest)).unwrap();
        assert!(producer.fail(first));
        assert!(!producer.finish());
        assert!(!producer.fail(DropProbe::new(92, &later_drops)));
        endpoint = Some(producer);
        (SharedUpdate::Present(initial), || {}, async { Ok(()) })
    });
    let producer = endpoint.unwrap();
    assert_eq!(later_drops.load(Ordering::SeqCst), 1);
    assert_eq!(initial_drops.load(Ordering::SeqCst), 0);
    assert_eq!(latest_drops.load(Ordering::SeqCst), 0);
    assert_eq!(first_drops.load(Ordering::SeqCst), 0);

    let rejected = DropProbe::new(9, &rejected_drops);
    let rejected_identity = rejected.identity();
    let SharedUpdate::Present(rejected) = producer
        .publish(SharedUpdate::Present(rejected))
        .unwrap_err()
    else {
        panic!("rejection lost the supplied payload");
    };
    assert_eq!(rejected.identity(), rejected_identity);
    assert_eq!(*rejected.value, 9);
    assert_eq!(rejected_drops.load(Ordering::SeqCst), 0);

    let Some(SharedUpdate::Present(initial)) = block_on(updates.next()).unwrap() else {
        panic!("failure overtook the initial value");
    };
    assert_eq!(initial.identity(), initial_identity);
    assert_eq!(*initial.value, 7);
    let Some(SharedUpdate::Present(latest)) = block_on(updates.next()).unwrap() else {
        panic!("failure overtook the latest value");
    };
    assert_eq!(latest.identity(), latest_identity);
    assert_eq!(*latest.value, 8);
    let error = block_on(updates.next()).unwrap_err();
    assert!(!error.is_cancelled());
    assert_eq!(first_drops.load(Ordering::SeqCst), 0);
    let first = error.into_failure().expect("missing first failure");
    assert_eq!(first.identity(), first_identity);
    assert_eq!(*first.value, 91);
    assert_eq!(first_drops.load(Ordering::SeqCst), 0);
    assert!(block_on(updates.next()).unwrap().is_none());
    block_on(join).unwrap();
    drop(updates);
    drop(producer);
    assert_eq!(initial_drops.load(Ordering::SeqCst), 0);
    assert_eq!(latest_drops.load(Ordering::SeqCst), 0);
    assert_eq!(first_drops.load(Ordering::SeqCst), 0);
    assert_eq!(rejected_drops.load(Ordering::SeqCst), 0);
    drop((initial, latest, first, rejected));
    assert_eq!(initial_drops.load(Ordering::SeqCst), 1);
    assert_eq!(latest_drops.load(Ordering::SeqCst), 1);
    assert_eq!(first_drops.load(Ordering::SeqCst), 1);
    assert_eq!(later_drops.load(Ordering::SeqCst), 1);
    assert_eq!(rejected_drops.load(Ordering::SeqCst), 1);
}

// Catches producer endpoints retaining cancellation ownership or cancellation before final alias drop.
#[test]
fn final_subscription_owner_discards_values_before_cancel_with_producers_retained() {
    let initial_drops = Arc::new(AtomicUsize::new(0));
    let latest_drops = Arc::new(AtomicUsize::new(0));
    let rejected_drops = Arc::new(AtomicUsize::new(0));
    let calls = Arc::new(AtomicUsize::new(0));
    let callback_initial = Arc::clone(&initial_drops);
    let callback_latest = Arc::clone(&latest_drops);
    let callback_calls = Arc::clone(&calls);
    let mut endpoint = None;
    let (updates, join) =
        SharedUpdates::<DropProbe, std::convert::Infallible>::activate(|producer| {
            producer
                .publish(SharedUpdate::Present(DropProbe::new(8, &latest_drops)))
                .unwrap();
            endpoint = Some(producer);
            (
                SharedUpdate::Present(DropProbe::new(7, &initial_drops)),
                move || {
                    assert_eq!(callback_initial.load(Ordering::SeqCst), 1);
                    assert_eq!(callback_latest.load(Ordering::SeqCst), 1);
                    callback_calls.fetch_add(1, Ordering::SeqCst);
                },
                async { Ok(()) },
            )
        });
    let producer = endpoint.unwrap();
    let producer_clone = producer.clone();
    let subscription = updates.subscription();
    drop(updates);
    assert!(!subscription.is_cancelled());
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(initial_drops.load(Ordering::SeqCst), 0);
    assert_eq!(latest_drops.load(Ordering::SeqCst), 0);
    drop(subscription);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(initial_drops.load(Ordering::SeqCst), 1);
    assert_eq!(latest_drops.load(Ordering::SeqCst), 1);

    let rejected = DropProbe::new(9, &rejected_drops);
    let rejected_identity = rejected.identity();
    let rejected = producer
        .publish(SharedUpdate::Present(rejected))
        .unwrap_err();
    let SharedUpdate::Present(rejected) = producer_clone.publish(rejected).unwrap_err() else {
        panic!("cancelled producer lost the supplied payload");
    };
    assert_eq!(rejected.identity(), rejected_identity);
    assert_eq!(*rejected.value, 9);
    assert_eq!(rejected_drops.load(Ordering::SeqCst), 0);
    drop(producer);
    drop(producer_clone);
    block_on(join).unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(initial_drops.load(Ordering::SeqCst), 1);
    assert_eq!(latest_drops.load(Ordering::SeqCst), 1);
    assert_eq!(rejected_drops.load(Ordering::SeqCst), 0);
    drop(rejected);
    assert_eq!(rejected_drops.load(Ordering::SeqCst), 1);
}

// Catches cancellation completing the join early or erasing/replacing its provider failure.
#[test]
fn cancelled_delivery_leaves_join_pending_until_original_owned_failure_completes() {
    let cause_drops = Arc::new(AtomicUsize::new(0));
    let cause = DropProbe::new(91, &cause_drops);
    let cause_identity = cause.identity();
    let calls = Arc::new(AtomicUsize::new(0));
    let callback_calls = Arc::clone(&calls);
    let (finished, completion) = async_channel::bounded(1);
    let (mut updates, mut join) = SharedUpdates::<u32, DropProbe>::activate(move |_producer| {
        (
            SharedUpdate::Present(7),
            move || {
                callback_calls.fetch_add(1, Ordering::SeqCst);
            },
            async move {
                completion.recv().await.unwrap();
                Err(cause)
            },
        )
    });
    let mut context = Context::from_waker(Waker::noop());
    assert!(Pin::new(&mut join).poll(&mut context).is_pending());
    let subscription = updates.subscription();
    subscription.cancel();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(cause_drops.load(Ordering::SeqCst), 0);
    assert!(Pin::new(&mut join).poll(&mut context).is_pending());
    let cancelled = block_on(updates.next()).unwrap_err();
    assert!(cancelled.is_cancelled());
    assert!(cancelled.into_failure().is_none());
    assert!(block_on(updates.next()).unwrap().is_none());
    assert!(Pin::new(&mut join).poll(&mut context).is_pending());
    finished.try_send(()).unwrap();
    let Poll::Ready(Err(cause)) = Pin::new(&mut join).poll(&mut context) else {
        panic!("released provider failure did not complete the join");
    };
    assert_eq!(cause.identity(), cause_identity);
    assert_eq!(*cause.value, 91);
    assert_eq!(cause_drops.load(Ordering::SeqCst), 0);
    drop(join);
    drop(updates);
    drop(subscription);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(cause_drops.load(Ordering::SeqCst), 0);
    drop(cause);
    assert_eq!(cause_drops.load(Ordering::SeqCst), 1);
}

// Catches wiring join destruction to delivery cancellation; this does not prove physical quiescence.
#[test]
fn dropping_pending_join_does_not_cancel_live_delivery() {
    let calls = Arc::new(AtomicUsize::new(0));
    let callback_calls = Arc::clone(&calls);
    let (_finished, completion) = async_channel::bounded::<()>(1);
    let mut endpoint = None;
    let (mut updates, mut join) =
        SharedUpdates::<u32, std::convert::Infallible>::activate(|producer| {
            endpoint = Some(producer);
            (
                SharedUpdate::Present(7),
                move || {
                    callback_calls.fetch_add(1, Ordering::SeqCst);
                },
                async move {
                    completion.recv().await.unwrap();
                    Ok(())
                },
            )
        });
    let producer = endpoint.unwrap();
    let subscription = updates.subscription();
    assert!(
        Pin::new(&mut join)
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_pending()
    );
    drop(join);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert!(!subscription.is_cancelled());
    assert_eq!(
        block_on(updates.next()).unwrap(),
        Some(SharedUpdate::Present(7))
    );
    producer.publish(SharedUpdate::Present(8)).unwrap();
    assert_eq!(
        block_on(updates.next()).unwrap(),
        Some(SharedUpdate::Present(8))
    );
    subscription.cancel();
    assert!(block_on(updates.next()).unwrap_err().is_cancelled());
    assert!(block_on(updates.next()).unwrap().is_none());
    drop(updates);
    drop(subscription);
    drop(producer);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}
