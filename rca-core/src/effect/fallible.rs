//! Routes owned fallible work through explicit handlers or retained reporting outcomes.

use std::convert::Infallible;
use std::fmt;
use std::panic::Location;

use futures_util::future::AbortHandle;

use super::{Effect, EffectSender, Program};
use crate::issue::{IssueDelivery, OperationReport};
use crate::store::command::Provenance;
use crate::{Issue, IssueReporter, RuntimeFuture};

/// Defers fallible work until the task supplies causal and cancellation authority.
///
/// The outer box erases closure types. The returned future erases operation and handler types.
pub(crate) type FallibleRun<Action> = Box<
    dyn FnOnce(EffectSender<Action>, Provenance, AbortHandle) -> RuntimeFuture + Send + 'static,
>;

/// Distinguishes explicit cancellation from an operation's original typed failure.
///
/// Cancellation is a routing outcome, not a request to cancel sibling work.
/// Neither variant requires the cause to implement `Clone`, `Debug` or `Send`.
pub enum EffectError<Error> {
    /// Completes without invoking an error handler or reporting a failure.
    Cancelled,
    /// Routes this original cause if the effect still owns live authority.
    Failed(Error),
}

impl<Error> fmt::Debug for EffectError<Error> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => f.write_str("EffectError::Cancelled"),
            Self::Failed(_) => f.write_str("EffectError::Failed(..)"),
        }
    }
}

impl<Action: 'static> Effect<Action> {
    /// Runs owned fallible work with an explicit asynchronous error handler.
    ///
    /// Construction defers operation execution until launch, just like [`Self::run`].
    /// Success and [`EffectError::Cancelled`] invoke no handler. For
    /// [`EffectError::Failed`], **live-at-result** routing checks the existing
    /// task, runtime abort, lineage and cancellation-generation authority after the operation
    /// resolves, before entering the handler. Structural removal and Store shutdown
    /// invalidate that task authority. A failure observed after invalidation is dropped.
    ///
    /// Cancellation racing after this guard does not retract handler entry. The
    /// handler's sender retains the same authority, so later cancellation still
    /// rejects stale actions at delivery. Pending operation and handler futures
    /// remain owned by the task; cancellation drops them without reporting a
    /// completed failure. Cancellation cannot pre-empt a non-yielding poll.
    ///
    /// Only transported captures and futures require `Send + 'static`. A cause
    /// created and consumed within one poll needs no `Send`, `Clone` or `Debug`.
    /// The action domain itself need not be `Send` during construction; mapping
    /// can convert owned local actions before shared Store admission.
    /// It remains `'static` because the erased runtime future retains its typed
    /// sender while deferring operation entry until polling.
    /// No optional issue reporter is invoked for a handled failure.
    ///
    /// # Examples
    ///
    /// ```
    /// use rca_core::{Effect, EffectError};
    /// let effect: Effect<u8> = Effect::try_run(
    ///     |_| async { Err(EffectError::Failed(7u8)) },
    ///     |cause, sender| async move { let _ = sender.send(cause).await; },
    /// );
    /// ```
    ///
    /// # Panics
    /// Polling propagates operation and handler panics.
    pub fn try_run<Error, Operation, OperationFuture, Handler, HandlerFuture>(
        operation: Operation,
        handler: Handler,
    ) -> Self
    where
        Operation: FnOnce(EffectSender<Action>) -> OperationFuture + Send + 'static,
        OperationFuture: Future<Output = Result<(), EffectError<Error>>> + Send + 'static,
        Handler: FnOnce(Error, EffectSender<Action>) -> HandlerFuture + Send + 'static,
        HandlerFuture: Future<Output = ()> + Send + 'static,
    {
        Self {
            program: Program::<Action>::FallibleRun(Box::new(
                move |sender, provenance, cancellation| {
                    Box::pin(async move {
                        let next = handle_result(
                            operation(sender.clone()).await,
                            sender,
                            &provenance,
                            &cancellation,
                            handler,
                        );
                        drop(provenance);
                        drop(cancellation);
                        if let Some(next) = next {
                            next.await;
                        }
                    })
                },
            )),
            scope: None,
        }
    }

    /// Reports unhandled failures and hands their typed evidence to a required consumer.
    ///
    /// Uses the same **live-at-result** guard and cancellation policy as
    /// [`Self::try_run`]. Only an ordinary, still-live failure invokes `issue`,
    /// diagnostic delivery and `outcome`, in that order. The lazy issue factory
    /// borrows the original cause. Its location is replaced with this constructor's
    /// caller location, captured before launch or polling.
    ///
    /// `outcome` always receives the original typed cause and independent diagnostic
    /// delivery evidence for this branch. A disabled or closed `reporter` cannot
    /// suppress it: [`crate::IssueDelivery`] retains the rejected issue.
    /// The explicit consumer is responsible for retaining or acting on the report;
    /// it is synchronous and is not optional. No ambient reporter is consulted.
    /// A cause needs no formatting traits; supply any safe cause representation
    /// through the issue factory. Neither cause nor result is cloned.
    ///
    /// # Examples
    ///
    /// ```
    /// use rca_core::{Effect, EffectError, Issue, IssueReporter};
    /// let effect: Effect<()> = Effect::try_run_reported(
    ///     |_| async { Err(EffectError::Failed(7u8)) },
    ///     IssueReporter::default(),
    ///     |_| Issue::new("load.failed", "loading failed"),
    ///     |report| {
    ///         assert_eq!(report.result, Err(7));
    ///         assert_eq!(report.deliveries.len(), 1);
    ///     },
    /// );
    /// ```
    ///
    /// # Panics
    /// Polling propagates operation, factory, reporter and outcome callback panics.
    /// A panicking reporter prevents the outcome callback from running.
    #[track_caller]
    pub fn try_run_reported<Error, Operation, OperationFuture, MakeIssue, Outcome>(
        operation: Operation,
        reporter: IssueReporter,
        issue: MakeIssue,
        outcome: Outcome,
    ) -> Self
    where
        Operation: FnOnce(EffectSender<Action>) -> OperationFuture + Send + 'static,
        OperationFuture: Future<Output = Result<(), EffectError<Error>>> + Send + 'static,
        MakeIssue: FnOnce(&Error) -> Issue + Send + 'static,
        Outcome: FnOnce(OperationReport<(), Error, Infallible>) + Send + 'static,
    {
        let location = Location::caller();
        Self {
            program: Program::<Action>::FallibleRun(Box::new(
                move |sender, provenance, cancellation| {
                    Box::pin(async move {
                        report_result(
                            operation(sender).await,
                            &provenance,
                            &cancellation,
                            &reporter,
                            location,
                            issue,
                            outcome,
                        );
                    })
                },
            )),
            scope: None,
        }
    }
}

/// Reports one live failure and passes its owned evidence to the consumer.
///
/// This synchronous boundary consumes the cause before the shared future can yield.
/// Local effects use the same helper without adding a `Send` bound.
pub(crate) fn report_result<Error>(
    result: Result<(), EffectError<Error>>,
    provenance: &Provenance,
    cancellation: &AbortHandle,
    reporter: &IssueReporter,
    location: &'static Location<'static>,
    issue: impl FnOnce(&Error) -> Issue,
    outcome: impl FnOnce(OperationReport<(), Error, Infallible>),
) {
    let Err(EffectError::Failed(error)) = result else {
        return;
    };
    if cancellation.is_aborted() || !provenance.is_live() {
        return;
    }
    let mut diagnostic = issue(&error);
    diagnostic.location = location;
    // A single endpoint returns the rejected issue, so no diagnostic clone is needed.
    // Test recording can still clone its own admitted copy inside the reporter.
    let delivery = match reporter.report(diagnostic) {
        Ok(()) => IssueDelivery::Accepted,
        Err(failure) if failure.is_disabled() => IssueDelivery::Disabled(failure.into_issue()),
        Err(failure) => IssueDelivery::Closed(failure.into_issue()),
    };
    outcome(OperationReport {
        result: Err(error),
        deliveries: vec![delivery],
    });
}

/// Starts the handler only for a live, ordinary failure.
///
/// The synchronous call lets the handler consume a non-`Send` cause before an await.
/// The returned future owns whatever the handler retains.
pub(crate) fn handle_result<Sender, Error, HandlerFuture>(
    result: Result<(), EffectError<Error>>,
    sender: Sender,
    provenance: &Provenance,
    cancellation: &AbortHandle,
    handler: impl FnOnce(Error, Sender) -> HandlerFuture,
) -> Option<HandlerFuture> {
    match result {
        Err(EffectError::Failed(error)) if !cancellation.is_aborted() && provenance.is_live() => {
            Some(handler(error, sender))
        }
        _ => None,
    }
}
