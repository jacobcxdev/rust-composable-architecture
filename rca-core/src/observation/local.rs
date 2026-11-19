//! Delivers owner-thread Store projections through the common observation engine.

use std::fmt;
use std::pin::Pin;
use std::sync::Arc;

use super::{
    ObservationCore, ObservationFamily, ObservationId, ObservationSignal, TakeOperation,
    TakeOutcome,
};
use crate::StoreStopped;
use crate::store::Local;

#[doc(inline)]
pub use super::wait::local::PredicateWait;

/// Applies the common predicate loop without imposing `Send` on local values.
pub(crate) fn wait_for_false(
    observation: Observation<bool>,
    terminal: impl Fn() -> Option<super::PredicateWaitOutcome>,
) -> impl Future<Output = super::PredicateWaitOutcome> {
    super::wait::wait_for_false_core(observation.core, terminal)
}

impl ObservationFamily for Local {
    type Operation<Value> = dyn TakeOperation<Value, Self>;
    type Deferred<Value> =
        Pin<Box<dyn Future<Output = Result<TakeOutcome<Value, Self>, StoreStopped>>>>;
}

/// Delivers latest projected values on their owning thread without cloning them.
///
/// Values, selectors and callbacks can hold thread-local data. This handle and
/// its futures cannot move to another thread, even when `Value` is `Send`.
/// Publication notifications contain no local payload and can wake the owner
/// from another thread. Store construction supplies the owner-thread executor.
///
/// Initial delivery, coalescing and closure follow [`super::Observation`].
/// Dropping this handle cancels registration without waiting for actor cleanup.
pub struct Observation<Value> {
    /// Keeps local operation and future bounds in the common polling protocol.
    core: ObservationCore<Value, Local>,
}

impl<Value> Observation<Value> {
    /// Owns the initial selection and reusable command on the actor's thread.
    pub(crate) fn new(
        id: ObservationId,
        signal: Arc<ObservationSignal>,
        initial: Value,
        delivered_version: u64,
        take_operation: Box<dyn TakeOperation<Value, Local>>,
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
    /// listener can retain the last task waker until another poll, publication,
    /// closure, or handle drop.
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

    /// Invokes one callback with the next owned post-commit publication.
    ///
    /// Discards an unread initial value and waits without spawning a task.
    /// Unread commits coalesce. Admission deactivates the registration and drops
    /// this owner before calling `callback`. Dropping the future cancels the
    /// registration. Returns `false` if authority closes before admission.
    ///
    /// # Panics
    ///
    /// Propagates callback, task-waker and value-destructor panics. Discarding the
    /// initial value precedes normal cancellation. If that destructor panics,
    /// unwinding drops this owner and requests registration cleanup instead.
    pub fn on_change(self, callback: impl FnOnce(Value)) -> impl Future<Output = bool> {
        self.core.on_change(callback)
    }
}

impl<Value> Unpin for Observation<Value> {}

impl<Value> fmt::Debug for Observation<Value> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.core.fmt(f)
    }
}

#[cfg(feature = "futures-stream")]
impl<Value: 'static> futures_core::Stream for Observation<Value> {
    type Item = Value;

    fn poll_next(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        self.get_mut().core.poll_next_value(cx)
    }
}
