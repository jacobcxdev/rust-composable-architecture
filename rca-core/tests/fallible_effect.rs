//! Checks error identity, cancellation, reporting and cleanup with explicit polls.

#[expect(dead_code, reason = "fallible tests manually poll runtime tasks")]
mod support;

use std::cell::Cell;
use std::convert::Infallible;
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

use rca_core::{
    ActionTask, Effect, EffectError, Issue, IssueReporter, ReduceContext, Reducer, RuntimeFuture,
    Store,
};
use rca_core::{IssueDelivery, OperationReport};

/// Starts one supplied effect and counts only its expected follow-up action.
struct Feature {
    effect: Option<Effect<u8>>,
    commits: Arc<AtomicUsize>,
}

impl Reducer for Feature {
    type State = ();
    type Action = u8;

    fn reduce(&mut self, (): &mut (), action: &u8, _: &mut ReduceContext<'_>) -> Effect<u8> {
        if *action == 0 {
            self.effect.take().expect("one launch")
        } else {
            assert_eq!(*action, 7);
            self.commits.fetch_add(1, Ordering::SeqCst);
            Effect::none()
        }
    }
}

/// Keeps Store ownership while tests poll its actor and effect separately.
struct Launched {
    /// Prevents dropping the last Store handle from cancelling the tested work.
    _store: Store<(), u8>,
    actor: RuntimeFuture,
    run: RuntimeFuture,
    task: ActionTask,
    commits: Arc<AtomicUsize>,
    reports: Arc<AtomicUsize>,
}

/// Commits the start action once and returns the still-unpolled effect task.
fn launch(runtime: &support::ManualRuntime, effect: Effect<u8>) -> Launched {
    let commits = Arc::new(AtomicUsize::new(0));
    let reports = Arc::new(AtomicUsize::new(0));
    let report_count = Arc::clone(&reports);
    let store = Store::builder(
        (),
        Feature {
            effect: Some(effect),
            commits: Arc::clone(&commits),
        },
        runtime.runtime(),
    )
    .with_issue_handler(move |_| {
        report_count.fetch_add(1, Ordering::SeqCst);
    })
    .build()
    .unwrap();
    let mut actor = runtime.take_only_queued_task();
    let receipt = store.try_send(0).unwrap();
    assert!(
        actor
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_pending()
    );
    let mut finish = std::pin::pin!(receipt.finish());
    let Poll::Ready(Ok(task)) = finish
        .as_mut()
        .poll(&mut Context::from_waker(Waker::noop()))
    else {
        panic!("the actor poll must commit the start action");
    };
    let run = runtime.take_only_queued_task();
    Launched {
        _store: store,
        actor,
        run,
        task,
        commits,
        reports,
    }
}

/// Deliberately neither `Clone` nor `Debug`: identity must survive error routing.
struct Cause {
    value: Box<u8>,
    drops: Arc<AtomicUsize>,
}

impl Drop for Cause {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}

#[test]
fn handled_error_preserves_original_cause_and_scoped_sender_without_reporting() {
    for mapped in [false, true] {
        let runtime = support::ManualRuntime::new();
        let operations = Arc::new(AtomicUsize::new(0));
        let handlers = Arc::new(AtomicUsize::new(0));
        let drops = Arc::new(AtomicUsize::new(0));
        let cause = Cause {
            value: Box::new(7),
            drops: Arc::clone(&drops),
        };
        let address = std::ptr::from_ref(&*cause.value).addr();
        let operation_count = Arc::clone(&operations);
        let handler_count = Arc::clone(&handlers);
        let effect = Effect::try_run(
            move |_| async move {
                operation_count.fetch_add(1, Ordering::SeqCst);
                let failure = EffectError::Failed(cause);
                assert_eq!(format!("{failure:?}"), "EffectError::Failed(..)");
                Err(failure)
            },
            move |cause, sender| async move {
                handler_count.fetch_add(1, Ordering::SeqCst);
                assert_eq!(std::ptr::from_ref(&*cause.value).addr(), address);
                sender.send(*cause.value).await.unwrap();
            },
        );
        let effect = if mapped {
            effect.map(|value| value).map(|value| value)
        } else {
            effect
        };
        let mut launched = launch(&runtime, effect);
        let mut cx = Context::from_waker(Waker::noop());
        assert_eq!(operations.load(Ordering::SeqCst), 0);
        assert!(launched.run.as_mut().poll(&mut cx).is_pending());
        assert!(launched.actor.as_mut().poll(&mut cx).is_pending());
        assert!(launched.run.as_mut().poll(&mut cx).is_ready());
        assert_eq!(operations.load(Ordering::SeqCst), 1);
        assert_eq!(handlers.load(Ordering::SeqCst), 1);
        assert_eq!(launched.reports.load(Ordering::SeqCst), 0);
        assert_eq!(launched.commits.load(Ordering::SeqCst), 1);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }
}

#[test]
fn unhandled_error_preserves_typed_cause_location_and_optional_delivery_evidence() {
    for enabled in [false, true] {
        let runtime = support::ManualRuntime::new();
        let operations = Arc::new(AtomicUsize::new(0));
        let reports = Arc::new(AtomicUsize::new(0));
        let outcomes = Arc::new(Mutex::new(Vec::new()));
        let issues = Arc::new(Mutex::new(Vec::new()));
        let drops = Arc::new(AtomicUsize::new(0));
        let cause = Cause {
            value: Box::new(7),
            drops: Arc::clone(&drops),
        };
        let address = std::ptr::from_ref(&*cause.value).addr();
        let report_count = Arc::clone(&reports);
        let reported_issues = Arc::clone(&issues);
        let reporter = if enabled {
            IssueReporter::new(move |issue| {
                report_count.fetch_add(1, Ordering::SeqCst);
                assert_eq!(issue.cause.as_deref(), Some("original cause"));
                reported_issues.lock().unwrap().push(issue.clone());
            })
        } else {
            IssueReporter::default()
        };
        let operation_count = Arc::clone(&operations);
        let retained = Arc::clone(&outcomes);
        let line = line!() + 1;
        let effect = Effect::try_run_reported(
            move |_| async move {
                operation_count.fetch_add(1, Ordering::SeqCst);
                Err(EffectError::Failed(cause))
            },
            reporter,
            |_| Issue::new("operation.failed", "failed").with_cause("original cause"),
            move |report| retained.lock().unwrap().push(report),
        );
        let mut launched = launch(&runtime, effect);
        assert!(
            launched
                .run
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_ready()
        );
        assert_eq!(operations.load(Ordering::SeqCst), 1);
        assert_eq!(reports.load(Ordering::SeqCst), usize::from(enabled));
        let outcome: OperationReport<(), Cause, Infallible> =
            outcomes.lock().unwrap().pop().unwrap();
        assert!(outcomes.lock().unwrap().is_empty());
        let cause = outcome.result.err().unwrap();
        assert_eq!(std::ptr::from_ref(&*cause.value).addr(), address);
        if enabled {
            assert!(matches!(
                outcome.deliveries.as_slice(),
                [IssueDelivery::Accepted]
            ));
            let issue = issues.lock().unwrap().pop().unwrap();
            assert_eq!(issue.location.file(), file!());
            assert_eq!(issue.location.line(), line);
        } else {
            let [IssueDelivery::Disabled(issue)] = outcome.deliveries.as_slice() else {
                panic!("missing required evidence")
            };
            assert_eq!(issue.location.file(), file!());
            assert_eq!(issue.location.line(), line);
        }
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        drop(cause);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }
}

#[test]
fn success_and_explicit_cancellation_reach_neither_handler_nor_report() {
    for cancelled in [false, true] {
        for handled in [false, true] {
            let runtime = support::ManualRuntime::new();
            let operation = move |_| async move {
                if cancelled {
                    let cancellation = EffectError::<u8>::Cancelled;
                    assert_eq!(format!("{cancellation:?}"), "EffectError::Cancelled");
                    Err(cancellation)
                } else {
                    Ok(())
                }
            };
            let effect = if handled {
                Effect::try_run(operation, |_, _| async { panic!("handler must not run") })
            } else {
                Effect::try_run_reported(
                    operation,
                    IssueReporter::new(|_| panic!("report must not run")),
                    |_| panic!("issue factory must not run"),
                    |_| panic!("outcome must not run"),
                )
            };
            let mut launched = launch(&runtime, effect);
            assert!(
                launched
                    .run
                    .as_mut()
                    .poll(&mut Context::from_waker(Waker::noop()))
                    .is_ready()
            );
            assert_eq!(launched.commits.load(Ordering::SeqCst), 0);
        }
    }
}

#[test]
fn cancellation_inside_error_poll_wins_before_handler_and_report_guard() {
    for handled in [false, true] {
        let runtime = support::ManualRuntime::new();
        let task = Arc::new(Mutex::new(None::<ActionTask>));
        let operation_task = Arc::clone(&task);
        let operation = move |_| async move {
            operation_task.lock().unwrap().as_ref().unwrap().cancel();
            Err(EffectError::Failed(7u8))
        };
        let effect = if handled {
            Effect::try_run(operation, |_, _| async { panic!("cancelled handler ran") })
        } else {
            Effect::try_run_reported(
                operation,
                IssueReporter::new(|_| panic!("cancelled report ran")),
                |_| panic!("cancelled issue factory ran"),
                |_| panic!("cancelled outcome ran"),
            )
        };
        let mut launched = launch(&runtime, effect);
        *task.lock().unwrap() = Some(launched.task.clone());
        assert!(
            launched
                .run
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_ready()
        );
        assert!(launched.task.is_cancelled());
        assert_eq!(launched.commits.load(Ordering::SeqCst), 0);
    }
}

#[test]
fn cancellation_after_handler_entry_rejects_its_stale_output() {
    let runtime = support::ManualRuntime::new();
    let task = Arc::new(Mutex::new(None::<ActionTask>));
    let handler_task = Arc::clone(&task);
    let rejections = Arc::new(AtomicUsize::new(0));
    let rejected = Arc::clone(&rejections);
    let effect = Effect::try_run(
        |_| async { Err(EffectError::Failed(7u8)) },
        move |value, sender| async move {
            handler_task.lock().unwrap().as_ref().unwrap().cancel();
            assert!(sender.send(value).await.unwrap_err().is_cancelled());
            rejected.fetch_add(1, Ordering::SeqCst);
        },
    );
    let mut launched = launch(&runtime, effect.map(|value| value));
    *task.lock().unwrap() = Some(launched.task.clone());
    let mut cx = Context::from_waker(Waker::noop());
    assert!(launched.run.as_mut().poll(&mut cx).is_ready());
    assert!(launched.actor.as_mut().poll(&mut cx).is_pending());
    assert_eq!(rejections.load(Ordering::SeqCst), 1);
    assert_eq!(launched.commits.load(Ordering::SeqCst), 0);
}

#[test]
fn error_created_and_consumed_within_one_poll_needs_no_send_clone_or_debug() {
    let runtime = support::ManualRuntime::new();
    let handled = Arc::new(AtomicUsize::new(0));
    let count = Arc::clone(&handled);
    let effect = Effect::try_run(
        |_| async { Err(EffectError::Failed(Rc::new(Cell::new(7)))) },
        move |cause, _| {
            assert_eq!(cause.get(), 7);
            count.fetch_add(1, Ordering::SeqCst);
            async {}
        },
    );
    let mut launched = launch(&runtime, effect);
    assert!(
        launched
            .run
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_ready()
    );
    assert_eq!(handled.load(Ordering::SeqCst), 1);
}

#[test]
fn reported_non_send_cause_stays_owned_through_factory_delivery_and_outcome() {
    let runtime = support::ManualRuntime::new();
    let stage = Arc::new(AtomicUsize::new(0));
    let factory_stage = Arc::clone(&stage);
    let reporter_stage = Arc::clone(&stage);
    let outcome_stage = Arc::clone(&stage);
    let effect = Effect::try_run_reported(
        |_| async { Err(EffectError::Failed(Rc::new(Cell::new(7)))) },
        IssueReporter::new(move |_| {
            assert_eq!(reporter_stage.fetch_add(1, Ordering::SeqCst), 1);
        }),
        move |cause| {
            assert_eq!(factory_stage.fetch_add(1, Ordering::SeqCst), 0);
            assert_eq!(Rc::strong_count(cause), 1);
            cause.set(9);
            Issue::new("failed", "owned cause")
        },
        move |report| {
            assert_eq!(outcome_stage.fetch_add(1, Ordering::SeqCst), 2);
            let cause = report.result.unwrap_err();
            assert_eq!(Rc::strong_count(&cause), 1);
            assert_eq!(cause.get(), 9);
            assert!(matches!(
                report.deliveries.as_slice(),
                [IssueDelivery::Accepted]
            ));
        },
    );
    let mut launched = launch(&runtime, effect);
    assert!(
        launched
            .run
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_ready()
    );
    assert_eq!(stage.load(Ordering::SeqCst), 3);
}

#[test]
fn reported_callback_panics_propagate_without_entering_later_callbacks() {
    for panic_at in 0..3 {
        let runtime = support::ManualRuntime::new();
        let stage = Arc::new(AtomicUsize::new(0));
        let factory_stage = Arc::clone(&stage);
        let reporter_stage = Arc::clone(&stage);
        let outcome_stage = Arc::clone(&stage);
        let drops = Arc::new(AtomicUsize::new(0));
        let cause = Cause {
            value: Box::new(7),
            drops: Arc::clone(&drops),
        };
        let effect = Effect::try_run_reported(
            move |_| async move { Err(EffectError::Failed(cause)) },
            IssueReporter::new(move |_| {
                assert_eq!(reporter_stage.fetch_add(1, Ordering::SeqCst), 1);
                if panic_at == 1 {
                    std::panic::panic_any(panic_at);
                }
            }),
            move |cause| {
                assert_eq!(factory_stage.fetch_add(1, Ordering::SeqCst), 0);
                assert_eq!(*cause.value, 7);
                if panic_at == 0 {
                    std::panic::panic_any(panic_at);
                }
                Issue::new("failed", "owned cause")
            },
            move |report| {
                assert_eq!(outcome_stage.fetch_add(1, Ordering::SeqCst), 2);
                assert_eq!(*report.result.err().unwrap().value, 7);
                std::panic::panic_any(panic_at);
            },
        );
        let mut launched = launch(&runtime, effect);
        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = launched
                .run
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()));
        }))
        .unwrap_err();
        assert_eq!(*panic.downcast::<usize>().unwrap(), panic_at);
        assert_eq!(stage.load(Ordering::SeqCst), panic_at + 1);
        drop(launched);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }
}

#[test]
fn operation_and_handler_panics_propagate_and_release_the_owned_cause() {
    // Operation construction, operation poll, handler construction, handler poll.
    for panic_at in 0..4 {
        let runtime = support::ManualRuntime::new();
        let stage = Arc::new(AtomicUsize::new(0));
        let operation_stage = Arc::clone(&stage);
        let handler_stage = Arc::clone(&stage);
        let drops = Arc::new(AtomicUsize::new(0));
        let cause = Cause {
            value: Box::new(7),
            drops: Arc::clone(&drops),
        };
        let effect = Effect::try_run(
            move |_| {
                assert_eq!(operation_stage.fetch_add(1, Ordering::SeqCst), 0);
                if panic_at == 0 {
                    std::panic::panic_any(panic_at);
                }
                async move {
                    assert_eq!(operation_stage.fetch_add(1, Ordering::SeqCst), 1);
                    if panic_at == 1 {
                        std::panic::panic_any(panic_at);
                    }
                    Err(EffectError::Failed(cause))
                }
            },
            move |cause, _| {
                assert_eq!(handler_stage.fetch_add(1, Ordering::SeqCst), 2);
                if panic_at == 2 {
                    std::panic::panic_any(panic_at);
                }
                async move {
                    assert_eq!(handler_stage.fetch_add(1, Ordering::SeqCst), 3);
                    assert_eq!(*cause.value, 7);
                    std::panic::panic_any(panic_at);
                }
            },
        );
        let mut launched = launch(&runtime, effect);
        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = launched
                .run
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()));
        }))
        .unwrap_err();
        assert_eq!(*panic.downcast::<usize>().unwrap(), panic_at);
        assert_eq!(stage.load(Ordering::SeqCst), panic_at + 1);
        drop(launched);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }
}

#[cfg(feature = "test-util")]
#[test]
fn closed_optional_endpoint_retains_required_unhandled_failure_evidence() {
    let runtime = support::ManualRuntime::new();
    let recorder = rca_core::testing::IssueRecorder::new();
    let reporter = recorder.reporter();
    let owner = Store::builder(
        (),
        Feature {
            effect: None,
            commits: Arc::new(AtomicUsize::new(0)),
        },
        runtime.runtime(),
    )
    .issue_recorder(recorder)
    .build()
    .unwrap();
    drop(runtime.take_only_queued_task());
    drop(owner);
    let outcomes = Arc::new(Mutex::new(Vec::new()));
    let retained = Arc::clone(&outcomes);
    let line = line!() + 1;
    let effect = Effect::try_run_reported(
        |_| async { Err(EffectError::Failed(7u8)) },
        reporter,
        |_| Issue::new("closed", "preserve failure"),
        move |report| retained.lock().unwrap().push(report),
    );
    let mut launched = launch(&runtime, effect);
    assert!(
        launched
            .run
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_ready()
    );
    let report = outcomes.lock().unwrap().pop().unwrap();
    assert_eq!(report.result, Err(7));
    let [IssueDelivery::Closed(issue)] = report.deliveries.as_slice() else {
        panic!("closed endpoint erased failure")
    };
    assert_eq!(issue.location.file(), file!());
    assert_eq!(issue.location.line(), line);
}

#[test]
fn unpolled_and_pending_effects_drop_owned_captures_once_without_failure_routing() {
    for poll in [false, true] {
        for handled in [false, true] {
            let runtime = support::ManualRuntime::new();
            let operations = Arc::new(AtomicUsize::new(0));
            let operation_count = Arc::clone(&operations);
            let drops = Arc::new(AtomicUsize::new(0));
            let cause = Cause {
                value: Box::new(7),
                drops: Arc::clone(&drops),
            };
            let operation = move |_| {
                operation_count.fetch_add(1, Ordering::SeqCst);
                async move {
                    std::future::pending::<()>().await;
                    Err(EffectError::Failed(cause))
                }
            };
            let effect = if handled {
                Effect::try_run(operation, |_, _| async {
                    panic!("dropped work was handled")
                })
            } else {
                Effect::try_run_reported(
                    operation,
                    IssueReporter::default(),
                    |_| panic!("dropped work made issue"),
                    |_| panic!("dropped work reported"),
                )
            };
            assert!(format!("{effect:?}").starts_with("Effect::"));
            assert_eq!(operations.load(Ordering::SeqCst), 0);
            assert_eq!(drops.load(Ordering::SeqCst), 0);
            let mut launched = launch(&runtime, effect);
            let mut cx = Context::from_waker(Waker::noop());
            if poll {
                assert!(launched.run.as_mut().poll(&mut cx).is_pending());
            }
            launched.task.cancel();
            assert!(launched.actor.as_mut().poll(&mut cx).is_pending());
            assert!(launched.run.as_mut().poll(&mut cx).is_ready());
            assert_eq!(operations.load(Ordering::SeqCst), usize::from(poll));
            assert_eq!(drops.load(Ordering::SeqCst), 1);
            drop(launched);
            assert_eq!(drops.load(Ordering::SeqCst), 1);
        }
    }
}

#[test]
fn pending_handler_keeps_cause_owned_until_cancellation_drops_it_once() {
    let runtime = support::ManualRuntime::new();
    let drops = Arc::new(AtomicUsize::new(0));
    let cause = Cause {
        value: Box::new(7),
        drops: Arc::clone(&drops),
    };
    let handlers = Arc::new(AtomicUsize::new(0));
    let handler_count = Arc::clone(&handlers);
    let effect = Effect::try_run(
        move |_| async move { Err(EffectError::Failed(cause)) },
        move |cause, sender| async move {
            handler_count.fetch_add(1, Ordering::SeqCst);
            std::future::pending::<()>().await;
            sender.send(*cause.value).await.unwrap();
        },
    );
    let mut launched = launch(&runtime, effect);
    let mut cx = Context::from_waker(Waker::noop());
    assert!(launched.run.as_mut().poll(&mut cx).is_pending());
    assert_eq!(handlers.load(Ordering::SeqCst), 1);
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    launched.task.cancel();
    assert!(launched.actor.as_mut().poll(&mut cx).is_pending());
    assert!(launched.run.as_mut().poll(&mut cx).is_ready());
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    assert_eq!(launched.commits.load(Ordering::SeqCst), 0);
    drop(launched);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
}

#[test]
fn concatenation_waits_for_the_fallible_handler_before_launching_its_continuation() {
    let runtime = support::ManualRuntime::new();
    let (release, gate) = futures_channel::oneshot::channel();
    let handled = Effect::try_run(
        |_| async { Err(EffectError::Failed(7u8)) },
        move |value, _| async move {
            assert_eq!(value, 7);
            gate.await.unwrap();
        },
    );
    let mut launched = launch(&runtime, Effect::concatenate([handled, Effect::send(7)]));
    let mut cx = Context::from_waker(Waker::noop());
    assert!(launched.run.as_mut().poll(&mut cx).is_pending());
    assert!(launched.actor.as_mut().poll(&mut cx).is_pending());
    assert_eq!(runtime.spawn_count(), 2);
    assert!(!launched.task.is_finished());
    release.send(()).unwrap();
    assert!(launched.run.as_mut().poll(&mut cx).is_ready());
    assert!(launched.actor.as_mut().poll(&mut cx).is_pending());
    let mut continuation = runtime.take_only_queued_task();
    assert!(continuation.as_mut().poll(&mut cx).is_pending());
    assert!(launched.actor.as_mut().poll(&mut cx).is_pending());
    assert!(continuation.as_mut().poll(&mut cx).is_ready());
    assert!(launched.actor.as_mut().poll(&mut cx).is_pending());
    assert_eq!(launched.commits.load(Ordering::SeqCst), 1);
    assert!(launched.task.is_finished());
}
