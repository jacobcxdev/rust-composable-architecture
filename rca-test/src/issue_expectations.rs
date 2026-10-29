//! Checks explicitly routed issue collections without changing `TestStore` FIFO.
//!
//! Each scope owns its collector and seals it before predicate evaluation.
//! Body results and issue expectations remain separate, mandatory outcomes.
//! The async variant is caller-polled and installs no executor or ambient reporter.

use std::backtrace::Backtrace;
use std::fmt;
use std::panic::Location;

use rca_core::{Issue, IssueCollector, IssueReporter};

/// Selects the explicit known-issue allowance policy.
///
/// Every mode runs the body once when the operation runs, and checks every captured
/// issue with the supplied predicate. Modes do not enable ambient diagnostic sinks.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IssueExpectationMode {
    /// Requires at least one matching issue and no unexpected issues.
    Strict,
    /// Allows zero reports but no unexpected issues.
    Intermittent,
    /// Disables known-issue allowance: only zero reports pass.
    Disabled,
}

/// Keeps the original body result separate from expectation failure.
///
/// An operation error neither satisfies nor suppresses an issue expectation.
/// Conversely, an expectation failure does not replace the original typed error.
#[must_use = "inspect both the body result and issue expectation"]
#[derive(Debug)]
pub struct IssueExpectation<R, E> {
    /// Original operation result.
    pub body_result: Result<R, E>,
    /// Independent diagnostic expectation result.
    pub expectation: Result<Vec<Issue>, IssueExpectationError>,
}

/// Retains missing and unexpected issue evidence.
///
/// Unexpected issues preserve their original source location, severity and cause.
/// This error describes required test evidence, not failure to deliver a diagnostic
/// to another reporter. It must be checked even if that reporter is disabled.
#[derive(Debug)]
pub struct IssueExpectationError {
    /// A strict expectation received no matching issue.
    missing: bool,
    /// Rejected issues in their original admission order.
    unexpected: Vec<Issue>,
    /// Public helper call site, captured before any asynchronous polling.
    origin: &'static Location<'static>,
    /// Failure trace captured under the standard environment policy.
    backtrace: Backtrace,
}

impl IssueExpectationError {
    /// Returns whether a required matching report was missing.
    #[must_use]
    pub const fn is_missing(&self) -> bool {
        self.missing
    }

    /// Returns unexpected reports in admission order.
    #[must_use]
    pub fn unexpected_issues(&self) -> &[Issue] {
        &self.unexpected
    }

    /// Returns the expectation call site.
    #[must_use]
    pub const fn location(&self) -> &'static Location<'static> {
        self.origin
    }

    /// Returns the failure backtrace captured under standard environment policy.
    pub const fn backtrace(&self) -> &Backtrace {
        &self.backtrace
    }
}

impl fmt::Display for IssueExpectationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "issue expectation failed at {}: missing required match={}, unexpected issues={:?}\n{}",
            self.origin, self.missing, self.unexpected, self.backtrace
        )
    }
}
impl std::error::Error for IssueExpectationError {}

/// Checks each sealed issue once and preserves full reports only on success.
///
/// Failure moves unexpected reports without cloning their owned text.
/// Predicates run outside collector locks, including in Disabled mode.
fn check(
    mut issues: Vec<Issue>,
    mode: IssueExpectationMode,
    matches: &mut impl FnMut(&Issue) -> bool,
    origin: &'static Location<'static>,
) -> Result<Vec<Issue>, IssueExpectationError> {
    let mut found = false;
    let unexpected: Vec<_> = issues
        .extract_if(.., |issue| {
            let known = matches(issue);
            found |= known;
            !known || mode == IssueExpectationMode::Disabled
        })
        .collect();
    let missing = mode == IssueExpectationMode::Strict && !found;
    if missing || !unexpected.is_empty() {
        Err(IssueExpectationError {
            missing,
            unexpected,
            origin,
            backtrace: Backtrace::capture(),
        })
    } else {
        Ok(issues)
    }
}

/// Runs one explicit issue expectation scope.
///
/// Runs the body once with the collector's reporter, then seals collection before
/// checking every issue predicate in FIFO order outside locks. Body `Ok` and `Err`
/// values are preserved independently. Reports through other endpoints cannot
/// satisfy this isolated scope; legacy `TestStore` issue obligations remain unchanged.
/// In `Disabled` mode only the known-issue allowance is disabled: the body still
/// runs, predicates still execute, and every captured issue is unexpected.
///
/// ```
/// use rca_core::{Issue, IssueCollector};
/// use rca_test::{IssueExpectationMode, expect_issues};
///
/// let outcome = expect_issues(
///     IssueCollector::new(),
///     IssueExpectationMode::Strict,
///     |issue| issue.code == "known",
///     |reporter| {
///         reporter
///             .report(Issue::new("known", "expected evidence"))
///             .unwrap();
///         Err::<(), _>("original operation error")
///     },
/// );
/// assert_eq!(outcome.body_result, Err("original operation error"));
/// assert_eq!(outcome.expectation.unwrap().len(), 1);
/// ```
///
/// # Panics
/// Propagates body, predicate and owned destructor panics without returning an outcome.
/// Collection closes during body unwinding, or before predicates are invoked.
#[track_caller]
pub fn expect_issues<R, E>(
    collector: IssueCollector,
    mode: IssueExpectationMode,
    mut matches: impl FnMut(&Issue) -> bool,
    body: impl FnOnce(IssueReporter) -> Result<R, E>,
) -> IssueExpectation<R, E> {
    let origin = Location::caller();
    let body_result = body(collector.reporter());
    let expectation = check(collector.finish(), mode, &mut matches, origin);
    // Keep the completed outcomes local while caller-owned predicate captures drop.
    drop(matches);
    IssueExpectation {
        body_result,
        expectation,
    }
}

/// Seals collection if a body poll unwinds, even when its future remains owned.
struct PollCloseGuard<'a> {
    /// The scope's still-open collector, taken exactly once on closure.
    collector: &'a mut Option<IssueCollector>,
    /// Cleared after a normal body poll, including Pending.
    armed: bool,
}

impl Drop for PollCloseGuard<'_> {
    fn drop(&mut self) {
        if self.armed {
            drop(self.collector.take());
        }
    }
}

/// Creates a lazy, caller-polled explicit issue expectation scope.
///
/// Takes ownership of the collector at construction. The body factory runs only on
/// first poll, and its generic future needs no `Send` or `'static` bound. Polling and
/// scheduling remain the caller's responsibility. The lazy body future is boxed
/// for safe pinning and normally allocates once; no executor is created.
///
/// Completion seals and evaluates the same policy as [`expect_issues`]. Dropping
/// an unpolled or pending future seals collection without running predicates or
/// manufacturing success. Cancellation is this caller-owned drop boundary, not an
/// ambient task flag; a body returning `Err` remains an ordinary completed result.
/// Escaped reporter handles reject reports after the scope closes. Accepted reports
/// abandoned by drop are not converted into an expectation result.
///
/// ```
/// use std::task::{Context, Poll, Waker};
///
/// use rca_core::IssueCollector;
/// use rca_test::{IssueExpectationMode, expect_issues_async};
///
/// let future = expect_issues_async(
///     IssueCollector::new(),
///     IssueExpectationMode::Intermittent,
///     |_| true,
///     |_| async { Ok::<_, ()>(41) },
/// );
/// let mut future = std::pin::pin!(future);
/// let Poll::Ready(outcome) = future
///     .as_mut()
///     .poll(&mut Context::from_waker(Waker::noop()))
/// else {
///     panic!("ready body");
/// };
/// assert_eq!(outcome.body_result, Ok(41));
/// assert!(outcome.expectation.unwrap().is_empty());
/// ```
///
/// # Panics
/// Propagates body-factory, future-poll, predicate and owned destructor panics. A poll guard
/// seals on unwinding even if the caller retains the panicked future. Predicates
/// run only after sealing; neither panic creates a completed body outcome.
#[track_caller]
pub fn expect_issues_async<R, E, F: Future<Output = Result<R, E>>>(
    collector: IssueCollector,
    mode: IssueExpectationMode,
    mut matches: impl FnMut(&Issue) -> bool,
    body: impl FnOnce(IssueReporter) -> F,
) -> impl Future<Output = IssueExpectation<R, E>> {
    let origin = Location::caller();
    let reporter = collector.reporter();
    let mut collector = Some(collector);
    let mut body = Box::pin(async move {
        let result;
        {
            let mut future = std::pin::pin!(body(reporter));
            result = future.as_mut().await;
        };
        // The inner future has dropped while its completed output remains local.
        result
    });
    async move {
        let body_result = std::future::poll_fn(|context| {
            let mut guard = PollCloseGuard {
                collector: &mut collector,
                armed: true,
            };
            let result = body.as_mut().poll(context);
            guard.armed = false;
            drop(guard);
            result
        })
        .await;
        let issues = collector.take().expect("unfinished issue scope").finish();
        let expectation = check(issues, mode, &mut matches, origin);
        drop(body);
        drop(matches);
        IssueExpectation {
            body_result,
            expectation,
        }
    }
}
