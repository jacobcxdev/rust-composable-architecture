//! Qualifies typed causal work and frozen operation endpoints through public APIs.

#[path = "support/cold_listener_alloc.rs"]
mod cold_listener_alloc;

#[expect(
    dead_code,
    reason = "this target uses only captured-task host controls"
)]
#[path = "support/mod.rs"]
mod support;

use std::cell::Cell;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

use rca_core::{
    Effect, Feature, FeatureContext, MountIdentity, Operation, OperationPolicy, OptionalMount,
    OutputDelivery, OutputFlow, RootMount, RuntimeFuture, SelfWait, Store, Task, TaskAccessReason,
    TaskContext, TaskProgress,
};

/// Polls one selected future without running other captured work.
fn poll<T>(future: Pin<&mut (impl Future<Output = T> + ?Sized)>) -> Poll<T> {
    future.poll(&mut Context::from_waker(Waker::noop()))
}

/// Gives an actor request at most four turns to account for queued control work.
fn ordered<T>(actor: &mut RuntimeFuture, request: impl Future<Output = T>) -> T {
    let mut request = Box::pin(request);
    for _ in 0..4 {
        if let Poll::Ready(value) = poll(request.as_mut()) {
            return value;
        }
        assert!(poll(actor.as_mut()).is_pending());
    }
    panic!("ordered request did not finish within its finite actor schedule");
}

/// Distinguishes lazy factory entry from physical destruction after cancellation.
#[derive(Default)]
struct TaskObservation {
    /// Counts factory calls independently of registered operation generations.
    starts: AtomicUsize,
    /// Counts captures released by either skipped or previously polled factories.
    drops: AtomicUsize,
}

/// Records destruction even when cancellation prevents the factory from running.
struct TaskCapture(Arc<TaskObservation>);

impl Drop for TaskCapture {
    fn drop(&mut self) {
        self.0.drops.fetch_add(1, Ordering::SeqCst);
    }
}

/// Keeps declared work pending so each test controls its cancellation boundary.
fn replacement_task(
    observation: Arc<TaskObservation>,
    policy: Option<OperationPolicy>,
) -> Task<(), ReplacementAction> {
    let capture = TaskCapture(observation);
    let run = move |_| {
        capture.0.starts.fetch_add(1, Ordering::SeqCst);
        async move {
            let _capture = capture;
            std::future::pending::<()>().await;
        }
    };
    match policy {
        Some(policy) => Task::run_named("work", policy, run),
        None => Task::run(run),
    }
}

/// Changes action cases while addressing the same named operation.
enum ReplacementAction {
    /// Starts an existing generation before the replacement transaction.
    Seed,
    /// Declares unnamed work followed by two explicitly governed named tasks.
    Pair([OperationPolicy; 2]),
    /// Replaces work last declared by an output handler.
    Restart,
}

/// Supplies one independently observed capture for each ordered declaration.
struct ReplacementTasks {
    /// Supplies each declaration with a distinct externally observed lifetime.
    observations: std::vec::IntoIter<Arc<TaskObservation>>,
}

impl Feature for ReplacementTasks {
    type State = ();
    type Action = ReplacementAction;

    fn update(
        &mut self,
        (): &mut (),
        action: &Self::Action,
        context: &mut FeatureContext<'_, (), Self::Action>,
    ) {
        let policies: &[_] = match action {
            ReplacementAction::Seed => &[Some(OperationPolicy::Coexist)],
            ReplacementAction::Pair(policies) => &[None, Some(policies[0]), Some(policies[1])],
            ReplacementAction::Restart => &[Some(OperationPolicy::Replace)],
        };
        for &policy in policies {
            context.task(replacement_task(self.observations.next().unwrap(), policy));
        }
    }
}

/// Every replacement applies in declaration order, including within one turn.
#[test]
fn typed_task_replacement_is_per_declaration_and_leaves_unnamed_work_alone() {
    use OperationPolicy::{Coexist, Replace};

    for (policies, cancelled, active) in [
        ([Coexist, Coexist], [false, false], 3),
        ([Coexist, Replace], [true, true], 1),
        ([Replace, Coexist], [true, false], 2),
        ([Replace, Replace], [true, true], 1),
    ] {
        let host = support::ManualRuntime::new();
        let observations: [_; 4] = std::array::from_fn(|_| Arc::new(TaskObservation::default()));
        let store = Store::builder(
            (),
            ReplacementTasks {
                observations: Vec::from(observations.clone()).into_iter(),
            },
            host.runtime(),
        )
        .build()
        .unwrap();
        let operation = store.operation("work");
        let mut actor = host.take_only_queued_task();
        let first_receipt = ordered(&mut actor, store.send(ReplacementAction::Seed)).unwrap();
        let mut first = host.take_only_queued_task();
        assert!(poll(first.as_mut()).is_pending());
        let mut first_wait = Box::pin(ordered(&mut actor, operation.capture()).unwrap());

        let receipt = ordered(&mut actor, store.send(ReplacementAction::Pair(policies))).unwrap();
        let mut bodies = host.take_queued_tasks();
        assert_eq!(bodies.len(), 3);
        let mut captured = Box::pin(ordered(&mut actor, operation.capture()).unwrap());
        assert!(poll(first_wait.as_mut()).is_pending());
        assert!(poll(captured.as_mut()).is_pending());
        assert!(!first_receipt.is_finished());

        assert_eq!(poll(first.as_mut()).is_ready(), cancelled[0]);
        assert!(
            poll(bodies[0].as_mut()).is_pending(),
            "unnamed work survives"
        );
        assert_eq!(poll(bodies[1].as_mut()).is_ready(), cancelled[1]);
        assert!(poll(bodies[2].as_mut()).is_pending());
        assert_eq!(poll(first_wait.as_mut()).is_ready(), cancelled[0]);
        assert_eq!(
            observations
                .each_ref()
                .map(|item| item.starts.load(Ordering::SeqCst)),
            [1, 1, usize::from(!cancelled[1]), 1]
        );
        assert_eq!(
            observations
                .each_ref()
                .map(|item| item.drops.load(Ordering::SeqCst)),
            [usize::from(cancelled[0]), 0, usize::from(cancelled[1]), 0]
        );
        assert_eq!(
            ordered(&mut actor, operation.status())
                .unwrap()
                .active_count(),
            active
        );
        assert_eq!(first_receipt.is_finished(), cancelled[0]);

        ordered(&mut actor, operation.cancel()).unwrap();
        assert!(poll(captured.as_mut()).is_pending());
        if !cancelled[0] {
            assert!(poll(first.as_mut()).is_ready());
        }
        if !cancelled[1] {
            assert!(poll(bodies[1].as_mut()).is_ready());
        }
        assert!(poll(bodies[2].as_mut()).is_ready());
        assert_eq!(poll(first_wait.as_mut()), Poll::Ready(Ok(())));
        assert_eq!(poll(captured.as_mut()), Poll::Ready(Ok(())));
        assert!(poll(bodies[0].as_mut()).is_pending());
        assert!(
            !receipt.is_finished(),
            "unnamed work retains its causal receipt"
        );
        assert_eq!(
            ordered(&mut actor, operation.status())
                .unwrap()
                .active_count(),
            0
        );

        drop(store);
        assert!(poll(actor.as_mut()).is_ready());
        assert!(poll(bodies[0].as_mut()).is_ready());
        assert!(receipt.is_finished());
        assert_eq!(
            observations
                .each_ref()
                .map(|item| item.drops.load(Ordering::SeqCst)),
            [1, 1, 1, 1]
        );
    }
}

/// Gives an output a strict child source without declaring any child-owned work.
struct ReplacementOutputSource;

impl Feature for ReplacementOutputSource {
    type State = ();
    type Action = ();

    fn update(&mut self, (): &mut (), (): &(), _: &mut FeatureContext<'_, (), ()>) {}
}

/// The same root operation spans reducer and output-handler transactions.
#[test]
fn typed_task_replacement_crosses_action_and_output_handler_boundaries() {
    let host = support::ManualRuntime::new();
    let observations: [_; 3] = std::array::from_fn(|_| Arc::new(TaskObservation::default()));
    let (builder, source_site) = Store::builder(
        (),
        ReplacementTasks {
            observations: vec![Arc::clone(&observations[0]), Arc::clone(&observations[2])]
                .into_iter(),
        },
        host.runtime(),
    )
    .mount_with_site(OptionalMount::new(
        |state: &()| Some(state),
        |state: &mut ()| Some(state),
        |()| {
            Some(MountIdentity {
                key: (),
                replacement: 0,
                configuration: 0,
            })
        },
        |()| ReplacementOutputSource,
    ));
    let (builder, output) = builder.output::<Arc<TaskObservation>>(&source_site, |handlers| {
        handlers.at_root(|(), observation, context| {
            context.task(replacement_task(
                observation,
                Some(OperationPolicy::Replace),
            ));
            OutputFlow::Consumed
        });
    });
    let store = builder.build().unwrap();
    let mut actor = host.take_only_queued_task();
    ordered(&mut actor, store.ready()).unwrap();
    let source = ordered(&mut actor, store.acquire(&source_site))
        .unwrap()
        .unwrap();
    let operation = store.operation("work");
    let first_receipt = ordered(&mut actor, store.send(ReplacementAction::Seed)).unwrap();
    let mut first = host.take_only_queued_task();
    assert!(poll(first.as_mut()).is_pending());
    let mut first_wait = Box::pin(ordered(&mut actor, operation.capture()).unwrap());

    let OutputDelivery::Consumed {
        task: output_receipt,
    } = ordered(
        &mut actor,
        source.output(&output, Arc::clone(&observations[1])),
    )
    .unwrap()
    else {
        panic!("the root output handler must consume its child's output");
    };
    let mut output_body = host.take_only_queued_task();
    assert!(poll(first_wait.as_mut()).is_pending());
    assert!(poll(first.as_mut()).is_ready());
    assert_eq!(poll(first_wait.as_mut()), Poll::Ready(Ok(())));
    assert!(poll(output_body.as_mut()).is_pending());
    let mut output_wait = Box::pin(ordered(&mut actor, operation.capture()).unwrap());
    assert!(first_receipt.is_finished());
    assert_eq!(
        ordered(&mut actor, operation.status())
            .unwrap()
            .active_count(),
        1
    );

    let final_receipt = ordered(&mut actor, store.send(ReplacementAction::Restart)).unwrap();
    let mut final_body = host.take_only_queued_task();
    assert!(poll(output_wait.as_mut()).is_pending());
    assert!(poll(output_body.as_mut()).is_ready());
    assert_eq!(poll(output_wait.as_mut()), Poll::Ready(Ok(())));
    assert!(poll(final_body.as_mut()).is_pending());
    assert_eq!(
        ordered(&mut actor, operation.status())
            .unwrap()
            .active_count(),
        1
    );
    assert!(output_receipt.is_finished());
    ordered(&mut actor, operation.cancel()).unwrap();
    assert!(!final_receipt.is_finished());
    assert!(poll(final_body.as_mut()).is_ready());
    assert!(poll(actor.as_mut()).is_pending());
    assert!(final_receipt.is_finished());
    assert_eq!(
        observations
            .each_ref()
            .map(|item| item.starts.load(Ordering::SeqCst)),
        [1, 1, 1]
    );
    assert_eq!(
        observations
            .each_ref()
            .map(|item| item.drops.load(Ordering::SeqCst)),
        [1, 1, 1]
    );
    drop(store);
    assert!(poll(actor.as_mut()).is_ready());
}

/// Holds either a passive wait or its cancel-on-drop wrapper without changing pins.
type CapturedWait = Pin<Box<dyn Future<Output = Result<(), SelfWait>> + Send>>;

/// Builds a completion chain whose descendant no longer inherits cancellation.
enum CausalAction {
    /// Starts the named root task.
    Start,
    /// Adds a child that still inherits cancellation.
    Sibling,
    /// Sends the next action without inherited cancellation.
    Handoff,
    /// Attempts to wait for its own operation, then waits for an explicit release.
    Descendant,
    /// Checks that the descendant's exported context still reaches live state.
    Add(usize),
}

/// Shares observations and the release signal between the test and task bodies.
struct CausalShared {
    /// Moves the same externally polled wait into the descendant.
    captured: Mutex<Option<CapturedWait>>,
    /// Exports the descendant's context for live access after cancellation.
    context: Mutex<Option<TaskContext<Cell<usize>, CausalAction>>>,
    /// Records the descendant's self-wait result independently of its lifetime.
    outcomes: Mutex<Vec<Result<(), SelfWait>>>,
    /// Distinguishes a returned root body from unfinished descendant work.
    root_done: AtomicBool,
    /// Keeps the descendant alive until its external checks finish.
    release: async_channel::Receiver<()>,
}

/// Starts typed tasks and an effect handoff within the same completion ancestry.
struct CausalFeature(Arc<CausalShared>);

impl Feature for CausalFeature {
    type State = Cell<usize>;
    type Action = CausalAction;

    fn update(
        &mut self,
        state: &mut Self::State,
        action: &Self::Action,
        context: &mut FeatureContext<'_, Self::State, Self::Action>,
    ) {
        match action {
            CausalAction::Start => {
                let shared = Arc::clone(&self.0);
                context.task(Task::run_named(
                    "work",
                    OperationPolicy::Coexist,
                    move |context| async move {
                        let _receipt = context.send(CausalAction::Sibling).await.unwrap();
                        let _receipt = context.send(CausalAction::Handoff).await.unwrap();
                        shared.root_done.store(true, Ordering::SeqCst);
                    },
                ));
            }
            CausalAction::Sibling => context.task(Task::run(|_| std::future::pending())),
            CausalAction::Handoff => context
                .effect(Effect::send(CausalAction::Descendant).without_inherited_cancellation()),
            CausalAction::Descendant => {
                let shared = Arc::clone(&self.0);
                context.task(Task::run(move |context| async move {
                    *shared.context.lock().unwrap() = Some(context);
                    let wait = shared.captured.lock().unwrap().take().unwrap();
                    let outcome = wait.await;
                    shared.outcomes.lock().unwrap().push(outcome);
                    shared.release.recv().await.unwrap();
                }));
            }
            CausalAction::Add(value) => state.set(state.get() + value),
        }
    }
}

/// Moving an external wait into a descendant must detect self-wait in all schedules.
///
/// The same pinned observer is first polled externally. The handoff removes
/// cancellation authority, but preserves the descendant's completion ancestry.
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "One descendant sequence preserves completion and self-wait through cancellation handover"
)]
fn typed_descendant_preserves_completion_and_self_wait_after_cancellation_handoff() {
    for (cancelling_guard, cancel_before_descendant) in
        [(false, false), (true, false), (false, true), (true, true)]
    {
        let host = support::ManualRuntime::new();
        let (release, released) = async_channel::bounded(1);
        let shared = Arc::new(CausalShared {
            captured: Mutex::new(None),
            context: Mutex::new(None),
            outcomes: Mutex::new(Vec::new()),
            root_done: AtomicBool::new(false),
            release: released,
        });
        let store = Store::builder(
            Cell::new(1),
            CausalFeature(Arc::clone(&shared)),
            host.runtime(),
        )
        .build()
        .unwrap();
        let operation = store.operation("work");
        let mut actor = host.take_only_queued_task();
        let root_receipt = ordered(&mut actor, store.send(CausalAction::Start)).unwrap();
        let mut root_body = host.take_only_queued_task();
        assert!(poll(root_body.as_mut()).is_pending());
        assert!(poll(actor.as_mut()).is_pending());
        let mut sibling = host.take_only_queued_task();
        assert!(poll(root_body.as_mut()).is_pending());
        assert!(poll(actor.as_mut()).is_pending());
        let mut handoff = host.take_only_queued_task();
        assert!(poll(root_body.as_mut()).is_ready());
        assert!(shared.root_done.load(Ordering::SeqCst));
        assert!(!root_receipt.is_finished());
        assert!(poll(handoff.as_mut()).is_pending());
        assert!(poll(actor.as_mut()).is_pending());
        assert!(poll(handoff.as_mut()).is_ready());
        let mut descendant = host.take_only_queued_task();
        let status = ordered(&mut actor, operation.status()).unwrap();
        assert_eq!(status.active_count(), 1);
        assert_eq!(
            format!("{status:?}"),
            "OperationStatus { active_count: 1, .. }"
        );

        let captured = ordered(&mut actor, operation.capture()).unwrap();
        assert_eq!(format!("{captured:?}"), "OperationWait { .. }");
        let mut captured: CapturedWait = if cancelling_guard {
            let cancelling = captured.finish_cancelling_on_drop();
            assert_eq!(format!("{cancelling:?}"), "CancellingOperationWait { .. }");
            Box::pin(cancelling)
        } else {
            Box::pin(captured)
        };
        assert!(poll(captured.as_mut()).is_pending());
        *shared.captured.lock().unwrap() = Some(captured);
        let mut external = Box::pin(ordered(&mut actor, operation.capture()).unwrap());
        assert!(poll(external.as_mut()).is_pending());

        if cancel_before_descendant {
            // The same already-Pending observer must check actual descendant
            // ancestry after cancellation, not return cached external polling.
            ordered(&mut actor, operation.cancel()).unwrap();
            assert!(poll(sibling.as_mut()).is_ready());
            assert!(poll(external.as_mut()).is_pending());
            assert_eq!(
                ordered(&mut actor, operation.status())
                    .unwrap()
                    .active_count(),
                1
            );
        }
        assert!(poll(descendant.as_mut()).is_pending());
        assert_eq!(*shared.outcomes.lock().unwrap(), [Err(SelfWait)]);
        let outcomes = shared.outcomes.lock().unwrap();
        let error = outcomes[0].as_ref().unwrap_err();
        assert_eq!(
            error.to_string(),
            "a managed operation cannot wait for its own completion"
        );
        drop(outcomes);
        assert!(poll(external.as_mut()).is_pending());
        if !cancel_before_descendant {
            assert!(
                poll(sibling.as_mut()).is_pending(),
                "SelfWait must disarm the guard before it can cancel a real inherited sibling"
            );
            ordered(&mut actor, operation.cancel()).unwrap();
            assert!(poll(sibling.as_mut()).is_ready());
        }
        assert!(poll(descendant.as_mut()).is_pending());
        assert_eq!(
            ordered(&mut actor, operation.status())
                .unwrap()
                .active_count(),
            1
        );

        let context = shared.context.lock().unwrap().as_ref().unwrap().clone();
        assert_eq!(ordered(&mut actor, context.read(Cell::get)).unwrap(), 1);
        let updated = ordered(
            &mut actor,
            context.update(|state| {
                state.set(state.get() + 2);
                state.get()
            }),
        )
        .unwrap();
        assert_eq!(*updated.value(), 3);
        let _receipt = ordered(&mut actor, context.send(CausalAction::Add(4))).unwrap();
        assert_eq!(ordered(&mut actor, context.read(Cell::get)).unwrap(), 7);
        assert!(poll(external.as_mut()).is_pending());

        release.try_send(()).unwrap();
        assert!(poll(descendant.as_mut()).is_ready());
        assert_eq!(poll(external.as_mut()), Poll::Ready(Ok(())));
        assert_eq!(format!("{external:?}"), "OperationWait { .. }");
        let status = ordered(&mut actor, operation.status()).unwrap();
        assert_eq!(status.active_count(), 0);
        assert_eq!(
            format!("{status:?}"),
            "OperationStatus { active_count: 0, .. }"
        );
        // Ready allocations and metadata contexts retain no completion ownership.
        assert!(root_receipt.is_finished());
        drop((context, descendant, handoff, sibling, root_body, store));
        assert!(poll(actor.as_mut()).is_ready());
    }
}

/// Holds two independent child sites and a configuration that can invalidate both.
struct ScopedState {
    /// The child removed and reinserted to test stale endpoints.
    first: Option<Cell<usize>>,
    /// An untouched sibling used to check cancellation isolation.
    second: Option<Cell<usize>>,
    /// Changes the scope identity without changing child storage.
    root_configuration: u64,
    /// Counts state selection so operation calls can prove they avoid it.
    selections: Arc<AtomicUsize>,
}

/// Changes named root work, child membership or root configuration.
enum ScopedAction {
    /// Starts one pending root task named `work`.
    Start,
    /// Removes the first child.
    Remove,
    /// Inserts a fresh first child at the same site.
    Insert,
    /// Changes the root configuration identity.
    Configure(u64),
}

/// Controls the root task and the two mounted child lifetimes.
struct ScopedFeature;

impl Feature for ScopedFeature {
    type State = ScopedState;
    type Action = ScopedAction;

    fn update(
        &mut self,
        state: &mut Self::State,
        action: &Self::Action,
        context: &mut FeatureContext<'_, Self::State, Self::Action>,
    ) {
        match action {
            ScopedAction::Start => {
                context.task(Task::run_named("work", OperationPolicy::Coexist, |_| {
                    std::future::pending()
                }));
            }
            ScopedAction::Remove => state.first = None,
            ScopedAction::Insert => state.first = Some(Cell::new(0)),
            ScopedAction::Configure(value) => state.root_configuration = *value,
        }
    }
}

/// Starts named work in whichever child or root owns this feature.
enum ScopedChildAction {
    /// Starts one pending task named `work`.
    Start,
}

/// Uses the same operation name in distinct stores and mount scopes.
struct ScopedChild;

impl Feature for ScopedChild {
    type State = Cell<usize>;
    type Action = ScopedChildAction;

    fn update(
        &mut self,
        _: &mut Self::State,
        _: &Self::Action,
        context: &mut FeatureContext<'_, Self::State, Self::Action>,
    ) {
        context.task(Task::run_named("work", OperationPolicy::Coexist, |_| {
            std::future::pending()
        }));
    }
}

/// Requires every stale endpoint method to fail before selecting child state.
fn assert_stale_operation(
    actor: &mut RuntimeFuture,
    operation: &Operation,
    selections: &AtomicUsize,
) {
    selections.store(0, Ordering::SeqCst);
    let status = ordered(actor, operation.status()).unwrap_err();
    let capture = ordered(actor, operation.capture()).unwrap_err();
    let cancel = ordered(actor, operation.cancel()).unwrap_err();
    for error in [status, capture, cancel] {
        assert_eq!(error.reason(), TaskAccessReason::ScopeEnded);
        assert_eq!(error.progress(), TaskProgress::NotStarted);
    }
    assert_eq!(selections.load(Ordering::SeqCst), 0);
}

/// Matching operation names in separate stores do not share cancellation authority.
#[test]
fn same_root_key_on_two_stores_has_independent_authority() {
    let first_host = support::ManualRuntime::new();
    let second_host = support::ManualRuntime::new();
    let first = Store::builder(Cell::new(0), ScopedChild, first_host.runtime())
        .build()
        .unwrap();
    let second = Store::builder(Cell::new(0), ScopedChild, second_host.runtime())
        .build()
        .unwrap();
    let mut first_actor = first_host.take_only_queued_task();
    let mut second_actor = second_host.take_only_queued_task();
    let first_operation = first.operation("work");
    let second_operation = second.operation("work");
    let _ = ordered(&mut first_actor, first.send(ScopedChildAction::Start)).unwrap();
    let mut first_body = first_host.take_only_queued_task();
    let _ = ordered(&mut second_actor, second.send(ScopedChildAction::Start)).unwrap();
    let mut second_body = second_host.take_only_queued_task();
    for (actor, operation) in [
        (&mut first_actor, &first_operation),
        (&mut second_actor, &second_operation),
    ] {
        assert_eq!(
            ordered(actor, operation.status()).unwrap().active_count(),
            1
        );
    }
    ordered(&mut first_actor, first_operation.cancel()).unwrap();
    assert!(poll(first_body.as_mut()).is_ready());
    assert!(poll(second_body.as_mut()).is_pending());
    assert_eq!(
        ordered(&mut first_actor, first_operation.status())
            .unwrap()
            .active_count(),
        0
    );
    assert_eq!(
        ordered(&mut second_actor, second_operation.status())
            .unwrap()
            .active_count(),
        1
    );
    ordered(&mut second_actor, second_operation.cancel()).unwrap();
    assert!(poll(second_body.as_mut()).is_ready());
    drop((first, second));
    assert!(poll(first_actor.as_mut()).is_ready());
    assert!(poll(second_actor.as_mut()).is_ready());
}

/// Rechecks completion after allocation, before a cold listener can receive a wake.
#[test]
fn operation_wait_observes_completion_during_cold_listener_creation() {
    let host = support::ManualRuntime::new();
    let store = Store::builder(Cell::new(0), ScopedChild, host.runtime())
        .build()
        .unwrap();
    let mut actor = host.take_only_queued_task();
    let _receipt = ordered(&mut actor, store.send(ScopedChildAction::Start)).unwrap();
    let mut body = host.take_only_queued_task();
    assert!(poll(body.as_mut()).is_pending());
    let operation = store.operation("work");
    assert_eq!(
        ordered(&mut actor, operation.status())
            .unwrap()
            .active_count(),
        1
    );
    let mut wait = Box::pin(ordered(&mut actor, operation.capture()).unwrap());

    // Keep the actual runtime body as the final named-work owner. The send
    // receipt has an independent lifetime; actor teardown can finish it first.
    // This observer has never been polled, so its event is still cold.
    drop(store);
    assert!(poll(actor.as_mut()).is_ready());
    drop(actor);

    cold_listener_alloc::prepare();
    let (ready, started) = std::sync::mpsc::sync_channel(0);
    let worker = std::thread::spawn(move || {
        ready.send(()).unwrap();
        let intercepted = cold_listener_alloc::await_interception();
        drop(body);
        if intercepted {
            cold_listener_alloc::produced();
        }
        intercepted
    });
    started
        .recv_timeout(std::time::Duration::from_secs(5))
        .expect("completion worker started");

    // The first allocation in this allocation-free polling prefix belongs to
    // Event::listen, before list registration. Complete while it is paused:
    // no notification can be saved for this not-yet-registered listener.
    cold_listener_alloc::arm();
    let first_poll = poll(wait.as_mut());
    cold_listener_alloc::disarm();
    let intercepted = worker.join().expect("completion worker joined");
    cold_listener_alloc::assert_completed();
    assert!(intercepted);
    assert_eq!(first_poll, Poll::Ready(Ok(())));
    assert_eq!(poll(wait.as_mut()), Poll::Ready(Ok(())));
}

/// Child operation endpoints keep their acquired scope without reselecting state.
///
/// These contexts were acquired externally. Their scope must not fall back to the
/// root merely because there is no producing task from which to derive it.
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "One external operation sequence preserves scoped frozen authority without selector entry"
)]
fn externally_acquired_operation_is_scoped_frozen_and_selector_free() {
    let host = support::ManualRuntime::new();
    let selections = Arc::new(AtomicUsize::new(0));
    let first = OptionalMount::new(
        |state: &ScopedState| {
            state.selections.fetch_add(1, Ordering::SeqCst);
            state.first.as_ref()
        },
        |state: &mut ScopedState| {
            state.selections.fetch_add(1, Ordering::SeqCst);
            state.first.as_mut()
        },
        |state: &ScopedState| {
            state.first.as_ref().map(|_| MountIdentity {
                key: (),
                replacement: 0,
                configuration: 0,
            })
        },
        |_| ScopedChild,
    );
    let second = OptionalMount::new(
        |state: &ScopedState| {
            state.selections.fetch_add(1, Ordering::SeqCst);
            state.second.as_ref()
        },
        |state: &mut ScopedState| {
            state.selections.fetch_add(1, Ordering::SeqCst);
            state.second.as_mut()
        },
        |state: &ScopedState| {
            state.second.as_ref().map(|_| MountIdentity {
                key: (),
                replacement: 0,
                configuration: 0,
            })
        },
        |_| ScopedChild,
    );
    let (builder, first_site) = RootMount::new(ScopedFeature)
        .configuration(|state: &ScopedState| state.root_configuration)
        .builder(
            ScopedState {
                first: Some(Cell::new(0)),
                second: Some(Cell::new(0)),
                root_configuration: 0,
                selections: Arc::clone(&selections),
            },
            host.runtime(),
        )
        .mount_with_site(first);
    let (builder, second_site) = builder.mount_with_site(second);
    let store = builder.build().unwrap();
    let mut actor = host.take_only_queued_task();
    ordered(&mut actor, store.ready()).unwrap();
    let first = ordered(&mut actor, store.acquire(&first_site))
        .unwrap()
        .unwrap();
    let second = ordered(&mut actor, store.acquire(&second_site))
        .unwrap()
        .unwrap();
    let root_operation = store.operation("work");
    let first_operation = first.operation("work");
    let second_operation = second.operation("work");

    let _ = ordered(&mut actor, store.send(ScopedAction::Start)).unwrap();
    let mut root_body = host.take_only_queued_task();
    let _ = ordered(&mut actor, first.send(ScopedChildAction::Start)).unwrap();
    let mut first_body = host.take_only_queued_task();
    let _ = ordered(&mut actor, second.send(ScopedChildAction::Start)).unwrap();
    let mut second_body = host.take_only_queued_task();
    selections.store(0, Ordering::SeqCst);
    for operation in [&root_operation, &first_operation, &second_operation] {
        assert_eq!(
            ordered(&mut actor, operation.status())
                .unwrap()
                .active_count(),
            1
        );
    }
    let mut old_capture = Box::pin(ordered(&mut actor, first_operation.capture()).unwrap());
    ordered(&mut actor, first_operation.cancel()).unwrap();
    assert_eq!(selections.load(Ordering::SeqCst), 0);
    assert!(poll(first_body.as_mut()).is_ready());
    assert_eq!(poll(old_capture.as_mut()), Poll::Ready(Ok(())));
    assert_eq!(
        ordered(&mut actor, first_operation.status())
            .unwrap()
            .active_count(),
        0
    );
    assert_eq!(
        ordered(&mut actor, root_operation.status())
            .unwrap()
            .active_count(),
        1
    );
    assert_eq!(
        ordered(&mut actor, second_operation.status())
            .unwrap()
            .active_count(),
        1
    );

    let _ = ordered(&mut actor, store.send(ScopedAction::Remove)).unwrap();
    assert_stale_operation(&mut actor, &first_operation, &selections);
    let _ = ordered(&mut actor, store.send(ScopedAction::Insert)).unwrap();
    assert_stale_operation(&mut actor, &first_operation, &selections);
    let replacement = ordered(&mut actor, store.acquire(&first_site))
        .unwrap()
        .unwrap();
    let replacement_operation = replacement.operation("work");
    let _ = ordered(&mut actor, replacement.send(ScopedChildAction::Start)).unwrap();
    let mut replacement_body = host.take_only_queued_task();
    assert_eq!(
        ordered(&mut actor, replacement_operation.status())
            .unwrap()
            .active_count(),
        1
    );
    assert_stale_operation(&mut actor, &first_operation, &selections);
    assert!(poll(replacement_body.as_mut()).is_pending());
    assert_eq!(poll(old_capture.as_mut()), Poll::Ready(Ok(())));

    let _ = ordered(&mut actor, store.send(ScopedAction::Configure(1))).unwrap();
    assert_stale_operation(&mut actor, &replacement_operation, &selections);
    assert_stale_operation(&mut actor, &second_operation, &selections);
    let current = ordered(&mut actor, store.acquire(&first_site))
        .unwrap()
        .unwrap();
    assert_eq!(
        ordered(&mut actor, current.operation("work").status())
            .unwrap()
            .active_count(),
        1
    );
    let _ = ordered(&mut actor, store.send(ScopedAction::Configure(0))).unwrap();
    assert_stale_operation(&mut actor, &replacement_operation, &selections);
    assert_stale_operation(&mut actor, &current.operation("work"), &selections);

    drop(store);
    assert!(poll(actor.as_mut()).is_ready());
    assert!(poll(root_body.as_mut()).is_ready());
    assert!(poll(second_body.as_mut()).is_ready());
    assert!(poll(replacement_body.as_mut()).is_ready());
}
