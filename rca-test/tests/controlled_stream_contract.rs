//! Checks bounded stream ownership with distinct domain failure, cancellation and EOF outcomes.

use std::cell::Cell;
use std::num::NonZeroUsize;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll, Waker};

use futures_core::Stream;
use rca_test::{ControlCancelled, TryControlledSendError, controlled_stream};

/// A move-only domain value whose `Cell` also excludes `Sync`.
struct Value(Cell<u64>, Arc<AtomicUsize>);

/// A move-only domain error with its own payload and destruction observation.
struct DomainError(Cell<u64>, Arc<AtomicUsize>);

impl Drop for Value {
    fn drop(&mut self) {
        self.1.fetch_add(1, Ordering::SeqCst);
    }
}

impl Drop for DomainError {
    fn drop(&mut self) {
        self.1.fetch_add(1, Ordering::SeqCst);
    }
}

/// Keeps domain failure inside the payload, distinct from stream cancellation.
type Item = Result<Box<Value>, Box<DomainError>>;

/// Converts the scenarios' positive literal queue bounds into the public capacity type.
fn capacity(size: usize) -> NonZeroUsize {
    NonZeroUsize::new(size).unwrap()
}

/// Polls once without scheduling; these direct checks do not assert wake delivery.
fn next<S: Stream + Unpin>(stream: &mut S) -> Poll<Option<S::Item>> {
    Pin::new(stream).poll_next(&mut Context::from_waker(Waker::noop()))
}

// Catches lost, cloned, reordered or flattened domain errors during normal close.
#[test]
fn typed_domain_error_preserves_ownership_order_and_graceful_once_eof() {
    let drops = Arc::new(AtomicUsize::new(0));
    let value = Box::new(Value(Cell::new(11), Arc::clone(&drops)));
    let error = Box::new(DomainError(Cell::new(23), Arc::clone(&drops)));
    let value_address = std::ptr::from_ref(value.as_ref());
    let error_address = std::ptr::from_ref(error.as_ref());
    let (control, mut stream) = controlled_stream::<Item>(capacity(3));
    control.try_send(Ok(value)).unwrap();
    control.try_send(Err(error)).unwrap();
    control
        .try_send(Ok(Box::new(Value(Cell::new(31), Arc::clone(&drops)))))
        .unwrap();
    control.close();
    assert_eq!(drops.load(Ordering::SeqCst), 0);

    let Poll::Ready(Some(Ok(Ok(value)))) = next(&mut stream) else {
        panic!("first item must be domain success");
    };
    assert_eq!(std::ptr::from_ref(value.as_ref()), value_address);
    assert_eq!(value.0.get(), 11);
    drop(value);
    let Poll::Ready(Some(Ok(Err(error)))) = next(&mut stream) else {
        panic!("second item must be the owned domain error, not cancellation");
    };
    assert_eq!(std::ptr::from_ref(error.as_ref()), error_address);
    assert_eq!(error.0.get(), 23);
    drop(error);
    let Poll::Ready(Some(Ok(Ok(value)))) = next(&mut stream) else {
        panic!("domain failure must not terminate later items");
    };
    assert_eq!(value.0.get(), 31);
    drop(value);
    assert_eq!(drops.load(Ordering::SeqCst), 3);
    for _ in 0..3 {
        assert!(matches!(next(&mut stream), Poll::Ready(None)));
    }
}

// Catches losing a typed Err on Full, or admitting a send before capacity frees.
#[test]
fn bounded_admission_returns_exact_domain_error_and_backpressures_owned_send() {
    /// Checks transferability at compile time without adding bounds to the domain fixture.
    fn assert_send<T: Send>(_: &T) {}

    let drops = Arc::new(AtomicUsize::new(0));
    let (control, mut stream) = controlled_stream::<Item>(capacity(1));
    assert_send(&control);
    assert_send(&stream);
    control
        .try_send(Ok(Box::new(Value(Cell::new(11), Arc::clone(&drops)))))
        .unwrap();
    let error = Box::new(DomainError(Cell::new(23), Arc::clone(&drops)));
    let address = std::ptr::from_ref(error.as_ref());
    let rejected = control.try_send(Err(error)).unwrap_err();
    assert!(matches!(&rejected, TryControlledSendError::Full(_)));
    let Err(returned) = rejected.into_inner() else {
        panic!("Full must return the domain error unchanged");
    };
    assert_eq!(std::ptr::from_ref(returned.as_ref()), address);
    assert_eq!(returned.0.get(), 23);
    let mut send = Box::pin(control.send(Err(returned)));
    assert_send(&send);
    assert!(
        send.as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_pending()
    );
    let Poll::Ready(Some(Ok(Ok(first)))) = next(&mut stream) else {
        panic!("first item must free capacity");
    };
    assert_eq!(first.0.get(), 11);
    drop(first);
    assert!(matches!(
        send.as_mut().poll(&mut Context::from_waker(Waker::noop())),
        Poll::Ready(Ok(()))
    ));
    drop(send);
    control.close();
    let Poll::Ready(Some(Ok(Err(error)))) = next(&mut stream) else {
        panic!("backpressured error must be admitted exactly once");
    };
    assert_eq!(std::ptr::from_ref(error.as_ref()), address);
    drop(error);
    assert_eq!(drops.load(Ordering::SeqCst), 2);
    assert!(matches!(next(&mut stream), Poll::Ready(None)));
}

// Catches cancellation draining as normal close or emitting a domain Err.
#[test]
fn explicit_cancel_and_controller_drop_discard_both_domain_variants_once() {
    for explicit in [true, false] {
        let drops = Arc::new(AtomicUsize::new(0));
        let (control, mut stream) = controlled_stream::<Item>(capacity(2));
        control
            .try_send(Ok(Box::new(Value(Cell::new(11), Arc::clone(&drops)))))
            .unwrap();
        control
            .try_send(Err(Box::new(DomainError(
                Cell::new(23),
                Arc::clone(&drops),
            ))))
            .unwrap();
        if explicit {
            control.cancel();
        } else {
            drop(control);
        }
        assert_eq!(drops.load(Ordering::SeqCst), 2);
        assert!(matches!(
            next(&mut stream),
            Poll::Ready(Some(Err(ControlCancelled)))
        ));
        for _ in 0..3 {
            assert!(matches!(next(&mut stream), Poll::Ready(None)));
        }
        assert_eq!(drops.load(Ordering::SeqCst), 2);
    }
}

// Catches conflating receiver abandonment with operation failure or losing Err.
#[test]
fn dropped_receiver_rejects_pending_and_immediate_owned_domain_errors() {
    let drops = Arc::new(AtomicUsize::new(0));
    let (control, stream) = controlled_stream::<Item>(capacity(1));
    control
        .try_send(Ok(Box::new(Value(Cell::new(11), Arc::clone(&drops)))))
        .unwrap();
    let error = Box::new(DomainError(Cell::new(23), Arc::clone(&drops)));
    let address = std::ptr::from_ref(error.as_ref());
    let mut send = Box::pin(control.send(Err(error)));
    assert!(
        send.as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_pending()
    );
    drop(stream);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    let Poll::Ready(Err(rejected)) = send.as_mut().poll(&mut Context::from_waker(Waker::noop()))
    else {
        panic!("receiver abandonment must reject pending admission");
    };
    let Err(error) = rejected.into_inner() else {
        panic!("receiver abandonment must preserve domain error");
    };
    assert_eq!(std::ptr::from_ref(error.as_ref()), address);
    assert_eq!(error.0.get(), 23);
    drop((send, error));
    let error = Box::new(DomainError(Cell::new(31), Arc::clone(&drops)));
    let address = std::ptr::from_ref(error.as_ref());
    let rejected = control.try_send(Err(error)).unwrap_err();
    assert!(matches!(
        &rejected,
        TryControlledSendError::ReceiverDropped(_)
    ));
    let Err(error) = rejected.into_inner() else {
        panic!("immediate rejection must preserve domain error");
    };
    assert_eq!(std::ptr::from_ref(error.as_ref()), address);
    assert_eq!(error.0.get(), 31);
    drop(error);
    assert_eq!(drops.load(Ordering::SeqCst), 3);
}

// Catches dropping a pending send cancelling the stream or its admitted item.
#[test]
fn dropped_send_destroys_only_unadmitted_domain_error() {
    let drops = Arc::new(AtomicUsize::new(0));
    let (control, mut stream) = controlled_stream::<Item>(capacity(1));
    control
        .try_send(Ok(Box::new(Value(Cell::new(11), Arc::clone(&drops)))))
        .unwrap();
    let mut send = Box::pin(control.send(Err(Box::new(DomainError(
        Cell::new(23),
        Arc::clone(&drops),
    )))));
    assert!(
        send.as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_pending()
    );
    drop(send);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    control.close();
    let Poll::Ready(Some(Ok(Ok(value)))) = next(&mut stream) else {
        panic!("pending send drop must leave admitted item intact");
    };
    assert_eq!(value.0.get(), 11);
    drop(value);
    assert!(matches!(next(&mut stream), Poll::Ready(None)));
    assert_eq!(drops.load(Ordering::SeqCst), 2);
}

// Catches imposing blanket Send on a locally polled owned sequence.
#[test]
fn local_non_send_domain_items_remain_local_without_sequence_promotion() {
    let (control, mut stream) =
        controlled_stream::<Result<Rc<Cell<u64>>, Rc<Cell<u64>>>>(capacity(2));
    let value = Rc::new(Cell::new(11));
    let error = Rc::new(Cell::new(23));
    let value_address = Rc::as_ptr(&value);
    let error_address = Rc::as_ptr(&error);
    control.try_send(Ok(value)).unwrap();
    control.try_send(Err(error)).unwrap();
    control.close();
    let Poll::Ready(Some(Ok(Ok(value)))) = next(&mut stream) else {
        panic!("local success");
    };
    assert_eq!(Rc::as_ptr(&value), value_address);
    assert_eq!(value.get(), 11);
    let Poll::Ready(Some(Ok(Err(error)))) = next(&mut stream) else {
        panic!("local error");
    };
    assert_eq!(Rc::as_ptr(&error), error_address);
    assert_eq!(error.get(), 23);
    assert!(matches!(next(&mut stream), Poll::Ready(None)));
}
