//! Delivers bounded external updates with separate cancellation and completion ownership.

use std::fmt;
use std::future::poll_fn;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

/// Owns a cancellation request that can run on the final owner's thread.
type Cancellation = Box<dyn FnOnce() + Send + 'static>;

/// Cancels the producer even if discarded data or a consumer wake panics.
struct CancelOnDrop<Callback: FnOnce()> {
    /// Allows the destructor to move the callback out before invoking it.
    callback: Option<Callback>,
}

impl<Callback: FnOnce()> Drop for CancelOnDrop<Callback> {
    fn drop(&mut self) {
        let callback = self
            .callback
            .take()
            .expect("cancellation guard owns its callback");
        callback();
    }
}

/// Shares one callback claim across explicit cancellation and final-owner drop.
struct CancellationOwner {
    /// `None` means claimed, even while the winning caller still runs the callback.
    callback: Mutex<Option<Cancellation>>,
}

impl CancellationOwner {
    /// Claims the callback under the lock, then invokes it after unlocking.
    fn cancel(&self) {
        let callback = self
            .callback
            .lock()
            .expect("subscription lock poisoned")
            .take();
        // Taking under the lock linearises all aliases. Invoke after unlocking
        // so cancellation can re-enter this guard or a shared reference.
        if let Some(callback) = callback {
            callback();
        }
    }
}

impl Drop for CancellationOwner {
    fn drop(&mut self) {
        self.cancel();
    }
}

/// Cloneable ownership of one exactly-once cancellation request.
///
/// Explicit cancellation or the final guard's drop invokes the callback. This
/// requests cancellation; it does not acknowledge physical producer completion.
/// A concurrent caller can return while another caller still runs the callback.
/// Callback panics propagate, including from the final owner's destructor.
#[derive(Clone)]
pub struct SharedSubscription {
    /// Producer endpoints do not retain this cancellation ownership.
    owner: Arc<CancellationOwner>,
}

impl SharedSubscription {
    /// Owns one cancellation callback, invoked outside the guard's lock.
    ///
    /// The callback runs on the caller of [`Self::cancel`] or the final owner's
    /// dropping thread. It can re-enter this subscription.
    pub fn new(cancel: impl FnOnce() + Send + 'static) -> Self {
        Self {
            owner: Arc::new(CancellationOwner {
                callback: Mutex::new(Some(Box::new(cancel))),
            }),
        }
    }

    /// Requests cancellation once across all aliases without joining work.
    ///
    /// The caller that claims the callback runs it synchronously. Other callers
    /// return without waiting for that callback or the producer to finish.
    ///
    /// # Panics
    /// Propagates a panic from the callback. Panics if the control lock was poisoned.
    pub fn cancel(&self) {
        self.owner.cancel();
    }

    /// Reports whether a cancellation callback has been claimed.
    ///
    /// A `true` result does not mean that the callback or producer has finished.
    ///
    /// # Panics
    /// Panics if a subscription control lock was poisoned.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.owner
            .callback
            .lock()
            .expect("subscription lock poisoned")
            .is_none()
    }
}

impl fmt::Debug for SharedSubscription {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SharedSubscription").finish_non_exhaustive()
    }
}

/// One storage observation, distinguishing absence from a stored optional value.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SharedUpdate<Value> {
    /// Storage contains this independently captured value.
    Present(Value),
    /// Storage contains no value.
    Absent,
}

/// Separates pending delivery from a producer result, cancellation and consumed termination.
enum Terminal<Failure> {
    /// Accepts new updates and waits when both value slots are empty.
    Open,
    /// Delivers buffered values, then reports normal completion.
    Finished,
    /// Delivers buffered values, then moves this failure to the consumer once.
    Failed(Failure),
    /// Discards buffered values and reports cancellation once.
    Cancelled,
    /// Retains normal end-of-delivery after either terminal result was consumed.
    Consumed,
}

/// Holds the independent initial value, latest update and one terminal result.
struct Queue<Value, Failure> {
    /// Cannot be replaced by updates published during registration.
    initial: Option<SharedUpdate<Value>>,
    /// Replaces earlier unread updates without growing a queue.
    latest: Option<SharedUpdate<Value>>,
    /// First producer termination wins; cancellation can replace an unconsumed result.
    terminal: Terminal<Failure>,
    /// Wakes the sole mutable consumer after a value or terminal result arrives.
    /// Present only while both value slots are empty.
    waker: Option<Waker>,
}

/// Shares delivery state without granting cancellation ownership to producers.
type SharedQueue<Value, Failure> = Arc<Mutex<Queue<Value, Failure>>>;

/// A producer endpoint with one coalescing latest-value slot.
///
/// Holding this endpoint does not retain subscription ownership. Values and
/// callbacks are never dropped or invoked under its queue lock.
/// Producers explicitly call [`Self::finish`] or [`Self::fail`]; dropping an
/// endpoint alone does not certify normal completion.
pub struct SharedProducer<Value, Failure> {
    /// Retains delivery storage, but not the subscription's cancellation callback.
    queue: SharedQueue<Value, Failure>,
}

impl<Value, Failure> Clone for SharedProducer<Value, Failure> {
    fn clone(&self) -> Self {
        Self {
            queue: Arc::clone(&self.queue),
        }
    }
}

impl<Value, Failure> fmt::Debug for SharedProducer<Value, Failure> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SharedProducer").finish_non_exhaustive()
    }
}

impl<Value, Failure> SharedProducer<Value, Failure> {
    /// Replaces the buffered update while delivery remains open.
    ///
    /// Success records acceptance, not delivery. A later update can replace this
    /// value, and cancellation can discard it before the consumer receives it.
    ///
    /// # Errors
    /// Returns the supplied update after cancellation or producer termination.
    ///
    /// # Panics
    /// Propagates a panic from a displaced value's destructor or the waiting waker.
    /// Panics if a subscription control lock was poisoned.
    pub fn publish(&self, update: SharedUpdate<Value>) -> Result<(), SharedUpdate<Value>> {
        let (displaced, waker) = {
            let mut queue = self.queue.lock().expect("subscription queue poisoned");
            if !matches!(queue.terminal, Terminal::Open) {
                return Err(update);
            }
            (queue.latest.replace(update), queue.waker.take())
        };
        drop(displaced);
        if let Some(waker) = waker {
            waker.wake();
        }
        Ok(())
    }

    /// Ends production normally, retaining the buffered update for drainage.
    ///
    /// Returns `true` only for the first accepted producer termination.
    /// Cancellation or an earlier termination returns `false`.
    ///
    /// # Panics
    /// Propagates a panic from the waiting waker. Panics if the control lock was poisoned.
    pub fn finish(&self) -> bool {
        self.terminate(Terminal::Finished)
    }

    /// Ends production with a typed failure after the buffered update drains.
    ///
    /// Returns `true` only for the first accepted producer termination.
    /// Otherwise, returns `false` and drops the supplied failure outside the lock.
    ///
    /// # Panics
    /// Propagates a panic from a rejected failure's destructor or the waiting waker.
    /// Panics if the control lock was poisoned.
    pub fn fail(&self, failure: Failure) -> bool {
        self.terminate(Terminal::Failed(failure))
    }

    /// Installs the first producer result and wakes the consumer after unlocking.
    fn terminate(&self, terminal: Terminal<Failure>) -> bool {
        let waker = {
            let mut queue = self.queue.lock().expect("subscription queue poisoned");
            if !matches!(queue.terminal, Terminal::Open) {
                return false;
            }
            queue.terminal = terminal;
            queue.waker.take()
        };
        if let Some(waker) = waker {
            waker.wake();
        }
        true
    }
}

/// One terminal delivery error, separate from normal drainage.
pub struct UpdateError<Failure> {
    /// Preserves the producer's cause; `None` represents delivery cancellation.
    failure: Option<Failure>,
}

impl<Failure> UpdateError<Failure> {
    /// Reports delivery cancellation rather than producer failure.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.failure.is_none()
    }

    /// Recovers the original producer failure, if this was not cancellation.
    pub fn into_failure(self) -> Option<Failure> {
        self.failure
    }
}

impl<Failure> fmt::Debug for UpdateError<Failure> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UpdateError")
            .field("cancelled", &self.is_cancelled())
            .finish_non_exhaustive()
    }
}

impl<Failure> fmt::Display for UpdateError<Failure> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(if self.is_cancelled() {
            "subscription cancelled"
        } else {
            "subscription producer failed"
        })
    }
}

impl<Failure: std::error::Error + 'static> std::error::Error for UpdateError<Failure> {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.failure.as_ref().map(|failure| failure as _)
    }
}

/// Independently captured initial state followed by bounded latest updates.
///
/// This consumer owns one cancellation alias. Dropping it cancels only when no
/// aliases from [`Self::subscription`] remain. Producer endpoints do not count as aliases.
pub struct SharedUpdates<Value, Failure> {
    /// Contains no unbounded history: one initial value and one latest update.
    queue: SharedQueue<Value, Failure>,
    /// Retains cancellation ownership separately from the producer's completion future.
    subscription: SharedSubscription,
}

impl<Value: Send + 'static, Failure: Send + 'static> SharedUpdates<Value, Failure> {
    /// Registers a producer and its actual completion future.
    ///
    /// `start` must independently capture the initial state and return it even
    /// when registration synchronously publishes later updates. Those updates
    /// coalesce separately and cannot replace that initial delivery. The
    /// returned join is independent of cancellation and must be awaited before
    /// replacing a producer whose physical work can still touch storage.
    ///
    /// Cancellation closes delivery, discards buffered values, wakes the consumer,
    /// then calls `Cancel`. Unwinding from a value destructor or wake still calls
    /// `Cancel`. This does not catch the panic or certify producer completion.
    ///
    /// # Panics
    /// Propagates a panic from `start`. Panics if a control lock was poisoned.
    /// If `start` panics before returning, it must clean up any external work it started.
    ///
    /// # Abort
    /// A second panic during cancellation cleanup aborts the process during unwinding.
    pub fn activate<Start, Cancel, Join>(start: Start) -> (Self, ProducerJoin<Failure>)
    where
        Start: FnOnce(SharedProducer<Value, Failure>) -> (SharedUpdate<Value>, Cancel, Join),
        Cancel: FnOnce() + Send + 'static,
        Join: Future<Output = Result<(), Failure>> + Send + 'static,
    {
        let queue = Arc::new(Mutex::new(Queue {
            initial: None,
            latest: None,
            terminal: Terminal::Open,
            waker: None,
        }));
        let (initial, cancel, join) = start(SharedProducer {
            queue: Arc::clone(&queue),
        });
        queue.lock().expect("subscription queue poisoned").initial = Some(initial);
        let cancelling = Arc::clone(&queue);
        let subscription = SharedSubscription::new(move || {
            let cancel = CancelOnDrop {
                callback: Some(cancel),
            };
            let (initial, latest, terminal, waker) = {
                let mut queue = cancelling.lock().expect("subscription queue poisoned");
                let terminal = if matches!(queue.terminal, Terminal::Consumed) {
                    Terminal::Consumed
                } else {
                    Terminal::Cancelled
                };
                (
                    queue.initial.take(),
                    queue.latest.take(),
                    std::mem::replace(&mut queue.terminal, terminal),
                    queue.waker.take(),
                )
            };
            // The guard retains cancellation if a destructor or wake panics.
            drop((initial, latest, terminal));
            if let Some(waker) = waker {
                waker.wake();
            }
            drop(cancel);
        });
        (
            Self {
                queue,
                subscription,
            },
            ProducerJoin {
                future: Box::pin(join),
            },
        )
    }
}

impl<Value, Failure> SharedUpdates<Value, Failure> {
    /// Retains another alias of this delivery's cancellation ownership.
    #[must_use]
    pub fn subscription(&self) -> SharedSubscription {
        self.subscription.clone()
    }

    /// Takes initial state, the latest buffered update, then terminal state.
    ///
    /// # Errors
    /// Reports producer failure or cancellation once. Subsequent calls drain
    /// normally. Cancellation discards pending deliveries, but does not join.
    /// Dropping a pending call does not cancel the subscription or consume a value.
    ///
    /// # Panics
    /// Propagates a panic from a waker's clone or drop callback.
    /// Panics if a subscription control lock was poisoned.
    pub async fn next(&mut self) -> Result<Option<SharedUpdate<Value>>, UpdateError<Failure>> {
        poll_fn(|context| {
            // Waker callbacks can run user code. Clone before taking the queue lock.
            let waker = context.waker().clone();
            let mut queue = self.queue.lock().expect("subscription queue poisoned");
            if let Some(initial) = queue.initial.take() {
                return Poll::Ready(Ok(Some(initial)));
            }
            if let Some(latest) = queue.latest.take() {
                return Poll::Ready(Ok(Some(latest)));
            }
            match std::mem::replace(&mut queue.terminal, Terminal::Consumed) {
                Terminal::Open => {
                    queue.terminal = Terminal::Open;
                    let old_waker = queue.waker.replace(waker);
                    drop(queue);
                    drop(old_waker);
                    Poll::Pending
                }
                Terminal::Finished | Terminal::Consumed => Poll::Ready(Ok(None)),
                Terminal::Failed(failure) => Poll::Ready(Err(UpdateError {
                    failure: Some(failure),
                })),
                Terminal::Cancelled => Poll::Ready(Err(UpdateError { failure: None })),
            }
        })
        .await
    }
}

impl<Value, Failure> fmt::Debug for SharedUpdates<Value, Failure> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SharedUpdates").finish_non_exhaustive()
    }
}

/// Completion of actual producer work, independent of delivery cancellation.
///
/// Dropping this future does not manufacture successful quiescence. Adapters
/// must supply a future that joins all admitted producer work, not merely its
/// delivery task.
#[must_use = "producer quiescence requires awaiting its join"]
pub struct ProducerJoin<Failure> {
    /// Owns the provider's actual completion future without owning cancellation.
    future: Pin<Box<dyn Future<Output = Result<(), Failure>> + Send + 'static>>,
}

impl<Failure> Future for ProducerJoin<Failure> {
    type Output = Result<(), Failure>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.future.as_mut().poll(cx)
    }
}

impl<Failure> fmt::Debug for ProducerJoin<Failure> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProducerJoin").finish_non_exhaustive()
    }
}
