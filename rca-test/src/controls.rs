//! Provides owned, executor-independent controls for asynchronous test boundaries.
//!
//! Endpoints do not clone. Payloads need neither `Clone` nor `Sync`; transferring
//! endpoints or their futures between threads requires only the relevant `Send`
//! ownership. Control cancellation is distinct from domain errors: use a domain
//! `Result` as the payload when testing a failed client call.
//!
//! Stream capacity bounds admitted items, not outstanding send operations. Each
//! outstanding send owns one additional value until admitted, rejected or dropped.
//! Admission proves neither consumption nor an RCA reducer commit. Callers own
//! and poll all futures: these controls never spawn work or drive a runtime.
//!
//! Endpoint operations invoke registered wakers synchronously. Their panics
//! propagate without undoing an item already admitted. Stream teardown still
//! discards the remaining queued values if one wake or payload destructor panics.
//! As usual in Rust, a second panic during unwinding aborts the process.

use std::fmt;
use std::num::NonZeroUsize;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll};

use async_channel::{Receiver, Sender, TrySendError};
use futures_core::Stream;

/// Indicates cancellation or abandonment of a control endpoint.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ControlCancelled;

impl fmt::Display for ControlCancelled {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("control cancelled")
    }
}

impl std::error::Error for ControlCancelled {}

/// Returns an unadmitted value after its receiving endpoint was dropped.
pub struct ReceiverDropped<T>(T);

impl<T> ReceiverDropped<T> {
    /// Recovers the exact owned value that could not be admitted.
    pub fn into_inner(self) -> T {
        self.0
    }
}

impl<T> fmt::Debug for ReceiverDropped<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ReceiverDropped(..)")
    }
}

impl<T> fmt::Display for ReceiverDropped<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("controlled receiver dropped")
    }
}

impl<T> std::error::Error for ReceiverDropped<T> {}

/// Describes why immediate stream admission failed, retaining the owned value.
pub enum TryControlledSendError<T> {
    /// The admitted queue has reached its explicit capacity.
    Full(T),
    /// The receiving stream was dropped.
    ReceiverDropped(T),
}

impl<T> TryControlledSendError<T> {
    /// Recovers the exact owned value that could not be admitted.
    pub fn into_inner(self) -> T {
        match self {
            Self::Full(value) | Self::ReceiverDropped(value) => value,
        }
    }
}

impl<T> fmt::Debug for TryControlledSendError<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Full(_) => "Full(..)",
            Self::ReceiverDropped(_) => "ReceiverDropped(..)",
        })
    }
}

impl<T> fmt::Display for TryControlledSendError<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Full(_) => "controlled stream full",
            Self::ReceiverDropped(_) => "controlled receiver dropped",
        })
    }
}

impl<T> std::error::Error for TryControlledSendError<T> {}

/// Owns the single completion or cancellation of a controlled future.
pub struct Completion<T> {
    /// The sole sender; consuming this endpoint prevents a second completion.
    sender: Sender<T>,
}

impl<T> fmt::Debug for Completion<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Completion").finish_non_exhaustive()
    }
}

impl<T> Completion<T> {
    /// Completes the future with an owned value.
    ///
    /// # Errors
    ///
    /// Returns the supplied value if the receiving future was dropped.
    pub fn complete(self, value: T) -> Result<(), ReceiverDropped<T>> {
        self.sender
            .try_send(value)
            .map_err(|error| ReceiverDropped(error.into_inner()))
    }

    /// Cancels the future and wakes its pending poll.
    pub fn cancel(self) {
        self.sender.close();
    }
}

/// Receives one owned completion, or explicit control cancellation.
pub struct ControlledFuture<T> {
    /// Keeps the channel's `!Unpin` receiver stable while this endpoint moves.
    receiver: Pin<Box<Receiver<T>>>,
}

impl<T> fmt::Debug for ControlledFuture<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ControlledFuture").finish_non_exhaustive()
    }
}

impl<T> Future for ControlledFuture<T> {
    type Output = Result<T, ControlCancelled>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.get_mut()
            .receiver
            .as_mut()
            .poll_next(cx)
            .map(|value| value.ok_or(ControlCancelled))
    }
}

impl<T> Drop for ControlledFuture<T> {
    fn drop(&mut self) {
        self.receiver.close();
    }
}

/// Creates an owned, single-completion future and its control endpoint.
///
/// Dropping an uncompleted controller cancels and wakes the future.
///
/// # Examples
///
/// ```
/// let (completion, response) = rca_test::controlled_future();
/// completion.complete(17).unwrap();
/// assert_eq!(futures_executor::block_on(response), Ok(17));
/// ```
#[must_use]
pub fn controlled_future<T>() -> (Completion<T>, ControlledFuture<T>) {
    // One controller admits at most one completion.
    let (sender, receiver) = async_channel::bounded(1);
    (
        Completion { sender },
        ControlledFuture {
            receiver: Box::pin(receiver),
        },
    )
}

/// Owns admission, graceful closure and cancellation of a bounded stream.
pub struct StreamControl<T> {
    /// Admits values until graceful closure or endpoint abandonment.
    sender: Sender<T>,

    /// Retains access to queued values so cancellation can discard them.
    receiver: Receiver<T>,

    /// Distinguishes abortive cancellation from graceful channel closure.
    cancelled: Arc<AtomicBool>,

    /// Suppresses abortive teardown after an explicit graceful close.
    closed: bool,
}

impl<T> fmt::Debug for StreamControl<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StreamControl").finish_non_exhaustive()
    }
}

impl<T> StreamControl<T> {
    /// Waits for bounded admission while retaining ownership of the supplied value.
    ///
    /// Dropping this future before admission drops its value, without cancelling
    /// the stream. Each outstanding send owns one value beyond queue capacity.
    /// Successful admission does not prove consumption or reducer commitment.
    ///
    /// # Errors
    ///
    /// Returns the exact supplied value if the receiving stream was dropped.
    pub async fn send(&self, value: T) -> Result<(), ReceiverDropped<T>> {
        self.sender
            .send(value)
            .await
            .map_err(|error| ReceiverDropped(error.0))
    }

    /// Attempts immediate bounded admission of an owned value.
    ///
    /// # Errors
    ///
    /// Returns `Full(value)` at capacity or `ReceiverDropped(value)` after receiver drop.
    pub fn try_send(&self, value: T) -> Result<(), TryControlledSendError<T>> {
        self.sender.try_send(value).map_err(|error| match error {
            TrySendError::Full(value) => TryControlledSendError::Full(value),
            TrySendError::Closed(value) => TryControlledSendError::ReceiverDropped(value),
        })
    }

    /// Closes admission, draining admitted items before end-of-stream.
    pub fn close(mut self) {
        self.closed = true;
        self.sender.close();
    }

    /// Cancels the stream and discards queued items.
    ///
    /// The stream reports one cancellation error followed by end-of-stream.
    /// Cancellation is observable while queued payloads are being dropped. A
    /// concurrent poll that already selected an item may still return that item.
    pub fn cancel(self) {
        // Drop owns the cancellation transition, including waking and draining.
    }
}

impl<T> Drop for StreamControl<T> {
    fn drop(&mut self) {
        if !self.closed {
            self.cancelled.store(true, Ordering::Release);
            close_and_discard(&self.receiver);
        }
    }
}

/// Closes admission and discards queued values even if a wake or destructor panics.
fn close_and_discard<T>(receiver: &Receiver<T>) {
    /// Finishes the drain if normal teardown is interrupted by one user panic.
    struct DiscardRemaining<'a, T> {
        /// The opposite endpoint may outlive this teardown and retain the queue.
        receiver: &'a Receiver<T>,
    }

    impl<T> Drop for DiscardRemaining<'_, T> {
        fn drop(&mut self) {
            while let Ok(value) = self.receiver.try_recv() {
                drop(value);
            }
        }
    }

    let remaining = DiscardRemaining { receiver };
    receiver.close();

    // Pop before dropping: a payload destructor may reenter either endpoint.
    // The guard is already armed if closing or dropping a value panics.
    while let Ok(value) = remaining.receiver.try_recv() {
        drop(value);
    }
}

/// Receives bounded admitted items or a single control cancellation error.
pub struct ControlledStream<T> {
    /// Keeps the channel's `!Unpin` receiver stable while this endpoint moves.
    receiver: Pin<Box<Receiver<T>>>,

    /// Shared cancellation evidence, published before closing and discarding.
    cancelled: Arc<AtomicBool>,

    /// Ensures cancellation is emitted once and all later polls return EOF.
    terminated: bool,

    /// Injects a test-only transition between checking cancellation and polling.
    #[cfg(test)]
    before_receiver_poll: Option<Box<dyn FnOnce() + Send + Sync>>,
}

impl<T> fmt::Debug for ControlledStream<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ControlledStream").finish_non_exhaustive()
    }
}

impl<T> Stream for ControlledStream<T> {
    type Item = Result<T, ControlCancelled>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        if this.terminated {
            return Poll::Ready(None);
        }

        if this.cancelled.load(Ordering::Acquire) {
            this.terminated = true;
            return Poll::Ready(Some(Err(ControlCancelled)));
        }

        #[cfg(test)]
        if let Some(before_receiver_poll) = this.before_receiver_poll.take() {
            before_receiver_poll();
        }

        match this.receiver.as_mut().poll_next(cx) {
            Poll::Ready(None) => {
                // Closing the channel may have woken this poll through cancellation.
                if this.cancelled.load(Ordering::Acquire) {
                    this.terminated = true;
                    Poll::Ready(Some(Err(ControlCancelled)))
                } else {
                    this.terminated = true;
                    Poll::Ready(None)
                }
            }
            Poll::Ready(Some(value)) => Poll::Ready(Some(Ok(value))),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl<T> Drop for ControlledStream<T> {
    fn drop(&mut self) {
        // The controller retains a receiver solely for cancellation draining.
        // Explicit close makes receiver abandonment visible to outstanding sends.
        close_and_discard(&self.receiver);
    }
}

/// Creates a stream with explicit nonzero admitted-item capacity.
///
/// Dropping the controller is abortive cancellation, not graceful EOF. Use
/// `close` to drain admitted items. Neither endpoint implements `Clone`, and no
/// background work is spawned.
///
/// # Examples
///
/// ```
/// use std::num::NonZeroUsize;
/// let (control, _stream) = rca_test::controlled_stream(NonZeroUsize::new(1).unwrap());
/// control.try_send(17).unwrap();
/// control.close();
/// ```
#[must_use]
pub fn controlled_stream<T>(capacity: NonZeroUsize) -> (StreamControl<T>, ControlledStream<T>) {
    let (sender, receiver) = async_channel::bounded(capacity.get());
    let cancelled = Arc::new(AtomicBool::new(false));
    (
        StreamControl {
            sender,
            receiver: receiver.clone(),
            cancelled: Arc::clone(&cancelled),
            closed: false,
        },
        ControlledStream {
            receiver: Box::pin(receiver),
            cancelled,
            terminated: false,
            #[cfg(test)]
            before_receiver_poll: None,
        },
    )
}

/// Indicates that the gate was dropped before the requested operation succeeded.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct GateDropped;

impl fmt::Display for GateDropped {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("controlled gate dropped")
    }
}

impl std::error::Error for GateDropped {}

/// Owns gate release and independently observes its first pending poll.
#[derive(Debug)]
pub struct GateControl {
    /// Owns the gate's single release or cancellation.
    completion: Completion<()>,

    /// An empty channel whose closure broadcasts arrival or abandonment.
    arrival: Receiver<()>,

    /// Records whether the gate reached a pending poll before abandonment.
    waiting: Arc<AtomicBool>,
}

impl GateControl {
    /// Releases the gate, including before its first poll.
    ///
    /// # Errors
    ///
    /// Returns `GateDropped` if the gate has already been dropped.
    pub fn release(self) -> Result<(), GateDropped> {
        self.completion
            .complete(())
            .map_err(|_unit_payload| GateDropped)
    }

    /// Cancels the gate and wakes its pending poll.
    pub fn cancel(self) {
        self.completion.cancel();
    }

    /// Observes sticky arrival without consuming or cancelling the gate.
    ///
    /// Dropping this observation future does not cancel the gate. After its first
    /// pending poll, later observations succeed even if the gate is then dropped.
    /// This records arrival, not that the gate is still blocked.
    ///
    /// # Errors
    ///
    /// Returns `GateDropped` if the gate is dropped without a pending poll.
    pub async fn wait_until_waiting(&self) -> Result<(), GateDropped> {
        // This empty channel closes on arrival or gate abandonment. No arrival
        // token is consumed, so concurrent and later observers see the same fact.
        let _ = self.arrival.recv().await;
        if self.waiting.load(Ordering::Acquire) {
            Ok(())
        } else {
            Err(GateDropped)
        }
    }
}

/// Waits for owned release and records its first pending poll.
///
/// Only this future waits; the gate does not pause an executor or other work.
#[derive(Debug)]
pub struct ControlledGate {
    /// Receives the controller's release or cancellation.
    future: ControlledFuture<()>,

    /// Closes on the first pending poll; dropping it also wakes arrival observers.
    arrival: Sender<()>,

    /// Distinguishes a first pending poll from abandonment without arrival.
    waiting: Arc<AtomicBool>,
}

impl Future for ControlledGate {
    type Output = Result<(), ControlCancelled>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        let result = Pin::new(&mut this.future).poll(cx);
        if result.is_pending() && !this.waiting.swap(true, Ordering::AcqRel) {
            this.arrival.close();
        }
        result
    }
}

/// Creates an owned gate and a sticky first-pending-poll observation endpoint.
///
/// # Examples
///
/// ```
/// let (control, gate) = rca_test::controlled_gate();
/// control.release().unwrap();
/// assert_eq!(futures_executor::block_on(gate), Ok(()));
/// ```
#[must_use]
pub fn controlled_gate() -> (GateControl, ControlledGate) {
    let (completion, future) = controlled_future();
    // Closure broadcasts one sticky arrival fact; no tokens are queued.
    let (arrival, observer) = async_channel::bounded(1);
    let waiting = Arc::new(AtomicBool::new(false));
    (
        GateControl {
            completion,
            arrival: observer,
            waiting: Arc::clone(&waiting),
        },
        ControlledGate {
            future,
            arrival,
            waiting,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::{ControlCancelled, controlled_stream};
    use futures_core::Stream as _;
    use std::num::NonZeroUsize;
    use std::pin::Pin;
    use std::task::{Context, Poll, Waker};

    #[test]
    fn cancellation_after_initial_check_is_not_mistaken_for_graceful_eof() {
        for cancel in [false, true] {
            let (control, mut stream) = controlled_stream::<()>(NonZeroUsize::new(1).unwrap());
            stream.before_receiver_poll = Some(Box::new(move || {
                if cancel {
                    control.cancel();
                } else {
                    control.close();
                }
            }));
            let mut context = Context::from_waker(Waker::noop());
            assert_eq!(
                Pin::new(&mut stream).poll_next(&mut context),
                Poll::Ready(cancel.then_some(Err(ControlCancelled))),
            );
            assert_eq!(
                Pin::new(&mut stream).poll_next(&mut context),
                Poll::Ready(None)
            );
            assert_eq!(
                Pin::new(&mut stream).poll_next(&mut context),
                Poll::Ready(None)
            );
        }
    }
}
