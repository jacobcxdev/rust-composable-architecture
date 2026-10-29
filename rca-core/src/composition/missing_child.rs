//! Selects the response to an authorised action whose child is absent.

use core::any::type_name;

use crate::{Issue, IssueSeverity, ReduceContext};

/// Selects how an authorised matching action handles an absent child.
///
/// This policy never makes contradictory projections or stale scope authority
/// recoverable. It does not roll back mutations made before detection.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum MissingChildPolicy {
    /// Reports one required issue, skips the child, then reduces the base once.
    ///
    /// Recovery requires accepted diagnostic delivery. Disabled or closed
    /// reporting panics with the original [`crate::IssueReportError`] payload;
    /// reporter panics propagate. These failures occur before base reduction.
    /// `TestStore`'s required recorder must consume the issue explicitly.
    #[default]
    ReportAndBase,
    /// Panics immediately without reducing the child or base.
    FailFast,
}

/// Reports an authorised missing child before base reduction.
///
/// The caller must reject stale authority and contradictory projections first.
/// This function never invokes either reducer or rolls back earlier mutations.
///
/// # Panics
/// Panics for `FailFast`, rejected required reporting, or a panicking reporter.
#[cold]
#[inline(never)]
#[track_caller]
pub(super) fn missing_child<ParentState, ParentAction, ChildState, ChildAction>(
    policy: MissingChildPolicy,
    context: &ReduceContext<'_>,
    combinator: &'static str,
    id_type: Option<&'static str>,
) {
    let message = match id_type {
        Some(id) => format!(
            "{combinator} received keyed child action `{}` with ID type `{id}` in parent action `{}` while child state `{}` was absent from KeyedState in parent state `{}`",
            type_name::<ChildAction>(),
            type_name::<ParentAction>(),
            type_name::<ChildState>(),
            type_name::<ParentState>(),
        ),
        None => format!(
            "{combinator} received child action `{}` in parent action `{}` while child state `{}` was absent from parent state `{}`",
            type_name::<ChildAction>(),
            type_name::<ParentAction>(),
            type_name::<ChildState>(),
            type_name::<ParentState>(),
        ),
    };
    match policy {
        MissingChildPolicy::FailFast => panic!("{message}"),
        MissingChildPolicy::ReportAndBase => {
            let issue =
                Issue::new("rca.missing_child", message).with_severity(IssueSeverity::Error);
            if let Err(error) = context.report_issue(issue) {
                std::panic::panic_any(error);
            }
        }
    }
}

#[cfg(all(test, feature = "test-util"))]
ordinary_test_items! {
mod tests {
    use super::*;
    use crate::store::scope::ScopeRegistry;
    use crate::{
        DependencyValues, IssueCollector, IssueReportError, IssueReporter, MonotonicInstant,
    };
    use std::panic::{AssertUnwindSafe, catch_unwind};

    #[test]
    fn rejected_required_delivery_retains_original_typed_issue_and_source() {
        let collector = IssueCollector::new();
        let closed = collector.reporter();
        assert!(collector.finish().is_empty());
        for (reporter, disabled) in [(IssueReporter::default(), true), (closed, false)] {
            let mut dependencies = DependencyValues::new();
            let (_scopes_lifecycle, mut scopes) = ScopeRegistry::for_test();
            let mut context_transitions = crate::store::scope::ScopeTransitionBatch::default();
            let context = ReduceContext::new(
                &mut dependencies,
                MonotonicInstant::ZERO,
                &mut scopes,
                &mut context_transitions,
            )
            .with_issue_reporter(reporter);
            let mut source_line = 0;
            let panic = catch_unwind(AssertUnwindSafe(|| {
                source_line = line!() + 1;
                missing_child::<(), (), u8, ()>(
                    MissingChildPolicy::ReportAndBase,
                    &context,
                    "IfSome",
                    None,
                );
            }))
            .unwrap_err();
            let error = panic
                .downcast::<IssueReportError>()
                .expect("original error, not text");
            assert_eq!(error.is_disabled(), disabled);
            assert_eq!(error.is_closed(), !disabled);
            let issue = error.into_issue();
            assert_eq!(issue.code, "rca.missing_child");
            assert_eq!(issue.severity, IssueSeverity::Error);
            assert!(issue.cause.is_none());
            assert!(issue.message.contains("IfSome received child action"));
            assert!(issue.location.file().ends_with("missing_child.rs"));
            assert_eq!(issue.location.line(), source_line);
        }
    }
}
}
