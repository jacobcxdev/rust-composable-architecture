//! Checks typed task transactions with non-Clone, non-Sync state and results.

#[expect(
    dead_code,
    reason = "this target uses only the captured-task host controls"
)]
mod support;

use rca_core::{
    DependencyKey, Feature, FeatureContext, Issue, IssueSeverity, OperationPolicy, Store, Task,
};
use std::cell::Cell;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};
use support::ManualRuntime;

/// Can move between threads, but deliberately implements neither Clone nor Sync.
struct State(Cell<usize>);

/// Starts the pipeline or applies its dependency-controlled increment.
enum Action {
    /// Runs a send, a read and an update through one causal task.
    Start,
    /// Changes state through an action rather than a direct update.
    Add(usize),
}

/// Records the task's observed values and final commit revision.
struct FeatureUnderTest(Arc<Mutex<Vec<usize>>>);

/// Uses a distinct live value so the tests detect a missing dependency override.
struct PipelineIncrement;

impl DependencyKey for PipelineIncrement {
    type Value = usize;

    fn live_value() -> Self::Value {
        40
    }
}

impl Feature for FeatureUnderTest {
    type State = State;
    type Action = Action;
    fn reduce(
        &mut self,
        state: &mut State,
        action: &Action,
        context: &mut FeatureContext<'_, State, Action>,
    ) {
        match action {
            Action::Start => {
                let results = Arc::clone(&self.0);
                let increment = *context.reducer_context().dependency::<PipelineIncrement>();
                let reporter = context.reducer_context().issue_reporter();
                let task = Task::<State, Action>::run_named(
                    "pipeline",
                    OperationPolicy::Coexist,
                    move |context| async move {
                        // Diagnostics stay opaque without Debug bounds on the domain.
                        assert_eq!(format!("{context:?}"), "TaskContext { .. }");
                        assert_eq!(
                            context
                                .operation("pipeline")
                                .status()
                                .await
                                .unwrap()
                                .active_count(),
                            1
                        );
                        let _ = context.send(Action::Add(increment)).await.unwrap();
                        let before = context.read(|state| state.0.get()).await.unwrap();
                        let updated = context
                            .update(|state| {
                                state.0.set(state.0.get() + 3);
                                State(Cell::new(state.0.get()))
                            })
                            .await
                            .unwrap();
                        assert!(
                            !updated.task().is_finished(),
                            "the update receipt retains the active pipeline's causal lineage"
                        );
                        results.lock().unwrap().extend([
                            before,
                            updated.value().0.get(),
                            usize::try_from(updated.revision()).expect("test revision fits usize"),
                        ]);
                        reporter
                            .report(
                                Issue::new(
                                    "pipeline.committed",
                                    format!("committed state {}", updated.value().0.get()),
                                )
                                .with_severity(IssueSeverity::Note),
                            )
                            .unwrap();
                    },
                );
                assert_eq!(format!("{task:?}"), "Task { .. }");
                context.task(task);
                // Formatting must not expose the non-Debug domain or execute
                // the deferred task retained by this reducer context.
                assert_eq!(format!("{context:?}"), "FeatureContext { .. }");
                assert_eq!(state.0.get(), 1);
                assert!(self.0.lock().unwrap().is_empty());
            }
            Action::Add(value) => state.0.set(state.0.get() + value),
        }
    }
}

// Catches missing typed admission, incorrect state pairing, lost task lineage,
// and requiring Sync/Clone for State or an update result.
#[test]
fn task_sends_reads_and_commits_non_clone_send_only_values() {
    let runtime = ManualRuntime::new();
    let results = Arc::new(Mutex::new(Vec::new()));
    let issues = Arc::new(Mutex::new(Vec::new()));
    let reported = Arc::clone(&issues);
    let builder = Store::feature_builder(
        State(Cell::new(1)),
        FeatureUnderTest(Arc::clone(&results)),
        runtime.runtime(),
    )
    .mailbox_capacity(1)
    .with_dependency::<PipelineIncrement>(2)
    .with_issue_handler(move |issue| reported.lock().unwrap().push(issue.clone()));
    for debug in [format!("{builder:?}"), format!("{builder:#?}")] {
        assert!(debug.starts_with("StoreBuilder"));
        assert!(debug.contains("mailbox_capacity: 1"));
        assert!(!debug.contains("FeatureUnderTest") && !debug.contains("Cell"));
    }
    assert_eq!(runtime.spawn_count(), 0);
    assert!(results.lock().unwrap().is_empty());
    assert!(issues.lock().unwrap().is_empty());
    let store = builder.build().unwrap();
    let mut actor = runtime.take_only_queued_task();
    let mut cx = Context::from_waker(Waker::noop());
    let mut send = Box::pin(store.send(Action::Start));
    assert!(send.as_mut().poll(&mut cx).is_pending());
    assert!(actor.as_mut().poll(&mut cx).is_pending());
    let Poll::Ready(Ok(task)) = send.as_mut().poll(&mut cx) else {
        panic!("start committed")
    };
    drop(send);
    assert!(!task.is_finished());
    assert!(issues.lock().unwrap().is_empty());
    let mut body = runtime.take_only_queued_task();
    // Each request needs one body/actor turn: status, send, read, then update.
    for _ in 0..4 {
        assert!(body.as_mut().poll(&mut cx).is_pending());
        assert!(actor.as_mut().poll(&mut cx).is_pending());
    }
    assert!(body.as_mut().poll(&mut cx).is_ready());
    drop(body);
    assert!(actor.as_mut().poll(&mut cx).is_pending());
    futures_executor::block_on(task.finish());
    assert!(task.is_finished());
    assert_eq!(*results.lock().unwrap(), [3, 6, 3]);
    let recorded_issues = issues.lock().unwrap();
    assert_eq!(recorded_issues.len(), 1);
    assert_eq!(recorded_issues[0].code, "pipeline.committed");
    assert_eq!(recorded_issues[0].message, "committed state 6");
    assert_eq!(recorded_issues[0].severity, IssueSeverity::Note);
    assert!(recorded_issues[0].cause.is_none());
    assert_eq!(recorded_issues[0].location.file(), file!());
    drop(recorded_issues);
    drop(store);
    assert!(actor.as_mut().poll(&mut cx).is_ready());
    for result in runtime.join_all() {
        result.unwrap();
    }
    assert_eq!(issues.lock().unwrap().len(), 1);
}

#[cfg(feature = "test-util")]
#[test]
fn feature_builder_validates_before_claiming_gate_and_releases_failed_inputs() {
    use rca_core::testing::TransactionGate;

    let runtime = ManualRuntime::new();
    let results = Arc::new(Mutex::new(Vec::new()));
    let gate = TransactionGate::new();
    let build = |capacity| {
        Store::feature_builder(
            State(Cell::new(1)),
            FeatureUnderTest(Arc::clone(&results)),
            runtime.runtime(),
        )
        .mailbox_capacity(capacity)
        .transaction_gate(gate.clone())
        .build()
    };

    let error = build(0).expect_err("zero capacity must fail before gate attachment");
    assert!(error.is_zero_capacity());
    assert!(!error.is_assertion_gate_attached());
    assert_eq!(runtime.spawn_count(), 0);
    assert_eq!(Arc::strong_count(&results), 1);

    let first = build(1).expect("invalid configuration must leave the gate available");
    assert_eq!(runtime.spawn_count(), 1);
    assert_eq!(Arc::strong_count(&results), 2);
    let error = build(0).expect_err("capacity validation must precede duplicate attachment");
    assert!(error.is_zero_capacity());
    assert!(!error.is_assertion_gate_attached());
    assert_eq!(runtime.spawn_count(), 1);
    assert_eq!(Arc::strong_count(&results), 2);
    let error = build(1).expect_err("one gate cannot attach to two live actors");
    assert!(error.is_assertion_gate_attached());
    assert!(!error.is_zero_capacity());
    assert_eq!(runtime.spawn_count(), 1);
    assert_eq!(Arc::strong_count(&results), 2);
    assert!(results.lock().unwrap().is_empty());

    drop(runtime.take_only_queued_task());
    assert_eq!(Arc::strong_count(&results), 1);
    let second = build(1).expect("unpolled actor teardown must release its gate");
    assert_eq!(runtime.spawn_count(), 2);
    drop((first, second));
    runtime.drop_queued();
    assert_eq!(Arc::strong_count(&results), 1);
}

#[cfg(feature = "test-util")]
#[test]
fn pending_task_send_is_borrowed_without_committing_and_update_is_not_an_action() {
    use rca_core::testing::{TransactionGate, TransactionKind};

    let runtime = ManualRuntime::new();
    let results = Arc::new(Mutex::new(Vec::new()));
    let gate = TransactionGate::new();
    let store = Store::feature_builder(
        State(Cell::new(1)),
        FeatureUnderTest(Arc::clone(&results)),
        runtime.runtime(),
    )
    .mailbox_capacity(1)
    .with_dependency::<PipelineIncrement>(2)
    .with_issue_handler(|_| {})
    .transaction_gate(gate.clone())
    .build()
    .unwrap();
    let mut actor = runtime.take_only_queued_task();
    let mut cx = Context::from_waker(Waker::noop());
    assert!(gate.reserve_root());
    let mut start = Box::pin(store.send(Action::Start));
    assert!(start.as_mut().poll(&mut cx).is_pending());
    assert!(actor.as_mut().poll(&mut cx).is_pending());
    let Poll::Ready(Ok(task)) = start.as_mut().poll(&mut cx) else {
        panic!("reserved start commits");
    };
    assert!(gate.acknowledge(gate.boundary().unwrap()));
    assert!(actor.as_mut().poll(&mut cx).is_pending());
    let mut body = runtime.take_only_queued_task();
    // The operation query completes before the task submits its typed send.
    assert!(body.as_mut().poll(&mut cx).is_pending());
    assert!(actor.as_mut().poll(&mut cx).is_pending());
    assert!(body.as_mut().poll(&mut cx).is_pending());
    assert!(actor.as_mut().poll(&mut cx).is_pending());

    let send_boundary = gate.boundary().unwrap();
    assert_eq!(gate.pending_kind(), Some(TransactionKind::Action));
    assert_eq!(
        gate.inspect_pending(|action| match action {
            Action::Add(value) => *value,
            Action::Start => panic!("the frontier holds the task's send, not its parent action"),
        }),
        Some(2)
    );
    assert_eq!(gate.boundary(), Some(send_boundary));
    assert_eq!(gate.pending_snapshot().unwrap().commit_version, 1);
    assert!(!gate.acknowledge(send_boundary));
    assert!(body.as_mut().poll(&mut cx).is_pending());
    assert!(results.lock().unwrap().is_empty());
    assert!(gate.release_pending());
    assert!(actor.as_mut().poll(&mut cx).is_pending());
    assert!(gate.acknowledge(send_boundary));
    assert!(body.as_mut().poll(&mut cx).is_pending());
    assert!(actor.as_mut().poll(&mut cx).is_pending());
    assert!(body.as_mut().poll(&mut cx).is_pending());
    assert!(actor.as_mut().poll(&mut cx).is_pending());

    let update_boundary = gate.boundary().unwrap();
    assert_ne!(send_boundary, update_boundary);
    assert_eq!(gate.pending_kind(), Some(TransactionKind::Update));
    assert_eq!(
        gate.inspect_pending(|_| panic!("an update must not enter an action inspector")),
        None::<()>
    );
    assert_eq!(gate.boundary(), Some(update_boundary));
    assert_eq!(gate.pending_snapshot().unwrap().commit_version, 2);
    assert!(body.as_mut().poll(&mut cx).is_pending());
    assert!(results.lock().unwrap().is_empty());
    assert!(gate.release_pending());
    assert!(actor.as_mut().poll(&mut cx).is_pending());
    assert!(body.as_mut().poll(&mut cx).is_ready());
    drop(body);
    assert_eq!(*results.lock().unwrap(), [3, 6, 3]);
    assert!(!gate.acknowledge(send_boundary));
    assert_eq!(gate.boundary(), Some(update_boundary));
    assert!(gate.acknowledge(update_boundary));
    assert!(actor.as_mut().poll(&mut cx).is_pending());
    assert!(task.is_finished());
    drop(start);
    drop(store);
    assert!(actor.as_mut().poll(&mut cx).is_ready());
    for result in runtime.join_all() {
        result.unwrap();
    }
}

#[cfg(feature = "test-util")]
#[test]
fn committed_acknowledgement_cannot_reserve_during_query_capture_retirement() {
    use rca_core::{Effect, Reduce, testing::TransactionGate};
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Re-enters the gate while a rejected assertion releases its capture.
    struct Capture {
        /// Must forbid new reservations until capture disposal finishes.
        gate: TransactionGate<usize, ()>,
        /// Identifies the completed root that the destructor acknowledges.
        boundary: u64,
        /// Counts destructor calls independently of the gate's own bookkeeping.
        drops: Arc<AtomicUsize>,
    }
    impl Drop for Capture {
        fn drop(&mut self) {
            assert_eq!(self.gate.boundary(), Some(self.boundary));
            assert!(self.gate.acknowledge(self.boundary));
            assert!(self.gate.boundary().is_none());
            assert!(
                !self.gate.reserve_root(),
                "acknowledgement cannot admit a new root while capture disposal is draining"
            );
            assert_eq!(self.drops.fetch_add(1, Ordering::SeqCst), 0);
        }
    }

    let runtime = ManualRuntime::new();
    let reductions = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&reductions);
    let gate = TransactionGate::new();
    let store = Store::builder(
        0usize,
        Reduce::new(move |state: &mut usize, (): &(), _| {
            *state += 1;
            observed.fetch_add(1, Ordering::SeqCst);
            Effect::none()
        }),
        runtime.runtime(),
    )
    .transaction_gate(gate.clone())
    .build()
    .unwrap();
    let mut actor = runtime.take_only_queued_task();
    let mut cx = Context::from_waker(Waker::noop());
    assert!(gate.reserve_root());
    let mut send = Box::pin(store.send(()));
    assert!(send.as_mut().poll(&mut cx).is_pending());
    assert!(actor.as_mut().poll(&mut cx).is_pending());
    let Poll::Ready(Ok(task)) = send.as_mut().poll(&mut cx) else {
        panic!("the reserved root action commits");
    };
    assert!(task.is_finished());
    assert_eq!(reductions.load(Ordering::SeqCst), 1);
    assert_eq!(gate.latest_snapshot().unwrap().commit_version, 1);
    let boundary = gate.boundary().unwrap();
    let drops = Arc::new(AtomicUsize::new(0));
    let capture = Capture {
        gate: gate.clone(),
        boundary,
        drops: Arc::clone(&drops),
    };
    let mut query = Box::pin(
        gate.assert_state(move |_| {
            drop(capture);
            panic!("actor retirement must reject, not execute, the queued query");
        })
        .finish(),
    );
    assert!(query.as_mut().poll(&mut cx).is_pending());
    assert_eq!(drops.load(Ordering::SeqCst), 0);

    // The runtime may discard its actor future before serving the diagnostic query.
    drop(actor);

    assert_eq!(drops.load(Ordering::SeqCst), 1);
    assert!(matches!(query.as_mut().poll(&mut cx), Poll::Ready(Err(_))));
    assert!(
        gate.boundary().is_none(),
        "successful work gained no terminal debt"
    );
    assert!(task.is_finished());
    assert_eq!(reductions.load(Ordering::SeqCst), 1);
    assert_eq!(runtime.spawn_count(), 1);
    assert!(
        gate.reserve_root(),
        "reservation resumes only after disposal finishes"
    );
    gate.cancel_root_reservation();
    drop(send);
    drop(store);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
}

#[cfg(feature = "test-util")]
#[test]
fn unreleased_frontier_drains_completion_and_runtime_drop_without_mutating() {
    use rca_core::{Effect, Reduce, testing::TransactionGate};
    use std::sync::atomic::{AtomicUsize, Ordering};

    let runtime = ManualRuntime::new();
    let reductions = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&reductions);
    let reducer = Reduce::new(move |(): &mut (), (): &(), _| {
        observed.fetch_add(1, Ordering::Relaxed);
        Effect::concatenate([
            Effect::future(async { None }),
            Effect::future(std::future::pending()),
        ])
    });
    assert_eq!(format!("{reducer:?}"), "Reduce { .. }");
    assert_eq!(reductions.load(Ordering::Relaxed), 0);
    assert_eq!(runtime.spawn_count(), 0);
    let gate = TransactionGate::new();
    assert!(gate.pending_snapshot().is_none());
    assert!(gate.latest_snapshot().is_none());
    let store = Store::builder((), reducer, runtime.runtime())
        .transaction_gate(gate.clone())
        .mailbox_capacity(1)
        .build()
        .unwrap();
    let mut actor = runtime.take_only_queued_task();
    let mut context = Context::from_waker(Waker::noop());
    assert!(gate.reserve_root());
    let mut initial = Box::pin(store.send(()));
    assert!(initial.as_mut().poll(&mut context).is_pending());
    assert!(actor.as_mut().poll(&mut context).is_pending());
    assert!(matches!(
        initial.as_mut().poll(&mut context),
        Poll::Ready(Ok(_))
    ));
    assert!(gate.acknowledge(gate.boundary().unwrap()));
    assert!(actor.as_mut().poll(&mut context).is_pending());
    let mut first = runtime.take_only_queued_task();
    let mut held = Box::pin(store.send(()));
    assert!(held.as_mut().poll(&mut context).is_pending());
    assert!(actor.as_mut().poll(&mut context).is_pending());
    let boundary = gate.boundary();
    assert!(gate.has_pending());
    let pending = gate.pending_snapshot().unwrap();
    let latest = gate.latest_snapshot().unwrap();
    assert_eq!(&pending, gate.pending_diagnostics().unwrap().runtime());
    assert_eq!(&latest, gate.latest_diagnostics().unwrap().runtime());
    assert_eq!(pending.commit_version, 1);
    assert_eq!(latest.commit_version, 1);
    assert_eq!(pending.live_task_ids.len(), 1);
    for debug in [format!("{gate:?}"), format!("{gate:#?}")] {
        assert!(debug.starts_with("TransactionGate"));
        assert!(debug.contains("boundary: Some"));
        assert!(!debug.contains("pending_snapshot") && !debug.contains("assertion_reply"));
    }
    assert_eq!(gate.boundary(), boundary);
    assert!(held.as_mut().poll(&mut context).is_pending());
    assert_eq!(reductions.load(Ordering::Relaxed), 1);
    assert!(first.as_mut().poll(&mut context).is_ready());
    drop(first);
    assert!(actor.as_mut().poll(&mut context).is_pending());
    assert_eq!(
        runtime.spawn_count(),
        3,
        "completion must launch the next concatenation leaf before release"
    );
    assert_eq!(gate.boundary(), boundary);
    assert_eq!(reductions.load(Ordering::Relaxed), 1);
    assert!(held.as_mut().poll(&mut context).is_pending());
    drop(runtime.take_only_queued_task());
    assert!(
        actor.as_mut().poll(&mut context).is_ready(),
        "runtime drop must terminate before gate release"
    );
    assert!(matches!(
        held.as_mut().poll(&mut context),
        Poll::Ready(Err(_))
    ));
    assert_eq!(reductions.load(Ordering::Relaxed), 1);
}
