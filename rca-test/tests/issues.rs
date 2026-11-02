//! Checks issue ordering, retained evidence, dependency context and teardown reporting.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_executor::block_on;
use rca_core::{
    Clock, DependencyKey, Effect, Issue, IssueReporter, ReduceContext, Reducer, Store,
    with_dependency,
};
use rca_test::{DeterministicRuntime, Exhaustivity, TestStore};

/// Reports distinguishable synchronous and asynchronous issues through the same store.
struct Feature;

impl Reducer for Feature {
    type State = ();
    type Action = bool;
    fn reduce(
        &mut self,
        (): &mut (),
        action: &bool,
        context: &mut ReduceContext<'_>,
    ) -> Effect<bool> {
        let reporter = context.issue_reporter();
        if *action {
            Effect::run(move |_| async move {
                reporter
                    .report(Issue::new("async", String::from("owned payload")))
                    .unwrap();
            })
        } else {
            context
                .report_issue(Issue::new("sync", "borrowed payload"))
                .unwrap();
            Effect::none()
        }
    }
}

#[test]
fn reducer_and_async_reports_preserve_payload_location_and_order() {
    let mut store = TestStore::new((), Feature);
    block_on(async {
        let _sync = store.send(false).await.unwrap();
        let _async = store.send(true).await.unwrap();
        let issue = store
            .expect_issue(|issue| issue.code == "sync")
            .await
            .unwrap();
        assert_eq!(issue.message, "borrowed payload");
        assert!(issue.location.file().ends_with("issues.rs"));
        let issue = store
            .expect_issue(|issue| issue.code == "async")
            .await
            .unwrap();
        assert_eq!(issue.message, "owned payload");
        assert!(
            store
                .expect_issue(|_| true)
                .await
                .unwrap_err()
                .is_no_issue()
        );
        store.finish().await.unwrap();
    });
}

#[test]
fn mismatch_panic_and_cancelled_expectations_retain_oldest_evidence() {
    let mut store = TestStore::new((), Feature);
    let _task = block_on(store.send(false)).unwrap();
    let expectation_line = line!() + 1;
    let error = block_on(store.expect_issue(|_| false)).unwrap_err();
    assert!(error.is_issue_mismatch());
    assert!(!error.is_no_issue());
    assert!(!error.is_unreceived_action());
    assert!(!error.is_no_received_action());
    assert!(!error.is_action_mismatch());
    assert!(!error.is_skip_requires_non_exhaustive());
    assert_eq!(error.origin().file(), file!());
    assert_eq!(error.origin().line(), expectation_line);
    let evidence = error.mismatched_issue().unwrap();
    assert_eq!(evidence.code, "sync");
    assert_eq!(evidence.message, "borrowed payload");
    assert_eq!(evidence.severity, rca_core::IssueSeverity::Warning);
    assert!(evidence.cause.is_none());
    assert_eq!(evidence.location.file(), file!());
    assert_eq!(evidence.location.line(), 35);
    assert_eq!(evidence.location.column(), 31);
    let diagnostic = error.to_string();
    assert!(diagnostic.starts_with(&format!(
        "oldest issue did not match expectation at {}:{expectation_line}:",
        file!()
    )));
    assert!(diagnostic.contains("code: \"sync\""));
    assert!(diagnostic.contains("message: \"borrowed payload\""));
    assert!(diagnostic.contains(&format!("file: {:?}, line: 35, column: 31", file!())));
    let debug = format!("{error:?}");
    assert!(debug.contains("kind: \"IssueMismatch\""));
    assert!(debug.contains("code: \"sync\""));
    assert!(debug.contains("message: \"borrowed payload\""));
    assert!(debug.contains(&format!("file: {:?}, line: 35, column: 31", file!())));
    let panic = catch_unwind(AssertUnwindSafe(|| {
        block_on(store.expect_issue(|_| panic!("matcher")))
    }))
    .unwrap_err();
    assert_eq!(panic.downcast_ref::<&str>(), Some(&"matcher"));

    drop(store.expect_issue(|_| true));
    let consumed = block_on(store.expect_issue(|_| true)).unwrap();
    assert_eq!(&consumed, evidence);
    let no_issue = block_on(store.expect_issue(|_| true)).unwrap_err();
    assert!(no_issue.is_no_issue());
    assert!(no_issue.mismatched_issue().is_none());
    assert_eq!(format!("{error:?}"), debug);
    block_on(store.finish()).unwrap();
}

#[test]
fn unexpected_issues_fail_finish_and_normal_drop_in_both_modes() {
    for mode in [Exhaustivity::Exhaustive, Exhaustivity::NonExhaustive] {
        let mut store = TestStore::builder((), Feature)
            .with_exhaustivity(mode)
            .build();
        let _task = block_on(store.send(false)).unwrap();
        let error = block_on(store.finish()).unwrap_err();
        assert_eq!(error.unconsumed_issues()[0].code, "sync");
        let panic = catch_unwind(AssertUnwindSafe(|| drop(store))).unwrap_err();
        assert!(panic.downcast_ref::<String>().unwrap().starts_with(
            "TestStore teardown failed: TestStore finished with unconsumed issues at "
        ));
    }
}

#[test]
fn parallel_stores_keep_issue_recorders_isolated() {
    std::thread::scope(|scope| {
        for asynchronous in [false, true] {
            scope.spawn(move || {
                let mut store = TestStore::new((), Feature);
                let _task = block_on(store.send(asynchronous)).unwrap();
                assert_eq!(
                    block_on(store.expect_issue(|_| true)).unwrap().code,
                    if asynchronous { "async" } else { "sync" }
                );
                block_on(store.finish()).unwrap();
            });
        }
    });
}

/// Keeps a reporter outside the store and reports when the reducer is released.
struct Captures(Arc<Mutex<Option<IssueReporter>>>);

impl Reducer for Captures {
    type State = ();
    type Action = ();
    fn reduce(&mut self, (): &mut (), (): &(), context: &mut ReduceContext<'_>) -> Effect<()> {
        *self.0.lock().unwrap() = Some(context.issue_reporter());
        Effect::none()
    }
}
impl Drop for Captures {
    fn drop(&mut self) {
        if let Some(reporter) = self.0.lock().unwrap().as_ref() {
            reporter
                .report(Issue::new("destructor", "reducer dropped"))
                .unwrap();
        }
    }
}

#[test]
fn shutdown_destructor_issues_remain_actionable_on_retry() {
    let reporter = Arc::new(Mutex::new(None));
    let mut store = TestStore::new((), Captures(Arc::clone(&reporter)));
    let _task = block_on(store.send(())).unwrap();
    let error = block_on(store.finish()).unwrap_err();
    assert_eq!(error.unconsumed_issues()[0].code, "destructor");
    assert_eq!(
        block_on(store.finish()).unwrap_err().unconsumed_issues()[0].code,
        "destructor"
    );
    assert!(
        block_on(store.expect_issue(|_| false))
            .unwrap_err()
            .is_issue_mismatch()
    );
    block_on(store.expect_issue(|issue| issue.code == "destructor")).unwrap();
    block_on(store.finish()).unwrap();
    let issue = Issue::new("late", String::from("rejected payload"));
    let location = issue.location;
    let error = reporter
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .report(issue)
        .unwrap_err();
    assert!(error.is_closed());
    let issue = error.into_issue();
    assert_eq!(issue.message, "rejected payload");
    assert_eq!(issue.location, location);
}

#[test]
fn retained_disabled_reporter_does_not_keep_store_alive() {
    /// Retains the reporter without requiring enabled reporting during destruction.
    struct Capture(Arc<Mutex<Option<IssueReporter>>>);
    impl Reducer for Capture {
        type State = ();
        type Action = ();
        fn reduce(&mut self, (): &mut (), (): &(), context: &mut ReduceContext<'_>) -> Effect<()> {
            *self.0.lock().unwrap() = Some(context.issue_reporter());
            Effect::none()
        }
    }
    let runtime = DeterministicRuntime::new();
    let reporter = Arc::new(Mutex::new(None));
    let store = Store::builder((), Capture(Arc::clone(&reporter)), runtime.runtime())
        .build()
        .unwrap();
    let _task = block_on(runtime.run(store.send(()))).unwrap().unwrap();
    drop(store);
    runtime.run_until_stalled().unwrap();
    assert_eq!(runtime.run_until_stalled().unwrap().polls(), 0);
    let error = reporter
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .report(Issue::new("disabled", "ignored"))
        .unwrap_err();
    assert!(error.is_disabled());
    assert_eq!(error.into_issue().code, "disabled");
}

#[test]
fn unpolled_expectation_does_not_run_matcher() {
    let mut store = TestStore::new((), Feature);
    let matched = Arc::new(Mutex::new(false));
    let marker = Arc::clone(&matched);
    drop(store.expect_issue(move |_| {
        *marker.lock().unwrap() = true;
        true
    }));
    assert!(!*matched.lock().unwrap());
    block_on(store.finish()).unwrap();
}

#[test]
fn expectation_drives_ready_work_without_advancing_time() {
    /// Requires the deterministic clock; using a live clock must fail the test.
    struct ClockKey;
    impl DependencyKey for ClockKey {
        type Value = Clock;
        fn live_value() -> Clock {
            panic!("explicit clock")
        }
    }
    /// Emits an issue only after the test clock reaches its deadline.
    struct Delayed;
    impl Reducer for Delayed {
        type State = ();
        type Action = ();
        fn reduce(&mut self, (): &mut (), (): &(), context: &mut ReduceContext<'_>) -> Effect<()> {
            let clock = context.dependency::<ClockKey>().clone();
            let reporter = context.issue_reporter();
            Effect::run(move |_| async move {
                clock.sleep(Duration::from_secs(1)).await;
                reporter
                    .report(Issue::new("delayed", "ready only after advance"))
                    .unwrap();
            })
        }
    }
    let mut store = TestStore::builder((), Delayed)
        .with_clock_dependency::<ClockKey>(|clock| clock)
        .build();
    let _task = block_on(store.send(())).unwrap();
    assert!(
        block_on(store.expect_issue(|_| true))
            .unwrap_err()
            .is_no_issue()
    );
    block_on(store.advance(Duration::from_secs(1))).unwrap();
    block_on(store.expect_issue(|issue| issue.code == "delayed")).unwrap();
    block_on(store.finish()).unwrap();
}

#[test]
fn lexical_overrides_preserve_original_issue_location_and_payload() {
    /// Creates nested dependency scopes without changing the issue itself.
    struct Key;
    impl DependencyKey for Key {
        type Value = ();
        fn live_value() {}
    }
    /// Forwards the supplied issue through a nested dependency context.
    struct Reports;
    impl Reducer for Reports {
        type State = ();
        type Action = Issue;
        fn reduce(
            &mut self,
            (): &mut (),
            action: &Issue,
            context: &mut ReduceContext<'_>,
        ) -> Effect<Issue> {
            context.with_dependency_override::<Key, _>((), |child| {
                child.report_issue(action.clone()).unwrap();
            });
            Effect::none()
        }
    }
    let mut store = TestStore::builder((), with_dependency::<Key, _>((), Reports))
        .with_dependency::<Key>(())
        .build();
    let expected_line = line!() + 1;
    let issue = Issue::new("original", String::from("kept payload"));
    let location = issue.location;
    assert_eq!(location.line(), expected_line);
    let _task = block_on(store.send(issue)).unwrap();
    let recorded = block_on(store.expect_issue(|issue| issue.code == "original")).unwrap();
    assert_eq!(recorded.location, location);
    assert_eq!(recorded.message, "kept payload");
    block_on(store.finish()).unwrap();
}

#[test]
fn missing_test_dependencies_remain_panics() {
    /// Has no test value; the live fallback is a separate failure sentinel.
    struct Missing;
    impl DependencyKey for Missing {
        type Value = ();
        fn live_value() {
            panic!("live must not run")
        }
    }
    /// Requests the missing value through the same reducer API as application code.
    struct Requires;
    impl Reducer for Requires {
        type State = ();
        type Action = ();
        fn reduce(&mut self, (): &mut (), (): &(), context: &mut ReduceContext<'_>) -> Effect<()> {
            context.dependency::<Missing>();
            Effect::none()
        }
    }
    let panic = catch_unwind(AssertUnwindSafe(|| {
        let mut store = TestStore::new((), Requires);
        let _task = block_on(store.send(()));
    }))
    .unwrap_err();
    let message = panic.downcast_ref::<String>().unwrap();
    assert!(message.contains("missing test dependency"));
    assert!(message.contains("Missing"));
}
