//! Checks controlled work, wake delivery and owned-value cleanup through public APIs.

use std::cell::Cell;
use std::num::NonZeroUsize;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::task::{Context, Poll, Wake, Waker};

use futures_core::Stream;
use futures_executor::block_on;
use rca_core::{Effect, ReduceContext, Reducer};
use rca_test::{
    ControlCancelled, ControlledFuture, GateDropped, TestStore, TryControlledSendError,
    controlled_future, controlled_gate, controlled_stream,
};

/// Counts notifications so tests can distinguish readiness from a missing wake.
#[derive(Default)]
struct WakeCount(AtomicUsize);

impl Wake for WakeCount {
    fn wake(self: Arc<Self>) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

/// Polls once with the caller's notification counter.
fn poll<F: Future>(future: Pin<&mut F>, wake: &Arc<WakeCount>) -> Poll<F::Output> {
    future.poll(&mut Context::from_waker(&Waker::from(Arc::clone(wake))))
}

/// Polls one stream item with the caller's notification counter.
fn next<S: Stream>(stream: Pin<&mut S>, wake: &Arc<WakeCount>) -> Poll<Option<S::Item>> {
    stream.poll_next(&mut Context::from_waker(&Waker::from(Arc::clone(wake))))
}

/// Uses one queue slot to expose backpressure with two sends.
fn capacity() -> NonZeroUsize {
    NonZeroUsize::new(1).unwrap()
}

/// Omits `Clone` and `Debug`; `Cell` also makes this owned payload non-`Sync`.
struct MoveOnly(Cell<u64>);

// Diagnostics must not expose, format, or consume a caller's rejected payload.
#[test]
fn control_error_diagnostics_preserve_exact_non_debug_payloads() {
    assert_eq!(ControlCancelled.to_string(), "control cancelled");
    assert_eq!(GateDropped.to_string(), "controlled gate dropped");

    let (completion, future) = controlled_future();
    drop(future);
    let value = Box::new(MoveOnly(Cell::new(17)));
    let address = std::ptr::from_ref(value.as_ref());
    let error = completion.complete(value).unwrap_err();
    assert_eq!(format!("{error:?}"), "ReceiverDropped(..)");
    assert_eq!(error.to_string(), "controlled receiver dropped");
    let returned = error.into_inner();
    assert_eq!(std::ptr::from_ref(returned.as_ref()), address);
    assert_eq!(returned.0.get(), 17);

    let (control, stream) = controlled_stream(capacity());
    control.try_send(Box::new(MoveOnly(Cell::new(23)))).unwrap();
    assert_eq!(format!("{control:?}"), "StreamControl { .. }");
    assert_eq!(format!("{stream:?}"), "ControlledStream { .. }");
    let value = Box::new(MoveOnly(Cell::new(31)));
    let address = std::ptr::from_ref(value.as_ref());
    let error = control.try_send(value).unwrap_err();
    assert_eq!(format!("{error:?}"), "Full(..)");
    assert_eq!(error.to_string(), "controlled stream full");
    let returned = error.into_inner();
    assert_eq!(std::ptr::from_ref(returned.as_ref()), address);
    assert_eq!(returned.0.get(), 31);

    drop(stream);
    let error = control.try_send(returned).unwrap_err();
    assert_eq!(format!("{error:?}"), "ReceiverDropped(..)");
    assert_eq!(error.to_string(), "controlled receiver dropped");
    let returned = error.into_inner();
    assert_eq!(std::ptr::from_ref(returned.as_ref()), address);
    assert_eq!(returned.0.get(), 31);
}

// Catches completion that fails to wake or requires Clone/Sync payloads.
#[test]
fn future_completion_wakes_and_moves_non_sync_value() {
    let (completion, future) = controlled_future();
    assert_eq!(format!("{completion:?}"), "Completion { .. }");
    assert_eq!(format!("{future:?}"), "ControlledFuture { .. }");
    let mut future = Box::pin(future);
    let wake = Arc::new(WakeCount::default());
    assert!(poll(future.as_mut(), &wake).is_pending());
    completion.complete(MoveOnly(Cell::new(17))).unwrap();
    assert!(wake.0.load(Ordering::SeqCst) > 0);
    let Poll::Ready(Ok(value)) = poll(future.as_mut(), &wake) else {
        panic!("completed future must resolve");
    };
    assert_eq!(value.0.get(), 17);
}

#[test]
fn future_controller_abandonment_and_explicit_cancel_wake() {
    for explicit in [false, true] {
        let (completion, future) = controlled_future::<()>();
        let mut future = Box::pin(future);
        let wake = Arc::new(WakeCount::default());
        assert!(poll(future.as_mut(), &wake).is_pending());
        if explicit {
            completion.cancel();
        } else {
            drop(completion);
        }
        assert!(wake.0.load(Ordering::SeqCst) > 0);
        assert_eq!(
            poll(future.as_mut(), &wake),
            Poll::Ready(Err(ControlCancelled))
        );
    }
}

#[test]
fn future_receiver_abandonment_returns_exact_move_only_value() {
    let (completion, future) = controlled_future();
    drop(future);
    let value = Box::new(MoveOnly(Cell::new(23)));
    let address = std::ptr::from_ref(value.as_ref());
    let returned = completion.complete(value).unwrap_err().into_inner();
    assert_eq!(std::ptr::from_ref(returned.as_ref()), address);
    assert_eq!(returned.0.get(), 23);
}

#[test]
fn stream_capacity_preserves_full_value_and_wakes_pending_send_after_consumption() {
    let (control, stream) = controlled_stream(capacity());
    let mut stream = Box::pin(stream);
    control.try_send(Cell::new(11)).unwrap();
    let full = control.try_send(Cell::new(23)).unwrap_err();
    assert!(matches!(&full, TryControlledSendError::Full(_)));
    assert_eq!(full.into_inner().get(), 23);
    let mut send = Box::pin(control.send(Cell::new(31)));
    let wake = Arc::new(WakeCount::default());
    assert!(poll(send.as_mut(), &wake).is_pending());
    let Poll::Ready(Some(Ok(value))) = next(stream.as_mut(), &wake) else {
        panic!("admitted first item");
    };
    assert_eq!(value.get(), 11);
    assert!(wake.0.load(Ordering::SeqCst) > 0);
    assert!(matches!(poll(send.as_mut(), &wake), Poll::Ready(Ok(()))));
    drop(send);
    control.close();
    let Poll::Ready(Some(Ok(value))) = next(stream.as_mut(), &wake) else {
        panic!("admitted second item");
    };
    assert_eq!(value.get(), 31);
    assert!(matches!(next(stream.as_mut(), &wake), Poll::Ready(None)));
}

/// Counts destruction independently of the queue or pending send that owns it.
struct DropCount(Arc<AtomicUsize>);

impl Drop for DropCount {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

#[test]
fn stream_teardown_discards_remaining_items_after_payload_drop_panics() {
    #[derive(Debug)]
    enum Endpoint {
        Cancel,
        Controller,
        Stream,
    }

    /// Counts destruction before optionally panicking to test remaining queue cleanup.
    struct Payload {
        drops: Arc<AtomicUsize>,
        panic: bool,
    }

    impl Drop for Payload {
        fn drop(&mut self) {
            self.drops.fetch_add(1, Ordering::SeqCst);
            assert!(!self.panic, "queued payload drop");
        }
    }

    for endpoint in [Endpoint::Cancel, Endpoint::Controller, Endpoint::Stream] {
        let drops = Arc::new(AtomicUsize::new(0));
        let (control, stream) = controlled_stream(NonZeroUsize::new(3).unwrap());
        for panic in [true, false, false] {
            control
                .try_send(Payload {
                    drops: Arc::clone(&drops),
                    panic,
                })
                .unwrap();
        }
        let mut control = Some(control);
        let mut stream = Some(stream);
        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| match endpoint {
            Endpoint::Cancel => control.take().unwrap().cancel(),
            Endpoint::Controller => drop(control.take()),
            Endpoint::Stream => drop(stream.take()),
        }))
        .expect_err("the first payload destructor must panic");
        assert_eq!(panic.downcast_ref::<&str>(), Some(&"queued payload drop"));
        assert_eq!(drops.load(Ordering::SeqCst), 3, "{endpoint:?}");
        drop((control, stream));
        assert_eq!(drops.load(Ordering::SeqCst), 3);
    }
}

#[test]
fn stream_teardown_discards_queue_after_sender_wake_panics() {
    /// Panics on its first wake so cleanup can be checked during unwinding.
    struct PanicOnce(AtomicBool);

    impl Wake for PanicOnce {
        fn wake(self: Arc<Self>) {
            assert!(!self.0.swap(false, Ordering::SeqCst), "pending sender wake");
        }
    }

    let queued_drops = Arc::new(AtomicUsize::new(0));
    let pending_drops = Arc::new(AtomicUsize::new(0));
    let (control, stream) = controlled_stream(capacity());
    control
        .try_send(DropCount(Arc::clone(&queued_drops)))
        .unwrap();
    let mut send = Box::pin(control.send(DropCount(Arc::clone(&pending_drops))));
    let waker = Waker::from(Arc::new(PanicOnce(AtomicBool::new(true))));
    assert!(
        send.as_mut()
            .poll(&mut Context::from_waker(&waker))
            .is_pending()
    );

    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(stream)))
        .expect_err("closing the stream must wake the pending sender");
    assert_eq!(panic.downcast_ref::<&str>(), Some(&"pending sender wake"));
    assert_eq!(queued_drops.load(Ordering::SeqCst), 1);
    assert_eq!(pending_drops.load(Ordering::SeqCst), 0);

    drop(send);
    assert_eq!(pending_drops.load(Ordering::SeqCst), 1);
    control.close();
}

#[test]
fn cancelling_pending_send_drops_only_its_owned_value() {
    let drops = Arc::new(AtomicUsize::new(0));
    let (control, stream) = controlled_stream(capacity());
    control
        .try_send(DropCount(Arc::clone(&drops)))
        .ok()
        .unwrap();
    let mut send = Box::pin(control.send(DropCount(Arc::clone(&drops))));
    assert!(poll(send.as_mut(), &Arc::new(WakeCount::default())).is_pending());
    drop(send);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    control.cancel();
    assert_eq!(drops.load(Ordering::SeqCst), 2);
    drop(stream);
}

#[test]
fn stream_close_drains_in_order_without_cancellation() {
    let (control, stream) = controlled_stream(NonZeroUsize::new(3).unwrap());
    for value in [11, 23, 31] {
        control.try_send(value).unwrap();
    }
    control.close();
    let mut stream = Box::pin(stream);
    let wake = Arc::new(WakeCount::default());
    for expected in [11, 23, 31] {
        assert_eq!(
            next(stream.as_mut(), &wake),
            Poll::Ready(Some(Ok(expected)))
        );
    }
    assert_eq!(next(stream.as_mut(), &wake), Poll::Ready(None));
}

#[test]
fn stream_cancel_and_abandonment_discard_queue_emit_one_error_then_eof() {
    for explicit in [false, true] {
        let drops = Arc::new(AtomicUsize::new(0));
        let (control, stream) = controlled_stream(capacity());
        control
            .try_send(DropCount(Arc::clone(&drops)))
            .ok()
            .unwrap();
        if explicit {
            control.cancel();
        } else {
            drop(control);
        }
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        let mut stream = Box::pin(stream);
        let wake = Arc::new(WakeCount::default());
        assert!(matches!(
            next(stream.as_mut(), &wake),
            Poll::Ready(Some(Err(ControlCancelled)))
        ));
        assert!(matches!(next(stream.as_mut(), &wake), Poll::Ready(None)));
        assert!(matches!(next(stream.as_mut(), &wake), Poll::Ready(None)));
    }
}

#[test]
fn stream_receiver_drop_wakes_send_and_preserves_exact_value() {
    let (control, stream) = controlled_stream(capacity());
    control.try_send(Box::new(MoveOnly(Cell::new(11)))).unwrap();
    let value = Box::new(MoveOnly(Cell::new(23)));
    let address = std::ptr::from_ref(value.as_ref());
    let mut send = Box::pin(control.send(value));
    let wake = Arc::new(WakeCount::default());
    assert!(poll(send.as_mut(), &wake).is_pending());
    drop(stream);
    assert!(wake.0.load(Ordering::SeqCst) > 0);
    let Poll::Ready(Err(error)) = poll(send.as_mut(), &wake) else {
        panic!("dropped stream must reject admission");
    };
    let returned = error.into_inner();
    assert_eq!(std::ptr::from_ref(returned.as_ref()), address);
    let error = control
        .try_send(Box::new(MoveOnly(Cell::new(31))))
        .unwrap_err();
    assert!(matches!(&error, TryControlledSendError::ReceiverDropped(_)));
    assert_eq!(error.into_inner().0.get(), 31);
}

// The payload destructor polls the same receiver: no user destructor under a lock.
#[test]
fn cancellation_allows_reentrant_payload_destructor() {
    /// Polls the receiver during destruction to detect cleanup under the queue lock.
    struct Reenter(Option<Box<dyn FnOnce() + Send>>);
    impl Drop for Reenter {
        fn drop(&mut self) {
            self.0.take().unwrap()();
        }
    }
    let (control, stream) = controlled_stream::<Reenter>(capacity());
    let stream = Arc::new(std::sync::Mutex::new(Box::pin(stream)));
    let observed = Arc::new(AtomicUsize::new(0));
    let receiver = Arc::clone(&stream);
    let count = Arc::clone(&observed);
    control
        .try_send(Reenter(Some(Box::new(move || {
            assert!(matches!(
                next(
                    receiver.lock().unwrap().as_mut(),
                    &Arc::new(WakeCount::default())
                ),
                Poll::Ready(Some(Err(ControlCancelled)))
            ));
            count.fetch_add(1, Ordering::SeqCst);
        }))))
        .ok()
        .unwrap();
    control.cancel();
    assert_eq!(observed.load(Ordering::SeqCst), 1);
    assert!(matches!(
        next(
            stream.lock().unwrap().as_mut(),
            &Arc::new(WakeCount::default())
        ),
        Poll::Ready(None)
    ));
}

#[test]
fn gate_arrival_is_sticky_observer_cancellation_is_independent_and_release_wakes() {
    let (control, gate) = controlled_gate();
    let mut gate = Box::pin(gate);
    let wake = Arc::new(WakeCount::default());
    let mut observation = Box::pin(control.wait_until_waiting());
    assert!(poll(observation.as_mut(), &wake).is_pending());
    drop(observation);
    assert!(poll(gate.as_mut(), &wake).is_pending());
    block_on(control.wait_until_waiting()).unwrap();
    block_on(control.wait_until_waiting()).unwrap();
    let before_release = wake.0.load(Ordering::SeqCst);
    control.release().unwrap();
    assert!(wake.0.load(Ordering::SeqCst) > before_release);
    assert_eq!(poll(gate.as_mut(), &wake), Poll::Ready(Ok(())));
}

#[test]
fn gate_early_release_and_counterpart_abandonment() {
    let (control, gate) = controlled_gate();
    control.release().unwrap();
    assert_eq!(block_on(gate), Ok(()));
    for explicit in [false, true] {
        let (control, gate) = controlled_gate();
        if explicit {
            control.cancel();
        } else {
            drop(control);
        }
        assert_eq!(block_on(gate), Err(ControlCancelled));
    }
    let (control, gate) = controlled_gate();
    let wake = Arc::new(WakeCount::default());
    let mut observation = Box::pin(control.wait_until_waiting());
    assert!(poll(observation.as_mut(), &wake).is_pending());
    drop(gate);
    assert!(wake.0.load(Ordering::SeqCst) > 0);
    assert_eq!(
        poll(observation.as_mut(), &wake),
        Poll::Ready(Err(GateDropped))
    );
    drop(observation);
    assert_eq!(control.release(), Err(GateDropped));
}

#[test]
fn gate_arrival_wakes_all_observers_and_remains_sticky_after_receiver_drop() {
    let (control, gate) = controlled_gate();
    let mut gate = Box::pin(gate);
    let first_wake = Arc::new(WakeCount::default());
    let second_wake = Arc::new(WakeCount::default());
    let mut first = Box::pin(control.wait_until_waiting());
    let mut second = Box::pin(control.wait_until_waiting());
    assert!(poll(first.as_mut(), &first_wake).is_pending());
    assert!(poll(second.as_mut(), &second_wake).is_pending());
    assert!(poll(gate.as_mut(), &first_wake).is_pending());
    assert!(first_wake.0.load(Ordering::SeqCst) > 0);
    assert!(second_wake.0.load(Ordering::SeqCst) > 0);
    drop(gate);
    assert_eq!(poll(first.as_mut(), &first_wake), Poll::Ready(Ok(())));
    assert_eq!(poll(second.as_mut(), &second_wake), Poll::Ready(Ok(())));
    drop((first, second));
    assert_eq!(control.release(), Err(GateDropped));
}

#[test]
fn empty_stream_and_pending_gate_wake_on_controller_abandonment() {
    let (control, stream) = controlled_stream::<()>(capacity());
    let mut stream = Box::pin(stream);
    let wake = Arc::new(WakeCount::default());
    assert!(next(stream.as_mut(), &wake).is_pending());
    drop(control);
    assert!(wake.0.load(Ordering::SeqCst) > 0);
    assert_eq!(
        next(stream.as_mut(), &wake),
        Poll::Ready(Some(Err(ControlCancelled)))
    );
    let (control, gate) = controlled_gate();
    let mut gate = Box::pin(gate);
    let wake = Arc::new(WakeCount::default());
    assert!(poll(gate.as_mut(), &wake).is_pending());
    drop(control);
    assert!(wake.0.load(Ordering::SeqCst) > 0);
    assert_eq!(
        poll(gate.as_mut(), &wake),
        Poll::Ready(Err(ControlCancelled))
    );
}

#[test]
fn controls_and_send_futures_are_send_for_non_sync_payloads() {
    fn assert_send<T: Send>(_: &T) {}
    let (completion, future) = controlled_future::<MoveOnly>();
    assert_send(&completion);
    assert_send(&future);
    let (control, stream) = controlled_stream::<MoveOnly>(capacity());
    assert_send(&control);
    assert_send(&stream);
    assert_send(&control.send(MoveOnly(Cell::new(17))));
    let (control, gate) = controlled_gate();
    assert_send(&control);
    assert_send(&gate);
    assert_send(&control.wait_until_waiting());
}

#[derive(Debug, PartialEq)]
enum Action {
    Start,
    Returned(u64),
}

/// Moves one controlled response into an effect and records its committed value.
struct Client(Option<ControlledFuture<Cell<u64>>>);

impl Reducer for Client {
    type State = Vec<u64>;
    type Action = Action;
    fn reduce(
        &mut self,
        state: &mut Vec<u64>,
        action: &Action,
        _: &mut ReduceContext<'_>,
    ) -> Effect<Action> {
        match action {
            Action::Start => {
                let response = self.0.take().unwrap();
                Effect::future(async move {
                    response
                        .await
                        .ok()
                        .map(|value| Action::Returned(value.get()))
                })
            }
            Action::Returned(value) => {
                state.push(*value);
                Effect::none()
            }
        }
    }
}

#[test]
fn client_completion_still_requires_test_store_reducer_commit() {
    block_on(async {
        let (completion, response) = controlled_future();
        let mut store = TestStore::new(Vec::new(), Client(Some(response)));
        let task = store.send(Action::Start).await.unwrap();
        let pending = task.finish().await.expect_err("client is pending");
        assert!(pending.drive_error().is_none());
        completion.complete(Cell::new(17)).unwrap();
        let admitted = task
            .finish()
            .await
            .expect_err("admission is not reducer commitment");
        assert!(admitted.drive_error().is_none());
        let _ = store
            .receive_with(
                |action| *action == Action::Returned(17),
                |state| state.push(17),
            )
            .await
            .unwrap();
        task.finish().await.unwrap();
        store.finish().await.unwrap();
    });
}

#[test]
fn effect_cancellation_drops_owned_controlled_receiver_and_finishes_work() {
    block_on(async {
        let (completion, response) = controlled_future();
        let mut store = TestStore::new(Vec::new(), Client(Some(response)));
        let task = store.send(Action::Start).await.unwrap();
        let pending = task.finish().await.expect_err("client is pending");
        assert!(pending.drive_error().is_none());
        task.cancel().await.unwrap();
        assert_eq!(
            completion
                .complete(Cell::new(23))
                .unwrap_err()
                .into_inner()
                .get(),
            23
        );
        task.finish().await.unwrap();
        store.finish().await.unwrap();
    });
}
