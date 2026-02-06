//! A small single-consumer channel with a synchronous “barrier” mode.
//!
//! This is tailored for `Store`:
//!
//! - Many senders can enqueue values.
//! - A single receiver stream drains them.
//! - The receiver maintains an internal buffer to minimise time spent holding the mutex.
//! - `Sender::sync` provides a blocking send that only returns once the receiver has advanced
//!   past the sent value.
//!
//! # Waker behaviour
//! The receiver stores at most one `Waker`, and each transition into `Poll::Pending` consumes it
//! exactly once. This avoids “extra” wakes when many values are sent quickly.

use futures::Stream;
use pin_project::pin_project;
use std::collections::VecDeque;
use std::sync::{Arc, Barrier, Mutex, MutexGuard, Weak};
use std::task::{Context, Poll, Waker};
use std::{mem::swap, pin::Pin};

enum Msg<T> {
    Value(T),
    Barrier(Arc<Barrier>),
}

/// Shared state protected by a mutex.
/// The receiver swaps the queue into a local buffer to reduce lock contention.
struct Shared<T> {
    queue: VecDeque<Msg<T>>,
    waker: Option<Waker>,
    senders: usize,
}

impl<T> Default for Shared<T> {
    fn default() -> Self {
        Shared {
            queue: Default::default(),
            waker: Default::default(),
            senders: 0,
        }
    }
}

/// Stream receiver end of the channel.
#[derive(Default)]
#[pin_project] // See: https://blog.adamchalmers.com/pin-unpin/
pub struct Receiver<T> {
    shared: Arc<Mutex<Shared<T>>>,
    buffer: VecDeque<Msg<T>>,
}

impl<T> Stream for Receiver<T> {
    type Item = T;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let inner = &mut self.project();

        loop {
            match inner.buffer.pop_front() {
                Some(Msg::Value(value)) => return Poll::Ready(Some(value)),
                Some(Msg::Barrier(barrier)) => barrier.wait(),
                _ => break,
            };
        }

        let mut shared = inner.shared.lock().unwrap_or_else(|err| err.into_inner());
        let external = &mut shared.queue;

        match external.pop_front() {
            Some(Msg::Value(value)) => {
                // move all other pending values (if any) into the (un-Mutex’d) internal buffer
                swap(external, inner.buffer);
                Poll::Ready(Some(value))
            }
            // A `Barrier` will always follow a `Value` and thus should have
            // been moved into the buffer during the swap above.
            Some(Msg::Barrier(_)) => unreachable!(),
            None if shared.senders == 0 => Poll::Ready(None), // no receivers remaining
            None => {
                match shared.waker.as_mut() {
                    None => shared.waker = Some(cx.waker().clone()),
                    Some(waker) => waker.clone_from(cx.waker()),
                };

                Poll::Pending
            }
        }
    }
}

/// Sender end of the channel.
pub struct Sender<T> {
    shared: Arc<Mutex<Shared<T>>>,
}

impl<T> Clone for Sender<T> {
    fn clone(&self) -> Self {
        let mut shared = self.shared.lock().unwrap();
        shared.senders += 1;
        drop(shared);

        Sender {
            shared: self.shared.clone(),
        }
    }
}

impl<T> Drop for Sender<T> {
    fn drop(&mut self) {
        self.wake_after(|mut shared| {
            shared.senders -= 1;
        })
    }
}

impl<T> Sender<T> {
    /// Enqueue a value for the receiver to observe.
    pub fn send(&self, value: T) {
        self.wake_after(move |mut shared| shared.queue.push_back(Msg::Value(value)))
    }

    /// Enqueue a value and block until the receiver has advanced past it.
    ///
    /// This is implemented by enqueueing the value followed by a barrier. The receiver yields the
    /// value first, then (on its next poll) observes the barrier and waits on it—unblocking the
    /// sender.
    ///
    /// In the `Store` runtime, this means `sync` returns once the runtime has finished processing
    /// the action and returned to awaiting the next action (including draining any synchronous
    /// follow-up effects emitted during that processing).
    pub fn sync(&self, value: T) {
        let barrier = Arc::new(Barrier::new(2));

        self.wake_after(|mut shared| {
            shared.queue.push_back(Msg::Value(value));
            shared.queue.push_back(Msg::Barrier(barrier.clone()));
        });

        barrier.wait();
    }

    /// Downgrade to a weak sender for use by long-lived tasks. If the receiver has been dropped,
    /// upgrading will fail.
    pub fn downgrade(&self) -> WeakSender<T> {
        WeakSender {
            shared: Arc::downgrade(&self.shared),
        }
    }

    /// Perform some work and then, if a `Receiver` was waiting, wake it.
    ///
    /// Note that the [`Waker`] will only ever be called once for each time it
    /// has entered the [`Poll::Pending`] state. Regardless of how many times
    /// `wake_after` is called.
    fn wake_after<F: FnOnce(MutexGuard<Shared<T>>)>(&self, f: F) {
        let mut shared = self
            .shared
            .lock() //
            .unwrap_or_else(|err| err.into_inner());

        let waker = shared.waker.take(); // there are no “extra” wakes
        f(shared);

        if let Some(waker) = waker {
            waker.wake() // wake _after_ the `MutexGuard` has been dropped by `f(…)`
        }
    }
}

/// Weak sender handle (used by store tasks).
pub struct WeakSender<T> {
    shared: Weak<Mutex<Shared<T>>>,
}

impl<T> Clone for WeakSender<T> {
    fn clone(&self) -> Self {
        WeakSender {
            shared: self.shared.clone(),
        }
    }
}

impl<T> WeakSender<T> {
    pub fn upgrade(&self) -> Option<Sender<T>> {
        self.shared.upgrade().map(|shared| {
            shared.lock().unwrap().senders += 1;
            Sender { shared }
        })
    }
}

/// Weak receiver handle returned by `channel()` to avoid keeping the channel alive accidentally.
pub struct WeakReceiver<T> {
    shared: Weak<Mutex<Shared<T>>>,
}

impl<T> WeakReceiver<T> {
    pub fn upgrade(self) -> Option<Receiver<T>> {
        self.shared
            .upgrade() //
            .map(|shared| Receiver {
                shared,
                buffer: Default::default(),
            })
    }
}

/// Create a new channel pair.
pub fn channel<T>() -> (Sender<T>, WeakReceiver<T>) {
    let shared = Shared {
        senders: 1,
        ..Default::default()
    };
    let shared = Arc::new(Mutex::new(shared));

    let recv = WeakReceiver {
        shared: Arc::downgrade(&shared),
    };
    let send = Sender { shared };

    (send, recv)
}
