//! Delivers barrier results after actor publication without blocking the actor.

use std::error::Error;
use std::fmt;
use std::pin::Pin;
use std::task::{Context, Poll};

use async_channel::{Sender, TrySendError};
use rca_core::Effect;

use crate::Failure;

/// Reports the revision captured by a successful barrier.
///
/// This is the document's local revision, not a backend revision identity.
/// Success follows actor publication and carries the backend's save guarantee.
/// It does not acknowledge that all physical backend work has stopped.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FlushOutcome {
    /// Revision captured when the flush or close action committed.
    pub revision: u64,
    /// Whether this receipt belongs to a close request.
    pub closing: bool,
}

/// Describes why a persistence barrier did not succeed.
#[derive(Clone, Debug)]
pub enum ReceiptError {
    /// Loading has not successfully completed.
    NotLoaded,
    /// A flush was requested after close permanently fenced edits.
    Closing,
    /// Encoding or saving failed, retaining the original typed error.
    Persistence(Failure),
    /// The document lifetime was explicitly aborted.
    Aborted,
    /// Every reply owner disappeared without delivering a result.
    Abandoned,
}

impl fmt::Display for ReceiptError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotLoaded => f.write_str("document not loaded"),
            Self::Closing => f.write_str("document closing"),
            Self::Persistence(failure) => failure.fmt(f),
            Self::Aborted => f.write_str("document aborted"),
            Self::Abandoned => f.write_str("persistence acknowledgement abandoned"),
        }
    }
}

impl Error for ReceiptError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Persistence(failure) => Some(failure),
            Self::NotLoaded | Self::Closing | Self::Aborted | Self::Abandoned => None,
        }
    }
}

/// Keeps one barrier's reply channel open until delivery or the last sender's drop.
pub(crate) type Reply = Sender<Result<FlushOutcome, ReceiptError>>;

/// Pairs a retained reply sender with its committed result for later effect delivery.
pub(crate) type Resolution = (Reply, Result<FlushOutcome, ReceiptError>);

/// Awaits a committed barrier's persistence result without owning its cancellation.
///
/// Dropping this future does not cancel a save. Cancellation or shutdown that
/// discards all senders returns [`ReceiptError::Abandoned`], not success. An
/// original retained action delays abandonment until its sender is also dropped.
/// Obtain this future from [`crate::Action::flush`] or [`crate::Action::close`],
/// then submit the paired action to the Store.
#[must_use = "submit the paired action and await the receipt to confirm persistence"]
pub struct Receipt {
    /// Owns the receiver and its pending wait without exposing the channel type.
    future: Pin<Box<dyn Future<Output = Result<FlushOutcome, ReceiptError>> + Send>>,
}

impl fmt::Debug for Receipt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Receipt").finish_non_exhaustive()
    }
}

impl Future for Receipt {
    type Output = Result<FlushOutcome, ReceiptError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.get_mut().future.as_mut().poll(cx)
    }
}

/// Creates one non-blocking reply slot and its independently owned receiving future.
pub(crate) fn channel() -> (Reply, Receipt) {
    // A barrier has exactly one resolution; extra queued replies are a bug.
    let (sender, receiver) = async_channel::bounded(1);
    let receipt = Receipt {
        future: Box::pin(async move {
            receiver
                .recv()
                .await
                .unwrap_or(Err(ReceiptError::Abandoned))
        }),
    };
    (sender, receipt)
}

/// Delivers committed results after publication without waiting for their consumers.
///
/// Dropped receipts are ignored. A second queued result is an internal protocol bug.
pub(crate) fn deliver<Action: Send + 'static>(resolutions: Vec<Resolution>) -> Effect<Action> {
    if resolutions.is_empty() {
        return Effect::none();
    }
    Effect::run(move |_| async move {
        for (reply, result) in resolutions {
            match reply.try_send(result) {
                Ok(()) | Err(TrySendError::Closed(_)) => {}
                Err(TrySendError::Full(_)) => panic!("persistence barrier resolved twice"),
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::task::{Context, Poll, Waker};

    use rca_core::{Reduce, Store};
    use rca_test::DeterministicRuntime;

    use super::{FlushOutcome, ReceiptError, channel, deliver};

    #[test]
    fn duplicate_resolution_panics_and_preserves_first_result() {
        let (reply, receipt) = channel();
        let first = FlushOutcome {
            revision: 7,
            closing: false,
        };
        let mut effect = Some(deliver::<()>(vec![
            (reply.clone(), Ok(first)),
            (reply, Err(ReceiptError::Aborted)),
        ]));
        let runtime = DeterministicRuntime::with_step_budget(64);
        let reducer = Reduce::new(move |(): &mut (), (): &(), _| effect.take().unwrap());
        let store = Store::new((), reducer, runtime.runtime());
        let admission = store.try_send(()).unwrap();

        // Inject duplicate resolution through the real effect runner, without
        // claiming that the public document protocol emits duplicate replies.
        let panic = catch_unwind(AssertUnwindSafe(|| runtime.run_until_stalled()))
            .expect_err("duplicate resolution must panic");
        drop(admission);
        drop(store);
        drop(runtime);

        assert_eq!(
            panic.downcast_ref::<&str>(),
            Some(&"persistence barrier resolved twice")
        );
        let mut receipt = Box::pin(receipt);
        assert!(matches!(
            receipt.as_mut().poll(&mut Context::from_waker(Waker::noop())),
            Poll::Ready(Ok(outcome)) if outcome == first
        ));
    }
}
