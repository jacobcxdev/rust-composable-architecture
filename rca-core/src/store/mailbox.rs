//! Private bounded command admission; the actor uses its existing control wake.
//!
//! The queue lock orders admission, dequeue and closure. Capacity notifications
//! grant a retry, not a reserved slot or producer fairness. Accepted messages
//! remain FIFO. All waker callbacks run after releasing the queue lock.

use std::collections::VecDeque;
use std::future::{Future, poll_fn};
use std::num::NonZeroUsize;
use std::pin::Pin;
#[cfg(not(all(test, loom)))]
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::task::Poll;

use async_channel::{SendError, TryRecvError, TrySendError};
use event_listener::Event;
#[cfg(all(test, loom))]
use loom::sync::{Arc, Mutex, MutexGuard, atomic::AtomicBool};
#[cfg(not(all(test, loom)))]
use parking_lot::{Mutex, MutexGuard};
#[cfg(not(all(test, loom)))]
use std::sync::atomic::AtomicBool;

/// Shared queue storage retained until both transport endpoints are dropped.
struct Channel<T> {
    /// Preallocated admitted payloads; the lock also serialises the close transition.
    queue: Mutex<VecDeque<T>>,
    /// Logical bound, independent of any spare capacity reserved by `VecDeque`.
    capacity: usize,
    /// Monotonic admission state, written under the queue lock and observable without it.
    closed: AtomicBool,
    /// Wakes blocked producers after capacity is released or admission closes.
    send_ops: Event,
}

// Like the previous ConcurrentQueue, the transport never lends payload access.
// Private mapping only moves fields; notifications and payload destruction
// happen after releasing the queue guard, leaving no partial queue mutation.
impl<T> std::panic::UnwindSafe for Channel<T> {}
impl<T> std::panic::RefUnwindSafe for Channel<T> {}

/// The ingress-owned endpoint; producers share ingress rather than clone senders.
pub(super) struct Sender<T>(
    /// Keeps queued payloads alive even after the receiver closes or is dropped.
    Arc<Channel<T>>,
);
/// The actor-owned endpoint; dropping it closes admission without draining payloads.
pub(super) struct Receiver<T>(
    /// Shares queue ownership until actor teardown releases the receiver.
    Arc<Channel<T>>,
);

/// Preallocates a nonempty bounded queue with one ingress and one actor endpoint.
pub(super) fn bounded<T>(capacity: NonZeroUsize) -> (Sender<T>, Receiver<T>) {
    let channel = Arc::new(Channel {
        queue: Mutex::new(VecDeque::with_capacity(capacity.get())),
        capacity: capacity.get(),
        closed: AtomicBool::new(false),
        send_ops: Event::new(),
    });
    (Sender(Arc::clone(&channel)), Receiver(channel))
}

impl<T> Sender<T> {
    /// Admits one input, converting it only after the closed and capacity checks.
    ///
    /// `make` runs under the queue lock. Only private, total command-envelope
    /// constructors belong here. A bounded library-owned erased-call allocation
    /// is permitted; state selection, user callbacks and payload destruction are
    /// not. A function pointer excludes captures but does not enforce this rule:
    /// audit every caller. Rejection returns the original input without conversion.
    pub(super) fn try_send_map<U>(
        &self,
        input: U,
        make: fn(U) -> T,
    ) -> Result<(), TrySendError<U>> {
        let mut queue = self.0.lock();
        if self.0.closed.load(Ordering::Relaxed) {
            return Err(TrySendError::Closed(input));
        }
        if queue.len() == self.0.capacity {
            return Err(TrySendError::Full(input));
        }
        // Queue storage is preallocated. Only an admitted envelope is made;
        // Full/Closed leave the typed input untouched and do not call make.
        queue.push_back(make(input));
        Ok(())
    }

    /// Admits an existing envelope or returns it unchanged without waiting.
    pub(super) fn try_send(&self, value: T) -> Result<(), TrySendError<T>> {
        self.try_send_map(value, std::convert::identity)
    }

    /// Waits for capacity, then notifies admission outside the queue lock.
    ///
    /// Dropping the future before admission drops its input; dropping it after
    /// admission cannot retract the queued value.
    pub(super) fn send(
        &self,
        value: T,
        on_admitted: impl Fn(),
    ) -> impl Future<Output = Result<(), SendError<T>>> {
        self.send_map(value, std::convert::identity, on_admitted)
    }

    /// Keeps input typed while awaiting capacity and converts only on admission.
    ///
    /// `make` has the same locked-construction contract as [`Self::try_send_map`].
    /// `on_admitted` runs after unlocking and before retiring the capacity listener,
    /// which may wake another producer. Its panic does not undo queue admission.
    /// Completed futures retain no capacity notification; repolling them is not
    /// part of this private protocol.
    pub(super) fn send_map<U>(
        &self,
        value: U,
        make: fn(U) -> T,
        on_admitted: impl Fn(),
    ) -> impl Future<Output = Result<(), SendError<U>>> {
        let mut message = Some(value);
        let mut listener: Option<event_listener::EventListener> = None;
        poll_fn(move |context| {
            while let Some(value) = message.take() {
                match self.try_send_map(value, make) {
                    Ok(()) => {
                        // Publish the actor wake after unlocking, before listener
                        // cleanup can run another producer's panicking waker.
                        on_admitted();
                        // Retaining a completed future must not retain a listener
                        // that can consume another sender's capacity notification.
                        drop(listener.take());
                        return Poll::Ready(Ok(()));
                    }
                    Err(TrySendError::Closed(value)) => {
                        drop(listener.take());
                        return Poll::Ready(Err(SendError(value)));
                    }
                    Err(TrySendError::Full(value)) => message = Some(value),
                }
                if let Some(waiting) = listener.as_mut() {
                    if Pin::new(waiting).poll(context).is_pending() {
                        return Poll::Pending;
                    }
                    listener = None;
                } else {
                    // Register then retry, including a concurrent close.
                    listener = Some(self.0.send_ops.listen());
                }
            }
            Poll::Pending
        })
    }

    /// Closes once, running the callback and waking senders outside the queue lock.
    ///
    /// Returns whether this call changed admission state; it does not drain payloads.
    pub(super) fn close(&self, on_closed: impl FnOnce()) -> bool {
        self.0.close(on_closed)
    }
}

impl<T> Receiver<T> {
    /// Removes the oldest payload and then wakes one capacity waiter outside the lock.
    ///
    /// Queued values remain readable after closure. A panicking producer waker
    /// unwinds after removal; the removed payload is dropped, not put back.
    pub(super) fn try_recv(&self) -> Result<T, TryRecvError> {
        let result = {
            let mut queue = self.0.lock();
            match queue.pop_front() {
                Some(value) => Ok(value),
                None if self.0.closed.load(Ordering::Relaxed) => Err(TryRecvError::Closed),
                None => Err(TryRecvError::Empty),
            }
        };
        if result.is_ok() {
            // No queue guard is held across waker notification.
            self.0.send_ops.notify_additional(1);
        }
        result
    }

    /// Closes producer admission without draining already queued values.
    pub(super) fn close(&self) -> bool {
        self.0.close(|| {})
    }

    /// Observes the published close transition without acquiring the queue lock.
    pub(super) fn is_closed(&self) -> bool {
        self.0.closed.load(Ordering::Acquire)
    }
}

impl<T> Channel<T> {
    /// Uses the same queue critical sections in ordinary code and Loom models.
    fn lock(&self) -> MutexGuard<'_, VecDeque<T>> {
        #[cfg(all(test, loom))]
        {
            self.queue.lock().expect("mailbox model lock was poisoned")
        }
        #[cfg(not(all(test, loom)))]
        {
            self.queue.lock()
        }
    }

    /// Linearises closure with admission, then notifies both actor and producers.
    fn close(&self, on_closed: impl FnOnce()) -> bool {
        /// Wakes capacity waiters even if the actor's closure callback unwinds.
        struct NotifySenders<'a>(
            /// Borrowed event remains live for this closure transition.
            &'a Event,
        );

        impl Drop for NotifySenders<'_> {
            fn drop(&mut self) {
                self.0.notify(usize::MAX);
            }
        }

        let was_closed = {
            let _queue = self.lock();
            self.closed.swap(true, Ordering::Release)
        };
        if !was_closed {
            // Both notifications happen after unlocking. If the actor waker
            // panics, unwinding still releases blocked producers; if a producer
            // waker panics, the actor's closure wake is already published.
            let _notify_senders = NotifySenders(&self.send_ops);
            on_closed();
        }
        !was_closed
    }
}

impl<T> Drop for Sender<T> {
    fn drop(&mut self) {
        self.close(|| {});
    }
}

impl<T> Drop for Receiver<T> {
    fn drop(&mut self) {
        self.close();
    }
}

#[cfg(test)]
ordinary_test_items! {
mod tests;
}
