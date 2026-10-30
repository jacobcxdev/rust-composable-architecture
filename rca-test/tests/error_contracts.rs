use std::error::Error;
use std::task::Poll;
use std::time::Duration;

use futures_executor::block_on;
use rca_core::{Effect, MonotonicInstant, ReduceContext, Reducer};
use rca_test::{DeterministicRuntime, DriveError, TestStore};

/// Exercises admitted receipt failures while another driver owns the actor.
mod stopped_receipts {
    use super::*;
    use rca_core::{Feature, FeatureContext, Task};
    use rca_test::{TestActionTask, TestStoreError};
    use std::panic::{AssertUnwindSafe, catch_unwind, resume_unwind};
    use std::pin::Pin;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
        mpsc,
    };
    use std::task::{Context, Waker};

    // A timeout makes a failed rendezvous terminate instead of hanging the suite.
    const WAIT: Duration = Duration::from_secs(5);

    /// Records release of one action or update capture.
    struct Payload(Arc<AtomicUsize>);

    impl Drop for Payload {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// Holds the actor during cancellation, then fails on the worker thread.
    struct Timer {
        entered: mpsc::Sender<()>,
        release: mpsc::Receiver<()>,
        drops: Arc<AtomicUsize>,
    }

    impl Drop for Timer {
        fn drop(&mut self) {
            self.drops.fetch_add(1, Ordering::SeqCst);
            if std::thread::panicking() {
                return;
            }
            assert!(
                self.entered.send(()).is_err() || self.release.recv_timeout(WAIT).is_err(),
                "receipt actor teardown"
            );
        }
    }

    /// Selects the public receipt whose actor can stop before committing.
    #[derive(Clone, Copy)]
    enum Operation {
        Send,
        Receive,
        Update,
    }

    /// Starts independent work or carries the selected mutation payload.
    enum Action {
        StartTimer,
        StartOutput,
        StartUpdate,
        Apply(Payload),
        Timer(Timer),
    }

    /// Exposes commit and payload ownership without reading a stopped actor.
    struct Work {
        timer: Option<Timer>,
        commits: Arc<AtomicUsize>,
        drops: Arc<AtomicUsize>,
    }

    impl Feature for Work {
        type State = usize;
        type Action = Action;

        fn update(
            &mut self,
            state: &mut usize,
            action: &Action,
            context: &mut FeatureContext<'_, usize, Action>,
        ) {
            match action {
                Action::StartTimer => context.effect(Effect::send_after(
                    Duration::from_secs(60),
                    Action::Timer(self.timer.take().unwrap()),
                )),
                Action::StartOutput => {
                    context.effect(Effect::send(Action::Apply(Payload(Arc::clone(
                        &self.drops,
                    )))));
                }
                Action::StartUpdate => {
                    let payload = Payload(Arc::clone(&self.drops));
                    let commits = Arc::clone(&self.commits);
                    context.task(Task::run(move |context| async move {
                        let _ = context
                            .update(move |state| {
                                *state += 1;
                                commits.fetch_add(1, Ordering::SeqCst);
                                drop(payload);
                            })
                            .await;
                    }));
                }
                Action::Apply(payload) => {
                    std::hint::black_box(payload);
                    *state += 1;
                    self.commits.fetch_add(1, Ordering::SeqCst);
                }
                Action::Timer(timer) => {
                    std::hint::black_box(timer);
                    panic!("the cancelled timer must not run");
                }
            }
        }
    }

    /// Keeps the same admitted future alive until the separate driver stops the actor.
    fn assert_stopped_receipt(operation: Operation) {
        let (entered, entry) = mpsc::channel();
        let (release, resume) = mpsc::channel();
        let timer_drops = Arc::new(AtomicUsize::new(0));
        let drops = Arc::new(AtomicUsize::new(0));
        let commits = Arc::new(AtomicUsize::new(0));
        let mut store = TestStore::builder(
            0,
            Work {
                timer: Some(Timer {
                    entered,
                    release: resume,
                    drops: Arc::clone(&timer_drops),
                }),
                commits: Arc::clone(&commits),
                drops: Arc::clone(&drops),
            },
        )
        .build();
        let timer = block_on(store.send(Action::StartTimer)).unwrap();
        let producer = match operation {
            Operation::Send => None,
            Operation::Receive => Some(block_on(store.send(Action::StartOutput)).unwrap()),
            Operation::Update => Some(block_on(store.send(Action::StartUpdate)).unwrap()),
        };
        let worker =
            std::thread::spawn(move || catch_unwind(AssertUnwindSafe(|| block_on(timer.cancel()))));
        let callbacks = Arc::new(AtomicUsize::new(0));
        let called = Arc::clone(&callbacks);
        let expected = move |state: &mut usize| {
            called.fetch_add(1, Ordering::SeqCst);
            *state = 1;
        };
        let mut receipt: Pin<
            Box<dyn Future<Output = Result<TestActionTask, TestStoreError>> + '_>,
        > = match operation {
            Operation::Send => {
                Box::pin(store.send_with(Action::Apply(Payload(Arc::clone(&drops))), expected))
            }
            Operation::Receive => {
                Box::pin(store.receive_with(|action| matches!(action, Action::Apply(_)), expected))
            }
            Operation::Update => Box::pin(store.receive_update(expected)),
        };
        let mut context = Context::from_waker(Waker::noop());
        let observed = catch_unwind(AssertUnwindSafe(|| {
            entry.recv_timeout(WAIT).expect("worker holds the actor");
            assert!(receipt.as_mut().poll(&mut context).is_pending());
            assert_eq!(callbacks.load(Ordering::SeqCst), 1);
            assert_eq!(commits.load(Ordering::SeqCst), 0);
            assert_eq!(drops.load(Ordering::SeqCst), 0);
        }));
        // Release and join before propagating an assertion failure on this thread.
        let _ = release.send(());
        let joined = worker.join();
        observed.unwrap_or_else(|panic| resume_unwind(panic));
        let panic = joined
            .unwrap()
            .expect_err("only the worker's cancellation future panics");
        assert_eq!(
            panic.downcast_ref::<&str>(),
            Some(&"receipt actor teardown")
        );
        let Poll::Ready(Err(error)) = receipt.as_mut().poll(&mut context) else {
            panic!("the retained receipt must resolve after actor destruction");
        };
        drop(receipt);
        assert!(error.source().unwrap().is::<rca_core::StoreStopped>());
        assert!(error.drive_error().is_none());
        assert_eq!(callbacks.load(Ordering::SeqCst), 1);
        assert_eq!(commits.load(Ordering::SeqCst), 0);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert_eq!(timer_drops.load(Ordering::SeqCst), 1);
        assert!(
            block_on(store.finish())
                .unwrap_err()
                .is_incomplete_assertion()
        );
        assert!(catch_unwind(AssertUnwindSafe(|| drop(store))).is_err());
        assert!(producer.is_none_or(|task| task.is_finished()));
    }

    #[test]
    fn admitted_receipts_report_actor_loss_without_committing_or_losing_debt() {
        for operation in [Operation::Send, Operation::Receive, Operation::Update] {
            assert_stopped_receipt(operation);
        }
    }
}

fn assert_failure_kind(error: &DriveError, expected: [bool; 6], reason: &str) {
    assert_eq!(
        [
            error.is_step_budget_exhausted(),
            error.is_deadline_budget_exhausted(),
            error.is_blocked(),
            error.is_host_closed(),
            error.is_partial_advance(),
            error.is_pending_time(),
        ],
        expected,
    );
    let display = error.to_string();
    assert!(display.starts_with(reason), "{display}");
    assert!(display.ends_with(&error.backtrace().to_string()));
    assert!(error.source().is_none());
}

#[test]
fn temporal_rejections_distinguish_pending_time_from_exhausted_deadline_budget() {
    // Confusing these failures would suggest more budget for an assertion-only operation.
    let host = DeterministicRuntime::default();
    assert_eq!(host.step_budget(), rca_test::DEFAULT_STEP_BUDGET);
    let clock = host.runtime().clock();
    host.spawn_task(Box::pin(async move {
        clock.sleep(Duration::from_secs(5)).await;
    }));
    host.run_until_stalled().expect("register one timer");
    let pending = host
        .assert_no_pending_time()
        .expect_err("registered timer remains");
    assert_failure_kind(
        &pending,
        [false, false, false, false, false, true],
        "deterministic scheduler registered timers remain",
    );
    assert_eq!(pending.deadline_budget(), None);
    assert_eq!(pending.pending_sleeps().len(), 1);
    assert_eq!(pending.pending_sleeps()[0].id(), 0);

    let exhausted = block_on(host.drain_deadlines(0)).expect_err("no deadline batch admitted");
    assert_failure_kind(
        &exhausted,
        [false, true, false, false, false, false],
        "deterministic scheduler exhausted deadline-batch budget",
    );
    assert_eq!(exhausted.deadline_budget(), Some(0));
    assert_eq!(exhausted.pending_sleeps(), pending.pending_sleeps());
    assert_eq!(host.now(), MonotonicInstant::ZERO);
    let report = block_on(host.drain_deadlines(1)).expect("same timer remains recoverable");
    assert_eq!(report.deadline_batches(), 1);
    assert_eq!(
        report.current_time(),
        MonotonicInstant::from_elapsed(Duration::from_secs(5))
    );
    host.assert_no_pending_time().expect("timer completed");
    assert_eq!(
        pending.pending_sleeps().len(),
        1,
        "earlier failure keeps its snapshot"
    );
}

#[test]
fn drain_poll_exhaustion_is_not_reported_as_deadline_exhaustion() {
    let host = DeterministicRuntime::with_step_budget(2);
    host.spawn_task(Box::pin(std::future::poll_fn(|context| {
        context.waker().wake_by_ref();
        Poll::Pending
    })));
    let error = block_on(host.drain_deadlines(3)).expect_err("ready task exhausts polls");
    assert_failure_kind(
        &error,
        [true, false, false, false, false, false],
        "deterministic scheduler exhausted task-poll budget",
    );
    assert_eq!(error.polls(), 2);
    assert_eq!(error.deadline_batches(), 0);
    assert_eq!(error.deadline_budget(), Some(3));
    assert_eq!(error.ready_task_ids(), [0]);
    assert_eq!(host.now(), MonotonicInstant::ZERO);
}

#[test]
fn closed_host_failure_is_not_reported_as_work_waiting_for_input() {
    let host = DeterministicRuntime::new();
    let drain = host.drain_deadlines(1);
    drop(host);
    let error = block_on(drain).expect_err("drain does not retain host authority");
    assert_failure_kind(
        &error,
        [false, false, false, true, false, false],
        "deterministic scheduler host closed",
    );
    assert_eq!(error.polls(), 0);
    assert!(error.live_task_ids().is_empty());
    assert!(error.pending_sleeps().is_empty());
}

struct Delayed;

impl Reducer for Delayed {
    type State = ();
    type Action = ();

    fn reduce(&mut self, (): &mut (), (): &(), _: &mut ReduceContext<'_>) -> Effect<()> {
        Effect::send_after(Duration::from_secs(5), ()).cancellable("delayed work")
    }
}

#[test]
fn task_access_failure_preserves_typed_formatting_and_source() {
    use std::ops::ControlFlow;

    use rca_core::{Fold, FoldKey, TaskAccessError, TaskAccessReason, TaskProgress};

    struct UnitFold;
    impl FoldKey for UnitFold {
        type Value = ();

        fn identity() -> Self::Value {}

        fn reduce((): Self::Value, (): Self::Value) -> ControlFlow<Self::Value, Self::Value> {
            ControlFlow::Continue(())
        }
    }

    let mut store = TestStore::new((), Delayed);
    let unrelated = block_on(store.expect_issue(|_| true)).expect_err("no issue was reported");
    assert!(unrelated.is_no_issue());
    assert!(unrelated.task_access_error().is_none());
    assert!(unrelated.source().is_none());
    block_on(store.finish()).unwrap();

    let fold = Fold::<(), UnitFold>::new();
    let Err(error) = block_on(store.fold(&fold)) else {
        panic!("closed Store must reject fold access");
    };
    let access = error
        .task_access_error()
        .expect("typed production access cause");
    assert_eq!(access.reason(), TaskAccessReason::StoreClosed);
    assert_eq!(access.progress(), TaskProgress::NotStarted);
    assert_eq!(error.to_string(), format!("task access rejected: {access}"));
    let debug = format!("{error:?}");
    assert!(debug.contains("kind: \"TaskAccess\""), "{debug}");
    assert!(debug.contains(&format!("error: {access:?}")), "{debug}");
    let source = error
        .source()
        .and_then(|source| source.downcast_ref::<TaskAccessError>())
        .expect("Error::source retains the typed cause");
    assert!(std::ptr::eq(source, access));
}

#[test]
fn finished_store_rejects_inspection_without_running_the_callback() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use futures_util::FutureExt as _;

    let mut store = TestStore::with_step_budget((), Delayed, 64);
    let calls = Arc::new(AtomicUsize::new(0));
    let live_calls = Arc::clone(&calls);
    store
        .assert_state(move |()| {
            live_calls.fetch_add(1, Ordering::SeqCst);
        })
        .now_or_never()
        .expect("live inspection must not wait for an external wake")
        .expect("live inspection succeeds");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    store
        .finish()
        .now_or_never()
        .expect("idle finish is bounded")
        .expect("idle store shuts down");

    let closed_calls = Arc::clone(&calls);
    let origin_line = line!() + 1;
    let inspection = store.assert_state(move |()| {
        closed_calls.fetch_add(1, Ordering::SeqCst);
    });
    let error = inspection
        .now_or_never()
        .expect("closed inspection must not wait for an external wake")
        .expect_err("closed store cannot execute an inspection");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(error.origin().file(), file!());
    assert_eq!(error.origin().line(), origin_line);
    assert!(
        error
            .source()
            .expect("typed closure cause")
            .is::<rca_core::StoreStopped>()
    );
    assert!(error.drive_error().is_none());
    assert!(!error.is_incomplete_assertion());
    assert!(
        error
            .to_string()
            .starts_with("production Store stopped during test operation at ")
    );
    store
        .finish()
        .now_or_never()
        .expect("repeated finish is bounded")
        .expect("rejected inspection does not reopen completed work");
}

#[test]
fn blocked_task_has_no_drive_cause_and_retains_diagnostics_after_cancellation() {
    // A blocked task is not a failed scheduler drive, and its evidence must be immutable.
    let mut store = TestStore::new((), Delayed);
    block_on(async {
        let task = store.send(()).await.expect("delayed effect admitted");
        let error = task
            .finish()
            .await
            .expect_err("virtual time has not advanced");
        assert!(error.drive_error().is_none());
        assert!(error.source().is_none());
        assert!(error.ready_task_ids().is_empty());
        assert_eq!(error.pending_sleeps().len(), 1);
        assert_eq!(error.pending_sleeps()[0].id(), 0);
        assert_eq!(error.cancellation_identities().len(), 1);
        let display = error.to_string();
        assert!(display.starts_with("causal task from "));
        assert!(display.contains("remains blocked"));
        assert!(display.contains("pending sleeps: [PendingSleep"));
        assert!(display.contains("cancellation_count: 1"));
        let debug = format!("{error:?}");
        assert!(debug.contains("TestTaskError"));
        assert!(debug.contains("cancellation_identities: [CancellationIdentityDiagnostic"));

        task.cancel().await.expect("cancellation releases timer");
        task.finish().await.expect("cancelled work finishes");
        store.finish().await.expect("no pending work remains");
        assert_eq!(error.to_string(), display);
        assert_eq!(format!("{error:?}"), debug);
        assert_eq!(
            error
                .runtime_snapshot()
                .expect("saved diagnostics")
                .cancellation_count,
            1
        );
    });
}

#[test]
fn unfinished_store_retains_cancellation_and_time_snapshot_after_recovery() {
    let mut store = TestStore::new((), Delayed);
    block_on(async {
        let task = store.send(()).await.expect("delayed effect admitted");
        store
            .advance(Duration::from_secs(2))
            .await
            .expect("advance before deadline");
        let error = store.finish().await.expect_err("timer remains live");
        let diagnostics = error.finish_diagnostics().expect("unfinished work").clone();
        assert_eq!(
            diagnostics.current_time(),
            MonotonicInstant::from_elapsed(Duration::from_secs(2))
        );
        assert!(!diagnostics.has_pending_action());
        assert_eq!(diagnostics.pending_sleeps().len(), 1);
        assert_eq!(
            diagnostics.pending_sleeps()[0].deadline(),
            MonotonicInstant::from_elapsed(Duration::from_secs(5))
        );
        let runtime = error.runtime_snapshot().expect("actor snapshot");
        assert_eq!(runtime.live_task_ids, [0]);
        assert_eq!(runtime.cancellation_count, 1);
        assert_eq!(error.cancellation_identities().len(), 1);
        let identity = &error.cancellation_identities()[0];
        assert_eq!(identity.diagnostic_id(), 0);
        assert_eq!(identity.scope_id(), 0);
        assert_eq!(identity.type_name(), std::any::type_name::<&str>());
        assert!(error.drive_error().is_none());
        assert!(error.source().is_none());

        task.cancel().await.expect("cancellation releases timer");
        task.finish().await.expect("cancelled work finishes");
        store
            .advance(Duration::from_secs(3))
            .await
            .expect("cancelled timer cannot emit an action");
        store.finish().await.expect("no pending work remains");

        // Error evidence belongs to the failure, not the now-recovered store.
        assert_eq!(error.finish_diagnostics(), Some(&diagnostics));
        assert_eq!(error.runtime_snapshot(), Some(diagnostics.runtime()));
        assert_eq!(
            error.cancellation_identities(),
            diagnostics.cancellation_identities()
        );
    });
}

#[test]
fn missing_issue_does_not_claim_runtime_failure_or_unfinished_work() {
    let mut store = TestStore::new((), Delayed);
    block_on(async {
        let error = store
            .expect_issue(|_| true)
            .await
            .expect_err("no issue was emitted");
        assert!(error.is_no_issue());
        assert!(!error.is_issue_mismatch());
        assert!(error.mismatched_issue().is_none());
        assert!(error.unconsumed_issues().is_empty());
        assert!(error.finish_diagnostics().is_none());
        assert!(error.drive_error().is_none());
        assert!(error.runtime_snapshot().is_none());
        assert!(error.cancellation_identities().is_empty());
        assert!(error.source().is_none());
        assert!(error.to_string().starts_with("no issue awaits expectation"));
        assert!(format!("{error:?}").contains("NoIssue"));
        store
            .finish()
            .await
            .expect("missing expectation is recoverable");
    });
}

#[test]
fn completion_budget_covers_diagnostics_and_declared_capture_admission() {
    use std::convert::Infallible;
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use futures_util::FutureExt as _;
    use rca_test::CaptureKey;

    struct Captured;
    impl CaptureKey for Captured {
        type Value = ();
        type Error = Infallible;
    }

    // Resetting the budget between completion stages would incorrectly report success.
    for (budget, declare_capture) in [(1, false), (2, true)] {
        let calls = Arc::new(AtomicUsize::new(0));
        let builder = TestStore::builder((), Delayed).with_step_budget(budget);
        let builder = if declare_capture {
            let capture_calls = Arc::clone(&calls);
            builder
                .with_capture::<Captured>(move |()| {
                    capture_calls.fetch_add(1, Ordering::SeqCst);
                    Ok(())
                })
                .unwrap()
        } else {
            builder
        };
        let mut store = builder.build();
        let origin_line = line!() + 1;
        let completion = store.finish();
        let error = completion
            .now_or_never()
            .expect("completion does not wait for an external wake")
            .expect_err("one budget is shared by all completion stages");
        assert_eq!(error.origin().file(), file!());
        assert_eq!(error.origin().line(), origin_line);
        let drive = error.drive_error().expect("original scheduler failure");
        assert!(drive.is_step_budget_exhausted());
        assert_eq!((drive.polls(), drive.step_budget()), (budget, budget));
        assert!(std::ptr::eq(
            error
                .source()
                .unwrap()
                .downcast_ref::<DriveError>()
                .unwrap(),
            drive,
        ));
        assert!(error.finish_diagnostics().is_none());
        assert_eq!(store.now(), MonotonicInstant::ZERO);
        assert_eq!(calls.load(Ordering::SeqCst), usize::from(declare_capture));

        if declare_capture {
            let incomplete = store
                .finish()
                .now_or_never()
                .expect("incomplete capture fails immediately")
                .expect_err("admitted final capture cannot be silently abandoned");
            assert!(incomplete.is_incomplete_assertion());
            assert_eq!(incomplete.origin(), error.origin());
            assert!(
                incomplete
                    .to_string()
                    .contains("incomplete assertion for finish")
            );
            assert_eq!(calls.load(Ordering::SeqCst), 1);
        }
        let panic = catch_unwind(AssertUnwindSafe(|| drop(store)))
            .expect_err("Drop must enforce the incomplete completion check");
        let expected = if declare_capture {
            "TestStore teardown failed: TestStore incomplete assertion for finish at "
        } else {
            "TestStore teardown failed: deterministic scheduler drive failed at "
        };
        assert!(
            panic
                .downcast_ref::<String>()
                .unwrap()
                .starts_with(expected)
        );
        assert_eq!(calls.load(Ordering::SeqCst), usize::from(declare_capture));
    }
}

mod admission_guards {
    use std::error::Error;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, mpsc};

    use futures_util::FutureExt as _;
    use rca_core::{DEFAULT_MAILBOX_CAPACITY, Effect, EffectSender, ReduceContext, Reducer};
    use rca_test::TestStore;

    struct DropCount(Arc<AtomicUsize>);

    impl Drop for DropCount {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    enum Action {
        Start,
        Output(DropCount),
        External(DropCount),
    }

    struct ExportSender {
        export: mpsc::Sender<EffectSender<Action>>,
        effect_drops: Arc<AtomicUsize>,
    }

    impl Reducer for ExportSender {
        type State = usize;
        type Action = Action;

        fn reduce(
            &mut self,
            state: &mut usize,
            action: &Action,
            _: &mut ReduceContext<'_>,
        ) -> Effect<Action> {
            match action {
                Action::Start => {
                    let export = self.export.clone();
                    let guard = DropCount(Arc::clone(&self.effect_drops));
                    Effect::run(move |sender| async move {
                        let _guard = guard;
                        export.send(sender).expect("test retains the receiver");
                        std::future::pending::<()>().await;
                    })
                }
                Action::Output(_payload) => {
                    *state += 100;
                    Effect::none()
                }
                Action::External(_payload) => {
                    *state += 1;
                    Effect::none()
                }
            }
        }
    }

    fn assert_recovery(store: &mut TestStore<usize, Action>) {
        store
            .assert_state(|state| assert_eq!(*state, 0))
            .now_or_never()
            .expect("inspection is bounded")
            .expect("rejected actions did not reduce");
        let drops = Arc::new(AtomicUsize::new(0));
        let task = store
            .send_with(Action::External(DropCount(Arc::clone(&drops))), |state| {
                *state += 1;
            })
            .now_or_never()
            .expect("retry is bounded")
            .expect("rejected expectation left no debt or changed baseline");
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        task.finish()
            .now_or_never()
            .expect("retry task completion is bounded")
            .expect("retry committed without retained work");
        store
            .finish()
            .now_or_never()
            .expect("shutdown is bounded")
            .expect("all cancelled payloads and causal work were released");
    }

    #[test]
    fn full_mailbox_rejects_external_action_without_assertion_debt() {
        let (export, exported) = mpsc::channel();
        let effect_drops = Arc::new(AtomicUsize::new(0));
        let mut store = TestStore::new(
            0,
            ExportSender {
                export,
                effect_drops: Arc::clone(&effect_drops),
            },
        );
        let task = store
            .send(Action::Start)
            .now_or_never()
            .expect("effect starts without external wakes")
            .unwrap();
        let sender = exported
            .try_recv()
            .expect("live effect exported its sender");
        let queued_drops = Arc::new(AtomicUsize::new(0));
        let mut sends = Vec::new();
        // First polling admits each action but cannot drive the actor or its gate.
        for _ in 0..DEFAULT_MAILBOX_CAPACITY {
            let mut send =
                Box::pin(sender.send(Action::Output(DropCount(Arc::clone(&queued_drops)))));
            assert!(send.as_mut().now_or_never().is_none());
            sends.push(send);
        }
        let rejected_drops = Arc::new(AtomicUsize::new(0));
        let rejected = Action::External(DropCount(Arc::clone(&rejected_drops)));
        let (mut updates, mut captures) = (0, 0);
        let origin_line = line!() + 1;
        let send = store.send_with_captures(
            rejected,
            |state| {
                updates += 1;
                *state += 1;
            },
            |_| {
                captures += 1;
                Ok(())
            },
        );
        let error = send
            .now_or_never()
            .expect("full mailbox rejects immediately")
            .expect_err("external action cannot enter the full mailbox");
        assert_eq!((updates, captures), (1, 1));
        assert_eq!(error.origin().file(), file!());
        assert_eq!(error.origin().line(), origin_line);
        assert!(error.source().is_none());
        assert!(error.drive_error().is_none());
        assert!(!error.is_incomplete_assertion());
        assert!(!error.is_unreceived_action());
        let display = error.to_string();
        let debug = format!("{error:?}");
        assert!(display.starts_with("TestStore mailbox is full at "));
        assert!(debug.contains("AdmissionFull"));
        assert_eq!(rejected_drops.load(Ordering::SeqCst), 1);
        assert_eq!(queued_drops.load(Ordering::SeqCst), 0);
        assert_eq!(effect_drops.load(Ordering::SeqCst), 0);

        task.cancel()
            .now_or_never()
            .expect("cancellation settles at the first retained terminal")
            .unwrap();
        for (index, send) in sends.into_iter().enumerate() {
            let boundary = store
                .pending_boundary()
                .expect("each admitted action retains its cancelled terminal");
            let cancelled = send
                .now_or_never()
                .expect("every admitted sender receives a reply")
                .expect_err("cancelled payloads must not commit");
            assert!(cancelled.is_cancelled());
            assert!(!cancelled.is_store_stopped());
            assert_eq!(queued_drops.load(Ordering::SeqCst), index + 1);
            store
                .receive_terminal(boundary, |error| {
                    error.reason() == rca_core::TaskAccessReason::Cancelled
                        && error.progress() == rca_core::TaskProgress::NotStarted
                })
                .now_or_never()
                .expect("each terminal expectation is bounded")
                .expect("the matching admitted action retired before reduction");
        }
        assert!(store.pending_boundary().is_none());
        assert_eq!(
            queued_drops.load(Ordering::SeqCst),
            DEFAULT_MAILBOX_CAPACITY
        );
        assert_eq!(effect_drops.load(Ordering::SeqCst), 1);
        assert!(task.is_cancelled());
        assert!(task.is_finished());
        assert_recovery(&mut store);
        assert_eq!(rejected_drops.load(Ordering::SeqCst), 1);
        assert_eq!(error.to_string(), display);
        assert_eq!(format!("{error:?}"), debug);
    }

    #[test]
    #[expect(
        clippy::too_many_lines,
        reason = "one causal sequence keeps an admitted send, assertion debt and owned teardown together"
    )]
    fn admitted_send_budget_failure_retains_assertion_debt() {
        use std::panic::{AssertUnwindSafe, catch_unwind};
        use std::task::{Context, Poll, Waker};

        let (control, gate) = rca_test::controlled_gate();
        let polls = Arc::new(AtomicUsize::new(0));
        let work_drops = Arc::new(AtomicUsize::new(0));
        let work_polls = Arc::clone(&polls);
        let work_guard = DropCount(Arc::clone(&work_drops));
        let work = Effect::run(move |_| async move {
            let _guard = work_guard;
            gate.await.expect("test releases the finite work");
            std::future::poll_fn(move |context| {
                // Eight finite polls exceed the send's seven-poll budget.
                if work_polls.fetch_add(1, Ordering::SeqCst) + 1 == 8 {
                    Poll::Ready(())
                } else {
                    context.waker().wake_by_ref();
                    Poll::Pending
                }
            })
            .await;
        });
        let (export, exported) = mpsc::channel();
        let effect_drops = Arc::new(AtomicUsize::new(0));
        let mut sender = ExportSender {
            export,
            effect_drops: Arc::clone(&effect_drops),
        };
        let mut work = Some(work);
        let mut store = TestStore::with_step_budget(
            0,
            rca_core::Reduce::new(move |state, action, context| {
                Effect::merge([
                    sender.reduce(state, action, context),
                    work.take().unwrap_or_else(Effect::none),
                ])
            }),
            7,
        );
        let task = store
            .send(Action::Start)
            .now_or_never()
            .expect("both effects park without an external wake")
            .unwrap();
        control
            .wait_until_waiting()
            .now_or_never()
            .expect("finite work reached its gate")
            .unwrap();
        assert_eq!(polls.load(Ordering::SeqCst), 0);
        let sender = exported
            .try_recv()
            .expect("live effect exported its sender");
        let output_drops = Arc::new(AtomicUsize::new(0));
        let mut output =
            Box::pin(sender.send(Action::Output(DropCount(Arc::clone(&output_drops)))));
        assert!(output.as_mut().now_or_never().is_none());
        control.release().unwrap();

        let external_drops = Arc::new(AtomicUsize::new(0));
        let (mut updates, mut captures) = (0, 0);
        let origin_line = line!() + 1;
        let send = store.send_with_captures(
            Action::External(DropCount(Arc::clone(&external_drops))),
            |state| {
                updates += 1;
                *state += 1;
            },
            |_| {
                captures += 1;
                Ok(())
            },
        );
        let error = {
            let mut send = std::pin::pin!(send);
            let mut context = Context::from_waker(Waker::noop());
            (0..64)
                .find_map(|_| match send.as_mut().poll(&mut context) {
                    Poll::Ready(result) => Some(result),
                    Poll::Pending => None,
                })
                .expect("finite ready work must report its budget without external wakes")
                .expect_err("the earlier gated output prevents the admitted root commit")
        };
        assert_eq!((updates, captures), (1, 1));
        assert_eq!(error.origin().file(), file!());
        assert_eq!(error.origin().line(), origin_line);
        let drive = error.drive_error().expect("original scheduler failure");
        assert!(drive.is_step_budget_exhausted());
        assert_eq!((drive.polls(), drive.step_budget()), (7, 7));
        assert!(std::ptr::eq(
            error
                .source()
                .unwrap()
                .downcast_ref::<rca_test::DriveError>()
                .unwrap(),
            drive,
        ));
        assert_eq!(error.runtime_snapshot().unwrap().commit_version, 1);
        assert_eq!(store.now(), rca_core::MonotonicInstant::ZERO);
        assert_eq!(output_drops.load(Ordering::SeqCst), 0);
        assert_eq!(external_drops.load(Ordering::SeqCst), 0);
        assert_eq!(effect_drops.load(Ordering::SeqCst), 0);
        store
            .assert_state(|state| assert_eq!(*state, 0))
            .now_or_never()
            .expect("remaining finite work and gated inspection are bounded")
            .expect("neither queued action has reduced");
        assert_eq!(polls.load(Ordering::SeqCst), 8);
        assert_eq!(work_drops.load(Ordering::SeqCst), 1);

        let next = store
            .send_with(Action::Start, |_| {
                panic!("assertion debt precedes callbacks")
            })
            .now_or_never()
            .expect("outstanding assertion is reported immediately")
            .expect_err("an admitted send cannot silently abandon its expectation");
        assert!(next.is_incomplete_assertion());
        assert_eq!(next.origin(), error.origin());
        let finish = store
            .finish()
            .now_or_never()
            .expect("completion reports the same assertion immediately")
            .expect_err("completion cannot forgive the admitted send");
        assert!(finish.is_incomplete_assertion());
        assert_eq!(finish.origin(), error.origin());
        assert!(
            finish
                .to_string()
                .starts_with("TestStore incomplete assertion for send at ")
        );

        let panic = catch_unwind(AssertUnwindSafe(|| drop(store)))
            .expect_err("Drop must enforce the outstanding send assertion");
        assert!(
            panic.downcast_ref::<String>().unwrap().starts_with(
                "TestStore teardown failed: TestStore incomplete assertion for send at "
            )
        );
        assert_eq!(output_drops.load(Ordering::SeqCst), 1);
        assert_eq!(external_drops.load(Ordering::SeqCst), 1);
        assert_eq!(effect_drops.load(Ordering::SeqCst), 1);
        assert_eq!(work_drops.load(Ordering::SeqCst), 1);
        assert!(task.is_finished());
        let rejected = output
            .now_or_never()
            .expect("teardown resolves the retained effect sender")
            .expect_err("the gated output did not commit during teardown");
        assert!(!rejected.is_cancelled());
        assert!(rejected.is_store_stopped());
    }

    #[test]
    fn cancelled_receive_rejects_release_without_assertion_debt() {
        let (export, exported) = mpsc::channel();
        let effect_drops = Arc::new(AtomicUsize::new(0));
        let mut store = TestStore::new(
            0,
            ExportSender {
                export,
                effect_drops: Arc::clone(&effect_drops),
            },
        );
        let task = store
            .send(Action::Start)
            .now_or_never()
            .expect("effect starts without external wakes")
            .unwrap();
        let sender = exported
            .try_recv()
            .expect("live effect exported its sender");
        let output_drops = Arc::new(AtomicUsize::new(0));
        let mut send = Box::pin(sender.send(Action::Output(DropCount(Arc::clone(&output_drops)))));
        assert!(send.as_mut().now_or_never().is_none());
        let (mut matches, mut updates, mut captures) = (0, 0, 0);
        let origin_line = line!() + 1;
        let receive = store.receive_with_captures(
            |action| {
                matches += 1;
                matches!(action, Action::Output(_))
            },
            |state| {
                updates += 1;
                *state += 100;
                // Inspection has restored the action before this callback runs.
                task.cancel()
                    .now_or_never()
                    .expect("callback cancellation is bounded")
                    .unwrap();
            },
            |_| {
                captures += 1;
                Ok(())
            },
        );
        let error = receive
            .now_or_never()
            .expect("cancelled release rejects immediately")
            .expect_err("matching cannot commit an action cancelled by its expectation");
        assert_eq!((matches, updates, captures), (1, 1, 1));
        assert!(error.is_no_received_action());
        assert!(!error.is_incomplete_assertion());
        assert!(!error.is_action_mismatch());
        assert_eq!(error.origin().file(), file!());
        assert_eq!(error.origin().line(), origin_line);
        assert!(error.source().is_none());
        assert!(error.drive_error().is_none());
        let display = error.to_string();
        let debug = format!("{error:?}");
        let cancelled = send
            .now_or_never()
            .expect("cancelled sender receives a reply")
            .expect_err("cancelled output must not commit");
        assert!(cancelled.is_cancelled());
        assert_eq!(output_drops.load(Ordering::SeqCst), 1);
        assert_eq!(effect_drops.load(Ordering::SeqCst), 1);
        assert!(task.is_cancelled());
        assert!(task.is_finished());
        let boundary = store
            .pending_boundary()
            .expect("cancelled selected action retains its terminal component");
        let finish = store
            .finish()
            .now_or_never()
            .expect("retained terminal is reported immediately")
            .expect_err("no snapshot debt does not acknowledge the terminal");
        assert!(finish.is_incomplete_assertion());
        store
            .assert_state(|state| assert_eq!(*state, 0))
            .now_or_never()
            .expect("terminal inspection is bounded")
            .expect("cancelled action did not reduce");
        store
            .receive_terminal(boundary, |error| {
                error.reason() == rca_core::TaskAccessReason::Cancelled
                    && error.progress() == rca_core::TaskProgress::NotStarted
            })
            .now_or_never()
            .expect("terminal expectation is bounded")
            .expect("the cancelled selected action retired before reduction");
        assert_recovery(&mut store);
        assert_eq!(output_drops.load(Ordering::SeqCst), 1);
        assert_eq!(error.to_string(), display);
        assert_eq!(format!("{error:?}"), debug);
    }
}
