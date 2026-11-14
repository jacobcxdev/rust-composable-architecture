//! Checks collector closure, ordered admission and recovery of rejected issues.

#![cfg(feature = "test-util")]

use rca_core::IssueCollector;
use rca_core::{Issue, IssueSeverity};

#[test]
fn finish_seals_and_returns_all_admitted_issues_in_fifo_order() {
    let collector = IssueCollector::default();
    let reporter = collector.reporter();
    let original = Issue::new("first", String::from("owned"))
        .with_severity(IssueSeverity::Error)
        .with_cause(String::from("cause"));
    let expected = original.clone();
    reporter.report(original).unwrap();
    let second = Issue::new("second", "fifo");
    reporter.report(second.clone()).unwrap();
    let issues = collector.finish();
    assert_eq!(issues.len(), 2);
    assert_eq!(issues[0], expected);
    assert_eq!(issues[1], second);
    let late = Issue::new("late", "recover");
    let error = reporter.report(late.clone()).unwrap_err();
    assert!(error.is_closed());
    assert_eq!(error.into_issue(), late);
}

#[test]
fn dropping_unfinished_collector_closes_its_retained_reporters() {
    let collector = IssueCollector::new();
    let reporter = collector.reporter();
    reporter.report(Issue::new("early", "recorded")).unwrap();
    drop(collector);
    assert!(
        reporter
            .report(Issue::new("late", "retained"))
            .unwrap_err()
            .is_closed()
    );
}

#[test]
fn concurrent_admission_is_either_in_finished_queue_or_recovered_closed() {
    let collector = IssueCollector::new();
    let reporter = collector.reporter();
    let barrier = std::sync::Barrier::new(2);
    std::thread::scope(|threads| {
        let reporting = threads.spawn(|| {
            barrier.wait();
            (0..64)
                .map(|index| reporter.report(Issue::new("raced", index.to_string())))
                .collect::<Vec<_>>()
        });
        barrier.wait();
        let captured = collector.finish();
        let outcomes = reporting.join().unwrap();
        let accepted = outcomes.iter().filter(|result| result.is_ok()).count();
        assert_eq!(captured.len(), accepted);
        let accepted_messages = outcomes
            .iter()
            .enumerate()
            .filter(|(_, result)| result.is_ok())
            .map(|(index, _)| index.to_string())
            .collect::<Vec<_>>();
        assert_eq!(
            captured
                .iter()
                .map(|issue| issue.message.as_ref())
                .collect::<Vec<_>>(),
            accepted_messages
        );
        for (index, outcome) in outcomes.into_iter().enumerate() {
            if let Err(error) = outcome {
                assert!(error.is_closed());
                let recovered = error.into_issue();
                assert_eq!(recovered.code, "raced");
                assert_eq!(recovered.message, index.to_string());
            }
        }
    });
}
