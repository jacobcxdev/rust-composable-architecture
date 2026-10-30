#![cfg(feature = "local")]

use std::cell::{Cell, RefCell};
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll, Waker};
use std::thread::{self, ThreadId};

use rca_core::local::{
    Feature, FeatureContext, Operation, OptionalMount, RootMount, Runtime, RuntimeDriver,
    RuntimeFuture, Store, Task,
};
use rca_core::{
    MonotonicInstant, MountIdentity, OperationPolicy, SelfWait, TaskAccessReason, TaskProgress,
};

/// Runs an owner-local actor through explicit polls at a fixed zero clock.
#[derive(Clone)]
struct Driver {
    /// Rejects accidental cross-thread use of the Local runtime fixture.
    owner: ThreadId,
    /// Holds actor and task futures until the test explicitly polls them.
    queued: Rc<RefCell<Vec<RuntimeFuture>>>,
}

impl std::fmt::Debug for Driver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("LocalOperationDriver")
    }
}

impl Driver {
    fn new() -> Self {
        Self {
            owner: thread::current().id(),
            queued: Rc::default(),
        }
    }

    /// Takes the sole queued future so callbacks run outside the queue borrow.
    fn take(&self) -> RuntimeFuture {
        assert_eq!(self.queued.borrow().len(), 1);
        self.queued.borrow_mut().pop().unwrap()
    }
}

impl RuntimeDriver for Driver {
    fn spawn(&self, task: RuntimeFuture) {
        assert_eq!(thread::current().id(), self.owner);
        self.queued.borrow_mut().push(task);
    }

    fn now(&self) -> MonotonicInstant {
        MonotonicInstant::ZERO
    }
    fn sleep_until(&self, deadline: MonotonicInstant) -> RuntimeFuture {
        if deadline <= self.now() {
            Box::pin(std::future::ready(()))
        } else {
            Box::pin(std::future::pending())
        }
    }
    fn yield_now(&self) -> RuntimeFuture {
        let mut yielded = false;
        Box::pin(std::future::poll_fn(move |context| {
            if yielded {
                Poll::Ready(())
            } else {
                yielded = true;
                context.waker().wake_by_ref();
                Poll::Pending
            }
        }))
    }
}

#[test]
fn local_operation_driver_honours_elapsed_and_future_sleep() {
    let driver = Driver::new();
    let mut context = Context::from_waker(Waker::noop());
    let mut elapsed = driver.sleep_until(driver.now());
    assert!(elapsed.as_mut().poll(&mut context).is_ready());
    for deadline in [
        driver
            .now()
            .saturating_add(std::time::Duration::from_nanos(1)),
        MonotonicInstant::MAX,
    ] {
        let mut future = driver.sleep_until(deadline);
        assert!(future.as_mut().poll(&mut context).is_pending());
    }
    assert!(driver.queued.borrow().is_empty());
}

#[test]
fn local_operation_driver_yields_once_and_wakes_once() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::Wake;

    /// Counts scheduling notifications without polling the actor from the waker.
    struct WakeCount(AtomicUsize);
    impl Wake for WakeCount {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
        fn wake_by_ref(self: &Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    let driver = Driver::new();
    let count = Arc::new(WakeCount(AtomicUsize::new(0)));
    let waker = Waker::from(Arc::clone(&count));
    let mut context = Context::from_waker(&waker);
    let mut yield_once = driver.yield_now();
    assert!(yield_once.as_mut().poll(&mut context).is_pending());
    assert_eq!(count.0.load(Ordering::SeqCst), 1);
    assert!(yield_once.as_mut().poll(&mut context).is_ready());
    assert_eq!(count.0.load(Ordering::SeqCst), 1);
    assert!(driver.queued.borrow().is_empty());
}

/// Polls a future once without granting the fixture any scheduler progress.
fn poll<T>(future: Pin<&mut (impl Future<Output = T> + ?Sized)>) -> Poll<T> {
    future.poll(&mut Context::from_waker(Waker::noop()))
}

/// Alternates request and actor polls for at most eight owner-thread rounds.
fn ordered<T>(actor: &mut RuntimeFuture, request: impl Future<Output = T>) -> T {
    let mut request = std::pin::pin!(request);
    for _ in 0..8 {
        if let Poll::Ready(value) = poll(request.as_mut()) {
            return value;
        }
        assert!(poll(actor.as_mut()).is_pending());
    }
    panic!("local operation exceeded its finite actor schedule");
}

/// Keeps named work live until cancellation; its write must never execute.
fn pending_task<S: 'static, A: 'static>(capture: Rc<Cell<usize>>) -> Task<S, A> {
    Task::run_named("work", OperationPolicy::Coexist, move |_| async move {
        std::future::pending::<()>().await;
        capture.set(99);
    })
}

/// Distinguishes owner-local factory entry from physical capture destruction.
#[derive(Default)]
struct TaskObservation {
    /// Counts factory calls without making the fixture thread-transferable.
    starts: Cell<usize>,
    /// Counts captures released by either skipped or previously polled factories.
    drops: Cell<usize>,
}

/// Remains non-Send even before its deferred task factory is called.
struct TaskCapture(Rc<TaskObservation>);

impl Drop for TaskCapture {
    fn drop(&mut self) {
        self.0.drops.set(self.0.drops.get() + 1);
    }
}

/// Supplies independently observed captures in declaration order.
struct ReplacementTasks {
    /// Supplies each declaration with a distinct externally observed lifetime.
    observations: std::vec::IntoIter<Rc<TaskObservation>>,
}

/// Changes action cases while addressing the same owner-local named operation.
enum ReplacementAction {
    /// Starts an existing generation before the replacement transaction.
    Seed,
    /// Declares unnamed work followed by two explicitly governed named tasks.
    Pair([OperationPolicy; 2]),
}

impl Feature for ReplacementTasks {
    type State = ();
    type Action = ReplacementAction;

    fn reduce(
        &mut self,
        (): &mut (),
        action: &Self::Action,
        context: &mut FeatureContext<'_, (), Self::Action>,
    ) {
        let policies: &[_] = match action {
            ReplacementAction::Seed => &[Some(OperationPolicy::Coexist)],
            ReplacementAction::Pair(policies) => &[None, Some(policies[0]), Some(policies[1])],
        };
        for &policy in policies {
            let capture = TaskCapture(self.observations.next().unwrap());
            let run = move |_| {
                capture.0.starts.set(capture.0.starts.get() + 1);
                async move {
                    let _capture = capture;
                    std::future::pending::<()>().await;
                }
            };
            context.task(match policy {
                Some(policy) => Task::run_named("work", policy, run),
                None => Task::run(run),
            });
        }
    }
}

/// Local tasks preserve per-declaration replacement and unnamed-task independence.
#[test]
fn local_task_replacement_is_per_declaration_and_leaves_unnamed_work_alone() {
    use OperationPolicy::{Coexist, Replace};

    for (policies, cancelled, active) in [
        ([Coexist, Coexist], [false, false], 3),
        ([Coexist, Replace], [true, true], 1),
        ([Replace, Coexist], [true, false], 2),
        ([Replace, Replace], [true, true], 1),
    ] {
        let host = Driver::new();
        let observations: [_; 4] = std::array::from_fn(|_| Rc::new(TaskObservation::default()));
        let store = Store::feature_builder(
            (),
            ReplacementTasks {
                observations: Vec::from(observations.clone()).into_iter(),
            },
            Runtime::new(host.clone()),
        )
        .build()
        .unwrap();
        let operation = store.operation("work");
        let mut actor = host.take();
        let first_receipt = ordered(&mut actor, store.send(ReplacementAction::Seed)).unwrap();
        let mut first = host.take();
        assert!(poll(first.as_mut()).is_pending());
        let mut first_wait = Box::pin(ordered(&mut actor, operation.capture()).unwrap());

        let receipt = ordered(&mut actor, store.send(ReplacementAction::Pair(policies))).unwrap();
        let mut bodies = std::mem::take(&mut *host.queued.borrow_mut());
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
            observations.each_ref().map(|item| item.starts.get()),
            [1, 1, usize::from(!cancelled[1]), 1]
        );
        assert_eq!(
            observations.each_ref().map(|item| item.drops.get()),
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
            observations.each_ref().map(|item| item.drops.get()),
            [1, 1, 1, 1]
        );
    }
}

/// Shares counters without making the owner-local projected child Send.
struct State {
    root: Rc<Cell<usize>>,
    child: Option<Rc<Cell<usize>>>,
    /// A refresh changes authority while preserving the child value.
    configuration: u64,
    /// Detects forbidden projection reads on stale operation paths.
    selections: Rc<Cell<usize>>,
}

enum Action {
    Start,
    Remove,
    Insert,
    Configure(u64),
    /// Exports the operation of a task that will finish before its consumer.
    ExportOperation(Rc<RefCell<Option<Operation>>>),
    /// Occupies the bounded mailbox without changing named work.
    Filler(Rc<Cell<usize>>),
}
struct Root;
struct Child;

impl Feature for Root {
    type State = State;
    type Action = Action;

    fn reduce(
        &mut self,
        state: &mut State,
        action: &Action,
        context: &mut FeatureContext<'_, State, Action>,
    ) {
        match action {
            Action::Start => context.task(pending_task(Rc::clone(&state.root))),
            Action::Remove => state.child = None,
            Action::Insert => state.child = Some(Rc::new(Cell::new(0))),
            Action::Configure(value) => state.configuration = *value,
            Action::ExportOperation(exported) => {
                let exported = Rc::clone(exported);
                context.task(Task::run(move |context| async move {
                    *exported.borrow_mut() = Some(context.operation("work"));
                }));
            }
            Action::Filler(calls) => calls.set(calls.get() + 1),
        }
    }
}

impl Feature for Child {
    type State = Rc<Cell<usize>>;
    type Action = ();

    fn reduce(
        &mut self,
        state: &mut Self::State,
        (): &(),
        context: &mut FeatureContext<'_, Self::State, ()>,
    ) {
        context.task(pending_task(Rc::clone(state)));
    }
}

/// Requires stale operations to reject before selecting the current state.
fn assert_stale(actor: &mut RuntimeFuture, operation: &Operation, selections: &Cell<usize>) {
    selections.set(0);
    for error in [
        ordered(actor, operation.status()).unwrap_err(),
        ordered(actor, operation.capture()).unwrap_err(),
        ordered(actor, operation.cancel()).unwrap_err(),
    ] {
        assert_eq!(error.reason(), TaskAccessReason::ScopeEnded);
        assert_eq!(error.progress(), TaskProgress::NotStarted);
    }
    assert_eq!(selections.get(), 0);
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "One Local operation sequence preserves root and frozen-child registry ownership"
)]
fn local_root_and_frozen_child_operations_share_the_registry_without_state_selection() {
    let host = Driver::new();
    let selections = Rc::new(Cell::new(0));
    let mount = OptionalMount::new(
        |state: &State| {
            state.selections.set(state.selections.get() + 1);
            state.child.as_ref()
        },
        |state: &mut State| {
            state.selections.set(state.selections.get() + 1);
            state.child.as_mut()
        },
        |state: &State| {
            state.child.as_ref().map(|_| MountIdentity {
                key: (),
                replacement: 0,
                configuration: 0,
            })
        },
        |_| Child,
    );
    let (builder, site) = RootMount::new(Root)
        .configuration(|state: &State| state.configuration)
        .builder(
            State {
                root: Rc::new(Cell::new(0)),
                child: Some(Rc::new(Cell::new(0))),
                configuration: 0,
                selections: Rc::clone(&selections),
            },
            Runtime::new(host.clone()),
        )
        .mount_with_site(mount);
    let store = builder.build().unwrap();
    let mut actor = host.take();
    ordered(&mut actor, store.ready()).unwrap();
    let child = ordered(&mut actor, store.acquire(&site)).unwrap().unwrap();
    let root_operation = store.operation("work");
    let child_operation = child.operation("work");
    selections.set(0);
    assert_eq!(format!("{root_operation:?}"), "Operation { .. }");
    assert_eq!(format!("{child_operation:?}"), "Operation { .. }");
    assert_eq!(selections.get(), 0);
    assert!(host.queued.borrow().is_empty());
    let _root_receipt = ordered(&mut actor, store.send(Action::Start)).unwrap();
    let mut root_body = host.take();
    let _child_receipt = ordered(&mut actor, child.send(())).unwrap();
    let mut child_body = host.take();

    selections.set(0);
    assert_eq!(
        ordered(&mut actor, root_operation.status())
            .unwrap()
            .active_count(),
        1
    );
    assert_eq!(
        ordered(&mut actor, child_operation.status())
            .unwrap()
            .active_count(),
        1
    );
    let mut captured = Box::pin(ordered(&mut actor, child_operation.capture()).unwrap());
    assert!(poll(captured.as_mut()).is_pending());
    ordered(&mut actor, child_operation.cancel()).unwrap();
    assert_eq!(selections.get(), 0);
    assert!(poll(captured.as_mut()).is_pending());
    assert!(poll(child_body.as_mut()).is_ready());
    assert_eq!(poll(captured.as_mut()), Poll::Ready(Ok(())));
    assert_eq!(
        ordered(&mut actor, child_operation.status())
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

    let _ = ordered(&mut actor, store.send(Action::Remove)).unwrap();
    assert_stale(&mut actor, &child_operation, &selections);
    let _ = ordered(&mut actor, store.send(Action::Insert)).unwrap();
    assert_stale(&mut actor, &child_operation, &selections);
    let replacement = ordered(&mut actor, store.acquire(&site)).unwrap().unwrap();
    let replacement_operation = replacement.operation("work");
    let _ = ordered(&mut actor, replacement.send(())).unwrap();
    let mut replacement_body = host.take();
    let _ = ordered(&mut actor, store.send(Action::Configure(1))).unwrap();
    assert_stale(&mut actor, &replacement_operation, &selections);
    let current = ordered(&mut actor, store.acquire(&site)).unwrap().unwrap();
    assert_eq!(
        ordered(&mut actor, current.operation("work").status())
            .unwrap()
            .active_count(),
        1
    );
    let _ = ordered(&mut actor, store.send(Action::Configure(0))).unwrap();
    assert_stale(&mut actor, &current.operation("work"), &selections);
    assert_eq!(poll(captured.as_mut()), Poll::Ready(Ok(())));

    drop(store);
    assert!(poll(actor.as_mut()).is_ready());
    assert!(poll(root_body.as_mut()).is_ready());
    assert!(poll(replacement_body.as_mut()).is_ready());
    assert_eq!(
        ordered_closed(root_operation.status()).reason(),
        TaskAccessReason::StoreClosed
    );
}

/// Polls a closed operation without an actor turn and returns its typed error.
fn ordered_closed(
    future: impl Future<Output = Result<rca_core::OperationStatus, rca_core::TaskAccessError>>,
) -> rca_core::TaskAccessError {
    let mut future = std::pin::pin!(future);
    match poll(future.as_mut()) {
        Poll::Ready(Err(error)) => error,
        _ => panic!("closed operation did not reject immediately"),
    }
}

/// Retains a passive operation wait across a managed task poll.
type Wait = Pin<Box<dyn Future<Output = Result<(), SelfWait>>>>;

/// Tests a task that polls its own captured wait without executor re-entry.
struct WaitFeature {
    /// Gives the managed task an externally captured wait on its own name.
    wait: Rc<RefCell<Option<Wait>>>,
    /// Records the managed poll's self-wait result.
    result: Rc<RefCell<Option<Result<(), SelfWait>>>>,
    /// Records the active work observed after self-wait rejection.
    count: Rc<Cell<usize>>,
}

impl Feature for WaitFeature {
    type State = Rc<Cell<usize>>;
    type Action = ();

    fn reduce(
        &mut self,
        _: &mut Self::State,
        (): &(),
        context: &mut FeatureContext<'_, Self::State, ()>,
    ) {
        let wait = Rc::clone(&self.wait);
        let result = Rc::clone(&self.result);
        let count = Rc::clone(&self.count);
        context.task(Task::run_named(
            "work",
            OperationPolicy::Coexist,
            move |context| async move {
                let captured = wait.borrow_mut().take().unwrap();
                *result.borrow_mut() = Some(captured.await);
                count.set(
                    context
                        .operation("work")
                        .status()
                        .await
                        .unwrap()
                        .active_count(),
                );
                std::future::pending::<()>().await;
            },
        ));
    }
}

#[test]
fn local_passive_wait_rechecks_self_wait_on_the_actual_managed_poll() {
    for cancelling_guard in [false, true] {
        let host = Driver::new();
        let wait = Rc::new(RefCell::new(None));
        let result = Rc::new(RefCell::new(None));
        let count = Rc::new(Cell::new(0));
        let store = Store::feature_builder(
            Rc::new(Cell::new(0)),
            WaitFeature {
                wait: Rc::clone(&wait),
                result: Rc::clone(&result),
                count: Rc::clone(&count),
            },
            Runtime::new(host.clone()),
        )
        .build()
        .unwrap();
        let operation = store.operation("work");
        let mut actor = host.take();
        ordered(&mut actor, store.ready()).unwrap();
        let _receipt = ordered(&mut actor, store.send(())).unwrap();
        let mut body = host.take();
        let captured = ordered(&mut actor, operation.capture()).unwrap();
        let mut captured: Wait = if cancelling_guard {
            Box::pin(captured.finish_cancelling_on_drop())
        } else {
            Box::pin(captured)
        };
        assert!(poll(captured.as_mut()).is_pending());
        *wait.borrow_mut() = Some(captured);
        let mut external = Box::pin(ordered(&mut actor, operation.capture()).unwrap());
        assert!(poll(body.as_mut()).is_pending());
        assert_eq!(*result.borrow(), Some(Err(SelfWait)));
        assert!(poll(actor.as_mut()).is_pending());
        assert!(poll(body.as_mut()).is_pending());
        assert_eq!(count.get(), 1);
        assert!(poll(external.as_mut()).is_pending());
        ordered(&mut actor, operation.cancel()).unwrap();
        assert!(poll(external.as_mut()).is_pending());
        assert!(poll(body.as_mut()).is_ready());
        assert_eq!(poll(external.as_mut()), Poll::Ready(Ok(())));
        drop(store);
        assert!(poll(actor.as_mut()).is_ready());
    }
}

/// Checks immediate typed rejection without advancing the actor or callbacks.
fn assert_operation_rejected<T>(
    future: impl Future<Output = Result<T, rca_core::TaskAccessError>>,
    reason: TaskAccessReason,
) {
    let mut future = std::pin::pin!(future);
    let Poll::Ready(Err(error)) = poll(future.as_mut()) else {
        panic!("operation must reject without an actor turn");
    };
    assert_eq!(error.reason(), reason);
    assert_eq!(error.progress(), TaskProgress::NotStarted);
}

#[test]
fn local_escaped_operation_rejects_completed_producer_and_blocked_requests_close_without_actor() {
    let host = Driver::new();
    let lifetime = Rc::new(Cell::new(0));
    let selections = Rc::new(Cell::new(0));
    let calls = Rc::new(Cell::new(0));
    let exported = Rc::new(RefCell::new(None));
    let store = Store::feature_builder(
        State {
            root: Rc::clone(&lifetime),
            child: None,
            configuration: 0,
            selections: Rc::clone(&selections),
        },
        Root,
        Runtime::new(host.clone()),
    )
    .mailbox_capacity(1)
    .build()
    .unwrap();
    let mut actor = host.take();
    ordered(&mut actor, store.ready()).unwrap();
    let target = store.operation("work");
    let named = ordered(&mut actor, store.send(Action::Start)).unwrap();
    let mut body = host.take();
    assert!(poll(body.as_mut()).is_pending());
    let mut wait = Box::pin(ordered(&mut actor, target.capture()).unwrap());
    let producer = ordered(
        &mut actor,
        store.send(Action::ExportOperation(Rc::clone(&exported))),
    )
    .unwrap();
    let mut producer_body = host.take();
    assert!(poll(producer_body.as_mut()).is_ready());
    assert!(poll(actor.as_mut()).is_pending());
    assert!(producer.is_finished());
    let escaped = exported.borrow_mut().take().unwrap();
    assert_eq!(
        ordered(&mut actor, target.status()).unwrap().active_count(),
        1
    );
    assert_operation_rejected(escaped.status(), TaskAccessReason::Cancelled);
    assert_operation_rejected(escaped.capture(), TaskAccessReason::Cancelled);
    assert_operation_rejected(escaped.cancel(), TaskAccessReason::Cancelled);
    assert!(!named.is_cancelled());
    assert!(!named.is_finished());
    assert!(poll(wait.as_mut()).is_pending());

    let mut filler = Box::pin(
        store
            .try_send(Action::Filler(Rc::clone(&calls)))
            .unwrap()
            .finish(),
    );
    assert!(
        store
            .try_send(Action::Filler(Rc::clone(&calls)))
            .unwrap_err()
            .is_full()
    );
    assert!(poll(filler.as_mut()).is_pending());
    let mut status = Box::pin(target.status());
    let mut capture = Box::pin(target.capture());
    let mut cancel = Box::pin(target.cancel());
    assert!(poll(status.as_mut()).is_pending());
    assert!(poll(capture.as_mut()).is_pending());
    assert!(poll(cancel.as_mut()).is_pending());
    drop(store);
    assert_operation_rejected(status.as_mut(), TaskAccessReason::StoreClosed);
    assert_operation_rejected(capture.as_mut(), TaskAccessReason::StoreClosed);
    assert_operation_rejected(cancel.as_mut(), TaskAccessReason::StoreClosed);
    assert_eq!(calls.get(), 0);
    assert_eq!(selections.get(), 0);
    assert!(poll(wait.as_mut()).is_pending());
    assert_eq!(Rc::strong_count(&lifetime), 3);
    assert!(poll(actor.as_mut()).is_ready());
    assert!(poll(body.as_mut()).is_ready());
    assert_eq!(poll(wait.as_mut()), Poll::Ready(Ok(())));
    assert!(named.is_finished());
    assert!(matches!(poll(filler.as_mut()), Poll::Ready(Err(_))));
    assert_eq!(lifetime.get(), 0);
    drop((
        status,
        capture,
        cancel,
        escaped,
        target,
        producer_body,
        body,
        actor,
    ));
    assert!(host.queued.borrow().is_empty());
    assert_eq!(Rc::strong_count(&lifetime), 1);
    assert_eq!(Rc::strong_count(&selections), 1);
    assert_eq!(Rc::strong_count(&calls), 1);
    assert_eq!(Rc::strong_count(&exported), 1);
}
