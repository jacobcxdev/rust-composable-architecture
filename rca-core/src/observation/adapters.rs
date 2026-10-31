//! Adapts explicitly owned external producers and post-commit Store callbacks.
//!
//! Store observations register eagerly when their registration future completes.
//! External observations activate only when `next` is first polled. Neither
//! adapter discovers property reads, polls in the background, or spawns tasks.

use std::cell::RefCell;
use std::fmt;
use std::rc::{Rc, Weak};
use std::task::{Poll, Waker};

use super::Observation;

/// Runs owned unsubscription once, including when discarded payloads unwind.
#[derive(Default)]
struct Shutdown(Option<Box<dyn FnOnce()>>);

impl Shutdown {
    /// Removes the callback before invocation so that a panic cannot repeat it.
    fn run(&mut self) {
        if let Some(shutdown) = self.0.take() {
            shutdown();
        }
    }
}

impl Drop for Shutdown {
    fn drop(&mut self) {
        self.run();
    }
}

/// Activates the source once and returns its initial value and unsubscription owner.
type Start<Value, Failure> =
    Box<dyn FnOnce(ObservationProducer<Value, Failure>) -> (Value, Shutdown)>;

/// Stores one coalesced value, one terminal request and the current consumer's waker.
struct SourceState<Value, Failure> {
    /// Closes admission before terminal delivery or owner teardown.
    accepting: bool,
    /// Replaces unread updates without changing the initial selection.
    latest: Option<Value>,
    /// Drains after the latest value unless explicit cancellation discards that value.
    terminal: Option<Result<(), ObservationError<Failure>>>,
    /// Belongs to the current `next` future, not the whole subscription.
    waker: Option<Waker>,
}

/// Releases a demand future's task waker without cancelling the subscription.
struct Waiter<'a, Value, Failure>(&'a RefCell<SourceState<Value, Failure>>);

impl<Value, Failure> Drop for Waiter<'_, Value, Failure> {
    fn drop(&mut self) {
        let waker = self.0.borrow_mut().waker.take();
        drop(waker);
    }
}

/// Distinguishes producer failure from explicit observation cancellation.
pub struct ObservationError<Failure> {
    /// Carries the typed producer error. Absence denotes explicit cancellation.
    failure: Option<Failure>,
}

impl<Failure> ObservationError<Failure> {
    /// Returns whether the consumer explicitly cancelled the sequence.
    #[must_use]
    pub const fn is_cancelled(&self) -> bool {
        self.failure.is_none()
    }

    /// Returns the owned producer failure, or `None` for cancellation.
    #[must_use]
    pub fn into_failure(self) -> Option<Failure> {
        self.failure
    }
}

impl<Failure> fmt::Debug for ObservationError<Failure> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ObservationError")
            .field("cancelled", &self.is_cancelled())
            .finish_non_exhaustive()
    }
}

impl<Failure> fmt::Display for ObservationError<Failure> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_cancelled() {
            f.write_str("observation cancelled")
        } else {
            f.write_str("observation producer failed")
        }
    }
}

impl<Failure: std::error::Error + 'static> std::error::Error for ObservationError<Failure> {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.failure.as_ref().map(|failure| failure as _)
    }
}

/// Publishes local latest values without owning the receiving sequence.
///
/// Clones share one weak endpoint. Values and failures need not implement
/// `Send`, `Clone`, or `Debug`. This local endpoint does not cross threads or
/// grant Store authority: forwarding a value still requires a live Store or
/// scoped handle. Dropping endpoints does not implicitly finish the sequence;
/// use [`Self::finish`], [`Self::fail`], or cancel the receiving owner.
pub struct ObservationProducer<Value, Failure> {
    /// Cannot keep the receiving subscription alive after its owner is dropped.
    state: Weak<RefCell<SourceState<Value, Failure>>>,
}

impl<Value, Failure> ObservationProducer<Value, Failure> {
    /// Replaces the buffered latest value without changing the initial selection.
    ///
    /// There is no implicit distinctness comparison. An external source may
    /// explicitly suppress equal values before publishing them.
    ///
    /// # Errors
    ///
    /// Returns the supplied value after finish, failure, cancellation, or owner drop.
    ///
    /// # Panics
    ///
    /// Propagates replaced-value destructor and task-waker panics after buffering the value.
    pub fn publish(&self, value: Value) -> Result<(), Value> {
        let Some(state) = self.state.upgrade() else {
            return Err(value);
        };
        let (previous, waker) = {
            let mut state = state.borrow_mut();
            if !state.accepting {
                return Err(value);
            }
            (state.latest.replace(value), state.waker.take())
        };
        // User values and wakers may reenter their source; never drop or wake
        // while the source is mutably borrowed.
        drop(previous);
        if let Some(waker) = waker {
            waker.wake();
        }
        Ok(())
    }

    /// Finishes after the initial and buffered latest values have drained.
    ///
    /// Returns `true` only for the first terminal request. The receiver runs
    /// shutdown when it consumes termination, cancels, or is dropped.
    ///
    /// # Panics
    ///
    /// Propagates task-waker panics after closing producer admission.
    #[must_use]
    pub fn finish(&self) -> bool {
        self.terminate(Ok(()))
    }

    /// Fails after the initial and buffered latest values have drained.
    ///
    /// Returns `false` if already terminated or the receiving owner was dropped.
    /// Only the first terminal request is retained; later failures are dropped.
    ///
    /// # Panics
    ///
    /// Propagates task-waker panics after closing producer admission.
    /// Destruction of a rejected failure can also panic.
    #[must_use]
    pub fn fail(&self, failure: Failure) -> bool {
        self.terminate(Err(ObservationError {
            failure: Some(failure),
        }))
    }

    /// Stores the first terminal request, then wakes without holding a source borrow.
    fn terminate(&self, terminal: Result<(), ObservationError<Failure>>) -> bool {
        let Some(state) = self.state.upgrade() else {
            return false;
        };
        let waker = {
            let mut state = state.borrow_mut();
            if !state.accepting {
                return false;
            }
            state.accepting = false;
            state.terminal = Some(terminal);
            state.waker.take()
        };
        if let Some(waker) = waker {
            waker.wake();
        }
        true
    }
}

impl<Value, Failure> Clone for ObservationProducer<Value, Failure> {
    fn clone(&self) -> Self {
        Self {
            state: Weak::clone(&self.state),
        }
    }
}

impl<Value, Failure> fmt::Debug for ObservationProducer<Value, Failure> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ObservationProducer")
            .finish_non_exhaustive()
    }
}

/// Lazily owns an external subscription and delivers initial then latest values.
///
/// This local, runtime-neutral sequence accepts non-`Send` values and failures.
/// Its start closure runs once on the first polled [`Self::next`], returning one
/// initial selection and an owned shutdown closure. Thereafter, unread updates
/// coalesce into one latest slot. Finish and failure drain that slot; explicit
/// cancellation discards it. Shutdown runs exactly once on terminal consumption,
/// cancellation, or drop, with producer admission already closed and no borrow held.
///
/// No Store scope is inferred. A consumer forwarding these values must retain
/// and use its existing scoped handle; a producer result cannot renew that handle.
pub struct ExternalObservation<Value, Failure> {
    /// Owns the receiving endpoint while producers hold only weak references.
    state: Rc<RefCell<SourceState<Value, Failure>>>,
    /// Exists only before activation or explicit cancellation.
    start: Option<Start<Value, Failure>>,
    /// Owns unsubscription independently of values and their destructors.
    shutdown: Shutdown,
    /// Makes an already-consumed terminal result permanent.
    done: bool,
}

impl<Value, Failure> ExternalObservation<Value, Failure> {
    /// Creates a lazy sequence with explicit activation and shutdown ownership.
    ///
    /// `start` may synchronously publish or terminate through its endpoint. Its
    /// returned initial selection still precedes any buffered latest value.
    /// Dropping this sequence before polling `next` never invokes `start`.
    ///
    /// # Examples
    ///
    /// ```
    /// # async fn example() {
    /// let mut values = rca_core::ExternalObservation::<_, std::convert::Infallible>::new(
    ///     |producer| {
    ///         producer.publish(2).unwrap();
    ///         assert!(producer.finish());
    ///         (1, || {})
    ///     },
    /// );
    /// assert_eq!(values.next().await.unwrap(), Some(1));
    /// assert_eq!(values.next().await.unwrap(), Some(2));
    /// assert_eq!(values.next().await.unwrap(), None);
    /// # }
    /// ```
    pub fn new<Stop>(
        start: impl FnOnce(ObservationProducer<Value, Failure>) -> (Value, Stop) + 'static,
    ) -> Self
    where
        Stop: FnOnce() + 'static,
    {
        Self {
            state: Rc::new(RefCell::new(SourceState {
                accepting: true,
                latest: None,
                terminal: None,
                waker: None,
            })),
            start: Some(Box::new(move |producer| {
                let (initial, shutdown) = start(producer);
                (initial, Shutdown(Some(Box::new(shutdown))))
            })),
            shutdown: Shutdown::default(),
            done: false,
        }
    }

    /// Returns the next initial/latest value, or `None` after normal completion.
    ///
    /// Dropping this future does not cancel the sequence or restart activation.
    /// Errors are emitted once; all subsequent calls return `Ok(None)`.
    ///
    /// # Errors
    ///
    /// Returns the producer's typed failure or explicit consumer cancellation.
    ///
    /// # Panics
    ///
    /// Propagates panics from the caller's start or shutdown closure.
    /// Task-waker callbacks and resource destructors can also panic.
    /// A panicking start is not retried and must clean up resources it did not return.
    pub async fn next(&mut self) -> Result<Option<Value>, ObservationError<Failure>> {
        if let Some(start) = self.start.take() {
            let (initial, shutdown) = start(ObservationProducer {
                state: Rc::downgrade(&self.state),
            });
            self.shutdown = shutdown;
            return Ok(Some(initial));
        }
        let waiter = Waiter(self.state.as_ref());
        let result = std::future::poll_fn(|context| {
            if self.done {
                return Poll::Ready(Ok(None));
            }
            let waker = context.waker().clone();
            let mut state = self.state.borrow_mut();
            if let Some(value) = state.latest.take() {
                return Poll::Ready(Ok(Some(value)));
            }
            if let Some(terminal) = state.terminal.take() {
                self.done = true;
                return Poll::Ready(terminal.map(|()| None));
            }
            let previous = state.waker.replace(waker);
            drop(state);
            drop(previous);
            Poll::Pending
        })
        .await;
        drop(waiter);
        if self.done {
            self.shutdown.run();
        }
        result
    }

    /// Cancels, discards queued values, and immediately runs owned shutdown.
    ///
    /// The next call reports cancellation once unless termination was already
    /// consumed. Repeated cancellation and subsequent drop do not repeat shutdown.
    ///
    /// # Panics
    ///
    /// Propagates panics from the caller's shutdown closure or value destructors.
    /// Admission closes first. With unwinding enabled, a discarded payload panic
    /// still runs owned shutdown. A panicking shutdown callback is not called again.
    pub fn cancel(&mut self) {
        if self.done {
            return;
        }
        let (latest, terminal, waker) = {
            let mut state = self.state.borrow_mut();
            state.accepting = false;
            let terminal = state
                .terminal
                .replace(Err(ObservationError { failure: None }));
            (state.latest.take(), terminal, state.waker.take())
        };
        let shutdown = std::mem::take(&mut self.shutdown);
        self.start.take();
        drop((latest, terminal, waker));
        drop(shutdown);
    }
}

impl<Value, Failure> Drop for ExternalObservation<Value, Failure> {
    fn drop(&mut self) {
        self.cancel();
    }
}

impl<Value, Failure> fmt::Debug for ExternalObservation<Value, Failure> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ExternalObservation")
            .field("started", &self.start.is_none())
            .field("done", &self.done)
            .finish_non_exhaustive()
    }
}

impl<Value> Observation<Value> {
    /// Invokes one callback with the next owned post-commit publication.
    ///
    /// Consumes this eager Store registration, discards its initial selection if
    /// unread, and waits for a publication. Callers must poll the returned future;
    /// there is no hidden task. Unread commits coalesce and distinctness follows
    /// the registration's policy. Every eligible commit still runs the selector,
    /// even when only fields excluded from its result changed.
    ///
    /// Returns `true` after invocation, or `false` if authority closes first.
    /// Admission atomically deactivates the registration before invocation;
    /// a later closure cannot retract an already admitted callback. Its owner is
    /// dropped before invocation, outside actor reduction, locks, and borrows.
    /// The callback may enqueue new ingress; it cannot perform nested reduction.
    /// This is post-commit delivery,
    /// not `willSet` notification. Dropping the future cancels its registration.
    ///
    /// # Panics
    ///
    /// Propagates callback and value-destructor panics after registration cancellation.
    pub fn on_change(self, callback: impl FnOnce(Value)) -> impl Future<Output = bool> {
        self.core.on_change(callback)
    }
}
