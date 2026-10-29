//! Owns latest-state predicate waits and their optional initiating causal work.
//!
//! Shared and local wrappers use one completion state machine. Explicit
//! cancellation also reaches action work, including work installed after the
//! request. Ordinary destruction only releases the wait's owned resources.

use std::fmt;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use super::{ObservationCore, ObservationFamily};
use crate::{ActionTask, Observation};

#[cfg(all(test, feature = "local"))]
ordinary_test_items! {
mod tests;
}

/// Identifies why an owned predicate wait completed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PredicateWaitOutcome {
    /// A consumed current or subsequent selection was false.
    Satisfied,
    /// Explicit cancellation ended the wait.
    Cancelled,
    /// Root Store authority ended.
    StoreStopped,
    /// The selected child lifetime ended while its root remained open.
    ScopeEnded,
}

/// Waits until a current or subsequent predicate selection is false.
///
/// Selections have latest-state semantics: a false value followed by true before
/// consumption can be coalesced away. This is not a transition latch. Success
/// retires observation but does not wait for or cancel remaining causal work.
/// Before accepting false, the wait checks its root and child lifetime. An
/// already-observable ending takes precedence over a buffered value. An ending
/// racing after that check has no promised ordering; observed outcomes are sticky.
///
/// Ordinary drop releases observation without cancelling an admitted action or
/// its causal work. Use [`Self::cancel`] for explicit cancellation. Neither form
/// retracts a committed action. The originating Store is retained only through
/// send/registration, not while waiting for later selections.
///
/// # Panics
///
/// Polling propagates observation, predicate and resource-destructor panics.
/// Resuming an inner future whose poll panicked is not supported.
#[must_use = "predicate waits do nothing until polled"]
pub struct PredicateWait {
    /// Owns a thread-movable future and its optional action-cancellation authority.
    core: WaitCore<Pin<Box<dyn Future<Output = PredicateWaitOutcome> + Send>>>,
}

impl PredicateWait {
    /// Erases owned future storage without starting observation or action admission.
    pub(crate) fn new(
        future: impl Future<Output = PredicateWaitOutcome> + Send + 'static,
        cancellation: Option<Arc<WaitCancellation>>,
    ) -> Self {
        Self {
            core: WaitCore::new(Box::pin(future), cancellation),
        }
    }

    /// Cancels this wait and requests cancellation of its initiating causal work.
    ///
    /// Cancelling before first poll prevents registration and action admission.
    /// Once admitted, cancellation suppresses reduction if the actor has not
    /// started it; otherwise the committed action remains and only its causal
    /// descendants receive cooperative cancellation. Sibling tasks and root
    /// authority are unaffected. Runtime progress may be needed to retire tasks.
    ///
    /// The next poll returns [`PredicateWaitOutcome::Cancelled`]. Repeated calls
    /// do nothing, including calls after any outcome has been observed. An
    /// unconsumed ready value or receipt does not override explicit cancellation.
    ///
    /// # Panics
    ///
    /// Propagates wake-callback and resource-destructor panics after recording
    /// cancellation. A wake-callback panic still drops the owned future.
    pub fn cancel(&mut self) {
        self.core.cancel();
    }
}

impl Future for PredicateWait {
    type Output = PredicateWaitOutcome;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        Pin::new(&mut self.get_mut().core).poll(cx)
    }
}

impl fmt::Debug for PredicateWait {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.core.fmt(f)
    }
}

/// Shares sticky completion and teardown ordering without imposing `Send` on local waits.
struct WaitCore<F> {
    /// Owns the active operation until cancellation or an accepted outcome.
    future: Option<F>,
    /// Controls only the initiating action, not the whole Store or sibling work.
    cancellation: Option<Arc<WaitCancellation>>,
    /// Records completion before teardown can invoke callbacks or destructors.
    outcome: Option<PredicateWaitOutcome>,
}

impl<F> WaitCore<F> {
    /// Creates an inactive wait without polling or cancelling its future.
    fn new(future: F, cancellation: Option<Arc<WaitCancellation>>) -> Self {
        Self {
            future: Some(future),
            cancellation,
            outcome: None,
        }
    }

    /// Records cancellation, requests causal cancellation, then retires the future.
    fn cancel(&mut self) {
        if self.outcome.is_some() {
            return;
        }
        self.outcome = Some(PredicateWaitOutcome::Cancelled);
        let future = self.future.take();
        if let Some(cancellation) = self.cancellation.take() {
            cancellation.cancel();
        }
        drop(future);
    }
}

impl<F: Future<Output = PredicateWaitOutcome> + Unpin> Future for WaitCore<F> {
    type Output = PredicateWaitOutcome;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        if let Some(outcome) = this.outcome {
            return Poll::Ready(outcome);
        }
        let outcome = std::task::ready!(
            Pin::new(
                this.future
                    .as_mut()
                    .expect("pending predicate wait owns its future")
            )
            .poll(cx)
        );
        this.outcome = Some(outcome);
        let future = this.future.take();
        let cancellation = this.cancellation.take();
        drop(future);
        drop(cancellation);
        Poll::Ready(outcome)
    }
}

impl<F> fmt::Debug for WaitCore<F> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PredicateWait")
            .field("outcome", &self.outcome)
            .finish_non_exhaustive()
    }
}

/// Checks authority at the point where a shared wait accepts a terminal result.
pub(crate) type WaitTerminal = Box<dyn Fn() -> Option<PredicateWaitOutcome> + Send>;

/// Transfers a shared observation into the common latest-state predicate loop.
pub(crate) fn wait_for_false(
    observation: Observation<bool>,
    terminal: WaitTerminal,
) -> impl Future<Output = PredicateWaitOutcome> {
    wait_for_false_core(observation.core, terminal)
}

/// Consumes latest selections, giving already-observable authority closure precedence.
///
/// A true selection continues waiting. Closure without a published lifecycle
/// reason still ends the wait, because observation shutdown can arrive first.
pub(super) async fn wait_for_false_core<Family: ObservationFamily>(
    mut observation: ObservationCore<bool, Family>,
    terminal: impl Fn() -> Option<PredicateWaitOutcome>,
) -> PredicateWaitOutcome {
    while let Some(value) = observation.next().await {
        if !value {
            return terminal().unwrap_or(PredicateWaitOutcome::Satisfied);
        }
    }
    // Observation closure may precede publication of the root lifecycle flag.
    terminal().unwrap_or(PredicateWaitOutcome::StoreStopped)
}

#[cfg(feature = "local")]
pub(super) mod local {
    //! Retains local captures while sharing predicate and cancellation semantics.

    use super::{Arc, Context, Pin, Poll, PredicateWaitOutcome, WaitCancellation, WaitCore, fmt};

    /// Owns an owner-thread predicate wait with latest-state semantics.
    ///
    /// Uses the same terminal precedence, coalescing and cancellation contract
    /// as [`crate::PredicateWait`]. Future and terminal captures may contain
    /// thread-local data. The wait cannot move to another thread, even when its
    /// captures happen to be `Send`. Ordinary drop releases observation without
    /// cancelling the initiating action's causal work.
    #[must_use = "predicate waits do nothing until polled"]
    pub struct PredicateWait {
        /// Keeps erased local captures on their owner thread.
        core: WaitCore<Pin<Box<dyn Future<Output = PredicateWaitOutcome>>>>,
    }

    impl PredicateWait {
        /// Erases a local future without polling or adding thread-safety bounds.
        pub(crate) fn new(
            future: impl Future<Output = PredicateWaitOutcome> + 'static,
            cancellation: Option<Arc<WaitCancellation>>,
        ) -> Self {
            Self {
                core: WaitCore::new(Box::pin(future), cancellation),
            }
        }

        /// Cancels the wait and requests cancellation of its initiating causal work.
        ///
        /// Before first poll this prevents registration and action admission.
        /// Later cancellation does not retract committed actions. Repeated calls
        /// and calls after a completed outcome do nothing. See
        /// [`crate::PredicateWait::cancel`] for the shared cancellation contract.
        ///
        /// # Panics
        ///
        /// Propagates wake-callback and resource-destructor panics after recording
        /// cancellation. A wake-callback panic still drops the owned future.
        pub fn cancel(&mut self) {
            self.core.cancel();
        }
    }

    impl Future for PredicateWait {
        type Output = PredicateWaitOutcome;

        fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
            Pin::new(&mut self.get_mut().core).poll(cx)
        }
    }

    impl fmt::Debug for PredicateWait {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            self.core.fmt(f)
        }
    }
}

/// Serialises explicit cancellation with installation of the initiating action's task.
#[derive(Default)]
pub(crate) struct WaitCancellation {
    /// Protects authority transfer only. Task cancellation runs after this lock is released.
    state: Mutex<CancellationState>,
}

/// Remembers cancellation even when the actor has not returned its task yet.
#[derive(Default)]
struct CancellationState {
    /// Prevents any later installation from escaping the cancellation request.
    cancelled: bool,
    /// Holds the current task until cancellation, replacement or ordinary destruction.
    task: Option<ActionTask>,
}

impl WaitCancellation {
    /// Reports whether explicit cancellation preceded the actor's admission check.
    pub(crate) fn is_cancelled(&self) -> bool {
        self.state.lock().expect("wait cancellation lock").cancelled
    }

    /// Takes task authority under the lock, then invokes cancellation outside it.
    fn cancel(&self) {
        let task = {
            let mut state = self.state.lock().expect("wait cancellation lock");
            state.cancelled = true;
            state.task.take()
        };
        // Cancellation may wake the actor; never call it under the mutex.
        if let Some(task) = task {
            task.cancel();
        }
    }

    /// Retains an admitted task or cancels it if cancellation already won the race.
    ///
    /// Replacing a retained task cancels the displaced task outside the lock.
    pub(crate) fn install(&self, task: ActionTask) {
        let displaced = {
            let mut state = self.state.lock().expect("wait cancellation lock");
            if state.cancelled {
                Some(task)
            } else {
                state.task.replace(task)
            }
        };
        if let Some(task) = displaced {
            task.cancel();
        }
    }
}
