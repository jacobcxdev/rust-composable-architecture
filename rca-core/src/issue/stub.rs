//! Reports explicit test endpoint use before a lazy fallback or invariant failure.

use std::fmt;
use std::panic::Location;

use super::Issue;

/// Captures an explicit test endpoint's diagnostic callback and construction origin.
///
/// Available only with `test-util`. Construction and Drop do not report or
/// evaluate diagnostic or fallback factories. Each invocation supplies those
/// factories explicitly; no default value, live dependency, or I/O is selected.
/// The diagnostic location is the stub's definition/construction site, not its
/// invocation site. Its remaining fields are supplied unchanged by the caller.
///
/// A report closure can borrow local, non-`Send` data or own an explicitly selected
/// [`super::IssueReporters`] value. This helper imposes no transport bounds. Existing
/// owned client endpoints still require their captures to be `Send + Sync + 'static`.
/// No ambient reporter selection, executor or new reporting policy is introduced.
/// Debug output contains only the endpoint label and construction location.
///
/// # Examples
///
/// ```
/// use rca_core::{EndpointStub, IssueCollector};
/// use rca_core::Issue;
///
/// let collector = IssueCollector::new();
/// let reporter = collector.reporter();
/// let stub = EndpointStub::new("Client.load", move |issue| reporter.report(issue));
/// let (result, delivery) = stub.call_with(
///     || Issue::new("unimplemented", "Client.load invoked"),
///     || Err::<(), _>("caller-provided failure"),
/// );
/// assert_eq!(result, Err("caller-provided failure"));
/// delivery.unwrap();
/// assert_eq!(collector.finish().len(), 1);
/// ```
///
/// Borrowed local callbacks cannot outlive their captured context:
///
/// ```compile_fail
/// use std::cell::Cell;
/// use rca_core::EndpointStub;
/// use rca_core::Issue;
/// let stub;
/// {
///     let calls = Cell::new(0);
///     stub = EndpointStub::new("Client.load", |_: Issue| calls.set(calls.get() + 1));
/// }
/// stub.call_with(|| Issue::new("stub", "load"), || ());
/// ```
///
/// Local captures do not become owned client transport merely by using a stub:
///
/// ```compile_fail
/// use std::{cell::Cell, rc::Rc, sync::Arc};
/// use rca_core::EndpointStub;
/// use rca_core::Issue;
/// let calls = Rc::new(Cell::new(0));
/// let stub = EndpointStub::new("Client.load", move |_: Issue| calls.set(calls.get() + 1));
/// let endpoint: Arc<dyn Fn() -> u8 + Send + Sync> = Arc::new(move || {
///     stub.call_with(|| Issue::new("stub", "load"), || 7).0
/// });
/// ```
pub struct EndpointStub<Report> {
    /// Identifies the missing endpoint in a forbidden-invocation panic.
    endpoint: &'static str,
    /// Preserves the construction site independently of later diagnostic factories.
    location: &'static Location<'static>,
    /// Receives each diagnostic before the corresponding fallback can run.
    report: Report,
}

impl<Report> EndpointStub<Report> {
    /// Captures a diagnostic callback without invoking it.
    ///
    /// `endpoint` labels forbidden-invocation panic diagnostics. Location is
    /// captured here, so constructing this inside an endpoint invocation captures
    /// that construction site rather than a separate client-definition site.
    #[must_use]
    #[track_caller]
    pub fn new(endpoint: &'static str, report: Report) -> Self {
        Self {
            endpoint,
            location: Location::caller(),
            report,
        }
    }

    /// Reports one lazy diagnostic before evaluating one explicit fallback.
    ///
    /// Diagnostic creation precedes reporting; reporting must return before the
    /// fallback runs. Delivery acceptance or rejection does not select the fallback
    /// or modify its output. In particular, `Result<Value, Error>` and successful
    /// `None` are preserved without cloning, formatting or inspecting their values.
    /// The second tuple member retains the callback's original delivery result.
    ///
    /// An async endpoint can call this inside its caller-polled async block and
    /// supply a lazy future as the fallback, then await it. Dropping that block
    /// unpolled does no work. Dropping it after reporting does not undo the report.
    /// This helper neither polls nor spawns a supplied future.
    ///
    /// # Panics
    /// Propagates diagnostic, callback and fallback panics. A diagnostic or callback
    /// panic prevents fallback evaluation. A fallback panic does not undo reporting;
    /// no completed tuple is returned in any of these cases.
    #[must_use]
    pub fn call_with<Output, Delivery>(
        &self,
        issue: impl FnOnce() -> Issue,
        fallback: impl FnOnce() -> Output,
    ) -> (Output, Delivery)
    where
        Report: Fn(Issue) -> Delivery,
    {
        let mut issue = issue();
        issue.location = self.location;
        let delivery = (self.report)(issue);
        let output = fallback();
        (output, delivery)
    }

    /// Reports an explicitly forbidden invocation, then panics regardless of delivery.
    ///
    /// No fallback exists. Disabled, closed or failed delivery cannot turn a missing
    /// mandatory endpoint into success. This does not install itself as a dependency
    /// default or change existing generated unconfigured-endpoint behaviour.
    ///
    /// # Panics
    /// Drops the delivery result before starting the invariant panic. Diagnostic,
    /// callback and delivery-destructor panics propagate instead. Otherwise, always
    /// panics. The diagnostic uses the captured construction location.
    pub fn forbidden<Delivery>(&self, issue: impl FnOnce() -> Issue) -> !
    where
        Report: Fn(Issue) -> Delivery,
    {
        let mut issue = issue();
        issue.location = self.location;
        drop((self.report)(issue));
        panic!("forbidden dependency endpoint `{}` invoked", self.endpoint)
    }
}

impl<Report> fmt::Debug for EndpointStub<Report> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EndpointStub")
            .field("endpoint", &self.endpoint)
            .field("location", &self.location)
            .finish_non_exhaustive()
    }
}
