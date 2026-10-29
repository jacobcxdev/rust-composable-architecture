//! Exposes clone-free latest-value Store projections.
//!
//! The actor owns selectors and unread updates. The receiving handle owns the
//! initial value and moves later values through one reusable take operation.
//! Shared and owner-thread handles use the same polling and cancellation rules.
//!
//! Use [`crate::Store::observe`] for projected Store state and
//! [`ExternalObservation`] for explicitly owned external subscriptions.

use std::fmt;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Weak;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::task::{Context, Poll};

use event_listener::{Event, EventListener};

use crate::StoreStopped;
use crate::effect::lineage::ControlWake;
use crate::store::carriers::Shared;

mod adapters;
#[cfg(feature = "local")]
pub mod local;
pub(crate) mod wait;

#[doc(inline)]
pub use adapters::{ExternalObservation, ObservationError, ObservationProducer};
#[doc(inline)]
pub use wait::{PredicateWait, PredicateWaitOutcome};

/// Identifies one registration within its Store actor.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct ObservationId(u64);

impl ObservationId {
    /// Wraps the actor's checked, monotonically increasing registration counter.
    pub(crate) const fn new(value: u64) -> Self {
        Self(value)
    }
}

/// Shares publication and cancellation signals without sharing projected values.
#[derive(Debug)]
pub(crate) struct ObservationSignal {
    /// Becomes false once, before cancellation wakes either participant.
    active: AtomicBool,
    /// Records the newest published commit without moving its value off the actor.
    version: AtomicU64,
    /// Wakes the receiver for publication or closure.
    changed: Event,
    /// Requests actor-side cleanup without retaining Store authority.
    control: Weak<ControlWake>,
}

/// Requests actor cleanup after the consumer wake, including during unwinding.
struct NotifyControlOnDrop<'a>(&'a Weak<ControlWake>);

impl Drop for NotifyControlOnDrop<'_> {
    fn drop(&mut self) {
        if let Some(control) = self.0.upgrade() {
            control.notify();
        }
    }
}

impl ObservationSignal {
    /// Creates an active signal before the actor accepts the registration.
    pub(crate) fn new(control: Weak<ControlWake>) -> Arc<Self> {
        Arc::new(Self {
            active: AtomicBool::new(true),
            version: AtomicU64::new(0),
            changed: Event::new(),
            control,
        })
    }

    /// Publishes the actor's commit version before waking the waiting receiver.
    pub(crate) fn publish(&self, version: u64) {
        self.version.store(version, Ordering::Release);
        self.changed.notify(usize::MAX);
    }

    /// Closes admission once, then wakes the receiver and requests actor cleanup.
    ///
    /// Consumer-waker panics still request cleanup while unwinding. A second
    /// panic from the actor waker during that unwind aborts the process.
    pub(crate) fn deactivate(&self) -> bool {
        let was_active = self.active.swap(false, Ordering::AcqRel);
        if was_active {
            let _notify_control = NotifyControlOnDrop(&self.control);
            self.changed.notify(usize::MAX);
        }
        was_active
    }

    /// Reads whether the receiver and actor still accept this registration.
    pub(crate) fn is_active(&self) -> bool {
        self.active.load(Ordering::Acquire)
    }

    /// Reads the newest actor publication for the listen-then-recheck protocol.
    fn version(&self) -> u64 {
        self.version.load(Ordering::Acquire)
    }
}

/// Moves a value, acknowledges an empty slot, or reports a retired registration.
pub(crate) enum TakeReply<Value> {
    /// Transfers the latest value and its commit version out of actor storage.
    Value {
        /// Identifies the commit that selected this value.
        version: u64,
        /// Moves to the receiving handle without a clone.
        value: Value,
    },
    /// Acknowledges the newest selection already removed from the actor slot.
    Empty {
        /// Prevents repeated requests for the same empty slot.
        version: u64,
    },
    /// Ends demand after cancellation or removal of the registration.
    Closed,
}

/// Selects erased transport bounds without duplicating the observation protocol.
pub(crate) trait ObservationFamily: Sized {
    /// Owns the reusable actor command with the family's thread-safety bounds.
    type Operation<Value>: ?Sized + TakeOperation<Value, Self>;
    /// Owns deferred admission when the actor's bounded mailbox is full.
    type Deferred<Value>: Future<Output = Result<TakeOutcome<Value, Self>, StoreStopped>> + Unpin;
}

impl ObservationFamily for Shared {
    type Operation<Value> = dyn TakeOperation<Value> + Send + Sync;
    type Deferred<Value> =
        Pin<Box<dyn Future<Output = Result<TakeOutcome<Value>, StoreStopped>> + Send>>;
}

/// Retains one take request across cancelled and resumed demand futures.
pub(crate) enum TakeFuture<Value, Family: ObservationFamily = Shared> {
    /// Waits for the reply to an admitted actor command.
    Receiving {
        /// Receives both the actor's answer and the reusable command allocation.
        receive: futures_channel::oneshot::Receiver<TakeOutcome<Value, Family>>,
    },
    /// Waits for both mailbox space and the actor's reply.
    Deferred(Family::Deferred<Value>),
    /// Reports that admission failed because actor authority ended.
    Closed,
}

impl<Value, Family: ObservationFamily> Future for TakeFuture<Value, Family> {
    type Output = Result<TakeOutcome<Value, Family>, StoreStopped>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match self.get_mut() {
            Self::Receiving { receive } => Pin::new(receive)
                .poll(cx)
                .map_err(|_closed| StoreStopped::new()),
            Self::Deferred(future) => Pin::new(future).poll(cx),
            Self::Closed => Poll::Ready(Err(StoreStopped::new())),
        }
    }
}

/// Moves one reusable command allocation between the receiver and actor.
pub(crate) trait TakeOperation<Value, Family: ObservationFamily = Shared> {
    /// Requests the latest value and returns this operation with a successful reply.
    fn take(self: Box<Self>) -> TakeFuture<Value, Family>;
}

/// Returns the actor's result together with the command allocation for reuse.
pub(crate) struct TakeOutcome<Value, Family: ObservationFamily = Shared> {
    /// Reports the actor slot's value, acknowledged version, or closure.
    pub(crate) reply: TakeReply<Value>,
    /// Returns the same command allocation for the next publication.
    pub(crate) operation: Box<Family::Operation<Value>>,
}

/// Keeps listener and request lifetimes independent of any single `next` future.
enum TakeState<Value, Family: ObservationFamily> {
    /// Owns its reusable operation and checks for new publications before listening.
    Idle,
    /// Retains a registered listener until publication, closure, or owner drop.
    Waiting(EventListener),
    /// Retains an admitted or deferred request until its reply arrives.
    Taking(TakeFuture<Value, Family>),
    /// Permanently returns `None` without registering further work.
    Closed,
}

/// Delivers latest projected Store values without cloning them.
///
/// The initial value is followed by coalesced post-commit updates. This is not
/// a log of every action. Dropping this handle cancels its registration without
/// waiting for the actor to release the selector and unread update.
///
/// This handle does not keep Store authority alive. An initial value or an
/// already-written take reply remains readable after authority closes. Values
/// still held by the actor are discarded when it retires the registration.
pub struct Observation<Value> {
    /// Uses transferable transport without changing the shared polling protocol.
    core: ObservationCore<Value, Shared>,
}

/// Owns demand state while the actor retains selectors and unread updates.
pub(crate) struct ObservationCore<Value, Family: ObservationFamily> {
    /// Identifies this registration in diagnostics without retaining its selector.
    id: ObservationId,
    /// Shares only atomic metadata and wake notifications with the actor.
    signal: Arc<ObservationSignal>,
    /// Keeps the handle's storage size independent of the selected value's size.
    initial: Option<Box<Value>>,
    /// Prevents another request until a later publication becomes visible.
    delivered_version: u64,
    /// Is available while idle and returns with a successful in-flight reply.
    take_operation: Option<Box<Family::Operation<Value>>>,
    /// Owns the listener or request across separate calls to `next`.
    take_state: TakeState<Value, Family>,
    // Force the real lost-notification ordering without changing non-test builds.
    #[cfg(test)]
    before_listen: Option<Box<dyn FnOnce() + Send + Sync>>,
}

impl<Value> Observation<Value> {
    /// Owns the actor's initial selection and the command used for later requests.
    pub(crate) fn new(
        id: ObservationId,
        signal: Arc<ObservationSignal>,
        initial: Value,
        delivered_version: u64,
        take_operation: Box<dyn TakeOperation<Value> + Send + Sync>,
    ) -> Self {
        Self {
            core: ObservationCore::new(id, signal, initial, delivered_version, take_operation),
        }
    }

    /// Returns the initial or latest projected value, then `None` after closure.
    ///
    /// An initial value or already-written take reply is delivered before closure.
    /// Once this returns `None`, later calls return `None` too.
    ///
    /// Dropping this future cancels only this demand, not the observation. Its
    /// listener or in-flight take stays in the handle for the next call. A waiting
    /// listener can therefore retain the last task waker until another poll,
    /// publication, closure, or handle drop.
    ///
    /// # Panics
    ///
    /// Propagates task-waker panics. Resuming a future after a poll panic is unsupported.
    #[expect(
        clippy::should_implement_trait,
        reason = "asynchronous next returns a Future, unlike Iterator::next"
    )]
    pub fn next(&mut self) -> impl Future<Output = Option<Value>> {
        self.core.next()
    }
}

impl<Value, Family: ObservationFamily> ObservationCore<Value, Family> {
    /// Stores the completed registration without starting background work.
    pub(crate) fn new(
        id: ObservationId,
        signal: Arc<ObservationSignal>,
        initial: Value,
        delivered_version: u64,
        take_operation: Box<Family::Operation<Value>>,
    ) -> Self {
        Self {
            id,
            signal,
            initial: Some(Box::new(initial)),
            delivered_version,
            take_operation: Some(take_operation),
            take_state: TakeState::Idle,
            #[cfg(test)]
            before_listen: None,
        }
    }

    /// Borrows the persistent state machine for one demand future.
    pub(crate) async fn next(&mut self) -> Option<Value> {
        std::future::poll_fn(|cx| self.poll_next_value(cx)).await
    }

    /// Delivers owned replies and registers before rechecking publication state.
    pub(crate) fn poll_next_value(&mut self, cx: &mut Context<'_>) -> Poll<Option<Value>> {
        if let Some(initial) = self.initial.take() {
            return Poll::Ready(Some(*initial));
        }

        loop {
            match &mut self.take_state {
                TakeState::Closed => return Poll::Ready(None),
                TakeState::Taking(future) => match Pin::new(future).poll(cx) {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(Ok(TakeOutcome { reply, operation })) => {
                        self.take_operation = Some(operation);
                        match reply {
                            TakeReply::Value { version, value } => {
                                self.take_state = TakeState::Idle;
                                self.delivered_version = version;
                                return Poll::Ready(Some(value));
                            }
                            TakeReply::Empty { version } => {
                                self.take_state = TakeState::Idle;
                                self.delivered_version = self.delivered_version.max(version);
                            }
                            TakeReply::Closed => return self.close(),
                        }
                    }
                    Poll::Ready(Err(_)) => return self.close(),
                },
                TakeState::Waiting(listener) => match Pin::new(listener).poll(cx) {
                    Poll::Pending => {
                        if self.signal.is_active() {
                            return Poll::Pending;
                        }
                        return self.close();
                    }
                    Poll::Ready(()) => self.take_state = TakeState::Idle,
                },
                TakeState::Idle => {
                    if !self.signal.is_active() {
                        return self.close();
                    }
                    if self.signal.version() > self.delivered_version {
                        self.take_state = TakeState::Taking(
                            self.take_operation
                                .take()
                                .expect("idle observation owns its take operation")
                                .take(),
                        );
                    } else {
                        #[cfg(test)]
                        if let Some(before_listen) = self.before_listen.take() {
                            before_listen();
                        }
                        let listener = self.signal.changed.listen();
                        if self.signal.version() > self.delivered_version {
                            continue;
                        }
                        self.take_state = TakeState::Waiting(listener);
                    }
                }
            }
        }
    }

    /// Commits terminal demand before releasing its listener, request and reusable command.
    fn close(&mut self) -> Poll<Option<Value>> {
        let state = std::mem::replace(&mut self.take_state, TakeState::Closed);
        let operation = self.take_operation.take();
        drop(state);
        drop(operation);
        Poll::Ready(None)
    }

    /// Discards the initial value and detaches before invoking one update callback.
    ///
    /// Initial-value destruction precedes normal deactivation. If it panics,
    /// unwinding drops this owner and requests registration cleanup instead.
    pub(crate) async fn on_change(mut self, callback: impl FnOnce(Value)) -> bool {
        self.initial.take();
        let Some(value) = self.next().await else {
            return false;
        };
        if !self.signal.deactivate() {
            return false;
        }
        drop(self);
        callback(value);
        true
    }
}

impl<Value> Unpin for Observation<Value> {}

#[cfg(feature = "futures-stream")]
impl<Value: Send + 'static> futures_core::Stream for Observation<Value> {
    type Item = Value;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.get_mut().core.poll_next_value(cx)
    }
}

impl<Value, Family: ObservationFamily> Drop for ObservationCore<Value, Family> {
    fn drop(&mut self) {
        self.signal.deactivate();
    }
}

impl<Value> fmt::Debug for Observation<Value> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.core.fmt(f)
    }
}

impl<Value, Family: ObservationFamily> fmt::Debug for ObservationCore<Value, Family> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Observation")
            .field("id", &self.id)
            .field("delivered_version", &self.delivered_version)
            .field("active", &self.signal.is_active())
            .finish_non_exhaustive()
    }
}

#[cfg(all(test, not(loom)))]
ordinary_test_items! {
mod tests {
    use std::sync::{Arc, Mutex};
    use std::task::{Context, Poll};

    use futures_util::FutureExt as _;

    use crate::test_support as support;
    use crate::{Effect, Reduce, Store};

    // Closed demand has no further use for its boxed command or weak ingress.
    #[test]
    fn closure_retires_take_operation_and_stays_closed_if_drop_panics() {
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

        use super::{
            Observation, ObservationId, ObservationSignal, TakeFuture, TakeOperation, TakeOutcome,
            TakeReply,
        };

        struct CloseReply {
            drops: Arc<AtomicUsize>,
            panic: Arc<AtomicBool>,
        }

        impl TakeOperation<u64> for CloseReply {
            fn take(self: Box<Self>) -> TakeFuture<u64> {
                let (reply, receive) = futures_channel::oneshot::channel();
                let _ = reply.send(TakeOutcome::<u64> {
                    reply: TakeReply::Closed,
                    operation: self,
                });
                TakeFuture::Receiving { receive }
            }
        }

        impl Drop for CloseReply {
            fn drop(&mut self) {
                self.drops.fetch_add(1, Ordering::SeqCst);
                assert!(!self.panic.load(Ordering::SeqCst), "operation drop panic");
            }
        }

        for (closed_reply, panic_on_drop) in [
            (false, false),
            (true, false),
            (false, true),
            (true, true),
        ] {
            let drops = Arc::new(AtomicUsize::new(0));
            let panic = Arc::new(AtomicBool::new(panic_on_drop));
            let signal = ObservationSignal::new(std::sync::Weak::new());
            let mut values = Observation::new(
                ObservationId::new(0),
                Arc::clone(&signal),
                0,
                0,
                Box::new(CloseReply {
                    drops: Arc::clone(&drops),
                    panic: Arc::clone(&panic),
                }),
            );
            assert_eq!(values.next().now_or_never(), Some(Some(0)));

            if closed_reply {
                signal.publish(1);
            } else {
                assert!(signal.deactivate());
            }
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                values.next().now_or_never()
            }));
            panic.store(false, Ordering::SeqCst);

            assert_eq!(drops.load(Ordering::SeqCst), 1);
            if panic_on_drop {
                outcome.expect_err("the operation destructor panic remains visible");
            } else {
                assert_eq!(outcome.unwrap(), Some(None));
            }
            assert_eq!(values.next().now_or_never(), Some(None));

            drop(values);
            assert_eq!(drops.load(Ordering::SeqCst), 1);
        }
    }

    #[test]
    fn initial_value_drop_panic_cancels_callback_registration_during_unwind() {
        use super::{Observation, ObservationId, ObservationSignal, TakeFuture, TakeOperation};

        struct InitialValue(Arc<ObservationSignal>);

        impl Drop for InitialValue {
            fn drop(&mut self) {
                assert!(
                    self.0.is_active(),
                    "initial destruction precedes normal deactivation"
                );
                panic!("initial value drop panic");
            }
        }

        struct NeverTake;

        impl TakeOperation<InitialValue> for NeverTake {
            fn take(self: Box<Self>) -> TakeFuture<InitialValue> {
                panic!("initial destruction precedes demand for another value");
            }
        }

        let signal = ObservationSignal::new(std::sync::Weak::new());
        let observation = Observation::new(
            ObservationId::new(0),
            Arc::clone(&signal),
            InitialValue(Arc::clone(&signal)),
            0,
            Box::new(NeverTake),
        );
        let mut callback =
            std::pin::pin!(observation.on_change(|_| panic!("no update is admitted")));

        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            callback.as_mut().poll(&mut Context::from_waker(std::task::Waker::noop()))
        }));
        let panic = panic.expect_err("initial-value destruction propagates its panic");
        assert_eq!(panic.downcast_ref::<&str>(), Some(&"initial value drop panic"));
        assert!(
            !signal.is_active(),
            "unwinding retires the registration before propagation"
        );
    }

    #[test]
    fn empty_take_acknowledges_version_before_later_publication() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        use super::{
            Observation, ObservationId, ObservationSignal, TakeFuture, TakeOperation, TakeOutcome,
            TakeReply,
        };

        struct Replies {
            replies: std::array::IntoIter<TakeReply<u64>, 2>,
            calls: Arc<AtomicUsize>,
        }

        impl TakeOperation<u64> for Replies {
            fn take(mut self: Box<Self>) -> TakeFuture<u64> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                let reply = self
                    .replies
                    .next()
                    .expect("no take is allowed without another publication");
                TakeFuture::<u64>::Deferred(Box::pin(std::future::ready(Ok(TakeOutcome::<u64> {
                    reply,
                    operation: self,
                }))))
            }
        }

        let calls = Arc::new(AtomicUsize::new(0));
        let signal = ObservationSignal::new(std::sync::Weak::new());
        // Test the private callback protocol, not a public Store interleaving.
        let operation = Box::new(Replies {
            replies: [
                TakeReply::Empty { version: 1 },
                TakeReply::Value {
                    version: 2,
                    value: 7,
                },
            ]
            .into_iter(),
            calls: Arc::clone(&calls),
        });
        let mut values = Observation::new(
            ObservationId::new(0),
            Arc::clone(&signal),
            0,
            0,
            operation,
        );
        assert_eq!(values.next().now_or_never(), Some(Some(0)));
        assert_eq!(calls.load(Ordering::SeqCst), 0);

        signal.publish(1);
        let mut context = Context::from_waker(std::task::Waker::noop());
        let mut next = Box::pin(values.next());
        assert!(next.as_mut().poll(&mut context).is_pending());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(next.as_mut().poll(&mut context).is_pending());
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        signal.publish(2);
        assert_eq!(next.as_mut().poll(&mut context), Poll::Ready(Some(7)));
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        drop(next);
        assert_eq!(values.next().now_or_never(), None);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert!(signal.deactivate());
        assert_eq!(values.next().now_or_never(), Some(None));
        drop(values);
        assert_eq!(Arc::strong_count(&calls), 1);
    }

    #[test]
    fn retained_take_operation_reports_stopped_after_actor_loss() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        for release_store in [false, true] {
            let runtime = support::ManualRuntime::new();
            let store = Store::new(
                7,
                Reduce::new(|_: &mut usize, (): &(), _| Effect::none()),
                runtime.runtime(),
            );
            let mut actor = runtime.take_only_queued_task();
            let mut context = Context::from_waker(std::task::Waker::noop());
            let selections = Arc::new(AtomicUsize::new(0));
            let selected = Arc::clone(&selections);
            let capture = Arc::new(());
            let weak_capture = Arc::downgrade(&capture);
            let mut values = {
                let mut registration = std::pin::pin!(store.observe(move |state| {
                    assert_eq!(Arc::strong_count(&capture), 1);
                    selected.fetch_add(1, Ordering::Relaxed);
                    *state
                }));
                assert!(registration.as_mut().poll(&mut context).is_pending());
                assert!(actor.as_mut().poll(&mut context).is_pending());
                let Poll::Ready(values) = registration.as_mut().poll(&mut context) else {
                    panic!("the actor must acknowledge observation registration");
                };
                values.unwrap()
            };
            assert_eq!(values.next().now_or_never(), Some(Some(7)));
            assert_eq!(selections.load(Ordering::Relaxed), 1);
            assert!(weak_capture.upgrade().is_some());
            // Retain the real operation, not a fabricated closed future. Public
            // next() normally checks the signal before trying this operation.
            let operation = values.core.take_operation.take().unwrap();
            drop(actor);
            let mut retained_store = Some(store);
            if release_store {
                drop(retained_store.take());
            }
            assert!(weak_capture.upgrade().is_none());
            let mut take = std::pin::pin!(operation.take());
            assert!(matches!(
                take.as_mut().poll(&mut context),
                Poll::Ready(Err(_))
            ));
            assert_eq!(selections.load(Ordering::Relaxed), 1);
            assert_eq!(values.next().now_or_never(), Some(None));
            drop(retained_store);
        }
    }

    #[test]
    fn publication_before_listener_registration_is_not_lost() {
        let runtime = support::ManualRuntime::new();
        let store = Store::new(
            0,
            Reduce::new(|state: &mut usize, (): &(), _| {
                *state += 1;
                Effect::none()
            }),
            runtime.runtime(),
        );
        let actor = Arc::new(Mutex::new(runtime.take_only_queued_task()));
        let mut context = Context::from_waker(std::task::Waker::noop());
        let mut values = {
            let mut registration = std::pin::pin!(store.observe(|state| *state));
            assert!(registration.as_mut().poll(&mut context).is_pending());
            assert!(actor.lock().unwrap().as_mut().poll(&mut context).is_pending());
            let Poll::Ready(values) = registration.as_mut().poll(&mut context) else {
                panic!("the actor must acknowledge observation registration");
            };
            values.unwrap()
        };
        assert_eq!(values.next().now_or_never(), Some(Some(0)));

        let publish_store = store.clone();
        let publish_actor = Arc::clone(&actor);
        values.core.before_listen = Some(Box::new(move || {
            let receipt = publish_store.try_send(()).unwrap();
            let mut context = Context::from_waker(std::task::Waker::noop());
            assert!(publish_actor
                .lock()
                .unwrap()
                .as_mut()
                .poll(&mut context)
                .is_pending());
            let _task = receipt
                .finish()
                .now_or_never()
                .expect("the scheduled commit must already be acknowledged")
                .unwrap();
        }));

        let mut next = Box::pin(values.next());
        assert!(next.as_mut().poll(&mut context).is_pending());
        assert!(actor.lock().unwrap().as_mut().poll(&mut context).is_pending());
        assert_eq!(next.as_mut().poll(&mut context), Poll::Ready(Some(1)));
        drop(next);

        let mut next = std::pin::pin!(values.next());
        assert!(next.as_mut().poll(&mut context).is_pending());
        drop(store);
        assert!(actor.lock().unwrap().as_mut().poll(&mut context).is_ready());
        assert_eq!(next.as_mut().poll(&mut context), Poll::Ready(None));
    }

    #[test]
    fn closure_before_listener_registration_is_not_lost() {
        let runtime = support::ManualRuntime::new();
        let store = Store::new(
            0,
            Reduce::new(|_: &mut usize, (): &(), _| Effect::none()),
            runtime.runtime(),
        );
        let actor = Arc::new(Mutex::new(runtime.take_only_queued_task()));
        let mut context = Context::from_waker(std::task::Waker::noop());
        let mut values = {
            let mut registration = std::pin::pin!(store.observe(|state| *state));
            assert!(registration.as_mut().poll(&mut context).is_pending());
            assert!(actor.lock().unwrap().as_mut().poll(&mut context).is_pending());
            let Poll::Ready(values) = registration.as_mut().poll(&mut context) else {
                panic!("the actor must acknowledge observation registration");
            };
            values.unwrap()
        };
        assert_eq!(values.next().now_or_never(), Some(Some(0)));

        let closed = store.closed();
        values.core.before_listen = Some(Box::new(move || {
            // Deactivation notifies before the next listener exists.
            drop(store);
            let mut context = Context::from_waker(std::task::Waker::noop());
            assert!(actor.lock().unwrap().as_mut().poll(&mut context).is_ready());
        }));

        assert_eq!(values.next().now_or_never(), Some(None));
        assert_eq!(closed.now_or_never(), Some(()));
        assert_eq!(values.next().now_or_never(), Some(None));
    }
}
}
