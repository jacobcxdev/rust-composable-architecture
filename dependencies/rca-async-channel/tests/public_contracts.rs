//! Public ownership, wake-order and cancellation checks for both channel capacities.

#[cfg(feature = "std")]
use std::error::Error;
use std::future::Future;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Wake, Waker};

use async_channel::{bounded, unbounded, TryRecvError};
use futures_core::stream::{FusedStream, Stream};
use futures_lite::future;

#[derive(Default)]
/// Counts actual notifications before the test polls a future again.
struct WakeCount(AtomicUsize);

impl Wake for WakeCount {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

/// Returns a waker and an independent counter for its notifications.
fn counting_waker() -> (Arc<WakeCount>, Waker) {
    let count = Arc::new(WakeCount::default());
    let waker = Waker::from(count.clone());
    (count, waker)
}

/// Identifies a transported value and counts its destruction independently.
struct Tracked {
    value: usize,
    drops: Arc<AtomicUsize>,
}

impl Drop for Tracked {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}

/// Lacks `Debug` so handles and send errors cannot require or expose the payload.
struct OpaquePayload(&'static str);

#[test]
fn stream_drains_closed_channel_before_terminating() {
    for (s, r) in [bounded(2), unbounded()] {
        let mut r = Box::pin(r);
        let (_, waker) = counting_waker();
        let mut cx = Context::from_waker(&waker);

        assert!(!r.is_terminated());
        s.try_send(11).unwrap();
        s.try_send(22).unwrap();
        s.close();
        assert!(!r.is_terminated());
        assert_eq!(r.as_mut().poll_next(&mut cx), Poll::Ready(Some(11)));
        assert!(!r.is_terminated());
        assert_eq!(r.as_mut().poll_next(&mut cx), Poll::Ready(Some(22)));
        assert_eq!(r.as_mut().poll_next(&mut cx), Poll::Ready(None));
        assert!(r.is_terminated());
        assert_eq!(r.as_mut().poll_next(&mut cx), Poll::Ready(None));
    }
}

#[test]
fn pending_stream_wakes_when_channel_closes() {
    for (s, r) in [bounded::<()>(1), unbounded()] {
        let mut r = Box::pin(r);
        let (wakes, waker) = counting_waker();
        let mut cx = Context::from_waker(&waker);

        assert_eq!(r.as_mut().poll_next(&mut cx), Poll::Pending);
        assert_eq!(wakes.0.load(Ordering::SeqCst), 0);
        s.close();
        assert!(wakes.0.load(Ordering::SeqCst) > 0);
        assert_eq!(r.as_mut().poll_next(&mut cx), Poll::Ready(None));
        assert!(r.is_terminated());
    }
}

#[test]
fn last_sender_commits_closure_before_waking_receivers_and_streams() {
    /// Keeps the three public closure observations together for assertion diagnostics.
    #[derive(Debug, Eq, PartialEq)]
    struct CloseObservation {
        senders: usize,
        closed: bool,
        received: Result<(), TryRecvError>,
    }

    /// Records public channel state synchronously inside each wake callback.
    struct CloseAtWake {
        receiver: async_channel::Receiver<()>,
        observations: Mutex<Vec<CloseObservation>>,
    }

    impl Wake for CloseAtWake {
        fn wake(self: Arc<Self>) {
            self.wake_by_ref();
        }

        fn wake_by_ref(self: &Arc<Self>) {
            let observed = CloseObservation {
                senders: self.receiver.sender_count(),
                closed: self.receiver.is_closed(),
                received: self.receiver.try_recv(),
            };
            self.observations.lock().unwrap().push(observed);
        }
    }

    for (sender, receiver) in [bounded::<()>(1), unbounded()] {
        let last_sender = sender.clone();
        let probes = std::array::from_fn::<_, 2, _>(|_| {
            Arc::new(CloseAtWake {
                receiver: receiver.clone(),
                observations: Mutex::new(Vec::new()),
            })
        });
        let wakers = probes
            .each_ref()
            .map(|probe| Waker::from(Arc::clone(probe)));
        let mut receive = Box::pin(receiver.recv());
        let mut stream = Box::pin(receiver.clone());
        assert_eq!(
            receive.as_mut().poll(&mut Context::from_waker(&wakers[0])),
            Poll::Pending
        );
        assert_eq!(
            stream
                .as_mut()
                .poll_next(&mut Context::from_waker(&wakers[1])),
            Poll::Pending
        );

        drop(sender);
        assert!(!receiver.is_closed());
        assert_eq!(receiver.sender_count(), 1);
        for probe in &probes {
            let no_wake = probe.observations.lock().unwrap().is_empty();
            assert!(no_wake);
        }

        drop(last_sender);
        // A later manual poll alone would hide notification before committed closure.
        for probe in &probes {
            let observations = std::mem::take(&mut *probe.observations.lock().unwrap());
            assert_eq!(
                observations,
                [CloseObservation {
                    senders: 0,
                    closed: true,
                    received: Err(TryRecvError::Closed),
                }]
            );
        }
        assert_eq!(
            receive.as_mut().poll(&mut Context::from_waker(&wakers[0])),
            Poll::Ready(Err(async_channel::RecvError))
        );
        assert_eq!(
            stream
                .as_mut()
                .poll_next(&mut Context::from_waker(&wakers[1])),
            Poll::Ready(None)
        );
        assert!(stream.is_terminated());
    }
}

#[test]
fn strong_clones_preserve_channel_identity_and_isolation() {
    for ((s, r), (other_s, other_r)) in [
        (bounded::<usize>(1), bounded(1)),
        (unbounded(), unbounded()),
    ] {
        let cloned_s = s.clone();
        let cloned_r = r.clone();
        assert!(s.same_channel(&cloned_s));
        assert!(r.same_channel(&cloned_r));
        assert!(!s.same_channel(&other_s));
        assert!(!r.same_channel(&other_r));

        // Equal capacities do not imply shared routing or shared closure state.
        cloned_s.try_send(11).unwrap();
        assert_eq!(other_r.try_recv(), Err(TryRecvError::Empty));
        assert_eq!(cloned_r.try_recv(), Ok(11));
        assert_eq!(r.try_recv(), Err(TryRecvError::Empty));
        assert!(cloned_r.close());
        assert!(s.is_closed());
        assert!(!other_s.is_closed());
        other_s.try_send(29).unwrap();
        assert_eq!(other_r.try_recv(), Ok(29));
        assert!(other_s.close());
        assert_eq!(other_r.try_recv(), Err(TryRecvError::Closed));
    }
}

#[test]
fn weak_clones_change_strong_counts_only_when_upgraded() {
    for (s, r) in [bounded(1), unbounded()] {
        let weak_s = s.downgrade();
        let weak_r = r.downgrade();
        let cloned_s = weak_s.clone();
        let cloned_r = weak_r.clone();
        assert_eq!(s.sender_count(), 1);
        assert_eq!(r.receiver_count(), 1);

        let upgraded_s = cloned_s.upgrade().unwrap();
        let upgraded_r = cloned_r.upgrade().unwrap();
        assert_eq!(s.sender_count(), 2);
        assert_eq!(r.receiver_count(), 2);
        upgraded_s.try_send(OpaquePayload("private value")).unwrap();
        for (rendered, kind) in [
            (format!("{:?}", s), "Sender"),
            (format!("{:?}", r), "Receiver"),
            (format!("{:?}", cloned_s), "WeakSender"),
            (format!("{:?}", cloned_r), "WeakReceiver"),
        ] {
            assert!(rendered.contains(kind));
            assert!(!rendered.contains("private value"));
        }
        assert_eq!(upgraded_r.try_recv().unwrap().0, "private value");

        drop((upgraded_s, upgraded_r));
        assert_eq!(s.sender_count(), 1);
        assert_eq!(r.sender_count(), 1);
        assert_eq!(s.receiver_count(), 1);
        assert_eq!(r.receiver_count(), 1);
        drop((weak_s, weak_r, cloned_s, cloned_r));
        assert!(!s.is_closed());
        assert!(!r.is_closed());
    }
}

#[test]
fn weak_handles_cannot_reopen_either_closed_endpoint() {
    for drop_sender in [false, true] {
        for (s, r) in [bounded::<()>(1), unbounded()] {
            let weak_s = s.downgrade();
            let weak_r = r.downgrade();
            if drop_sender {
                drop(s);
                assert!(r.is_closed());
                assert_eq!(r.sender_count(), 0);
                assert_eq!(r.receiver_count(), 1);
            } else {
                drop(r);
                assert!(s.is_closed());
                assert_eq!(s.receiver_count(), 0);
                assert_eq!(s.sender_count(), 1);
            }
            assert!(weak_s.upgrade().is_none());
            assert!(weak_r.upgrade().is_none());
        }
    }

    let (s, r) = bounded::<()>(1);
    let weak_s = s.downgrade();
    let weak_r = r.downgrade();
    r.close();
    assert!(weak_s.upgrade().is_none());
    assert!(weak_r.upgrade().is_none());
    assert_eq!(s.sender_count(), 1);
    assert_eq!(r.receiver_count(), 1);
}

#[test]
#[cfg(not(target_family = "wasm"))]
fn weak_sender_upgrade_racing_last_sender_drop_never_reopens_channel() {
    use std::sync::{mpsc, Barrier};

    for bounded_channel in [false, true] {
        let rendezvous = Barrier::new(2);
        let (jobs, pending) = mpsc::channel::<async_channel::Sender<usize>>();
        std::thread::scope(|scope| {
            let dropper = scope.spawn(|| {
                for sender in pending {
                    rendezvous.wait();
                    drop(sender);
                    rendezvous.wait();
                }
            });
            for _ in 0..100_000 {
                let (sender, receiver) = if bounded_channel {
                    bounded(1)
                } else {
                    unbounded()
                };
                let weak = sender.downgrade();
                jobs.send(sender).unwrap();
                rendezvous.wait();
                let upgraded = weak.upgrade();
                rendezvous.wait();

                match upgraded {
                    Some(sender) => {
                        assert_eq!(sender.sender_count(), 1);
                        assert!(
                            !sender.is_closed(),
                            "an upgrade resurrected a closed sender"
                        );
                        sender.try_send(7).unwrap();
                        assert_eq!(receiver.try_recv(), Ok(7));
                    }
                    None => {
                        assert_eq!(receiver.sender_count(), 0);
                        assert_eq!(receiver.try_recv(), Err(TryRecvError::Closed));
                    }
                }
                assert!(weak.upgrade().is_none());
                assert_eq!(receiver.sender_count(), 0);
                assert!(receiver.is_closed());
            }
            drop(jobs);
            dropper.join().unwrap();
        });
    }
}

#[test]
#[cfg(not(target_family = "wasm"))]
fn weak_receiver_upgrade_racing_last_receiver_drop_never_reopens_channel() {
    use std::sync::{mpsc, Barrier};

    for bounded_channel in [false, true] {
        let rendezvous = Barrier::new(2);
        let (jobs, pending) = mpsc::channel::<async_channel::Receiver<usize>>();
        std::thread::scope(|scope| {
            let dropper = scope.spawn(|| {
                for receiver in pending {
                    rendezvous.wait();
                    drop(receiver);
                    rendezvous.wait();
                }
            });
            for _ in 0..100_000 {
                let (sender, receiver) = if bounded_channel {
                    bounded(1)
                } else {
                    unbounded()
                };
                let weak = receiver.downgrade();
                jobs.send(receiver).unwrap();
                rendezvous.wait();
                let upgraded = weak.upgrade();
                rendezvous.wait();

                match upgraded {
                    Some(receiver) => {
                        assert_eq!(receiver.receiver_count(), 1);
                        assert!(
                            !receiver.is_closed(),
                            "an upgrade resurrected a closed receiver"
                        );
                        sender.try_send(7).unwrap();
                        assert_eq!(receiver.try_recv(), Ok(7));
                    }
                    None => {
                        assert_eq!(sender.receiver_count(), 0);
                        assert!(sender.try_send(7).unwrap_err().is_closed());
                    }
                }
                assert!(weak.upgrade().is_none());
                assert_eq!(sender.receiver_count(), 0);
                assert!(sender.is_closed());
            }
            drop(jobs);
            dropper.join().unwrap();
        });
    }
}

#[test]
fn force_send_preserves_rejected_payload_and_unbounded_backlog() {
    let (s, r) = unbounded();
    for value in 0..4 {
        assert_eq!(s.force_send(value), Ok(None));
    }
    for value in 0..4 {
        assert_eq!(r.try_recv(), Ok(value));
    }

    for (s, r) in [bounded(1), unbounded()] {
        r.close();
        let error = match s.force_send(OpaquePayload("rejected value")) {
            Err(error) => error,
            Ok(_) => panic!("a closed channel accepted a forced message"),
        };
        assert_eq!(error.into_inner().0, "rejected value");
        assert!(matches!(r.try_recv(), Err(TryRecvError::Closed)));
    }
}

#[test]
fn send_errors_classify_and_recover_opaque_payloads() {
    let (s, r) = bounded(1);
    s.try_send(OpaquePayload("buffered")).unwrap();
    let full = s.try_send(OpaquePayload("full payload")).unwrap_err();
    assert!(full.is_full());
    assert!(!full.is_closed());
    assert!(format!("{:?}", full).contains("Full"));
    assert!(!format!("{:?}", full).contains("full payload"));
    assert!(full.to_string().contains("full"));
    assert!(!full.to_string().contains("full payload"));
    #[cfg(feature = "std")]
    assert!(full.source().is_none());
    assert_eq!(full.into_inner().0, "full payload");

    r.close();
    let closed = s.try_send(OpaquePayload("closed payload")).unwrap_err();
    assert!(!closed.is_full());
    assert!(closed.is_closed());
    assert!(format!("{:?}", closed).contains("Closed"));
    assert!(!format!("{:?}", closed).contains("closed payload"));
    assert!(closed.to_string().contains("closed"));
    assert!(!closed.to_string().contains("closed payload"));
    #[cfg(feature = "std")]
    assert!(closed.source().is_none());
    assert_eq!(closed.into_inner().0, "closed payload");

    let closed = future::block_on(s.send(OpaquePayload("async payload"))).unwrap_err();
    assert!(format!("{:?}", closed).contains("SendError"));
    assert!(!format!("{:?}", closed).contains("async payload"));
    assert!(closed.to_string().contains("closed"));
    assert!(!closed.to_string().contains("async payload"));
    #[cfg(feature = "std")]
    assert!(closed.source().is_none());
    assert_eq!(closed.into_inner().0, "async payload");
    assert_eq!(r.try_recv().unwrap().0, "buffered");
}

#[test]
fn receive_errors_distinguish_empty_from_closed() {
    let (s, r) = unbounded::<()>();
    let empty = r.try_recv().unwrap_err();
    assert!(empty.is_empty());
    assert!(!empty.is_closed());
    assert!(format!("{:?}", empty).contains("Empty"));
    assert!(empty.to_string().contains("empty"));
    #[cfg(feature = "std")]
    assert!(empty.source().is_none());

    s.close();
    let closed = r.try_recv().unwrap_err();
    assert!(!closed.is_empty());
    assert!(closed.is_closed());
    assert!(format!("{:?}", closed).contains("Closed"));
    assert!(closed.to_string().contains("closed"));
    #[cfg(feature = "std")]
    assert!(closed.source().is_none());

    let closed = future::block_on(r.recv()).unwrap_err();
    assert!(format!("{:?}", closed).contains("RecvError"));
    assert!(closed.to_string().contains("closed"));
    #[cfg(feature = "std")]
    assert!(closed.source().is_none());
}

#[test]
fn cancelling_notified_send_hands_the_only_slot_to_another_waiter() {
    let (s, r) = bounded(1);
    let drops = Arc::new(AtomicUsize::new(0));
    s.try_send(Tracked {
        value: 0,
        drops: drops.clone(),
    })
    .unwrap();
    let mut sends = [
        Some(Box::pin(s.send(Tracked {
            value: 1,
            drops: drops.clone(),
        }))),
        Some(Box::pin(s.send(Tracked {
            value: 2,
            drops: drops.clone(),
        }))),
    ];
    let counters = [counting_waker(), counting_waker()];
    for index in 0..2 {
        let mut cx = Context::from_waker(&counters[index].1);
        assert!(sends[index]
            .as_mut()
            .unwrap()
            .as_mut()
            .poll(&mut cx)
            .is_pending());
        assert_eq!(counters[index].0 .0.load(Ordering::SeqCst), 0);
    }

    let buffered = r.try_recv().unwrap();
    assert_eq!(buffered.value, 0);
    drop(buffered);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    let notified = (0..2)
        .find(|&index| counters[index].0 .0.load(Ordering::SeqCst) > 0)
        .expect("freeing one slot must wake a sender");
    let survivor = 1 - notified;
    assert_eq!(counters[survivor].0 .0.load(Ordering::SeqCst), 0);

    drop(sends[notified].take());
    assert_eq!(drops.load(Ordering::SeqCst), 2);
    assert!(counters[survivor].0 .0.load(Ordering::SeqCst) > 0);
    let mut cx = Context::from_waker(&counters[survivor].1);
    assert!(matches!(
        sends[survivor].as_mut().unwrap().as_mut().poll(&mut cx),
        Poll::Ready(Ok(()))
    ));
    assert_eq!(drops.load(Ordering::SeqCst), 2);
    let delivered = r.try_recv().unwrap();
    assert_eq!(delivered.value, survivor + 1);
    drop(delivered);
    drop(sends);
    assert_eq!(drops.load(Ordering::SeqCst), 3);
    assert!(r.is_empty());
}

#[test]
fn cancelling_notified_receive_hands_the_only_message_to_another_waiter() {
    let (s, r) = bounded(1);
    let drops = Arc::new(AtomicUsize::new(0));
    let mut receives = [Some(Box::pin(r.recv())), Some(Box::pin(r.recv()))];
    let counters = [counting_waker(), counting_waker()];
    for index in 0..2 {
        let mut cx = Context::from_waker(&counters[index].1);
        assert!(receives[index]
            .as_mut()
            .unwrap()
            .as_mut()
            .poll(&mut cx)
            .is_pending());
        assert_eq!(counters[index].0 .0.load(Ordering::SeqCst), 0);
    }

    s.try_send(Tracked {
        value: 7,
        drops: drops.clone(),
    })
    .unwrap();
    let notified = (0..2)
        .find(|&index| counters[index].0 .0.load(Ordering::SeqCst) > 0)
        .expect("sending one message must wake a receiver");
    let survivor = 1 - notified;
    assert_eq!(counters[survivor].0 .0.load(Ordering::SeqCst), 0);
    drop(receives[notified].take());
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    assert!(counters[survivor].0 .0.load(Ordering::SeqCst) > 0);

    let mut cx = Context::from_waker(&counters[survivor].1);
    let delivered = match receives[survivor].as_mut().unwrap().as_mut().poll(&mut cx) {
        Poll::Ready(Ok(value)) => value,
        _ => panic!("the surviving receiver did not receive the queued message"),
    };
    assert_eq!(delivered.value, 7);
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    drop(delivered);
    drop(receives);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    assert!(r.is_empty());
}

#[test]
fn dropping_unpolled_send_drops_its_payload_without_enqueuing() {
    for (s, r) in [bounded(1), unbounded()] {
        let drops = Arc::new(AtomicUsize::new(0));
        let send = s.send(Tracked {
            value: 7,
            drops: drops.clone(),
        });
        assert!(r.is_empty());
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        drop(send);
        assert!(r.is_empty());
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        drop((s, r));
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }
}

#[test]
fn cancelling_pending_receive_does_not_consume_a_later_message() {
    for (s, r) in [bounded(1), unbounded()] {
        let drops = Arc::new(AtomicUsize::new(0));
        let (_, waker) = counting_waker();
        let mut cx = Context::from_waker(&waker);
        let mut receive = Box::pin(r.recv());
        assert!(receive.as_mut().poll(&mut cx).is_pending());
        drop(receive);

        s.try_send(Tracked {
            value: 7,
            drops: drops.clone(),
        })
        .unwrap();
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        let delivered = r.try_recv().unwrap();
        assert_eq!(delivered.value, 7);
        drop(delivered);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }
}

#[test]
fn pending_closed_future_wakes_for_manual_close_from_either_endpoint() {
    for close_sender in [false, true] {
        for (s, r) in [bounded::<()>(1), unbounded()] {
            let (wakes, waker) = counting_waker();
            let mut cx = Context::from_waker(&waker);
            let mut closed = Box::pin(s.closed());
            assert_eq!(closed.as_mut().poll(&mut cx), Poll::Pending);
            assert_eq!(wakes.0.load(Ordering::SeqCst), 0);
            if close_sender {
                s.close();
            } else {
                r.close();
            }
            assert!(wakes.0.load(Ordering::SeqCst) > 0);
            assert_eq!(closed.as_mut().poll(&mut cx), Poll::Ready(()));
            let mut already_closed = Box::pin(s.closed());
            assert_eq!(already_closed.as_mut().poll(&mut cx), Poll::Ready(()));
        }
    }
}

#[test]
fn panicking_send_wake_does_not_strand_a_close_observer() {
    /// Panics on its first notification and records any later notifications.
    struct PanicOnce(AtomicUsize);

    impl Wake for PanicOnce {
        fn wake(self: Arc<Self>) {
            self.wake_by_ref();
        }

        fn wake_by_ref(self: &Arc<Self>) {
            if self.0.fetch_add(1, Ordering::SeqCst) == 0 {
                panic!("first send wake panicked");
            }
        }
    }

    let (s, r) = bounded(1);
    s.try_send(11).unwrap();
    let panic_count = Arc::new(PanicOnce(AtomicUsize::new(0)));
    let panic_waker = Waker::from(panic_count.clone());
    let mut send = Box::pin(s.send(22));
    assert!(send
        .as_mut()
        .poll(&mut Context::from_waker(&panic_waker))
        .is_pending());

    let (closed_wakes, closed_waker) = counting_waker();
    let mut closed = Box::pin(s.closed());
    assert_eq!(
        closed
            .as_mut()
            .poll(&mut Context::from_waker(&closed_waker)),
        Poll::Pending
    );
    assert_eq!(closed_wakes.0.load(Ordering::SeqCst), 0);

    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| s.close()))
        .expect_err("close must propagate the first sender's wake panic");
    assert_eq!(
        panic.downcast_ref::<&str>().copied(),
        Some("first send wake panicked")
    );
    assert!(s.is_closed());
    assert_eq!(panic_count.0.load(Ordering::SeqCst), 1);
    assert!(!r.close(), "the channel already committed its closed state");
    assert!(
        closed_wakes.0.load(Ordering::SeqCst) > 0,
        "close must wake its observer without an artificial future poll"
    );
    assert_eq!(
        closed
            .as_mut()
            .poll(&mut Context::from_waker(&closed_waker)),
        Poll::Ready(())
    );
    let Poll::Ready(Err(error)) = send.as_mut().poll(&mut Context::from_waker(&panic_waker)) else {
        panic!("the pending sender must recover its unsent value after closure");
    };
    assert_eq!(error.0, 22);
    assert_eq!(r.try_recv(), Ok(11));
}
