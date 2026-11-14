//! Delivers owned diagnostics through an explicit, ordered endpoint collection.

use super::{Issue, IssueReportError, IssueReporter};

/// Owns an explicit, ordered selection of issue endpoints.
///
/// Pass this value directly to clients, capture a clone in a future, or install it
/// using an application-defined [`crate::DependencyKey`]. It does not change
/// [`crate::ReduceContext::report_issue`] or introduce ambient reporting state.
/// Cloning copies the endpoint list and shares its existing endpoints, not Store
/// or ingress authority. An empty selection is explicitly disabled.
///
/// ```
/// use rca_core::{Issue, IssueReporter, IssueReporters};
///
/// let inherited = IssueReporters::new([IssueReporter::new(|issue| {
///     assert_eq!(issue.code, "selected");
/// })]);
/// let disabled = IssueReporters::default();
/// inherited.with_scope(None, |selected| {
///     assert!(selected.report(Issue::new("selected", "inherited"))[0].is_ok());
///     selected.with_scope(Some(&disabled), |selected| {
///         let error = selected.report(Issue::new("hidden", "retained"))
///             .pop().unwrap().unwrap_err();
///         assert!(error.is_disabled());
///         assert_eq!(error.into_issue().message, "retained");
///     });
/// });
/// ```
#[derive(Clone, Debug, Default)]
pub struct IssueReporters {
    /// Shares endpoint handles in invocation order, including duplicates.
    reporters: Vec<IssueReporter>,
}

impl IssueReporters {
    /// Collects endpoints in invocation order.
    ///
    /// Duplicate handles are retained and invoked separately. Collection storage
    /// allocates when nonempty; cloning the collection copies this list of handles.
    #[must_use]
    pub fn new(reporters: impl IntoIterator<Item = IssueReporter>) -> Self {
        Self {
            reporters: reporters.into_iter().collect(),
        }
    }

    /// Invokes every selected endpoint in order, recovering each rejected issue.
    ///
    /// Returns one outcome per endpoint without stopping on disabled or closed
    /// delivery. An empty selection returns one disabled error containing the
    /// original issue. Earlier endpoints receive independent issue clones; the
    /// final endpoint receives the original. Location, severity and cause remain
    /// unchanged. Owned message/cause clones and the outcome vector can allocate.
    /// Recorder endpoints separately copy admitted issues into their queues.
    ///
    /// Endpoint callbacks execute synchronously outside admission locks and may
    /// overlap with other calls or reenter. Admission is not callback completion:
    /// a Store can seal its endpoint while an already admitted callback is running.
    /// This method returns accepted outcomes only after those callbacks return.
    ///
    /// # Panics
    /// Propagates callback panics. Later endpoints are not invoked, and no completed
    /// aggregate result is returned. Earlier admission is not undone.
    #[must_use]
    pub fn report(&self, issue: Issue) -> Vec<Result<(), IssueReportError>> {
        let Some((last, prefix)) = self.reporters.split_last() else {
            return vec![Err(IssueReportError {
                issue,
                closed: false,
            })];
        };
        let mut outcomes = Vec::with_capacity(self.reporters.len());
        for reporter in prefix {
            outcomes.push(reporter.report(issue.clone()));
        }
        outcomes.push(last.report(issue));
        outcomes
    }

    /// Invokes an operation with a lexical, explicit reporter selection.
    ///
    /// `None` inherits this context. `Some` replaces it, including an empty context.
    /// Neither context is mutated, so nested scopes leave their parent unchanged
    /// when returning an error or unwinding. Clone the selected value into futures
    /// that need to retain the choice after this invocation; polling and scheduling
    /// remain the caller's responsibility.
    ///
    /// # Panics
    /// Propagates operation panics without interception.
    pub fn with_scope<Output>(
        &self,
        supplied: Option<&Self>,
        operation: impl FnOnce(&Self) -> Output,
    ) -> Output {
        operation(supplied.unwrap_or(self))
    }
}
