//! Contracts exercise real endpoints, futures and notifier callbacks, not queue internals.

use super::{Sender, bounded};
use async_channel::{SendError, TryRecvError, TrySendError};
use std::cell::Cell;
use std::future::Future;
use std::num::NonZeroUsize;
use std::pin::Pin;
use std::sync::Arc as WakerArc;
use std::task::{Context, Poll, Wake, Waker};

#[cfg(loom)]
use loom::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    thread,
};
#[cfg(not(loom))]
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    thread,
};

/// Runs the same endpoint contract natively or through a bounded Loom search.
fn model(f: impl Fn() + Send + Sync + 'static) {
    #[cfg(loom)]
    {
        // Observer only: it must count across model executions, not enter the model.
        let runs = WakerArc::new(std::sync::atomic::AtomicUsize::new(0));
        let count = WakerArc::clone(&runs);
        let mut builder = loom::model::Builder::new();
        builder.max_threads = 3;
        builder.max_branches = 1_000;
        builder.preemption_bound = Some(2);
        builder.max_permutations = Some(10_000);
        builder.check(move || {
            count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            f();
        });
        let runs = runs.load(std::sync::atomic::Ordering::Relaxed);
        // A capped search must not silently earn completed bounded-search credit.
        assert!(runs < 10_000, "model permutation limit reached");
        println!("mailbox model completed schedules={runs} preemptions=2 threads=3 branches=1000");
    }
    #[cfg(not(loom))]
    f();
}

/// Keeps a stable heap identity and counts destruction of owned, non-`Sync` input.
#[derive(Debug)]
struct Payload {
    id: Box<Cell<usize>>,
    drops: Arc<AtomicUsize>,
}

impl Payload {
    /// Creates one independently owned input with a shared destruction counter.
    fn new(id: usize, drops: &Arc<AtomicUsize>) -> Self {
        Self {
            id: Box::new(Cell::new(id)),
            drops: Arc::clone(drops),
        }
    }
}

impl Drop for Payload {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}

/// Counts actual notifier callbacks without scheduling the waiting future.
struct CountWake(Arc<AtomicUsize>);

impl Wake for CountWake {
    fn wake(self: WakerArc<Self>) {
        self.wake_by_ref();
    }
    fn wake_by_ref(self: &WakerArc<Self>) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

/// Pairs a registered callback with the counter used to check notification.
fn counted_waker() -> (Waker, Arc<AtomicUsize>) {
    let count = Arc::new(AtomicUsize::new(0));
    (
        Waker::from(WakerArc::new(CountWake(Arc::clone(&count)))),
        count,
    )
}

/// Polls once with the callback chosen by the contract being tested.
fn poll<F: Future>(future: Pin<&mut F>, waker: &Waker) -> Poll<F::Output> {
    future.poll(&mut Context::from_waker(waker))
}

// Catches over-admission, eager rejection destruction and a LIFO queue mutation.
#[test]
fn capacity_mapping_preserves_rejected_ownership_and_fifo() {
    model(|| {
        let drops = Arc::new(AtomicUsize::new(0));
        let (sender, receiver) = bounded(NonZeroUsize::new(2).unwrap());
        let first = Payload::new(11, &drops);
        let pointer = std::ptr::from_ref(first.id.as_ref());
        sender.try_send_map(first, |value| (value,)).unwrap();
        sender
            .try_send_map(Payload::new(22, &drops), |value| (value,))
            .unwrap();
        let rejected = Payload::new(33, &drops);
        let rejected_pointer = std::ptr::from_ref(rejected.id.as_ref());
        let Err(TrySendError::Full(rejected)) = sender.try_send_map(rejected, |value| (value,))
        else {
            panic!("third value must be returned full");
        };
        assert_eq!(std::ptr::from_ref(rejected.id.as_ref()), rejected_pointer);
        assert_eq!(rejected.id.get(), 33);
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        drop(rejected);
        let (first,) = receiver.try_recv().unwrap();
        assert_eq!(std::ptr::from_ref(first.id.as_ref()), pointer);
        assert_eq!(first.id.get(), 11);
        drop(first);
        let (second,) = receiver.try_recv().unwrap();
        assert_eq!(second.id.get(), 22);
        drop(second);
        assert_eq!(receiver.try_recv().unwrap_err(), TryRecvError::Empty);
        assert!(sender.close(|| {}));
        assert!(!receiver.close());
        assert!(receiver.is_closed());
        let rejected = Payload::new(44, &drops);
        let pointer = std::ptr::from_ref(rejected.id.as_ref());
        let Err(TrySendError::Closed(rejected)) = sender.try_send_map(rejected, |value| (value,))
        else {
            panic!("closed value must be returned");
        };
        assert_eq!(std::ptr::from_ref(rejected.id.as_ref()), pointer);
        drop(rejected);
        assert_eq!(receiver.try_recv().unwrap_err(), TryRecvError::Closed);
        assert_eq!(drops.load(Ordering::SeqCst), 4);
    });
}

// Moving construction before either rejection check would invoke this sentinel.
#[test]
fn rejected_mapping_never_constructs_envelope() {
    model(|| {
        for closed in [false, true] {
            let drops = Arc::new(AtomicUsize::new(0));
            let (sender, receiver) = bounded(NonZeroUsize::new(1).unwrap());
            sender.try_send((Payload::new(0, &drops),)).unwrap();
            if closed {
                assert!(receiver.close());
            }
            let rejected = Payload::new(1, &drops);
            let identity = std::ptr::from_ref(rejected.id.as_ref());
            // This non-total constructor is supplied only on rejection paths.
            let error = sender
                .try_send_map(rejected, |_| panic!("rejected envelope constructor ran"))
                .unwrap_err();
            let ((false, TrySendError::Full(rejected)) | (true, TrySendError::Closed(rejected))) =
                (closed, error)
            else {
                panic!("rejection did not preserve the expected admission state");
            };
            assert_eq!(std::ptr::from_ref(rejected.id.as_ref()), identity);
            assert_eq!(rejected.id.get(), 1);
            assert_eq!(drops.load(Ordering::SeqCst), 0);
            drop(rejected);
            assert_eq!(drops.load(Ordering::SeqCst), 1);
            let (queued,) = receiver.try_recv().unwrap();
            assert_eq!(queued.id.get(), 0);
            drop(queued);
            assert_eq!(drops.load(Ordering::SeqCst), 2);
        }
    });
}

// Catches typed payload loss when admission suspends or the full queue closes.
#[test]
fn mapped_async_admission_preserves_typed_payload_through_wait_and_close() {
    model(|| {
        let drops = Arc::new(AtomicUsize::new(0));
        let admissions = Arc::new(AtomicUsize::new(0));
        let (sender, receiver) = bounded(NonZeroUsize::new(1).unwrap());
        sender.try_send((Payload::new(0, &drops),)).unwrap();
        let value = Payload::new(1, &drops);
        let identity = std::ptr::from_ref(value.id.as_ref());
        let admitted = Arc::clone(&admissions);
        let mut waiting = Box::pin(sender.send_map(
            value,
            |value| (value,),
            move || {
                admitted.fetch_add(1, Ordering::SeqCst);
            },
        ));
        let (waker, notifications) = counted_waker();
        assert!(poll(waiting.as_mut(), &waker).is_pending());
        assert_eq!(notifications.load(Ordering::SeqCst), 0);
        assert_eq!(admissions.load(Ordering::SeqCst), 0);
        drop(receiver.try_recv().unwrap());
        assert_eq!(notifications.load(Ordering::SeqCst), 1);
        assert!(matches!(
            poll(waiting.as_mut(), &waker),
            Poll::Ready(Ok(()))
        ));
        assert_eq!(admissions.load(Ordering::SeqCst), 1);

        let rejected = Payload::new(2, &drops);
        let rejected_identity = std::ptr::from_ref(rejected.id.as_ref());
        let admitted = Arc::clone(&admissions);
        let mut closing = Box::pin(sender.send_map(
            rejected,
            |value| (value,),
            move || {
                admitted.fetch_add(1, Ordering::SeqCst);
            },
        ));
        let (close_waker, close_notifications) = counted_waker();
        assert!(poll(closing.as_mut(), &close_waker).is_pending());
        assert_eq!(close_notifications.load(Ordering::SeqCst), 0);
        assert!(receiver.close());
        assert_eq!(close_notifications.load(Ordering::SeqCst), 1);
        let Poll::Ready(Err(SendError(rejected))) = poll(closing.as_mut(), &close_waker) else {
            panic!("closed admission must return its original unwrapped payload");
        };
        assert_eq!(std::ptr::from_ref(rejected.id.as_ref()), rejected_identity);
        assert_eq!(rejected.id.get(), 2);
        assert_eq!(admissions.load(Ordering::SeqCst), 1);
        drop(rejected);
        let (accepted,) = receiver.try_recv().unwrap();
        assert_eq!(std::ptr::from_ref(accepted.id.as_ref()), identity);
        assert_eq!(accepted.id.get(), 1);
        drop(accepted);
        assert_eq!(drops.load(Ordering::SeqCst), 3);
    });
}

#[test]
fn mapped_async_unpolled_drop_does_not_admit_or_notify() {
    model(|| {
        let drops = Arc::new(AtomicUsize::new(0));
        let admissions = Arc::new(AtomicUsize::new(0));
        let (sender, receiver) = bounded(NonZeroUsize::new(1).unwrap());
        let admitted = Arc::clone(&admissions);
        let pending = sender.send_map(
            Payload::new(1, &drops),
            |value| (value,),
            move || {
                admitted.fetch_add(1, Ordering::SeqCst);
            },
        );
        assert_eq!(receiver.try_recv().unwrap_err(), TryRecvError::Empty);
        drop(pending);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert_eq!(admissions.load(Ordering::SeqCst), 0);
        assert_eq!(receiver.try_recv().unwrap_err(), TryRecvError::Empty);
    });
}

// Catches cancellation consuming a notification needed by another producer.
#[test]
fn cancelled_waiter_races_receive_without_stranding_survivor() {
    model(|| {
        let drops = Arc::new(AtomicUsize::new(0));
        let (sender, receiver) = bounded(NonZeroUsize::new(1).unwrap());
        sender.try_send(Payload::new(0, &drops)).unwrap();
        let mut cancelled = Box::pin(sender.send(Payload::new(1, &drops), || {}));
        let mut survivor = Box::pin(sender.send(Payload::new(2, &drops), || {}));
        let (first_waker, _) = counted_waker();
        let (second_waker, second_notifications) = counted_waker();
        assert!(poll(cancelled.as_mut(), &first_waker).is_pending());
        assert!(poll(survivor.as_mut(), &second_waker).is_pending());
        let receive = thread::spawn(move || {
            let value = receiver.try_recv().unwrap();
            assert_eq!(value.id.get(), 0);
            drop(value);
            receiver
        });
        drop(cancelled);
        let receiver = receive.join().unwrap();
        assert!(second_notifications.load(Ordering::SeqCst) > 0);
        assert!(matches!(
            poll(survivor.as_mut(), &second_waker),
            Poll::Ready(Ok(()))
        ));
        let value = receiver.try_recv().unwrap();
        assert_eq!(value.id.get(), 2);
        drop(value);
        drop(survivor);
        assert_eq!(drops.load(Ordering::SeqCst), 3);
    });
}

// Catches a completed future retaining and consuming the next capacity signal.
#[test]
fn retained_completed_future_does_not_steal_capacity_notification() {
    model(|| {
        let (sender, receiver) = bounded(NonZeroUsize::new(1).unwrap());
        sender.try_send(0).unwrap();
        let mut first = Box::pin(sender.send(1, || {}));
        let mut second = Box::pin(sender.send(2, || {}));
        let (first_waker, _) = counted_waker();
        let (second_waker, second_notifications) = counted_waker();
        assert!(poll(first.as_mut(), &first_waker).is_pending());
        assert!(poll(second.as_mut(), &second_waker).is_pending());
        assert_eq!(receiver.try_recv().unwrap(), 0);
        assert!(matches!(
            poll(first.as_mut(), &first_waker),
            Poll::Ready(Ok(()))
        ));
        assert_eq!(receiver.try_recv().unwrap(), 1);
        assert!(second_notifications.load(Ordering::SeqCst) > 0);
        assert!(matches!(
            poll(second.as_mut(), &second_waker),
            Poll::Ready(Ok(()))
        ));
        assert_eq!(receiver.try_recv().unwrap(), 2);
        drop(first);
    });
}

// Catches a consumed capacity signal failing to register the still-owned payload again.
#[test]
fn notified_sender_rearms_when_another_producer_takes_capacity() {
    model(|| {
        let drops = Arc::new(AtomicUsize::new(0));
        let admissions = Arc::new(AtomicUsize::new(0));
        let (sender, receiver) = bounded(NonZeroUsize::new(1).unwrap());
        sender.try_send(Payload::new(0, &drops)).unwrap();
        let waiting_payload = Payload::new(1, &drops);
        let waiting_pointer = std::ptr::from_ref(waiting_payload.id.as_ref());
        let admitted = Arc::clone(&admissions);
        let mut waiting = Box::pin(sender.send(waiting_payload, move || {
            admitted.fetch_add(1, Ordering::SeqCst);
        }));
        let (waker, notifications) = counted_waker();
        assert!(poll(waiting.as_mut(), &waker).is_pending());
        assert_eq!(notifications.load(Ordering::SeqCst), 0);
        assert_eq!(admissions.load(Ordering::SeqCst), 0);
        assert_eq!(drops.load(Ordering::SeqCst), 0);

        let first = receiver.try_recv().unwrap();
        assert_eq!(first.id.get(), 0);
        drop(first);
        assert_eq!(notifications.load(Ordering::SeqCst), 1);
        // A notification grants a retry, not a reservation. A different producer
        // can admit a value before the notified sender gets another poll.
        sender.try_send(Payload::new(2, &drops)).unwrap();
        assert!(poll(waiting.as_mut(), &waker).is_pending());
        assert_eq!(notifications.load(Ordering::SeqCst), 1);
        assert_eq!(admissions.load(Ordering::SeqCst), 0);
        assert_eq!(drops.load(Ordering::SeqCst), 1);

        let competitor = receiver.try_recv().unwrap();
        assert_eq!(competitor.id.get(), 2);
        drop(competitor);
        assert_eq!(notifications.load(Ordering::SeqCst), 2);
        assert_eq!(drops.load(Ordering::SeqCst), 2);
        assert!(matches!(
            poll(waiting.as_mut(), &waker),
            Poll::Ready(Ok(()))
        ));
        assert_eq!(admissions.load(Ordering::SeqCst), 1);
        let delivered = receiver.try_recv().unwrap();
        assert_eq!(delivered.id.get(), 1);
        assert_eq!(std::ptr::from_ref(delivered.id.as_ref()), waiting_pointer);
        drop(delivered);
        assert_eq!(receiver.try_recv().unwrap_err(), TryRecvError::Empty);
        assert_eq!(drops.load(Ordering::SeqCst), 3);
        // Retention and destruction of the completed future do not readmit or
        // destroy the payload a second time.
        drop(waiting);
        assert_eq!(admissions.load(Ordering::SeqCst), 1);
        assert_eq!(drops.load(Ordering::SeqCst), 3);
    });
}

// Catches admission callback unwinding before ownership reaches the queue.
#[test]
fn caught_admission_panic_preserves_payload_across_failed_poll_reuse() {
    use std::panic::{AssertUnwindSafe, catch_unwind};

    model(|| {
        let drops = Arc::new(AtomicUsize::new(0));
        let notifications = Arc::new(AtomicUsize::new(0));
        let (sender, receiver) = bounded(NonZeroUsize::new(1).unwrap());
        let payload = Payload::new(7, &drops);
        let pointer = std::ptr::from_ref(payload.id.as_ref());
        let notified = Arc::clone(&notifications);
        let mut sending = Box::pin(sender.send(payload, move || {
            notified.fetch_add(1, Ordering::SeqCst);
            panic!("admission callback panic");
        }));
        let panic = catch_unwind(AssertUnwindSafe(|| poll(sending.as_mut(), Waker::noop())))
            .expect_err("the callback unwinds before the poll returns Ready");
        assert_eq!(
            panic.downcast_ref::<&str>(),
            Some(&"admission callback panic")
        );
        assert_eq!(notifications.load(Ordering::SeqCst), 1);
        assert_eq!(drops.load(Ordering::SeqCst), 0);

        // Characterise this private implementation after a caught failed poll:
        // it stays Pending without repeating admission or its callback. This is
        // an ownership check, not a public recovery or scheduling guarantee.
        assert!(poll(sending.as_mut(), Waker::noop()).is_pending());
        assert_eq!(notifications.load(Ordering::SeqCst), 1);
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        drop(sending);
        assert_eq!(notifications.load(Ordering::SeqCst), 1);
        assert_eq!(drops.load(Ordering::SeqCst), 0);

        let delivered = receiver.try_recv().unwrap();
        assert_eq!(delivered.id.get(), 7);
        assert_eq!(std::ptr::from_ref(delivered.id.as_ref()), pointer);
        drop(delivered);
        assert_eq!(receiver.try_recv().unwrap_err(), TryRecvError::Empty);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    });
}

// Catches admission succeeding after close linearised under the same queue lock.
#[test]
fn concurrent_close_and_admission_have_one_ownership_outcome() {
    model(|| {
        let drops = Arc::new(AtomicUsize::new(0));
        let (sender, receiver) = bounded(NonZeroUsize::new(1).unwrap());
        let sender = Arc::new(sender);
        let send = Arc::clone(&sender);
        let value = Payload::new(7, &drops);
        let producer = thread::spawn(move || send.try_send(value));
        let closer = thread::spawn(move || {
            assert!(receiver.close());
            receiver
        });
        let result = producer.join().unwrap();
        let receiver = closer.join().unwrap();
        match result {
            Ok(()) => {
                let value = receiver.try_recv().unwrap();
                assert_eq!(value.id.get(), 7);
                drop(value);
            }
            Err(TrySendError::Closed(value)) => {
                assert_eq!(value.id.get(), 7);
                drop(value);
            }
            Err(TrySendError::Full(_)) => {
                panic!("one send into an initially empty queue cannot be full")
            }
        }
        assert_eq!(receiver.try_recv().unwrap_err(), TryRecvError::Closed);
        assert!(receiver.is_closed());
        let Err(TrySendError::Closed(value)) = sender.try_send(Payload::new(8, &drops)) else {
            panic!("post-close admission");
        };
        assert_eq!(value.id.get(), 8);
        drop(value);
        assert_eq!(drops.load(Ordering::SeqCst), 2);
    });
}

// Catches a missed close between a full check and listener registration/retry.
#[test]
fn close_races_waiter_registration_without_losing_wake_or_payload() {
    model(|| {
        let drops = Arc::new(AtomicUsize::new(0));
        let (sender, receiver) = bounded(NonZeroUsize::new(1).unwrap());
        sender.try_send(Payload::new(0, &drops)).unwrap();
        let mut waiting = Box::pin(sender.send(Payload::new(1, &drops), || {}));
        let (waker, notifications) = counted_waker();
        let closer = thread::spawn(move || {
            receiver.close();
            drop(receiver);
        });
        let first = poll(waiting.as_mut(), &waker);
        closer.join().unwrap();
        let result = match first {
            Poll::Ready(result) => result,
            Poll::Pending => {
                assert!(notifications.load(Ordering::SeqCst) > 0);
                let Poll::Ready(result) = poll(waiting.as_mut(), &waker) else {
                    panic!("closed send stayed pending");
                };
                result
            }
        };
        let Err(SendError(value)) = result else {
            panic!("full queue must close without admitting pending value");
        };
        assert_eq!(value.id.get(), 1);
        drop(value);
        drop(waiting);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        drop(sender);
        assert_eq!(drops.load(Ordering::SeqCst), 2);
    });
}

// Catches a receive notification lost before the sender's listener is registered.
#[test]
fn receive_races_waiter_registration_without_losing_capacity_wake() {
    model(|| {
        let (sender, receiver) = bounded(NonZeroUsize::new(1).unwrap());
        sender.try_send(0).unwrap();
        let mut waiting = Box::pin(sender.send(1, || {}));
        let (waker, notifications) = counted_waker();
        let receive = thread::spawn(move || {
            assert_eq!(receiver.try_recv().unwrap(), 0);
            receiver
        });
        let first = poll(waiting.as_mut(), &waker);
        let receiver = receive.join().unwrap();
        match first {
            Poll::Ready(result) => result.unwrap(),
            Poll::Pending => {
                assert!(notifications.load(Ordering::SeqCst) > 0);
                assert!(matches!(
                    poll(waiting.as_mut(), &waker),
                    Poll::Ready(Ok(()))
                ));
            }
        }
        assert_eq!(receiver.try_recv().unwrap(), 1);
        assert_eq!(receiver.try_recv(), Err(TryRecvError::Empty));
    });
}

// Catches cross-producer corruption or reordering within one producer.
#[test]
fn concurrent_producers_preserve_per_producer_fifo() {
    model(|| {
        let (sender, receiver) = bounded(NonZeroUsize::new(4).unwrap());
        let sender = Arc::new(sender);
        let one = Arc::clone(&sender);
        let first = thread::spawn(move || {
            one.try_send((0, 0)).unwrap();
            one.try_send((0, 1)).unwrap();
        });
        let two = Arc::clone(&sender);
        let second = thread::spawn(move || {
            two.try_send((1, 0)).unwrap();
            two.try_send((1, 1)).unwrap();
        });
        first.join().unwrap();
        second.join().unwrap();
        let mut next = [0, 0];
        for _ in 0..4 {
            let (producer, sequence) = receiver.try_recv().unwrap();
            assert_eq!(sequence, next[producer]);
            next[producer] += 1;
        }
        assert_eq!(next, [2, 2]);
        assert_eq!(receiver.try_recv(), Err(TryRecvError::Empty));
    });
}

// Catches endpoint destruction failing to close or prematurely dropping queued ownership.
#[test]
fn receiver_drop_closes_without_dropping_sender_owned_backlog() {
    model(|| {
        let drops = Arc::new(AtomicUsize::new(0));
        let (sender, receiver) = bounded(NonZeroUsize::new(1).unwrap());
        sender.try_send(Payload::new(1, &drops)).unwrap();
        drop(receiver);
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        let Err(TrySendError::Closed(value)) = sender.try_send(Payload::new(2, &drops)) else {
            panic!("receiver drop did not close");
        };
        drop(value);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        drop(sender);
        assert_eq!(drops.load(Ordering::SeqCst), 2);
    });
}

/// Re-enters mailbox closure from a real producer-notification callback.
struct ReenterClose {
    sender: Arc<Sender<usize>>,
    calls: Arc<AtomicUsize>,
}

impl Wake for ReenterClose {
    fn wake(self: WakerArc<Self>) {
        self.wake_by_ref();
    }
    fn wake_by_ref(self: &WakerArc<Self>) {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.sender.close(|| {});
    }
}

// Moving notification under the queue guard would deadlock this real callback.
#[test]
fn receive_wake_can_reenter_close() {
    model(|| {
        let (sender, receiver) = bounded(NonZeroUsize::new(1).unwrap());
        let sender = Arc::new(sender);
        let calls = Arc::new(AtomicUsize::new(0));
        let waker = Waker::from(WakerArc::new(ReenterClose {
            sender: Arc::clone(&sender),
            calls: Arc::clone(&calls),
        }));
        sender.try_send(0).unwrap();
        let mut waiting = Box::pin(sender.send(1, || {}));
        assert!(poll(waiting.as_mut(), &waker).is_pending());
        assert_eq!(receiver.try_recv().unwrap(), 0);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(receiver.is_closed());
        assert!(matches!(
            poll(waiting.as_mut(), &waker),
            Poll::Ready(Err(SendError(1)))
        ));
    });
}

// A receive notification may unwind, but its removed payload must drop unlocked.
#[test]
fn receive_wake_panic_drops_removed_payload_outside_lock() {
    use std::panic::{AssertUnwindSafe, catch_unwind};

    /// Counts notification before unwinding the receive that freed capacity.
    struct PanicWake(Arc<AtomicUsize>);

    impl Wake for PanicWake {
        fn wake(self: WakerArc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
            panic!("producer wake panic");
        }
    }

    /// Re-enters admission during cleanup without asserting during an unwind.
    struct ReenterPayload {
        value: Payload,
        sender: Option<std::sync::Weak<Sender<ReenterPayload>>>,
        admitted: Arc<AtomicUsize>,
    }

    impl Drop for ReenterPayload {
        fn drop(&mut self) {
            if let Some(sender) = self.sender.take() {
                let admitted = sender.upgrade().is_some_and(|sender| {
                    sender
                        .try_send(Self {
                            value: Payload::new(9, &self.value.drops),
                            sender: None,
                            admitted: Arc::clone(&self.admitted),
                        })
                        .is_ok()
                });
                self.admitted.store(usize::from(admitted), Ordering::SeqCst);
            }
        }
    }

    model(|| {
        let drops = Arc::new(AtomicUsize::new(0));
        let calls = Arc::new(AtomicUsize::new(0));
        let admitted = Arc::new(AtomicUsize::new(0));
        let (sender, receiver) = bounded(NonZeroUsize::new(1).unwrap());
        // Loom has no Weak Arc. This owner stays on one thread; the mailbox and
        // its counters remain modelled. Weak ownership avoids a queue-payload cycle.
        let sender = WakerArc::new(sender);
        assert!(matches!(
            sender.try_send(ReenterPayload {
                value: Payload::new(0, &drops),
                sender: Some(WakerArc::downgrade(&sender)),
                admitted: Arc::clone(&admitted),
            }),
            Ok(())
        ));
        let value = Payload::new(1, &drops);
        let identity = std::ptr::from_ref(value.id.as_ref());
        let mut waiting = Box::pin(sender.send(
            ReenterPayload {
                value,
                sender: None,
                admitted: Arc::clone(&admitted),
            },
            || {},
        ));
        let waker = Waker::from(WakerArc::new(PanicWake(Arc::clone(&calls))));
        assert!(poll(waiting.as_mut(), &waker).is_pending());

        let panic = catch_unwind(AssertUnwindSafe(|| receiver.try_recv()));
        let Err(panic) = panic else {
            panic!("producer wake must unwind receive");
        };
        assert_eq!(panic.downcast_ref::<&str>(), Some(&"producer wake panic"));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert_eq!(admitted.load(Ordering::SeqCst), 1);

        // Destruction re-entered admission; the pending producer still owns its input.
        assert!(poll(waiting.as_mut(), Waker::noop()).is_pending());
        let reentered = receiver.try_recv().unwrap();
        assert_eq!(reentered.value.id.get(), 9);
        drop(reentered);
        assert_eq!(drops.load(Ordering::SeqCst), 2);
        assert!(matches!(
            poll(waiting.as_mut(), Waker::noop()),
            Poll::Ready(Ok(()))
        ));
        let delivered = receiver.try_recv().unwrap();
        assert_eq!(std::ptr::from_ref(delivered.value.id.as_ref()), identity);
        assert_eq!(delivered.value.id.get(), 1);
        drop(delivered);
        assert_eq!(drops.load(Ordering::SeqCst), 3);
        assert!(matches!(receiver.try_recv(), Err(TryRecvError::Empty)));
    });
}

/// Re-enters admission when cancellation releases the registered callback.
struct ReenterOnDrop {
    sender: Arc<Sender<usize>>,
    drops: Arc<AtomicUsize>,
}

#[expect(
    clippy::manual_noop_waker,
    reason = "the owned waker's Drop callback is the reentrancy contract under test"
)]
impl Wake for ReenterOnDrop {
    fn wake(self: WakerArc<Self>) {}
}

impl Drop for ReenterOnDrop {
    fn drop(&mut self) {
        assert_eq!(self.sender.try_send(9), Err(TrySendError::Full(9)));
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}

// Catches cancellation destroying the registered waker under a queue guard.
#[test]
fn cancelled_waiter_waker_drop_can_reenter_admission() {
    model(|| {
        let (sender, receiver) = bounded(NonZeroUsize::new(1).unwrap());
        let sender = Arc::new(sender);
        let drops = Arc::new(AtomicUsize::new(0));
        sender.try_send(0).unwrap();
        let waker = Waker::from(WakerArc::new(ReenterOnDrop {
            sender: Arc::clone(&sender),
            drops: Arc::clone(&drops),
        }));
        let mut waiting = Box::pin(sender.send(1, || {}));
        assert!(poll(waiting.as_mut(), &waker).is_pending());
        drop(waker);
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        drop(waiting);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert_eq!(receiver.try_recv().unwrap(), 0);
    });
}
