//! Checks owned reporter selection, delivery order and lifetime boundaries.

#[expect(
    dead_code,
    reason = "manual polling uses only the captured actor helpers"
)]
mod support;

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(feature = "test-util")]
use std::sync::mpsc;
use std::sync::{Arc, Barrier, Mutex};
use std::task::{Context, Poll, Waker};
#[cfg(feature = "test-util")]
use std::time::Duration;

use rca_core::IssueReporters;
use rca_core::{
    DependencyKey, Effect, Issue, IssueReporter, IssueSeverity, ReduceContext, Reducer, Store,
    with_dependency,
};

/// Copies delivered issues into the caller's independent observation buffer.
fn recording(events: &Arc<Mutex<Vec<Issue>>>) -> IssueReporter {
    let events = Arc::clone(events);
    IssueReporter::new(move |issue| events.lock().unwrap().push(issue.clone()))
}

#[test]
fn multiplex_invokes_in_order_and_recovers_rejections_without_short_circuiting() {
    let events = Arc::new(Mutex::new(Vec::new()));
    let first = {
        let events = Arc::clone(&events);
        IssueReporter::new(move |issue| events.lock().unwrap().push(("first", issue.clone())))
    };
    let last = {
        let events = Arc::clone(&events);
        IssueReporter::new(move |issue| events.lock().unwrap().push(("last", issue.clone())))
    };
    let context = IssueReporters::new([first, IssueReporter::default(), last]);
    let issue = Issue::new("original", String::from("retained payload"))
        .with_severity(IssueSeverity::Error)
        .with_cause(String::from("retained cause"));
    let location = issue.location;
    let outcomes = context.report(issue);
    let [Ok(()), Err(rejected), Ok(())] = outcomes.as_slice() else {
        panic!("each endpoint needs its ordered outcome")
    };
    assert!(rejected.is_disabled());
    let observed = events.lock().unwrap();
    assert_eq!(
        observed.iter().map(|(name, _)| *name).collect::<Vec<_>>(),
        ["first", "last"]
    );
    for (_, accepted) in &*observed {
        assert_eq!(accepted.code, "original");
        assert_eq!(accepted.message, "retained payload");
        assert_eq!(accepted.cause.as_deref(), Some("retained cause"));
        assert_eq!(accepted.severity, IssueSeverity::Error);
        assert_eq!(accepted.location, location);
    }
    drop(observed);
    let rejected = outcomes
        .into_iter()
        .nth(1)
        .unwrap()
        .unwrap_err()
        .into_issue();
    assert_eq!(rejected.code, "original");
    assert_eq!(rejected.message, "retained payload");
    assert_eq!(rejected.cause.as_deref(), Some("retained cause"));
    assert_eq!(rejected.severity, IssueSeverity::Error);
    assert_eq!(rejected.location, location);
}

#[test]
fn final_endpoint_receives_original_buffers_after_independent_prefix_copies() {
    for count in [1, 3] {
        for accepted in [false, true] {
            let issue = Issue::new("owned", String::from("original message"))
                .with_severity(IssueSeverity::Error)
                .with_cause(String::from("original cause"));
            let message = issue.message.as_ptr().addr();
            let cause = issue.cause.as_ref().unwrap().as_ptr().addr();
            let location = issue.location;
            let called = Arc::new(AtomicBool::new(false));
            let last = if accepted {
                let called = Arc::clone(&called);
                IssueReporter::new(move |issue| {
                    assert_eq!(issue.message.as_ptr().addr(), message);
                    assert_eq!(issue.cause.as_ref().unwrap().as_ptr().addr(), cause);
                    assert_eq!(issue.location, location);
                    assert_eq!(issue.severity, IssueSeverity::Error);
                    called.store(true, Ordering::SeqCst);
                })
            } else {
                IssueReporter::default()
            };
            let endpoints = std::iter::repeat_n(IssueReporter::default(), count - 1).chain([last]);
            let mut outcomes = IssueReporters::new(endpoints).report(issue);
            assert_eq!(outcomes.len(), count);
            let final_outcome = outcomes.pop().unwrap();
            if accepted {
                final_outcome.unwrap();
            } else {
                let rejected = final_outcome.unwrap_err();
                assert!(rejected.is_disabled());
                let recovered = rejected.into_issue();
                assert_eq!(recovered.message.as_ptr().addr(), message);
                assert_eq!(recovered.cause.as_ref().unwrap().as_ptr().addr(), cause);
                assert_eq!(recovered.location, location);
                assert_eq!(recovered.severity, IssueSeverity::Error);
            }
            for outcome in outcomes {
                let recovered = outcome.unwrap_err().into_issue();
                assert_ne!(recovered.message.as_ptr().addr(), message);
                assert_ne!(recovered.cause.as_ref().unwrap().as_ptr().addr(), cause);
                assert_eq!(recovered.message, "original message");
                assert_eq!(recovered.cause.as_deref(), Some("original cause"));
                assert_eq!(recovered.location, location);
                assert_eq!(recovered.severity, IssueSeverity::Error);
            }
            assert_eq!(called.load(Ordering::SeqCst), accepted);
        }
    }
}

#[cfg(feature = "test-util")]
#[test]
fn duplicate_recorder_endpoints_keep_independent_entries_and_final_closed_recovery() {
    let mut recorder = rca_core::testing::IssueRecorder::new();
    let runtime = support::ManualRuntime::new();
    let store = Store::builder((), NoWork, runtime.runtime())
        .issue_recorder(recorder.clone())
        .build()
        .unwrap();
    let context = IssueReporters::new([recorder.reporter(), recorder.reporter()]);
    let issue = Issue::new("recorded", String::from("message"))
        .with_severity(IssueSeverity::Error)
        .with_cause(String::from("cause"));
    let expected = issue.clone();
    assert!(matches!(context.report(issue).as_slice(), [Ok(()), Ok(())]));
    let mut first = recorder.consume_oldest().unwrap();
    let second = recorder.consume_oldest().unwrap();
    assert_eq!(first, expected);
    assert_eq!(second, expected);
    first.message.to_mut().push_str(" changed");
    assert_eq!(second, expected);
    assert!(recorder.oldest().is_none());

    drop(runtime.take_only_queued_task());
    let late =
        Issue::new("closed", String::from("late message")).with_cause(String::from("late cause"));
    let message = late.message.as_ptr();
    let cause = late.cause.as_ref().unwrap().as_ptr();
    let mut outcomes = context.report(late);
    assert!(
        outcomes
            .iter()
            .all(|outcome| outcome.as_ref().unwrap_err().is_closed())
    );
    let recovered = outcomes.pop().unwrap().unwrap_err().into_issue();
    assert_eq!(recovered.message.as_ptr(), message);
    assert_eq!(recovered.cause.as_ref().unwrap().as_ptr(), cause);
    assert!(recorder.oldest().is_none());
    drop(store);
}

#[test]
fn nested_scope_selection_restores_after_error_and_empty_replacement_keeps_evidence() {
    let outer_events = Arc::new(Mutex::new(Vec::new()));
    let inner_events = Arc::new(Mutex::new(Vec::new()));
    let outer = IssueReporters::new([recording(&outer_events)]);
    let inner = IssueReporters::new([recording(&inner_events)]);
    let empty = IssueReporters::default();
    let result: Result<(), i32> = outer.with_scope(Some(&inner), |selected| {
        assert!(
            selected
                .report(Issue::new("inner-before", "inner"))
                .into_iter()
                .all(|result| result.is_ok())
        );
        selected.with_scope(Some(&empty), |disabled| {
            let rejected = disabled.report(Issue::new("empty", "kept"));
            assert_eq!(rejected.len(), 1);
            let error = rejected.into_iter().next().unwrap().unwrap_err();
            assert!(error.is_disabled());
            assert_eq!(error.into_issue().message, "kept");
            Err::<(), _>(7)
        })?;
        panic!("inner error must propagate")
    });
    assert_eq!(result, Err(7));
    assert!(
        inner
            .with_scope(None, |selected| selected
                .report(Issue::new("inner-after", "restored")))
            .into_iter()
            .all(|result| result.is_ok())
    );
    assert!(
        outer
            .with_scope(None, |selected| selected
                .report(Issue::new("outer-after", "restored")))
            .into_iter()
            .all(|result| result.is_ok())
    );
    assert_eq!(
        inner_events
            .lock()
            .unwrap()
            .iter()
            .map(|issue| issue.code)
            .collect::<Vec<_>>(),
        ["inner-before", "inner-after"]
    );
    assert_eq!(
        outer_events
            .lock()
            .unwrap()
            .iter()
            .map(|issue| issue.code)
            .collect::<Vec<_>>(),
        ["outer-after"]
    );
}

#[test]
fn independent_futures_keep_owned_choices_after_the_lexical_scope_ends() {
    /// Checks the owned future without imposing bounds on an executor.
    fn is_send<T: Send>(_: &T) {}

    let parent_events = Arc::new(Mutex::new(Vec::new()));
    let left_events = Arc::new(Mutex::new(Vec::new()));
    let right_events = Arc::new(Mutex::new(Vec::new()));
    let parent = IssueReporters::new([recording(&parent_events)]);
    let left = IssueReporters::new([recording(&left_events)]);
    let right = IssueReporters::new([recording(&right_events)]);
    let left_gate = Arc::new(AtomicBool::new(false));
    let right_gate = Arc::new(AtomicBool::new(false));
    let make_future = |selected: &IssueReporters, gate: Arc<AtomicBool>, code| {
        let chosen = selected.clone();
        async move {
            // This fixture is polled directly after its gate changes.
            // It does not claim to wake an executor when that happens.
            std::future::poll_fn(|_| {
                if gate.load(Ordering::Acquire) {
                    Poll::Ready(())
                } else {
                    Poll::Pending
                }
            })
            .await;
            chosen.report(Issue::new(code, "owned choice"))
        }
    };
    let mut left_future = Box::pin(parent.with_scope(Some(&left), |selected| {
        make_future(selected, Arc::clone(&left_gate), "left")
    }));
    let mut right_future = Box::pin(parent.with_scope(Some(&right), |selected| {
        make_future(selected, Arc::clone(&right_gate), "right")
    }));
    is_send(&left_future);
    is_send(&right_future);
    drop(left);
    drop(right);
    let mut poll_context = Context::from_waker(Waker::noop());
    assert!(left_future.as_mut().poll(&mut poll_context).is_pending());
    assert!(right_future.as_mut().poll(&mut poll_context).is_pending());
    assert!(left_events.lock().unwrap().is_empty());
    assert!(right_events.lock().unwrap().is_empty());
    right_gate.store(true, Ordering::Release);
    assert!(
        matches!(right_future.as_mut().poll(&mut poll_context), Poll::Ready(results) if results.iter().all(Result::is_ok))
    );
    assert!(
        parent
            .report(Issue::new("parent", "still selected"))
            .into_iter()
            .all(|result| result.is_ok())
    );
    left_gate.store(true, Ordering::Release);
    assert!(
        matches!(left_future.as_mut().poll(&mut poll_context), Poll::Ready(results) if results.iter().all(Result::is_ok))
    );
    for (events, expected) in [
        (&parent_events, "parent"),
        (&left_events, "left"),
        (&right_events, "right"),
    ] {
        assert_eq!(
            events
                .lock()
                .unwrap()
                .iter()
                .map(|issue| issue.code)
                .collect::<Vec<_>>(),
            [expected]
        );
    }
}

#[test]
fn concurrent_explicit_scopes_never_cross_route_or_mutate_the_parent() {
    let parent_events = Arc::new(Mutex::new(Vec::new()));
    let left_events = Arc::new(Mutex::new(Vec::new()));
    let right_events = Arc::new(Mutex::new(Vec::new()));
    let parent = IssueReporters::new([recording(&parent_events)]);
    let left = IssueReporters::new([recording(&left_events)]);
    let right = IssueReporters::new([recording(&right_events)]);
    let start = Barrier::new(2);
    std::thread::scope(|threads| {
        for (choice, code) in [(&left, "left"), (&right, "right")] {
            let parent = &parent;
            let start = &start;
            threads.spawn(move || {
                parent.with_scope(Some(choice), |selected| {
                    start.wait();
                    for _ in 0..32 {
                        assert!(
                            selected
                                .report(Issue::new(code, "concurrent"))
                                .into_iter()
                                .all(|result| result.is_ok())
                        );
                    }
                });
            });
        }
    });
    assert!(parent_events.lock().unwrap().is_empty());
    for (events, expected) in [(&left_events, "left"), (&right_events, "right")] {
        let observed = events.lock().unwrap();
        assert_eq!(observed.len(), 32);
        assert!(observed.iter().all(|issue| issue.code == expected));
    }
    assert!(
        parent
            .report(Issue::new("parent", "not overwritten"))
            .into_iter()
            .all(|result| result.is_ok())
    );
    assert_eq!(parent_events.lock().unwrap()[0].code, "parent");
}

/// Requires an explicitly selected reporter collection.
struct ReportingKey;

impl DependencyKey for ReportingKey {
    type Value = IssueReporters;

    fn live_value() -> Self::Value {
        panic!("explicit context required")
    }
}

/// Observes feature and operation-local selections during a real reduction.
struct ScopedReducer {
    /// Supplies the temporary operation-local replacement.
    inner: IssueReporters,
}

impl Reducer for ScopedReducer {
    type State = usize;
    type Action = ();

    fn reduce(
        &mut self,
        state: &mut usize,
        (): &(),
        context: &mut ReduceContext<'_>,
    ) -> Effect<()> {
        let feature = context.dependency::<ReportingKey>().clone();
        assert!(
            feature
                .report(Issue::new("feature-before", "feature overlay"))
                .into_iter()
                .all(|result| result.is_ok())
        );
        let result =
            context.with_dependency_override::<ReportingKey, _>(self.inner.clone(), |child| {
                assert!(
                    child
                        .dependency::<ReportingKey>()
                        .report(Issue::new("inner", "action override"))
                        .into_iter()
                        .all(|result| result.is_ok())
                );
                Err::<(), _>(9)
            });
        assert_eq!(result, Err(9));
        assert!(
            context
                .dependency::<ReportingKey>()
                .report(Issue::new("feature-after", "restored overlay"))
                .into_iter()
                .all(|result| result.is_ok())
        );
        // The explicit collection must not force changes to the legacy Store endpoint.
        assert!(
            context
                .report_issue(Issue::new("legacy", "disabled Store endpoint"))
                .unwrap_err()
                .is_disabled()
        );
        *state += 1;
        Effect::none()
    }
}

#[test]
fn real_reducer_uses_existing_feature_and_action_dependency_overrides() {
    let store_events = Arc::new(Mutex::new(Vec::new()));
    let feature_events = Arc::new(Mutex::new(Vec::new()));
    let inner_events = Arc::new(Mutex::new(Vec::new()));
    let runtime = support::ManualRuntime::new();
    let store = Store::builder(
        0usize,
        with_dependency::<ReportingKey, _>(
            IssueReporters::new([recording(&feature_events)]),
            ScopedReducer {
                inner: IssueReporters::new([recording(&inner_events)]),
            },
        ),
        runtime.runtime(),
    )
    .with_dependency::<ReportingKey>(IssueReporters::new([recording(&store_events)]))
    .build()
    .unwrap();
    let mut actor = runtime.take_only_queued_task();
    let mut poll_context = Context::from_waker(Waker::noop());
    let mut send = Box::pin(store.send(()));
    assert!(send.as_mut().poll(&mut poll_context).is_pending());
    assert!(actor.as_mut().poll(&mut poll_context).is_pending());
    assert!(matches!(
        send.as_mut().poll(&mut poll_context),
        Poll::Ready(Ok(_))
    ));
    let mut read = Box::pin(store.read(|state| *state));
    assert!(read.as_mut().poll(&mut poll_context).is_pending());
    assert!(actor.as_mut().poll(&mut poll_context).is_pending());
    assert!(matches!(
        read.as_mut().poll(&mut poll_context),
        Poll::Ready(Ok(1))
    ));
    assert!(store_events.lock().unwrap().is_empty());
    assert_eq!(
        feature_events
            .lock()
            .unwrap()
            .iter()
            .map(|issue| issue.code)
            .collect::<Vec<_>>(),
        ["feature-before", "feature-after"]
    );
    assert_eq!(
        inner_events
            .lock()
            .unwrap()
            .iter()
            .map(|issue| issue.code)
            .collect::<Vec<_>>(),
        ["inner"]
    );
}

// Ignoring a feature overlay, leaking it to a sibling, or running cancelled work misroutes issues.
#[test]
fn deferred_effect_keeps_feature_reporting_context_after_sibling_and_root_activity() {
    /// Separates deferred capture from later sibling and root observations.
    enum Action {
        /// Captures the feature collection in an effect without running it yet.
        Start,
        /// Reports through the sibling and root collections.
        Report,
    }

    for cancelled in [false, true] {
        let events = Arc::new(Mutex::new(Vec::new()));
        let endpoint = |name| {
            let events = Arc::clone(&events);
            IssueReporters::new([IssueReporter::new(move |issue| {
                events.lock().unwrap().push((name, issue.code));
            })])
        };
        let feature = rca_core::Reduce::new(
            |(): &mut (), action: &Action, context: &mut ReduceContext<'_>| {
                if !matches!(action, Action::Start) {
                    return Effect::none();
                }
                let selected = context.dependency::<ReportingKey>().clone();
                Effect::run(move |_| async move {
                    assert!(
                        selected
                            .report(Issue::new("deferred", "captured feature context"))
                            .into_iter()
                            .all(|result| result.is_ok())
                    );
                })
            },
        );
        let reports = |code| {
            rca_core::Reduce::new(
                move |(): &mut (), action: &Action, context: &mut ReduceContext<'_>| {
                    if matches!(action, Action::Report) {
                        assert!(
                            context
                                .dependency::<ReportingKey>()
                                .report(Issue::new(code, "later reduction"))
                                .into_iter()
                                .all(|result| result.is_ok())
                        );
                    }
                    Effect::none()
                },
            )
        };
        let runtime = support::ManualRuntime::new();
        let reducer = rca_core::combine((
            with_dependency::<ReportingKey, _>(endpoint("feature"), feature),
            with_dependency::<ReportingKey, _>(endpoint("sibling"), reports("sibling")),
            reports("root"),
        ));
        let store = Store::builder((), reducer, runtime.runtime())
            .with_dependency::<ReportingKey>(endpoint("root"))
            .build()
            .unwrap();
        let mut actor = runtime.take_only_queued_task();
        let mut poll_context = Context::from_waker(Waker::noop());
        let started = store.try_send(Action::Start).unwrap();
        assert!(actor.as_mut().poll(&mut poll_context).is_pending());
        let task = futures_executor::block_on(started.finish()).unwrap();
        let mut effect = runtime.take_only_queued_task();
        assert!(events.lock().unwrap().is_empty());

        let later = store.try_send(Action::Report).unwrap();
        assert!(actor.as_mut().poll(&mut poll_context).is_pending());
        let later_task = futures_executor::block_on(later.finish()).unwrap();
        assert!(later_task.is_finished());
        assert_eq!(
            *events.lock().unwrap(),
            [("sibling", "sibling"), ("root", "root")]
        );
        assert!(!task.is_finished());
        if cancelled {
            task.cancel();
            assert!(actor.as_mut().poll(&mut poll_context).is_pending());
        }
        assert!(effect.as_mut().poll(&mut poll_context).is_ready());
        drop(effect);
        assert!(actor.as_mut().poll(&mut poll_context).is_pending());
        assert!(task.is_finished());
        assert_eq!(task.is_cancelled(), cancelled);
        if cancelled {
            assert_eq!(
                *events.lock().unwrap(),
                [("sibling", "sibling"), ("root", "root")]
            );
        } else {
            assert_eq!(
                *events.lock().unwrap(),
                [
                    ("sibling", "sibling"),
                    ("root", "root"),
                    ("feature", "deferred"),
                ]
            );
        }
        drop(actor);
    }
}

#[test]
fn standalone_callbacks_can_reenter_the_owned_collection_without_deadlock() {
    let target = Arc::new(Mutex::new(None::<IssueReporters>));
    let events = Arc::new(Mutex::new(Vec::new()));
    let endpoint = {
        let target = Arc::downgrade(&target);
        let events = Arc::clone(&events);
        IssueReporter::new(move |issue| {
            events.lock().unwrap().push(issue.code);
            if issue.code == "outer" {
                let target = target.upgrade().expect("test retains the reentry target");
                let chosen = target.lock().unwrap().as_ref().unwrap().clone();
                assert!(
                    chosen
                        .report(Issue::new("nested", "reentrant"))
                        .into_iter()
                        .all(|result| result.is_ok())
                );
            }
        })
    };
    let context = IssueReporters::new([endpoint]);
    *target.lock().unwrap() = Some(context.clone());
    assert!(
        context
            .report(Issue::new("outer", "outside guards"))
            .into_iter()
            .all(|result| result.is_ok())
    );
    assert_eq!(*events.lock().unwrap(), ["outer", "nested"]);
    drop(target.lock().unwrap().take());
}

#[test]
fn callback_panic_propagates_stops_the_tail_and_leaves_scope_selection_unchanged() {
    let tail_called = Arc::new(AtomicBool::new(false));
    let panicking = IssueReporter::new(|_| panic!("handler panic"));
    let tail = {
        let called = Arc::clone(&tail_called);
        IssueReporter::new(move |_| called.store(true, Ordering::Release))
    };
    let selected = IssueReporters::new([panicking, tail]);
    let parent_events = Arc::new(Mutex::new(Vec::new()));
    let parent = IssueReporters::new([recording(&parent_events)]);
    let panic = catch_unwind(AssertUnwindSafe(|| {
        parent.with_scope(Some(&selected), |context| {
            context.report(Issue::new("panic", "not caught"))
        })
    }));
    assert_eq!(
        panic.unwrap_err().downcast_ref::<&str>(),
        Some(&"handler panic")
    );
    assert!(!tail_called.load(Ordering::Acquire));
    assert!(
        parent
            .with_scope(None, |context| context
                .report(Issue::new("parent", "unchanged")))
            .into_iter()
            .all(|result| result.is_ok())
    );
    assert_eq!(parent_events.lock().unwrap()[0].code, "parent");
}

/// Keeps store lifetime tests independent of reducer work.
#[cfg(feature = "test-util")]
struct NoWork;

#[cfg(feature = "test-util")]
impl Reducer for NoWork {
    type State = ();
    type Action = ();
    fn reduce(&mut self, (): &mut (), (): &(), _: &mut ReduceContext<'_>) -> Effect<()> {
        Effect::none()
    }
}

#[cfg(feature = "test-util")]
#[test]
fn closed_endpoint_recovers_payload_and_still_attempts_later_members() {
    let recorder = rca_core::testing::IssueRecorder::new();
    let runtime = support::ManualRuntime::new();
    let store = Store::builder((), NoWork, runtime.runtime())
        .issue_recorder(recorder.clone())
        .build()
        .unwrap();
    let events = Arc::new(Mutex::new(Vec::new()));
    let context = IssueReporters::new([recorder.reporter(), recording(&events)]);
    drop(runtime.take_only_queued_task());
    let original = Issue::new("closed", String::from("retained"))
        .with_severity(IssueSeverity::Note)
        .with_cause(String::from("cause"));
    let expected = original.clone();
    let outcomes = context.report(original);
    assert!(matches!(outcomes.as_slice(), [Err(error), Ok(())] if error.is_closed()));
    assert_eq!(
        outcomes
            .into_iter()
            .next()
            .unwrap()
            .unwrap_err()
            .into_issue(),
        expected
    );
    assert_eq!(*events.lock().unwrap(), [expected]);
    assert!(recorder.snapshot().is_empty());
    drop(store);
}

#[test]
fn retained_store_endpoint_rejects_late_issue_after_shutdown_without_recorder() {
    let runtime = support::ManualRuntime::new();
    let exported = Arc::new(Mutex::new(None::<IssueReporter>));
    let output = Arc::clone(&exported);
    let events = Arc::new(Mutex::new(Vec::new()));
    let observed = Arc::clone(&events);
    let reducer = rca_core::Reduce::new(
        move |(): &mut (), (): &(), context: &mut ReduceContext<'_>| {
            let reporter = context.issue_reporter();
            reporter.report(Issue::new("live", "accepted")).unwrap();
            *output.lock().unwrap() = Some(reporter);
            Effect::none()
        },
    );
    let store = Store::builder((), reducer, runtime.runtime())
        .with_issue_handler(move |issue| observed.lock().unwrap().push(issue.clone()))
        .build()
        .unwrap();
    let mut actor = runtime.take_only_queued_task();
    let receipt = store.try_send(()).unwrap();
    let mut cx = Context::from_waker(Waker::noop());
    assert!(actor.as_mut().poll(&mut cx).is_pending());
    let mut receipt = std::pin::pin!(receipt.finish());
    let Poll::Ready(Ok(task)) = receipt.as_mut().poll(&mut cx) else {
        panic!("the actor must acknowledge the completed reduction");
    };
    assert!(task.is_finished());
    assert_eq!(events.lock().unwrap().len(), 1);
    assert_eq!(events.lock().unwrap()[0].code, "live");
    let reporter = exported.lock().unwrap().take().unwrap();

    let mut shutdown = Box::pin(store.shutdown());
    assert!(shutdown.as_mut().poll(&mut cx).is_pending());
    assert!(actor.as_mut().poll(&mut cx).is_ready());
    assert!(matches!(
        shutdown.as_mut().poll(&mut cx),
        Poll::Ready(Ok(()))
    ));
    let issue = Issue::new("late", String::from("retained payload"))
        .with_severity(IssueSeverity::Error)
        .with_cause(String::from("retained cause"));
    let rejected = reporter.report(issue.clone()).unwrap_err();
    assert!(rejected.is_closed());
    assert!(!rejected.is_disabled());
    assert_eq!(rejected.into_issue(), issue);
    assert_eq!(events.lock().unwrap().len(), 1);
}

#[cfg(feature = "test-util")]
#[test]
fn admitted_callback_can_outlive_store_closure_without_forged_completion() {
    let recorder = rca_core::testing::IssueRecorder::new();
    let runtime = support::ManualRuntime::new();
    let (entered, entrance) = mpsc::channel();
    let (release, released) = mpsc::channel();
    let released = Mutex::new(released);
    let completed = Arc::new(AtomicBool::new(false));
    let store = Store::builder((), NoWork, runtime.runtime())
        .issue_recorder(recorder.clone())
        .with_issue_handler({
            let completed = Arc::clone(&completed);
            move |_| {
                entered
                    .send(())
                    .expect("parent must observe callback entry");
                // Parent unwinding drops the sender before the scope joins us.
                if released
                    .lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(30))
                    .is_err()
                {
                    return;
                }
                completed.store(true, Ordering::Release);
            }
        })
        .build()
        .unwrap();
    let context = IssueReporters::new([recorder.reporter()]);
    std::thread::scope(|threads| {
        let callback_release = release;
        let reporting = threads.spawn(|| context.report(Issue::new("accepted", "paused callback")));
        entrance
            .recv_timeout(Duration::from_secs(30))
            .expect("callback must enter");
        assert_eq!(recorder.snapshot()[0].code, "accepted");
        assert!(!completed.load(Ordering::Acquire));
        drop(runtime.take_only_queued_task());
        let outcomes = context.report(Issue::new("late", "recovered"));
        let error = outcomes.into_iter().next().unwrap().unwrap_err();
        assert!(error.is_closed());
        assert_eq!(error.into_issue().code, "late");
        assert!(!completed.load(Ordering::Acquire));
        assert_eq!(recorder.snapshot().len(), 1);
        callback_release
            .send(())
            .expect("callback must remain paused until release");
        assert!(matches!(reporting.join().unwrap().as_slice(), [Ok(())]));
        assert!(completed.load(Ordering::Acquire));
    });
    drop(store);
}
