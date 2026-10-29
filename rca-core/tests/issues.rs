//! Checks issue admission through shutdown, reentry and owned resource destruction.

#![cfg(feature = "test-util")]

#[expect(dead_code, reason = "manual actor polling needs only captured tasks")]
mod support;

use rca_core::{
    DependencyKey, Effect, Issue, IssueReporter, ReduceContext, Reducer, Store,
    testing::IssueRecorder,
};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier, Mutex, mpsc};
use std::task::{Context, Poll, Waker};
use std::time::Duration;

/// Reports destruction while the store still owns its diagnostic endpoint.
struct ReportsOnDrop {
    /// Receives the destructor's observation.
    reporter: IssueReporter,
    /// Distinguishes state, reducer, dependency and temporary override destruction.
    code: &'static str,
}

impl Drop for ReportsOnDrop {
    fn drop(&mut self) {
        let result = self.reporter.report(Issue::new(self.code, "dropped"));
        // Keep an original panic intact. Later assertions still require every report.
        if !std::thread::panicking() {
            result.expect("destruction must precede endpoint closure");
        }
    }
}

/// Adds an independently owned destructor to the store's dependency environment.
struct Key;

impl DependencyKey for Key {
    type Value = ReportsOnDrop;

    fn live_value() -> Self::Value {
        panic!("explicit dependency")
    }
}
/// Reports from an operation override and retains a reducer-owned destructor.
struct Feature {
    /// Observes reducer destruction without changing reduction behaviour.
    _drop: ReportsOnDrop,
}

impl Reducer for Feature {
    type State = ReportsOnDrop;
    type Action = ();

    fn reduce(
        &mut self,
        _: &mut Self::State,
        (): &(),
        context: &mut ReduceContext<'_>,
    ) -> Effect<()> {
        context.with_dependency_override::<Key, _>(
            ReportsOnDrop {
                reporter: context.issue_reporter(),
                code: "override",
            },
            |child| {
                child
                    .report_issue(Issue::new("child", "same endpoint"))
                    .unwrap();
            },
        );
        Effect::none()
    }
}

#[test]
fn graceful_shutdown_seals_after_state_reducer_and_dependency_destruction() {
    let runtime = support::ManualRuntime::new();
    let recorder = IssueRecorder::default();
    let reporter = recorder.reporter();
    let probe = |code| ReportsOnDrop {
        reporter: reporter.clone(),
        code,
    };
    let store = Store::builder(
        probe("state"),
        Feature {
            _drop: probe("reducer"),
        },
        runtime.runtime(),
    )
    .with_dependency::<Key>(probe("dependency"))
    .issue_recorder(recorder.clone())
    .build()
    .unwrap();
    let mut actor = runtime.take_only_queued_task();
    let mut context = Context::from_waker(Waker::noop());
    let mut send = Box::pin(store.send(()));
    assert!(send.as_mut().poll(&mut context).is_pending());
    assert!(actor.as_mut().poll(&mut context).is_pending());
    assert!(matches!(
        send.as_mut().poll(&mut context),
        Poll::Ready(Ok(_))
    ));
    let mut shutdown = Box::pin(store.shutdown());
    assert!(shutdown.as_mut().poll(&mut context).is_pending());
    assert!(actor.as_mut().poll(&mut context).is_ready());
    assert!(matches!(
        shutdown.as_mut().poll(&mut context),
        Poll::Ready(Ok(()))
    ));
    assert_eq!(
        recorder
            .snapshot()
            .iter()
            .map(|issue| issue.code)
            .collect::<Vec<_>>(),
        ["child", "override", "state", "reducer", "dependency"]
    );
    assert!(
        reporter
            .report(Issue::new("late", "rejected"))
            .unwrap_err()
            .is_closed()
    );
}

#[test]
fn unpolled_and_suspended_actor_drop_seal_after_owned_fields() {
    for poll_first in [false, true] {
        let runtime = support::ManualRuntime::new();
        let recorder = IssueRecorder::new();
        let reporter = recorder.reporter();
        let probe = |code| ReportsOnDrop {
            reporter: reporter.clone(),
            code,
        };
        let store = Store::builder(
            probe("state"),
            Feature {
                _drop: probe("reducer"),
            },
            runtime.runtime(),
        )
        .with_dependency::<Key>(probe("dependency"))
        .issue_recorder(recorder.clone())
        .build()
        .unwrap();
        let mut actor = runtime.take_only_queued_task();
        if poll_first {
            assert!(
                actor
                    .as_mut()
                    .poll(&mut Context::from_waker(Waker::noop()))
                    .is_pending()
            );
        }
        drop(actor);
        assert_eq!(
            recorder
                .snapshot()
                .iter()
                .map(|issue| issue.code)
                .collect::<Vec<_>>(),
            ["state", "reducer", "dependency"]
        );
        assert!(
            reporter
                .report(Issue::new("late", "rejected"))
                .unwrap_err()
                .is_closed()
        );
        drop(store);
    }
}

#[test]
fn handler_runs_after_recording_and_closure_does_not_wait_for_it() {
    /// Releases the known-paused handler before a scoped join during unwinding.
    struct ReleaseHandler<'a>(&'a Barrier);

    impl Drop for ReleaseHandler<'_> {
        fn drop(&mut self) {
            self.0.wait();
        }
    }

    let recorder = IssueRecorder::new();
    let entered = Arc::new(Barrier::new(2));
    let release = Arc::new(Barrier::new(2));
    let runtime = support::ManualRuntime::new();
    let store = Store::builder((), Capture, runtime.runtime())
        .issue_recorder(recorder.clone())
        .with_issue_handler({
            let entered = Arc::clone(&entered);
            let release = Arc::clone(&release);
            move |issue| {
                if issue.code == "accepted" {
                    entered.wait();
                    release.wait();
                }
            }
        })
        .build()
        .unwrap();
    std::thread::scope(|scope| {
        let reporter = recorder.reporter();
        let reporting =
            scope.spawn(move || reporter.report(Issue::new("accepted", "paused handler")));
        entered.wait();
        // A failing assertion must release the worker before scope joins it.
        let release_handler = ReleaseHandler(&release);
        assert_eq!(recorder.snapshot()[0].code, "accepted");
        drop(runtime.take_only_queued_task());
        assert!(
            recorder
                .reporter()
                .report(Issue::new("late", "closed"))
                .unwrap_err()
                .is_closed()
        );
        drop(release_handler);
        reporting.join().unwrap().unwrap();
    });
    drop(store);
}

#[test]
fn handler_can_reenter_reporter_without_deadlock() {
    let recorder = IssueRecorder::new();
    let reporter = recorder.reporter();
    let reentry = Arc::new(Mutex::new(Some(reporter.clone())));
    let handler_reentry = Arc::downgrade(&reentry);
    let runtime = support::ManualRuntime::new();
    let store = Store::builder(
        ReportsOnDrop {
            reporter: reporter.clone(),
            code: "state",
        },
        Feature {
            _drop: ReportsOnDrop {
                reporter: reporter.clone(),
                code: "reducer",
            },
        },
        runtime.runtime(),
    )
    .issue_recorder(recorder.clone())
    .with_issue_handler(move |issue| {
        if issue.code == "first" {
            handler_reentry
                .upgrade()
                .expect("test retains the reentry target")
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .report(Issue::new("nested", "reentrant"))
                .unwrap();
        }
    })
    .build()
    .unwrap();
    recorder
        .reporter()
        .report(Issue::new("first", "outer"))
        .unwrap();
    assert_eq!(
        recorder
            .snapshot()
            .iter()
            .map(|issue| issue.code)
            .collect::<Vec<_>>(),
        ["first", "nested"]
    );
    // Release the retained reporter before checking actor destruction.
    drop(reentry.lock().unwrap().take());
    drop(runtime.take_only_queued_task());
    drop(store);
}

#[test]
fn concurrent_admissions_and_closure_record_every_accepted_issue_once() {
    let recorder = IssueRecorder::new();
    let accepted = AtomicUsize::new(0);
    let (ready, readiness) = mpsc::channel();
    let runtime = support::ManualRuntime::new();
    let store = Store::builder((), Capture, runtime.runtime())
        .issue_recorder(recorder.clone())
        .build()
        .unwrap();
    let mut expected = std::thread::scope(|scope| {
        // Both collections drop before the scope joins workers during unwinding.
        let mut worker_gates = Vec::new();
        let mut workers = Vec::new();
        for worker in 0..8 {
            let (release, released) = mpsc::channel();
            worker_gates.push(release);
            let ready = ready.clone();
            let recorder = &recorder;
            let accepted = &accepted;
            workers.push(scope.spawn(move || {
                let first = Issue::new("before", format!("{worker}:before"));
                recorder.reporter().report(first.clone()).unwrap();
                let mut admitted = vec![first];
                accepted.fetch_add(1, Ordering::SeqCst);
                ready
                    .send(())
                    .expect("parent must observe initial admission");
                drop(ready);
                // A failed peer closes the release channels before scoped joins.
                if released.recv_timeout(Duration::from_secs(30)).is_err() {
                    return admitted;
                }
                for index in 0..100 {
                    let issue = Issue::new("race", format!("{worker}:{index}"));
                    match recorder.reporter().report(issue.clone()) {
                        Ok(()) => {
                            accepted.fetch_add(1, Ordering::SeqCst);
                            admitted.push(issue);
                        }
                        Err(error) => {
                            assert!(error.is_closed());
                            assert_eq!(error.into_issue(), issue);
                        }
                    }
                }
                admitted
            }));
        }
        drop(ready);
        for _ in 0..8 {
            readiness
                .recv_timeout(Duration::from_secs(30))
                .expect("all initial admissions must finish");
        }
        for release in worker_gates {
            release
                .send(())
                .expect("worker must wait for the admission race");
        }
        drop(runtime.take_only_queued_task());
        workers
            .into_iter()
            .flat_map(|worker| worker.join().unwrap())
            .collect::<Vec<_>>()
    });
    let mut captured = recorder.snapshot();
    assert_eq!(captured.len(), accepted.load(Ordering::SeqCst));
    assert!(accepted.load(Ordering::SeqCst) >= 8);
    captured.sort_by(|left, right| left.message.cmp(&right.message));
    expected.sort_by(|left, right| left.message.cmp(&right.message));
    assert_eq!(captured, expected);
    assert!(
        recorder
            .reporter()
            .report(Issue::new("late", "closed"))
            .unwrap_err()
            .is_closed()
    );
    drop(store);
}

/// Creates a store endpoint without producing reducer effects.
struct Capture;

impl Reducer for Capture {
    type State = ();
    type Action = ();

    fn reduce(&mut self, (): &mut (), (): &(), _: &mut ReduceContext<'_>) -> Effect<()> {
        Effect::none()
    }
}

#[test]
fn completed_actor_seals_even_when_runtime_retains_completed_future() {
    let runtime = support::ManualRuntime::new();
    let recorder = IssueRecorder::new();
    let reporter = recorder.reporter();
    let store = Store::builder(
        ReportsOnDrop {
            reporter: reporter.clone(),
            code: "state",
        },
        Feature {
            _drop: ReportsOnDrop {
                reporter,
                code: "reducer",
            },
        },
        runtime.runtime(),
    )
    .issue_recorder(recorder.clone())
    .build()
    .unwrap();
    let mut actor = runtime.take_only_queued_task();
    drop(store);
    assert!(
        actor
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_ready()
    );
    assert_eq!(
        recorder
            .snapshot()
            .iter()
            .map(|issue| issue.code)
            .collect::<Vec<_>>(),
        ["state", "reducer"]
    );
    assert!(
        recorder
            .reporter()
            .report(Issue::new("late", "closed"))
            .unwrap_err()
            .is_closed()
    );
}

#[test]
fn panicking_actor_drops_fields_before_outer_guard_closes() {
    /// Starts the intended unwind while retaining a reducer-owned destructor.
    struct Panics {
        /// Reports when the panicking reducer is destroyed.
        _drop: ReportsOnDrop,
    }

    impl Reducer for Panics {
        type State = ReportsOnDrop;
        type Action = ();
        fn reduce(
            &mut self,
            _: &mut Self::State,
            (): &(),
            _: &mut ReduceContext<'_>,
        ) -> Effect<()> {
            panic!("invariant");
        }
    }
    let runtime = support::ManualRuntime::new();
    let recorder = IssueRecorder::new();
    let reporter = recorder.reporter();
    let probe = |code| ReportsOnDrop {
        reporter: reporter.clone(),
        code,
    };
    let store = Store::builder(
        probe("state"),
        Panics {
            _drop: probe("reducer"),
        },
        runtime.runtime(),
    )
    .issue_recorder(recorder.clone())
    .build()
    .unwrap();
    let mut actor = runtime.take_only_queued_task();
    let mut context = Context::from_waker(Waker::noop());
    let mut send = Box::pin(store.send(()));
    assert!(send.as_mut().poll(&mut context).is_pending());
    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        actor.as_mut().poll(&mut context)
    }))
    .unwrap_err();
    assert_eq!(panic.downcast_ref::<&str>(), Some(&"invariant"));
    drop(actor);
    assert_eq!(
        recorder
            .snapshot()
            .iter()
            .map(|issue| issue.code)
            .collect::<Vec<_>>(),
        ["state", "reducer"]
    );
    assert!(
        reporter
            .report(Issue::new("late", "closed"))
            .unwrap_err()
            .is_closed()
    );
}

#[test]
fn panicking_handler_keeps_recorded_evidence_and_leaves_endpoint_usable() {
    let runtime = support::ManualRuntime::new();
    let recorder = IssueRecorder::new();
    let store = Store::builder((), Capture, runtime.runtime())
        .issue_recorder(recorder.clone())
        .with_issue_handler(|issue| {
            assert_ne!(issue.code, "panic", "handler");
        })
        .build()
        .unwrap();
    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        recorder.reporter().report(Issue::new("panic", "retained"))
    }))
    .unwrap_err();
    assert_eq!(
        panic.downcast_ref::<String>().unwrap().lines().next(),
        Some("assertion `left != right` failed: handler")
    );
    recorder
        .reporter()
        .report(Issue::new("next", "not poisoned"))
        .unwrap();
    assert_eq!(
        recorder
            .snapshot()
            .iter()
            .map(|issue| issue.code)
            .collect::<Vec<_>>(),
        ["panic", "next"]
    );
    drop(runtime.take_only_queued_task());
    drop(store);
}

#[test]
fn runtime_owned_effect_destructor_after_abort_receives_closed_with_its_payload() {
    /// Retains an endpoint until the runtime releases this future after abort.
    struct LateReport {
        /// Attempts a diagnostic after the actor has ended.
        reporter: IssueReporter,
        /// Transfers rejected evidence back to the test.
        rejected: Arc<Mutex<Option<rca_core::IssueReportError>>>,
    }

    impl Future for LateReport {
        type Output = ();
        fn poll(self: std::pin::Pin<&mut Self>, _: &mut Context<'_>) -> Poll<()> {
            Poll::Pending
        }
    }
    impl Drop for LateReport {
        fn drop(&mut self) {
            let rejected = self
                .reporter
                .report(Issue::new("effect-drop", "late payload"))
                .unwrap_err();
            *self.rejected.lock().unwrap() = Some(rejected);
        }
    }
    /// Creates the runtime-owned future without polling its body.
    struct Launch(Arc<Mutex<Option<rca_core::IssueReportError>>>);

    impl Reducer for Launch {
        type State = ();
        type Action = ();
        fn reduce(&mut self, (): &mut (), (): &(), context: &mut ReduceContext<'_>) -> Effect<()> {
            let future = LateReport {
                reporter: context.issue_reporter(),
                rejected: Arc::clone(&self.0),
            };
            Effect::run(move |_| future)
        }
    }
    let runtime = support::ManualRuntime::new();
    let recorder = IssueRecorder::new();
    let rejected = Arc::new(Mutex::new(None));
    let store = Store::builder((), Launch(Arc::clone(&rejected)), runtime.runtime())
        .issue_recorder(recorder.clone())
        .build()
        .unwrap();
    let mut actor = runtime.take_only_queued_task();
    let receipt = store.try_send(()).unwrap();
    assert!(
        actor
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_pending()
    );
    let _task = futures_executor::block_on(receipt.finish()).unwrap();
    let effect = runtime.take_only_queued_task();
    drop(actor);
    assert!(rejected.lock().unwrap().is_none());
    drop(effect);
    let error = rejected.lock().unwrap().take().unwrap();
    assert!(error.is_closed());
    assert_eq!(error.into_issue().message, "late payload");
    assert!(recorder.snapshot().is_empty());
}
