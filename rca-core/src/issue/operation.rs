//! Reports caller-local results through an explicitly selected context.

use std::cell::Cell;
use std::fmt;
use std::panic::Location;

use super::Issue;

/// Describes rejection by an explicit diagnostic callback.
#[derive(Debug)]
pub enum IssueDeliveryFailure<Error> {
    /// The selected endpoint is disabled.
    Disabled,
    /// The selected endpoint has closed.
    Closed,
    /// The callback failed with its original typed delivery error.
    Failed(Error),
}

impl<Error> IssueDeliveryFailure<Error> {
    /// Attaches recovery evidence without changing the callback's typed error.
    fn into_delivery(self, issue: Issue) -> IssueDelivery<Error> {
        match self {
            Self::Disabled => IssueDelivery::Disabled(issue),
            Self::Closed => IssueDelivery::Closed(issue),
            Self::Failed(error) => IssueDelivery::Failed { issue, error },
        }
    }
}

/// Retains diagnostic delivery independently of the operation result.
#[derive(Debug)]
pub enum IssueDelivery<Error> {
    /// The callback accepted the diagnostic, not an assertion of failure.
    Accepted,
    /// No callback was selected, or the selected endpoint was disabled.
    Disabled(Issue),
    /// The selected endpoint had closed; the diagnostic remains recoverable.
    Closed(Issue),
    /// A callback failed; its diagnostic and typed error remain recoverable.
    Failed {
        /// Original diagnostic evidence.
        issue: Issue,
        /// Original callback delivery error.
        error: Error,
    },
    /// This context was already delivering; the nested diagnostic was rejected.
    Reentrant(Issue),
}

/// Preserves a typed result alongside independent diagnostic delivery evidence.
#[derive(Debug)]
pub struct OperationReport<Value, Error, DeliveryError> {
    /// Original operation result, including successful `None` and cancellation errors.
    pub result: Result<Value, Error>,
    /// Delivery outcomes in callback order; empty for success or explicit cancellation.
    pub deliveries: Vec<IssueDelivery<DeliveryError>>,
}

/// Borrows a diagnostic and its original typed operation error.
pub type IssueCallback<'a, Error, DeliveryError> =
    dyn Fn(&Issue, &Error) -> Result<(), IssueDeliveryFailure<DeliveryError>> + 'a;

/// Selects caller-local callbacks without ambient state or executor authority.
///
/// Callbacks may borrow non-`Send`, non-`'static` data, including the operation
/// error. This context is deliberately local rather than actor transport.
/// An empty context returns disabled delivery evidence instead of discarding it.
/// Reentry into the same context is rejected; independent contexts remain usable.
///
/// # Examples
///
/// ```
/// use std::cell::Cell;
/// use std::convert::Infallible;
/// use rca_core::{IssueContext, IssueDelivery, IssueDeliveryFailure};
/// use rca_core::Issue;
///
/// let reports = Cell::new(0);
/// let callback = |issue: &Issue, cause: &u8| {
///     assert_eq!(*cause, 7);
///     assert_eq!(issue.cause.as_deref(), Some("invalid input"));
///     reports.set(reports.get() + 1);
///     Ok::<_, IssueDeliveryFailure<Infallible>>(())
/// };
/// let reporters = [&callback as &_];
/// let context = IssueContext::new(&reporters);
/// let outcome = context.report_operation(
///     None, // Inherit this explicit context, never an ambient reporter.
///     || Err::<(), _>(7u8),
///     |cause| *cause == 0, // Caller-defined cancellation policy.
///     |_| Issue::new("parse", "could not parse").with_cause("invalid input"),
/// );
/// assert_eq!(outcome.result, Err(7));
/// assert!(matches!(outcome.deliveries.as_slice(), [IssueDelivery::Accepted]));
/// assert_eq!(reports.get(), 1);
/// ```
///
/// A callback can bridge an existing endpoint without changing its admission
/// policy. Rejection is returned separately; the context retains the diagnostic.
///
/// ```
/// use std::convert::Infallible;
/// use rca_core::{IssueContext, IssueDelivery, IssueDeliveryFailure};
/// use rca_core::{Issue, IssueReporter};
///
/// let endpoint = IssueReporter::default();
/// let callback = |issue: &Issue, _: &u8| {
///     endpoint.report(issue.clone()).map_err(|error| {
///         if error.is_disabled() {
///             IssueDeliveryFailure::<Infallible>::Disabled
///         } else {
///             IssueDeliveryFailure::Closed
///         }
///     })
/// };
/// let reporters = [&callback as &_];
/// let context = IssueContext::new(&reporters);
/// let empty = IssueContext::new(&[]);
/// let outcome = context.report_operation(Some(&empty), || Err::<(), _>(3u8),
///     |_| false, |_| Issue::new("required", "recover this evidence"));
/// assert_eq!(outcome.result, Err(3));
/// assert!(matches!(outcome.deliveries.as_slice(), [IssueDelivery::Disabled(_)]));
/// ```
pub struct IssueContext<'a, Error, DeliveryError> {
    /// Callback references in invocation order, including deliberate duplicates.
    reporters: &'a [&'a IssueCallback<'a, Error, DeliveryError>],
    /// True only while this context owns an active delivery guard.
    reporting: Cell<bool>,
}

impl<Error, DeliveryError> fmt::Debug for IssueContext<'_, Error, DeliveryError> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IssueContext")
            .field("reporter_count", &self.reporters.len())
            .field("reporting", &self.reporting.get())
            .finish()
    }
}

/// Clears only the outer delivery's marker, including during callback unwinding.
///
/// Reentrant calls return before creating a guard, so they cannot clear it early.
struct ReportingGuard<'a>(&'a Cell<bool>);

impl Drop for ReportingGuard<'_> {
    fn drop(&mut self) {
        self.0.set(false);
    }
}

impl<'a, Error, DeliveryError> IssueContext<'a, Error, DeliveryError> {
    /// Borrows an explicitly ordered set of diagnostic callbacks.
    ///
    /// Construction allocates nothing. Duplicate callbacks run separately.
    #[must_use]
    pub const fn new(reporters: &'a [&'a IssueCallback<'a, Error, DeliveryError>]) -> Self {
        Self {
            reporters,
            reporting: Cell::new(false),
        }
    }

    /// Delivers a diagnostic with a borrowed original cause, retaining rejection evidence.
    ///
    /// No mutex or mutable borrow is held across callbacks. A context-local
    /// activity marker rejects recursive delivery and resets during unwinding.
    /// Existing [`super::IssueReporter`] reentrancy policy is unchanged.
    /// Callbacks run in order, including after a delivery rejection.
    ///
    /// The outcome vector allocates once. Rejections before the final callback
    /// clone the issue, including its owned strings. A final rejection moves the
    /// original issue instead. Accepted callbacks require no issue clone.
    ///
    /// # Panics
    /// Propagates callback panics. Later callbacks do not run, and no completed
    /// delivery list is returned. Earlier callback effects are not undone.
    #[must_use]
    pub fn report(&self, issue: Issue, cause: &Error) -> Vec<IssueDelivery<DeliveryError>> {
        if self.reporting.replace(true) {
            return vec![IssueDelivery::Reentrant(issue)];
        }
        let _guard = ReportingGuard(&self.reporting);
        let Some((last, prefix)) = self.reporters.split_last() else {
            return vec![IssueDelivery::Disabled(issue)];
        };
        let mut deliveries = Vec::with_capacity(self.reporters.len());
        for reporter in prefix {
            deliveries.push(match reporter(&issue, cause) {
                Ok(()) => IssueDelivery::Accepted,
                Err(failure) => failure.into_delivery(issue.clone()),
            });
        }
        deliveries.push(match last(&issue, cause) {
            Ok(()) => IssueDelivery::Accepted,
            Err(failure) => failure.into_delivery(issue),
        });
        deliveries
    }

    /// Runs synchronous caller work and reports only non-cancellation errors.
    ///
    /// `supplied` replaces this context when present, even if explicitly empty.
    /// `None` inherits this context. The lazy `issue` factory sees the borrowed
    /// original error exactly once for ordinary errors and never for success or
    /// explicit cancellation. Its diagnostic is located at this method's caller.
    /// Neither diagnostic rejection nor acceptance changes the original result.
    ///
    /// # Panics
    /// Propagates operation, cancellation predicate, issue factory and callback panics.
    #[track_caller]
    #[must_use]
    pub fn report_operation<Value>(
        &self,
        supplied: Option<&Self>,
        operation: impl FnOnce() -> Result<Value, Error>,
        cancelled: impl FnOnce(&Error) -> bool,
        issue: impl FnOnce(&Error) -> Issue,
    ) -> OperationReport<Value, Error, DeliveryError> {
        supplied
            .unwrap_or(self)
            .report_result(operation(), cancelled, issue, Location::caller())
    }

    /// Borrows caller-polled work without spawning or adding transport bounds.
    ///
    /// Selection, cancellation and lazy diagnostic rules match [`Self::report_operation`].
    /// The future does no operation or reporting work until polled. Dropping it
    /// while pending reports no completed error. Its location is captured here,
    /// not at the eventual poll. No `Clone`, `Send` or `'static` bound is required
    /// on the operation, result, error, predicate, factory or callback.
    ///
    /// # Panics
    /// Polling propagates operation, predicate, factory and callback panics.
    #[track_caller]
    pub fn report_future<'context, Value, Fut, Cancel, MakeIssue>(
        &'context self,
        supplied: Option<&'context Self>,
        operation: Fut,
        cancelled: Cancel,
        issue: MakeIssue,
    ) -> impl Future<Output = OperationReport<Value, Error, DeliveryError>>
    + use<'a, 'context, Value, Error, DeliveryError, Fut, Cancel, MakeIssue>
    where
        Fut: Future<Output = Result<Value, Error>>,
        Cancel: FnOnce(&Error) -> bool,
        MakeIssue: FnOnce(&Error) -> Issue,
    {
        let location = Location::caller();
        let selected = supplied.unwrap_or(self);
        async move { selected.report_result(operation.await, cancelled, issue, location) }
    }

    /// Classifies one completed result without consuming its typed error.
    ///
    /// Success and explicit cancellation bypass diagnostic creation. Other errors
    /// receive the public call site's location before synchronous delivery.
    fn report_result<Value>(
        &self,
        result: Result<Value, Error>,
        cancelled: impl FnOnce(&Error) -> bool,
        issue: impl FnOnce(&Error) -> Issue,
        location: &'static Location<'static>,
    ) -> OperationReport<Value, Error, DeliveryError> {
        let deliveries = match &result {
            Err(error) if !cancelled(error) => {
                let mut issue = issue(error);
                issue.location = location;
                self.report(issue, error)
            }
            _ => Vec::new(),
        };
        OperationReport { result, deliveries }
    }
}
