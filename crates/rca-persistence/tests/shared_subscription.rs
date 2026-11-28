//! Checks cancellation, coalesced delivery and independent producer completion through public APIs.
//!
//! Explicit channels order the concurrent schedules. Caught panics check terminal
//! cleanup obligations, not general recovery of an application after a panic.

use std::convert::Infallible;
use std::error::Error;
use std::fmt;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Wake, Waker};

use futures_executor::block_on;
use rca_core::Shared;
use rca_persistence::{SharedSubscription, SharedUpdate, SharedUpdates};

/// Checks that cancellation releases its locks before calling user code and runs only once.
#[test]
fn explicit_cancel_can_reenter_and_drop_never_repeats_it() {
    let calls = Arc::new(AtomicUsize::new(0));
    let reentry = Arc::new(Mutex::new(None::<SharedSubscription>));
    let value = Shared::new(7);
    let callback_calls = Arc::clone(&calls);
    let callback_reentry = Arc::clone(&reentry);
    let guard = SharedSubscription::new(move || {
        callback_calls.fetch_add(1, Ordering::SeqCst);
        let guard = callback_reentry.lock().unwrap().take().unwrap();
        assert!(guard.is_cancelled());
        guard.cancel();
        assert_eq!(value.read(|value| *value).unwrap(), 7);
    });
    *reentry.lock().unwrap() = Some(guard.clone());
    assert_eq!(format!("{guard:?}"), "SharedSubscription { .. }");
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert!(!guard.is_cancelled());
    guard.cancel();
    guard.cancel();
    drop(guard);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

/// Keeps cancellation alive until its final owner drops.
#[test]
fn only_final_guard_drop_cancels() {
    let calls = Arc::new(AtomicUsize::new(0));
    let callback_calls = Arc::clone(&calls);
    let guard = SharedSubscription::new(move || {
        callback_calls.fetch_add(1, Ordering::SeqCst);
    });
    let alias = guard.clone();
    drop(guard);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    drop(alias);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

/// Lets a concurrent caller observe claimed cancellation without waiting for its callback.
#[test]
fn concurrent_cancel_returns_without_joining_the_claimed_callback() {
    let calls = Arc::new(AtomicUsize::new(0));
    let callback_calls = Arc::clone(&calls);
    let (entered, callback_entered) = std::sync::mpsc::channel();
    let (release, released) = std::sync::mpsc::channel();
    let guard = SharedSubscription::new(move || {
        callback_calls.fetch_add(1, Ordering::SeqCst);
        entered.send(()).unwrap();
        released.recv().unwrap();
    });
    let first_guard = guard.clone();
    let first = std::thread::spawn(move || first_guard.cancel());
    callback_entered.recv().unwrap();

    let second_guard = guard.clone();
    let (returned, second_returned) = std::sync::mpsc::channel();
    let second = std::thread::spawn(move || {
        let claimed = second_guard.is_cancelled();
        second_guard.cancel();
        returned.send(claimed).unwrap();
    });

    // Bound a lock-regression failure, then always release the winning caller.
    // Success means the second caller returned while that callback remained blocked.
    let claimed = second_returned.recv_timeout(std::time::Duration::from_secs(10));
    release.send(()).unwrap();
    first.join().unwrap();
    second.join().unwrap();
    assert!(claimed.unwrap());
    assert!(guard.is_cancelled());
    drop(guard);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

/// Panics when the one selected buffered payload is discarded.
#[derive(Debug)]
struct PanicOnDrop;

impl Drop for PanicOnDrop {
    fn drop(&mut self) {
        panic!("buffered payload destructor panicked");
    }
}

/// Selects exactly one panicking payload to avoid a second panic during unwinding.
#[derive(Clone, Copy)]
enum BufferedPanic {
    /// Discards the independent initial value before any delivery.
    Initial,
    /// Discards the buffered update while the initial value is absent.
    Latest,
    /// Discards a producer failure before the consumer receives it.
    Failure,
}

/// Checks terminal delivery and producer cleanup after the selected payload panics.
fn assert_cancel_after_payload_panic(source: BufferedPanic) {
    let calls = Arc::new(AtomicUsize::new(0));
    let callback_calls = Arc::clone(&calls);
    let mut endpoint = None;
    let (mut updates, join) = SharedUpdates::<PanicOnDrop, PanicOnDrop>::activate(|producer| {
        let initial = match source {
            BufferedPanic::Initial => SharedUpdate::Present(PanicOnDrop),
            BufferedPanic::Latest => {
                producer
                    .publish(SharedUpdate::Present(PanicOnDrop))
                    .unwrap();
                SharedUpdate::Absent
            }
            BufferedPanic::Failure => {
                assert!(producer.fail(PanicOnDrop));
                SharedUpdate::Absent
            }
        };
        endpoint = Some(producer);
        (
            initial,
            move || {
                callback_calls.fetch_add(1, Ordering::SeqCst);
            },
            async { Ok(()) },
        )
    });
    let producer = endpoint.unwrap();
    let subscription = updates.subscription();

    let panic = catch_unwind(AssertUnwindSafe(|| subscription.cancel())).unwrap_err();
    assert_eq!(
        panic.downcast_ref::<&str>(),
        Some(&"buffered payload destructor panicked")
    );
    assert!(subscription.is_cancelled());
    assert!(producer.publish(SharedUpdate::Absent).is_err());
    assert!(!producer.finish());
    assert!(block_on(updates.next()).unwrap_err().is_cancelled());
    assert!(block_on(updates.next()).unwrap().is_none());
    subscription.cancel();
    drop(updates);
    drop(subscription);
    block_on(join).unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

/// Runs producer cancellation even when dropping the unread initial value panics.
#[test]
fn cancellation_runs_after_initial_value_drop_panics() {
    assert_cancel_after_payload_panic(BufferedPanic::Initial);
}

/// Runs producer cancellation even when dropping the unread latest update panics.
#[test]
fn cancellation_runs_after_latest_value_drop_panics() {
    assert_cancel_after_payload_panic(BufferedPanic::Latest);
}

/// Runs producer cancellation even when dropping an undelivered producer failure panics.
#[test]
fn cancellation_runs_after_buffered_failure_drop_panics() {
    assert_cancel_after_payload_panic(BufferedPanic::Failure);
}

/// Runs the claimed producer cancellation even when notifying the consumer panics.
#[test]
fn cancellation_runs_after_waiting_waker_panics() {
    /// Exposes cancellation paths that skip producer cleanup when notification panics.
    struct PanicOnWake;

    impl Wake for PanicOnWake {
        fn wake(self: Arc<Self>) {
            panic!("consumer wake panicked");
        }
    }

    let calls = Arc::new(AtomicUsize::new(0));
    let callback_calls = Arc::clone(&calls);
    let mut endpoint = None;
    let (mut updates, join) = SharedUpdates::<u32, Infallible>::activate(|producer| {
        endpoint = Some(producer);
        (
            SharedUpdate::Absent,
            move || {
                callback_calls.fetch_add(1, Ordering::SeqCst);
            },
            async { Ok(()) },
        )
    });
    let producer = endpoint.unwrap();
    let subscription = updates.subscription();
    assert_eq!(
        block_on(updates.next()).unwrap(),
        Some(SharedUpdate::Absent)
    );

    let waker = Waker::from(Arc::new(PanicOnWake));
    let mut context = Context::from_waker(&waker);
    let cancelled = {
        let mut next = std::pin::pin!(updates.next());
        assert!(next.as_mut().poll(&mut context).is_pending());
        let panic = catch_unwind(AssertUnwindSafe(|| subscription.cancel())).unwrap_err();
        assert_eq!(
            panic.downcast_ref::<&str>(),
            Some(&"consumer wake panicked")
        );
        let Poll::Ready(Err(error)) = next.as_mut().poll(&mut context) else {
            panic!("panicking wake lost cancellation delivery");
        };
        error.is_cancelled()
    };
    assert!(cancelled);

    assert!(subscription.is_cancelled());
    assert_eq!(
        producer.publish(SharedUpdate::Present(1)),
        Err(SharedUpdate::Present(1))
    );
    assert!(!producer.finish());
    subscription.cancel();
    drop(updates);
    drop(subscription);
    block_on(join).unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

/// Delivers the independent initial snapshot before the latest registration update.
#[test]
fn independent_initial_capture_precedes_buffered_registration_updates() {
    let live = Arc::new(Mutex::new(vec![7]));
    let (mut updates, join) = SharedUpdates::<Vec<u32>, Infallible>::activate({
        let live = Arc::clone(&live);
        move |producer| {
            let initial = live.lock().unwrap().clone();
            live.lock().unwrap().push(8);
            producer.publish(SharedUpdate::Present(vec![7, 8])).unwrap();
            producer
                .publish(SharedUpdate::Present(vec![7, 8, 9]))
                .unwrap();
            assert!(producer.finish());
            (SharedUpdate::Present(initial), || {}, async { Ok(()) })
        }
    });
    live.lock().unwrap().push(10);
    assert_eq!(
        block_on(updates.next()).unwrap(),
        Some(SharedUpdate::Present(vec![7]))
    );
    assert_eq!(
        block_on(updates.next()).unwrap(),
        Some(SharedUpdate::Present(vec![7, 8, 9]))
    );
    assert_eq!(block_on(updates.next()).unwrap(), None);
    block_on(join).unwrap();
}

/// Distinguishes the first terminal failure from later rejected failures.
#[derive(Debug, Eq, PartialEq)]
struct ProducerFailure(
    /// Identifies which producer failure reached the consumer.
    u32,
);

/// Counts actual notifications, independently of subsequent manual future polls.
struct WakeCount(
    /// Increments only when a registered waker is invoked.
    AtomicUsize,
);

impl Wake for WakeCount {
    fn wake(self: Arc<Self>) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

/// Lets a notified consumer cancel each kind of producer delivery before consuming it.
#[test]
fn producer_wake_can_cancel_delivery_before_the_terminal_value_is_consumed() {
    /// Exercises each producer operation that can wake a waiting consumer.
    enum Delivery {
        /// Publishes a value while delivery remains open.
        Update,
        /// Ends delivery without a producer failure.
        Finish,
        /// Ends delivery with the original typed producer failure.
        Failure,
    }

    /// Re-enters cancellation synchronously from a producer's consumer notification.
    struct CancelOnWake {
        /// Keeps cancellation ownership alive until notification can claim it.
        subscription: SharedSubscription,
        /// Counts notifications even when cancellation replaces the pending delivery.
        wakes: Arc<AtomicUsize>,
    }

    impl Wake for CancelOnWake {
        fn wake(self: Arc<Self>) {
            self.wakes.fetch_add(1, Ordering::SeqCst);
            self.subscription.cancel();
        }
    }

    for delivery in [Delivery::Update, Delivery::Finish, Delivery::Failure] {
        let calls = Arc::new(AtomicUsize::new(0));
        let callback_calls = Arc::clone(&calls);
        let mut endpoint = None;
        let (mut updates, join) = SharedUpdates::<u32, ProducerFailure>::activate(|producer| {
            endpoint = Some(producer);
            (
                SharedUpdate::Absent,
                move || {
                    callback_calls.fetch_add(1, Ordering::SeqCst);
                },
                async { Ok(()) },
            )
        });
        let producer = endpoint.unwrap();
        assert_eq!(
            block_on(updates.next()).unwrap(),
            Some(SharedUpdate::Absent)
        );

        let wake_count = Arc::new(AtomicUsize::new(0));
        let waker = Waker::from(Arc::new(CancelOnWake {
            subscription: updates.subscription(),
            wakes: Arc::clone(&wake_count),
        }));
        let mut context = Context::from_waker(&waker);
        let cancelled = {
            let mut next = std::pin::pin!(updates.next());
            assert!(next.as_mut().poll(&mut context).is_pending());
            match delivery {
                Delivery::Update => producer.publish(SharedUpdate::Present(7)).unwrap(),
                Delivery::Finish => assert!(producer.finish()),
                Delivery::Failure => assert!(producer.fail(ProducerFailure(91))),
            }
            let Poll::Ready(Err(error)) = next.as_mut().poll(&mut context) else {
                panic!("reentrant cancellation did not replace the unconsumed delivery");
            };
            error.is_cancelled()
        };
        assert!(cancelled);
        assert_eq!(wake_count.load(Ordering::SeqCst), 1);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(block_on(updates.next()).unwrap().is_none());
        assert_eq!(
            producer.publish(SharedUpdate::Present(8)),
            Err(SharedUpdate::Present(8))
        );
        block_on(join).unwrap();
    }
}

/// Keeps only the latest update without erasing or replacing the first producer failure.
///
/// This proves coalescing behaviour, not a bound on transient allocations.
#[test]
fn burst_coalesces_latest_without_erasing_typed_terminal_failure() {
    let (mut updates, join) = SharedUpdates::activate(|producer| {
        for value in 1..=1000 {
            producer.publish(SharedUpdate::Present(value)).unwrap();
        }
        assert!(producer.fail(ProducerFailure(91)));
        assert_eq!(
            producer.publish(SharedUpdate::Present(1001)),
            Err(SharedUpdate::Present(1001))
        );
        assert!(!producer.finish());
        (SharedUpdate::Absent, || {}, async { Ok(()) })
    });
    assert_eq!(
        block_on(updates.next()).unwrap(),
        Some(SharedUpdate::Absent)
    );
    assert_eq!(
        block_on(updates.next()).unwrap(),
        Some(SharedUpdate::Present(1000))
    );
    let failure = block_on(updates.next()).unwrap_err();
    assert!(!failure.is_cancelled());
    assert_eq!(failure.into_failure(), Some(ProducerFailure(91)));
    assert_eq!(block_on(updates.next()).unwrap(), None);
    block_on(join).unwrap();
}

/// Hides private cause text while retaining the original cause allocation and type.
#[test]
fn failed_update_diagnostics_redact_and_retain_the_original_typed_cause() {
    /// Makes any accidental delegation to the cause's formatter expose a secret.
    #[derive(Debug)]
    struct Cause(
        /// Keeps the private text at a stable address through error inspection and recovery.
        Box<str>,
    );

    impl fmt::Display for Cause {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str(&self.0)
        }
    }

    impl Error for Cause {}

    let cause = Cause("secret-producer-cause".into());
    let identity = std::ptr::from_ref(cause.0.as_ref());
    let (mut updates, join) = SharedUpdates::<u32, Cause>::activate(move |producer| {
        assert!(producer.fail(cause));
        (SharedUpdate::Absent, || {}, async { Ok(()) })
    });
    assert_eq!(
        block_on(updates.next()).unwrap(),
        Some(SharedUpdate::Absent)
    );
    let error = block_on(updates.next()).unwrap_err();
    assert_eq!(format!("{error:?}"), "UpdateError { cancelled: false, .. }");
    assert_eq!(error.to_string(), "subscription producer failed");
    let source = error.source().unwrap().downcast_ref::<Cause>().unwrap();
    assert!(std::ptr::eq(source.0.as_ref(), identity));
    assert_eq!(source.0.as_ref(), "secret-producer-cause");
    let recovered = error.into_failure().unwrap();
    assert!(std::ptr::eq(recovered.0.as_ref(), identity));
    assert_eq!(recovered.0.as_ref(), "secret-producer-cause");
    assert_eq!(block_on(updates.next()).unwrap(), None);
    block_on(join).unwrap();
}

/// Wakes a waiting consumer once on normal completion without completing producer work.
#[test]
fn finish_wakes_pending_delivery_once_without_completing_producer_join() {
    let (gate, completion) = async_channel::bounded(1);
    let (endpoint, received) = async_channel::bounded(1);
    let cancels = Arc::new(AtomicUsize::new(0));
    let callback_cancels = Arc::clone(&cancels);
    let (mut updates, mut join) =
        SharedUpdates::<u32, ProducerFailure>::activate(move |producer| {
            endpoint.try_send(producer).unwrap();
            (
                SharedUpdate::Present(7),
                move || {
                    callback_cancels.fetch_add(1, Ordering::SeqCst);
                },
                async move {
                    completion.recv().await.unwrap();
                    Ok(())
                },
            )
        });
    let producer = received.try_recv().unwrap();
    assert_eq!(
        block_on(updates.next()).unwrap(),
        Some(SharedUpdate::Present(7))
    );

    let wake_count = Arc::new(WakeCount(AtomicUsize::new(0)));
    let waker = Waker::from(Arc::clone(&wake_count));
    let mut context = Context::from_waker(&waker);
    let reached_eof = {
        let mut next = std::pin::pin!(updates.next());
        assert!(next.as_mut().poll(&mut context).is_pending());
        assert!(producer.finish());
        assert!(!producer.finish());
        assert!(!producer.fail(ProducerFailure(99)));
        assert_eq!(wake_count.0.load(Ordering::SeqCst), 1);
        matches!(next.as_mut().poll(&mut context), Poll::Ready(Ok(None)))
    };
    assert!(reached_eof);
    assert_eq!(block_on(updates.next()).unwrap(), None);
    assert_eq!(
        producer.publish(SharedUpdate::Present(8)),
        Err(SharedUpdate::Present(8))
    );
    assert!(matches!(
        Pin::new(&mut join).poll(&mut context),
        Poll::Pending
    ));
    drop(updates);
    assert_eq!(cancels.load(Ordering::SeqCst), 1);
    assert!(matches!(
        Pin::new(&mut join).poll(&mut context),
        Poll::Pending
    ));
    gate.try_send(()).unwrap();
    block_on(join).unwrap();
    drop(producer);
    assert_eq!(cancels.load(Ordering::SeqCst), 1);
}

/// Wakes a waiting consumer with the first failure without completing producer work.
#[test]
fn fail_wakes_pending_delivery_once_with_original_cause_and_separate_join() {
    let (gate, completion) = async_channel::bounded(1);
    let (endpoint, received) = async_channel::bounded(1);
    let cancels = Arc::new(AtomicUsize::new(0));
    let callback_cancels = Arc::clone(&cancels);
    let (mut updates, mut join) =
        SharedUpdates::<u32, ProducerFailure>::activate(move |producer| {
            endpoint.try_send(producer).unwrap();
            (
                SharedUpdate::Present(7),
                move || {
                    callback_cancels.fetch_add(1, Ordering::SeqCst);
                },
                async move {
                    completion.recv().await.unwrap();
                    Ok(())
                },
            )
        });
    let producer = received.try_recv().unwrap();
    assert_eq!(
        block_on(updates.next()).unwrap(),
        Some(SharedUpdate::Present(7))
    );

    let wake_count = Arc::new(WakeCount(AtomicUsize::new(0)));
    let waker = Waker::from(Arc::clone(&wake_count));
    let mut context = Context::from_waker(&waker);
    let delivered_failure = {
        let mut next = std::pin::pin!(updates.next());
        assert!(next.as_mut().poll(&mut context).is_pending());
        assert!(producer.fail(ProducerFailure(91)));
        assert!(!producer.fail(ProducerFailure(92)));
        assert!(!producer.finish());
        assert_eq!(wake_count.0.load(Ordering::SeqCst), 1);
        let Poll::Ready(Err(failure)) = next.as_mut().poll(&mut context) else {
            panic!("missing original producer failure");
        };
        failure.into_failure()
    };
    assert_eq!(delivered_failure, Some(ProducerFailure(91)));
    assert_eq!(block_on(updates.next()).unwrap(), None);
    assert_eq!(
        producer.publish(SharedUpdate::Present(8)),
        Err(SharedUpdate::Present(8))
    );
    assert!(matches!(
        Pin::new(&mut join).poll(&mut context),
        Poll::Pending
    ));
    drop(updates);
    assert_eq!(cancels.load(Ordering::SeqCst), 1);
    assert!(matches!(
        Pin::new(&mut join).poll(&mut context),
        Poll::Pending
    ));
    gate.try_send(()).unwrap();
    block_on(join).unwrap();
    drop(producer);
    assert_eq!(cancels.load(Ordering::SeqCst), 1);
}

/// Distinguishes a stored optional `None`, absent storage and the end of delivery.
#[test]
fn explicit_absence_and_stored_none_are_distinct_deliveries() {
    let (mut updates, join) = SharedUpdates::<Option<u32>, Infallible>::activate(|producer| {
        producer.publish(SharedUpdate::Present(None)).unwrap();
        assert!(producer.finish());
        (SharedUpdate::Absent, || {}, async { Ok(()) })
    });
    assert_eq!(
        block_on(updates.next()).unwrap(),
        Some(SharedUpdate::Absent)
    );
    assert_eq!(
        block_on(updates.next()).unwrap(),
        Some(SharedUpdate::Present(None))
    );
    assert_eq!(block_on(updates.next()).unwrap(), None);
    block_on(join).unwrap();
}

/// Rejects cancelled delivery while the independent producer completion remains pending.
#[test]
fn cancellation_rejects_delivery_but_join_waits_for_the_actual_producer() {
    let (finished, completion) = async_channel::bounded(1);
    let (endpoint, received) = async_channel::bounded(1);
    let calls = Arc::new(AtomicUsize::new(0));
    let callback_calls = Arc::clone(&calls);
    let (mut updates, mut join) = SharedUpdates::<u32, Infallible>::activate(move |producer| {
        endpoint.try_send(producer).unwrap();
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
    let producer = received.try_recv().unwrap();
    assert_eq!(
        block_on(updates.next()).unwrap(),
        Some(SharedUpdate::Present(7))
    );
    producer.publish(SharedUpdate::Present(8)).unwrap();
    let guard = updates.subscription();
    guard.cancel();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        producer.publish(SharedUpdate::Present(9)),
        Err(SharedUpdate::Present(9))
    );
    let error = block_on(updates.next()).unwrap_err();
    assert!(error.is_cancelled());
    assert_eq!(format!("{error:?}"), "UpdateError { cancelled: true, .. }");
    assert_eq!(error.to_string(), "subscription cancelled");
    assert!(error.source().is_none());
    assert!(error.into_failure().is_none());
    assert_eq!(block_on(updates.next()).unwrap(), None);
    assert_eq!(
        std::pin::Pin::new(&mut join).poll(&mut Context::from_waker(Waker::noop())),
        Poll::Pending
    );
    finished.try_send(()).unwrap();
    block_on(join).unwrap();
    drop(updates);
    drop(guard);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

/// Preserves transferable values and failures without requiring extra payload traits.
#[test]
fn transferable_delivery_preserves_opaque_move_only_values_and_causes() {
    // Cell deliberately permits Send but not Sync. Neither payload adds Clone,
    // Debug or Error, so these assertions constrain the public generic bounds.
    /// Moves an opaque value without permitting shared cross-thread references.
    struct Value(
        /// Distinguishes the initial value from the later update.
        std::cell::Cell<u32>,
    );

    /// Moves an opaque failure without implementing the standard error trait.
    struct Cause(
        /// Identifies the producer's original failure after recovery.
        std::cell::Cell<u32>,
    );

    /// Requires a value or future to support transfer to another thread.
    fn assert_send<T: Send>(_: &T) {}

    /// Requires cancellation owners to support transfer and shared cross-thread access.
    fn assert_send_sync<T: Send + Sync>() {}

    let (mut updates, join) = SharedUpdates::activate(|producer| {
        assert_eq!(format!("{producer:?}"), "SharedProducer { .. }");
        assert!(
            producer
                .publish(SharedUpdate::Present(Value(std::cell::Cell::new(8))))
                .is_ok()
        );
        assert!(producer.fail(Cause(std::cell::Cell::new(91))));
        (
            SharedUpdate::Present(Value(std::cell::Cell::new(7))),
            || {},
            async { Ok(()) },
        )
    });
    assert_send(&updates);
    assert_send(&join);
    assert_eq!(format!("{updates:?}"), "SharedUpdates { .. }");
    assert_eq!(format!("{join:?}"), "ProducerJoin { .. }");
    assert_send_sync::<SharedSubscription>();
    let Some(SharedUpdate::Present(initial)) = block_on(updates.next()).unwrap() else {
        panic!("missing initial value")
    };
    assert_eq!(initial.0.get(), 7);
    let Some(SharedUpdate::Present(latest)) = block_on(updates.next()).unwrap() else {
        panic!("missing latest value")
    };
    assert_eq!(latest.0.get(), 8);
    let Err(failure) = block_on(updates.next()) else {
        panic!("missing producer failure");
    };
    assert_eq!(
        format!("{failure:?}"),
        "UpdateError { cancelled: false, .. }"
    );
    assert_eq!(failure.to_string(), "subscription producer failed");
    assert_eq!(failure.into_failure().unwrap().0.get(), 91);
    assert!(block_on(join).is_ok());
}

/// Lets a displaced value's destructor finish delivery without re-entering a held lock.
#[test]
fn displaced_value_destructor_can_reenter_producer_delivery() {
    /// Calls back only from the displaced value's destructor, after publication.
    struct Reentrant(
        /// Takes ownership once, so destruction cannot repeat the callback.
        Option<Box<dyn FnOnce() + Send>>,
    );

    impl Drop for Reentrant {
        fn drop(&mut self) {
            if let Some(callback) = self.0.take() {
                callback();
            }
        }
    }
    let calls = Arc::new(AtomicUsize::new(0));
    let (mut updates, join) = SharedUpdates::<Reentrant, Infallible>::activate({
        let calls = Arc::clone(&calls);
        move |producer| {
            let reentrant = producer.clone();
            assert!(
                producer
                    .publish(SharedUpdate::Present(Reentrant(Some(Box::new(
                        move || {
                            calls.fetch_add(1, Ordering::SeqCst);
                            assert!(reentrant.finish());
                        }
                    )))))
                    .is_ok()
            );
            assert!(
                producer
                    .publish(SharedUpdate::Present(Reentrant(None)))
                    .is_ok()
            );
            (SharedUpdate::Absent, || {}, async { Ok(()) })
        }
    });
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(matches!(
        block_on(updates.next()).unwrap(),
        Some(SharedUpdate::Absent)
    ));
    assert!(matches!(
        block_on(updates.next()).unwrap(),
        Some(SharedUpdate::Present(_))
    ));
    assert!(block_on(updates.next()).unwrap().is_none());
    block_on(join).unwrap();
}
