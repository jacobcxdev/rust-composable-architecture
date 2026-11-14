//! Defines action commit and causal-work receipts.
//!
//! An [`ActionReceipt`] waits for the store to process an admitted action.
//! The resulting [`ActionTask`] tracks effects and follow-up work from that action.
//! Dropping either handle leaves the work running. [`ActionTask::cancel`] requests
//! cancellation.

use std::fmt;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use crate::StoreStopped;
use crate::effect::lineage::LineageState;

use super::command::{CommitError, CommitReceiver};

/// Future returned by [`ActionReceipt::into_future`].
///
/// Conversion stores the existing commit receiver inline without allocating.
/// Dropping this future does not cancel the admitted action or its causal work.
/// Its result has the same semantics as [`ActionReceipt::finish`].
///
/// # Panics
///
/// Polling this future after it returns [`Poll::Ready`] panics.
#[must_use = "futures do nothing unless polled or awaited"]
pub struct ActionReceiptFuture {
    /// Receives the commit decision until completion, then releases its storage.
    reply: Option<CommitReceiver>,
}

impl Future for ActionReceiptFuture {
    type Output = Result<ActionTask, StoreStopped>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        let reply = this
            .reply
            .as_mut()
            .expect("receipt polled after completion");
        let result = match std::task::ready!(Pin::new(reply).poll(cx)) {
            Ok(Ok(task)) => Ok(task),
            Ok(Err(
                CommitError::Cancelled | CommitError::ScopeEnded | CommitError::InputClosed,
            ))
            | Err(_) => Err(StoreStopped::new()),
        };
        // Release reply storage even if the caller retains the completed future.
        this.reply.take();
        Poll::Ready(result)
    }
}

impl fmt::Debug for ActionReceiptFuture {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ActionReceiptFuture")
            .finish_non_exhaustive()
    }
}

/// Waits for one admitted action to commit.
///
/// Admission means the store accepted the action into its input queue, not that
/// the action has committed. Await this receipt to get an [`ActionTask`] after
/// commit. Await [`ActionTask::finish`] to also wait for the action's effects and
/// follow-up work.
///
/// Dropping this receipt does not cancel the admitted action or its work.
///
/// # Examples
///
/// ```
/// # async fn wait(receipt: rca_core::ActionReceipt) -> Result<(), rca_core::StoreStopped> {
/// let task = receipt.await?;
/// task.finish().await;
/// # Ok(())
/// # }
/// ```
#[must_use = "dropping a receipt does not cancel its admitted action"]
pub struct ActionReceipt {
    /// Receives the store's commit decision without keeping the store alive.
    reply: CommitReceiver,
}

impl ActionReceipt {
    /// Wraps the receiver for an action already admitted to the input queue.
    pub(crate) const fn new(reply: CommitReceiver) -> Self {
        Self { reply }
    }

    /// Waits for the store to process this admitted action.
    ///
    /// When returned by [`crate::ScopedStore::try_send`], admission only means
    /// the command entered the root store's input queue. The store checks the
    /// scope again when it reaches that command in the queue.
    ///
    /// # Errors
    ///
    /// Returns [`StoreStopped`] if the root store stops before returning a commit
    /// result. The same error occurs if a scoped command's scope ends before the
    /// command is processed. That rejection does not mean the root store stopped.
    /// This receipt does not distinguish [`crate::ScopedStoreEnded`] from root
    /// store shutdown.
    pub async fn finish(self) -> Result<ActionTask, StoreStopped> {
        self.into_future().await
    }
}

impl IntoFuture for ActionReceipt {
    type Output = Result<ActionTask, StoreStopped>;
    type IntoFuture = ActionReceiptFuture;

    fn into_future(self) -> Self::IntoFuture {
        ActionReceiptFuture {
            reply: Some(self.reply),
        }
    }
}

impl fmt::Debug for ActionReceipt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ActionReceipt").finish_non_exhaustive()
    }
}

/// Tracks causal work launched by one committed action.
///
/// Causal work includes the action's effects and the follow-up work those effects
/// produce. It does not include work from unrelated actions.
///
/// Clones track the same work. Calling [`Self::cancel`] on any clone requests
/// cancellation for that shared work. Dropping a task does not cancel it.
#[derive(Clone)]
#[must_use = "dropping an action task does not cancel causal work"]
pub struct ActionTask {
    /// Shares tracking only when the action has work that needs it.
    inner: ActionTaskInner,
}

/// Keeps tasks with no tracked work free of heap allocations.
#[derive(Clone)]
enum ActionTaskInner {
    /// No work needs tracking, so completion is immediate and never cancelled.
    Completed,
    /// Shares the completion and cancellation state of one action's work.
    Live(Arc<LineageState>),
}

impl ActionTask {
    /// Creates an already-finished task without allocating tracking state.
    pub(crate) const fn completed() -> Self {
        Self {
            inner: ActionTaskInner::Completed,
        }
    }

    /// Takes a shared reference to the action's existing work tracking.
    pub(crate) fn live(lineage: Arc<LineageState>) -> Self {
        Self {
            inner: ActionTaskInner::Live(lineage),
        }
    }

    /// Requests cancellation of causal work.
    ///
    /// Cancellation is cooperative. This call does not wait for work to stop or
    /// undo actions that have committed. Call [`Self::finish`] to wait for all
    /// tracked work to end. If the work has already finished, this has no effect.
    pub fn cancel(&self) {
        if let ActionTaskInner::Live(lineage) = &self.inner {
            lineage.cancel();
        }
    }

    /// Waits for all causal work to finish.
    ///
    /// This includes work that ends because of cancellation. It does not mean
    /// that every effect completed successfully. Dropping this wait does not
    /// cancel work. Use [`Self::finish_cancelling_on_drop`] for that policy.
    pub async fn finish(&self) {
        if let ActionTaskInner::Live(lineage) = &self.inner {
            lineage.finish().await;
        }
    }

    /// Waits for causal work, cancelling it if the returned future is dropped.
    ///
    /// This opt-in wait owns the task. Dropping even an unpolled wait requests
    /// cancellation of this task's unfinished descendants, not sibling tasks.
    /// A completed wait has the same `()` result as [`Self::finish`]. After a
    /// successful wait, dropping the future does not request cancellation.
    /// Cancellation remains cooperative and does not retract committed actions.
    ///
    /// The policy starts when this method is called on an acquired task. Dropping
    /// a send or receipt future before obtaining its task retains that future's
    /// ordinary admission semantics. It does not install this cancellation guard.
    /// Ordinary task drops and [`Self::finish`] waits remain non-cancelling.
    ///
    /// # Examples
    ///
    /// ```
    /// # async fn wait(task: rca_core::ActionTask) {
    /// task.finish_cancelling_on_drop().await;
    /// # }
    /// ```
    #[must_use = "dropping this wait requests cancellation of unfinished causal work"]
    pub fn finish_cancelling_on_drop(self) -> impl Future<Output = ()> + Send + 'static {
        let guard = CancelOnDrop(self);
        async move {
            guard.0.finish().await;
        }
    }

    /// Returns whether all causal work finished.
    ///
    /// Cancelled work also counts as finished once it ends. A true result remains
    /// true.
    #[must_use]
    pub fn is_finished(&self) -> bool {
        match &self.inner {
            ActionTaskInner::Completed => true,
            ActionTaskInner::Live(lineage) => lineage.is_finished(),
        }
    }

    /// Returns whether this task recorded a cancellation request.
    ///
    /// This does not mean the work has stopped. Use [`Self::finish`] to wait for
    /// completion.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        match &self.inner {
            ActionTaskInner::Completed => false,
            ActionTaskInner::Live(lineage) => lineage.is_cancelled(),
        }
    }
}

/// Requests cancellation when an owned wait ends, including before its first poll.
///
/// Constructed before the async body so that dropping an unpolled future works.
/// After a successful wait, cancellation has no effect on the completed task.
struct CancelOnDrop(ActionTask);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

impl fmt::Debug for ActionTask {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.inner {
            ActionTaskInner::Completed => f.write_str("ActionTask::Completed"),
            ActionTaskInner::Live(_) => f.write_str("ActionTask::Live"),
        }
    }
}

#[cfg(all(test, loom))]
ordinary_test_items! {
/// Checks completion racing with cancellation from an owned wait's drop.
mod cancel_guard_loom_tests {
    use std::sync::{Arc, Weak};
    use std::task::{Context, Poll, Wake, Waker};

    use loom::sync::atomic::{AtomicBool, Ordering};

    use super::{ActionTask, LineageState};

    /// Records whether completion woke the waiter retained by the test.
    struct WakeProbe(AtomicBool);

    impl Wake for WakeProbe {
        fn wake(self: Arc<Self>) {
            self.0.store(true, Ordering::Release);
        }
    }

    #[test]
    /// Both race outcomes must wake the waiter and leave unrelated work alone.
    fn completion_racing_guard_drop_cannot_strand_waiter_or_cancel_sibling() {
        for poll_guard in [false, true] {
            loom::model(move || {
                let selected = LineageState::new(Weak::new());
                let sibling = LineageState::new(Weak::new());
                let task = ActionTask::live(Arc::clone(&selected));
                let mut guard = Box::pin(task.clone().finish_cancelling_on_drop());
                if poll_guard {
                    assert!(
                        guard
                            .as_mut()
                            .poll(&mut Context::from_waker(Waker::noop()))
                            .is_pending()
                    );
                }

                let probe = Arc::new(WakeProbe(AtomicBool::new(false)));
                let waker = Waker::from(Arc::clone(&probe));
                let mut waiter = std::pin::pin!(task.finish());
                assert!(
                    waiter
                        .as_mut()
                        .poll(&mut Context::from_waker(&waker))
                        .is_pending()
                );

                let completing = Arc::clone(&selected);
                let completion = loom::thread::spawn(move || completing.finish_registration());
                let cancellation = loom::thread::spawn(move || drop(guard));
                completion.join().expect("selected work completes once");
                cancellation.join().expect("guard drops once");

                // Either cancellation is observed or completion already won;
                // both orders settle the same causal work without double exit.
                assert!(task.is_finished());
                assert!(
                    probe.0.load(Ordering::Acquire),
                    "remaining waiter must wake"
                );
                assert_eq!(
                    waiter.as_mut().poll(&mut Context::from_waker(&waker)),
                    Poll::Ready(())
                );
                assert!(!sibling.is_finished());
                assert!(!sibling.is_cancelled());
                sibling.finish_registration();
            });
        }
    }
}
}
