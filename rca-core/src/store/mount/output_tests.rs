//! Runtime checks for mounted output, admission races and task ownership.

use super::*;
use crate::{
    Effect, FeatureContext, KeyedState, Output, OutputFlow, Runtime, RuntimeDriver, RuntimeFuture,
    Store,
};
use std::cell::Cell;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll, Waker};

/// Holds real runtime tasks for explicit polling; its clock never advances.
///
/// Elapsed sleeps complete immediately; future waits stay pending. Yields offer
/// one pending poll and request a wake before completing on their next poll.
#[derive(Clone, Default)]
struct Driver(Arc<Mutex<Vec<RuntimeFuture>>>);
impl fmt::Debug for Driver {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("AncestorOutputDriver")
    }
}
impl RuntimeDriver for Driver {
    fn spawn(&self, task: RuntimeFuture) {
        self.0.lock().push(task);
    }
    fn now(&self) -> crate::MonotonicInstant {
        crate::MonotonicInstant::ZERO
    }
    fn sleep_until(&self, deadline: crate::MonotonicInstant) -> RuntimeFuture {
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
impl Driver {
    /// Polls the current batch without holding the queue lock.
    ///
    /// Tasks spawned during this turn wait until a later turn.
    fn turn(&self) {
        let pending = std::mem::take(&mut *self.0.lock());
        for mut future in pending {
            if poll(future.as_mut()).is_pending() {
                self.0.lock().push(future);
            }
        }
    }
    /// Allows at most 64 turns; this bounds fixture progress, not elapsed time.
    fn finish<F: Future>(&self, future: F) -> F::Output {
        let mut future = Box::pin(future);
        for _ in 0..64 {
            if let Poll::Ready(value) = poll(future.as_mut()) {
                return value;
            }
            self.turn();
        }
        panic!("finite ancestor output gate did not finish");
    }
}

/// Polls directly, so successful progress alone does not prove wake delivery.
fn poll<F: Future + ?Sized>(future: Pin<&mut F>) -> Poll<F::Output> {
    future.poll(&mut Context::from_waker(Waker::noop()))
}

/// A non-`Clone`, `Send` but non-`Sync` value with visible identity and disposal.
struct Outcome {
    value: Box<Cell<usize>>,
    drops: Arc<AtomicUsize>,
}

/// Runs a one-shot completion observer without holding its callback lock.
struct ObserverWake {
    callback: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    calls: AtomicUsize,
}

impl std::task::Wake for ObserverWake {
    fn wake(self: Arc<Self>) {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let callback = self.callback.lock().take();
        if let Some(callback) = callback {
            callback();
        }
    }
}

#[test]
fn inner_acquisition_future_rejects_expired_lifecycle_without_retaining_payload() {
    let driver = Driver::default();
    let constructors = Arc::new(AtomicUsize::new(0));
    let constructed = Arc::clone(&constructors);
    let capture = Arc::new(());
    let capture_lifetime = Arc::downgrade(&capture);
    let descriptor = OptionalMount::new(
        Option::as_ref,
        Option::as_mut,
        |state: &Option<usize>| {
            state.map(|_| MountIdentity {
                key: (),
                replacement: 0,
                configuration: 0,
            })
        },
        move |_| {
            let _ = &capture;
            constructed.fetch_add(1, Ordering::SeqCst);
            crate::ReducerFeature::new(crate::Reduce::new(
                |_: &mut usize, (): &(), _: &mut ReduceContext<'_>| {
                    panic!("metadata acquisition cannot reduce the child")
                },
            ))
        },
    );
    let root = crate::ReducerFeature::new(crate::Reduce::new(
        |_: &mut Option<usize>, (): &(), _: &mut ReduceContext<'_>| {
            panic!("metadata acquisition cannot reduce the root")
        },
    ));
    let (builder, site) = Store::feature_builder(Some(7), root, Runtime::new(driver.clone()))
        .mount_with_site(descriptor);
    let store = builder.build().unwrap();
    driver.finish(store.ready()).unwrap();
    assert_eq!(constructors.load(Ordering::SeqCst), 1);
    assert!(capture_lifetime.upgrade().is_some());
    let lifecycle = Arc::downgrade(&store.lifecycle);
    let ingress = Arc::downgrade(&store.ingress);
    let closed = store.closed();

    // Split the real future boundary where the outer acquisition has upgraded
    // its header but the inner future has not yet validated weak metadata.
    let declaration = site.declaration.upgrade().unwrap();
    let mut acquisition = declaration.acquire(store.lifecycle.root_identity(), None);
    drop(store);
    driver.turn();
    assert!(driver.0.lock().is_empty());
    assert!(lifecycle.upgrade().is_none());
    assert!(ingress.upgrade().is_none());
    assert!(site.declaration.upgrade().is_none());
    assert!(capture_lifetime.upgrade().is_none());

    let Poll::Ready(Err(error)) = poll(acquisition.as_mut()) else {
        panic!("expired metadata rejects before actor admission");
    };
    assert_eq!(error.reason(), TaskAccessReason::StoreClosed);
    assert_eq!(error.progress(), TaskProgress::NotStarted);
    assert_eq!(constructors.load(Ordering::SeqCst), 1);
    assert!(driver.0.lock().is_empty());
    driver.finish(closed);
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "The same real actor-loss race covers static and recipe declarations through ordinary and diagnostic acquisition."
)]
fn command_acquisition_rejects_ingress_lost_after_validation() {
    use crate::store::mounted::mounted_admission_test_hook;

    struct Root;
    impl Feature for Root {
        type State = Option<usize>;
        type Action = ();
        fn reduce(
            &mut self,
            _: &mut Option<usize>,
            (): &(),
            _: &mut FeatureContext<'_, Option<usize>, ()>,
        ) {
        }
    }
    struct Child(Arc<AtomicUsize>);
    impl Feature for Child {
        type State = usize;
        type Action = usize;
        fn reduce(
            &mut self,
            state: &mut usize,
            action: &usize,
            _: &mut FeatureContext<'_, usize, usize>,
        ) {
            self.0.fetch_add(1, Ordering::SeqCst);
            *state += action;
        }
    }

    for (recipe, mounted) in [(false, false), (true, false), (false, true), (true, true)] {
        for diagnostic in [false, true] {
            let driver = Driver::default();
            let declared = Arc::new(Mutex::new(None));
            let export = Arc::clone(&declared);
            let maps = Arc::new(AtomicUsize::new(0));
            let mapped = Arc::clone(&maps);
            let reductions = Arc::new(AtomicUsize::new(0));
            let reduced = Arc::clone(&reductions);
            let capture = Arc::new(());
            let lifetime = Arc::downgrade(&capture);
            let descriptor = OptionalMount::new(
                Option::as_ref,
                Option::as_mut,
                |state: &Option<usize>| {
                    state.map(|_| MountIdentity {
                        key: (),
                        replacement: 0,
                        configuration: 0,
                    })
                },
                move |_| Child(Arc::clone(&reduced)),
            );
            let builder = Store::feature_builder(Some(7), Root, Runtime::new(driver.clone()));
            let (builder, mount_site, static_site) = if recipe {
                let (builder, mount_site) =
                    builder.mount_with_children(descriptor, move |_, children| {
                        let capture = Arc::clone(&capture);
                        let mapped = Arc::clone(&mapped);
                        *export.lock() = Some(children.command(move |input: usize| {
                            let _ = &capture;
                            mapped.fetch_add(1, Ordering::SeqCst);
                            input
                        }));
                    });
                (builder, mount_site, None)
            } else {
                let (builder, mount_site, site) =
                    builder.mount_with_command(descriptor, move |input: usize| {
                        let _ = &capture;
                        mapped.fetch_add(1, Ordering::SeqCst);
                        input
                    });
                (builder, mount_site, Some(site))
            };
            let store = builder.build().unwrap();
            driver.finish(store.ready()).unwrap();
            let site = static_site.or_else(|| declared.lock().take()).unwrap();
            let gate = crate::testing::TransactionGate::new();
            let mut acquisition: Pin<Box<dyn Future<Output = Result<(), TaskAccessError>> + Send>> =
                if mounted && diagnostic {
                    let pending = gate.acquire(&store, &mount_site);
                    Box::pin(async move { pending.await.map(|_| ()) })
                } else if mounted {
                    let pending = store.acquire(&mount_site);
                    Box::pin(async move { pending.await.map(|_| ()) })
                } else if diagnostic {
                    let pending = gate.command_port(&store, &site);
                    Box::pin(async move { pending.await.map(|_| ()) })
                } else {
                    let pending = store.command_port(&site);
                    Box::pin(async move { pending.await.map(|_| ()) })
                };
            let lifecycle = Arc::downgrade(&store.lifecycle);
            let ingress = Arc::downgrade(&store.ingress);
            let closed = store.closed();
            let calls = Arc::new(AtomicUsize::new(0));
            let observed = Arc::clone(&calls);
            let closing = driver.clone();
            let outcome = mounted_admission_test_hook::with_before_ingress(
                move || {
                    observed.fetch_add(1, Ordering::SeqCst);
                    // Actual owner and actor destruction happens on another thread,
                    // after validation has released its temporary lifecycle reference.
                    std::thread::spawn(move || {
                        drop(store);
                        closing.turn();
                        assert!(closing.0.lock().is_empty());
                    })
                    .join()
                    .unwrap();
                },
                || poll(acquisition.as_mut()),
            );
            let Poll::Ready(Err(error)) = outcome else {
                panic!("destroyed ingress must reject acquisition before queue admission");
            };
            assert_eq!(error.reason(), crate::TaskAccessReason::StoreClosed);
            assert_eq!(error.progress(), crate::TaskProgress::NotStarted);
            assert_eq!(calls.load(Ordering::SeqCst), 1);
            assert_eq!(maps.load(Ordering::SeqCst), 0);
            assert_eq!(reductions.load(Ordering::SeqCst), 0);
            assert!(lifecycle.upgrade().is_none());
            assert!(ingress.upgrade().is_none());
            assert!(lifetime.upgrade().is_none());
            assert!(driver.0.lock().is_empty());
            driver.finish(closed);
            assert!(gate.boundary().is_none());
            assert!(!gate.has_pending());
        }
    }
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "One ownership schedule compares lost ingress with retained closed transport and exact payload recovery."
)]
fn recipe_command_recovers_original_when_last_owner_drops_after_admission() {
    use crate::store::mounted::mounted_admission_test_hook;

    struct Root;
    impl Feature for Root {
        type State = Option<usize>;
        type Action = ();
        fn reduce(
            &mut self,
            _: &mut Self::State,
            (): &(),
            _: &mut FeatureContext<'_, Self::State, ()>,
        ) {
        }
    }
    struct Child;
    impl Feature for Child {
        type State = usize;
        type Action = usize;
        fn reduce(
            &mut self,
            state: &mut usize,
            action: &usize,
            _: &mut FeatureContext<'_, usize, usize>,
        ) {
            *state += action;
        }
    }

    for (immediate, retain_ingress) in [(false, false), (true, false), (true, true)] {
        let driver = Driver::default();
        let exported = Arc::new(Mutex::new(None));
        let export = Arc::clone(&exported);
        let mappings = Arc::new(AtomicUsize::new(0));
        let mapped = Arc::clone(&mappings);
        let descriptor = OptionalMount::new(
            Option::as_ref,
            Option::as_mut,
            |state: &Option<usize>| {
                state.map(|_| MountIdentity {
                    key: (),
                    replacement: 0,
                    configuration: 0,
                })
            },
            |_| Child,
        );
        let (builder, _) = Store::feature_builder(Some(0), Root, Runtime::new(driver.clone()))
            .mailbox_capacity(1)
            .mount_with_children(descriptor, move |_, children| {
                let mapped = Arc::clone(&mapped);
                *export.lock() = Some(children.command(move |input: Outcome| {
                    mapped.fetch_add(1, Ordering::SeqCst);
                    input.value.get()
                }));
            });
        let store = builder.build().unwrap();
        driver.finish(store.ready()).unwrap();
        let site = exported.lock().take().unwrap();
        let port = driver.finish(store.command_port(&site)).unwrap().unwrap();
        let drops = Arc::new(AtomicUsize::new(0));
        // A real suspended admission can retain ingress after its final Store
        // owner disappears. That reaches transport closure, not failed upgrade.
        let queued = retain_ingress.then(|| store.try_send(()).unwrap());
        let mut held_send = None;
        let mut held_identity = None;
        if retain_ingress {
            let value = Box::new(Cell::new(41));
            held_identity = Some((&raw const *value).addr());
            let mut send = Box::pin(port.send(Outcome {
                value,
                drops: Arc::clone(&drops),
            }));
            assert!(poll(send.as_mut()).is_pending());
            held_send = Some(send);
        }
        let payload = Outcome {
            value: Box::new(Cell::new(79)),
            drops: Arc::clone(&drops),
        };
        let identity = (&raw const *payload.value).addr();
        let hook_calls = Arc::new(AtomicUsize::new(0));
        let called = Arc::clone(&hook_calls);
        let close = move || {
            called.fetch_add(1, Ordering::SeqCst);
            std::thread::scope(|scope| scope.spawn(move || drop(store)).join().unwrap());
        };
        let (returned, error) = if immediate {
            let result =
                mounted_admission_test_hook::with_before_ingress(close, || port.try_send(payload));
            let Err(crate::TryCommandError::Rejected { command, error }) = result else {
                panic!("lost ingress must return the original command before admission");
            };
            (command, error)
        } else {
            let mut send = Box::pin(port.send(payload));
            let result =
                mounted_admission_test_hook::with_before_ingress(close, || poll(send.as_mut()));
            let Poll::Ready(Err(crate::TaskSendError::Rejected { action, error })) = result else {
                panic!("lost ingress must return the original command before enqueueing");
            };
            (action, error)
        };
        assert_eq!(hook_calls.load(Ordering::SeqCst), 1);
        assert_eq!(error.reason(), crate::TaskAccessReason::StoreClosed);
        assert_eq!(error.progress(), crate::TaskProgress::NotStarted);
        assert_eq!((&raw const *returned.value).addr(), identity);
        assert_eq!(returned.value.get(), 79);
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        assert_eq!(mappings.load(Ordering::SeqCst), 0);
        drop(returned);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        if let Some(mut send) = held_send {
            let Poll::Ready(Err(crate::TaskSendError::Rejected { action, error })) =
                poll(send.as_mut())
            else {
                panic!("the suspended admission must also recover its original command");
            };
            assert_eq!(error.reason(), crate::TaskAccessReason::StoreClosed);
            assert_eq!(error.progress(), crate::TaskProgress::NotStarted);
            assert_eq!(Some((&raw const *action.value).addr()), held_identity);
            assert_eq!(action.value.get(), 41);
            drop(action);
            assert_eq!(drops.load(Ordering::SeqCst), 2);
        }
        drop(queued);
        drop((port, site));
        driver.turn();
        assert!(driver.0.lock().is_empty());
        assert_eq!(Arc::strong_count(&mappings), 1);
        assert_eq!(Arc::strong_count(&drops), 1);
    }
}

impl Drop for Outcome {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one final-owner race verifies both fold entry points and exact callback teardown"
)]
fn fold_and_acquire_reject_after_final_store_close_before_ingress() {
    use crate::store::mounted::mounted_admission_test_hook;

    struct Root;
    impl Feature for Root {
        type State = Option<usize>;
        type Action = ();
        fn reduce(
            &mut self,
            _: &mut Self::State,
            (): &(),
            _: &mut FeatureContext<'_, Self::State, ()>,
        ) {
        }
    }
    struct Child;
    impl Feature for Child {
        type State = usize;
        type Action = ();
        fn reduce(
            &mut self,
            _: &mut Self::State,
            (): &(),
            _: &mut FeatureContext<'_, Self::State, ()>,
        ) {
        }
    }
    struct Sum;
    impl crate::FoldKey for Sum {
        type Value = usize;
        fn identity() -> usize {
            0
        }
        fn reduce(accumulator: usize, next: usize) -> std::ops::ControlFlow<usize, usize> {
            std::ops::ControlFlow::Continue(accumulator + next)
        }
    }
    struct Capture(Arc<AtomicUsize>);
    impl Drop for Capture {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    for acquire in [false, true] {
        let driver = Driver::default();
        let calls = Arc::new(AtomicUsize::new(0));
        let capture_drops = Arc::new(AtomicUsize::new(0));
        let fold = crate::Fold::<Option<usize>, Sum>::new();
        let declared = fold.clone();
        let contributed = Arc::clone(&calls);
        let disposals = Arc::clone(&capture_drops);
        let descriptor = OptionalMount::new(
            Option::as_ref,
            Option::as_mut,
            |state: &Option<usize>| {
                state.map(|_| MountIdentity {
                    key: (),
                    replacement: 0,
                    configuration: 0,
                })
            },
            |_| Child,
        );
        let (builder, site) = Store::feature_builder(Some(7), Root, Runtime::new(driver.clone()))
            .mount_with_children(descriptor, move |_, children| {
                let capture = Capture(Arc::clone(&disposals));
                let contributed = Arc::clone(&contributed);
                children.contribute(&declared, move |value| {
                    let _ = &capture;
                    contributed.fetch_add(1, Ordering::SeqCst);
                    *value
                });
            });
        let store = builder.build().unwrap();
        driver.finish(store.ready()).unwrap();
        let mut request: Pin<Box<dyn Future<Output = Result<(), crate::TaskAccessError>> + Send>> =
            if acquire {
                let pending = store.acquire_fold(&fold, &site);
                Box::pin(async move { pending.await.map(|_| ()) })
            } else {
                let pending = store.fold(&fold);
                Box::pin(async move { pending.await.map(|_| ()) })
            };
        let hook_calls = Arc::new(AtomicUsize::new(0));
        let hooked = Arc::clone(&hook_calls);
        let result = mounted_admission_test_hook::with_before_ingress(
            move || {
                hooked.fetch_add(1, Ordering::SeqCst);
                std::thread::scope(|scope| scope.spawn(move || drop(store)).join().unwrap());
            },
            || poll(request.as_mut()),
        );
        let Poll::Ready(Err(error)) = result else {
            panic!("lost fold ingress must reject before entering a contribution");
        };
        assert_eq!(error.reason(), crate::TaskAccessReason::StoreClosed);
        assert_eq!(error.progress(), crate::TaskProgress::NotStarted);
        assert_eq!(hook_calls.load(Ordering::SeqCst), 1);
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        drop((request, site, fold));
        driver.turn();
        assert_eq!(capture_drops.load(Ordering::SeqCst), 1);
        assert!(driver.0.lock().is_empty());
        assert_eq!(Arc::strong_count(&calls), 1);
        assert_eq!(Arc::strong_count(&capture_drops), 1);
    }
}
/// Separates ancestor handler, effect and task mutations.
struct Row {
    child: Option<()>,
    handled: usize,
    effects: usize,
    tasks: usize,
}
/// Retains the original output value after its final root handler consumes it.
struct State {
    rows: KeyedState<String, Row>,
    received: Option<Outcome>,
}
/// Replaces the keyed row without sharing the descendant action type.
enum RootAction {
    Replace,
}
/// Marks the ancestor effect, independently of the output handler and task.
enum RowAction {
    Effect,
}
/// Keeps the output source's action distinct from both ancestor action types.
struct SourceAction;

/// Starts a named task or carries an owned value back to its root.
enum RootRequestAction {
    Start,
    Value(Outcome),
}

/// Exports a real named task context and lets the test finish its body.
struct RootRequestFeature {
    export: Arc<Mutex<Option<TaskContext<usize, RootRequestAction>>>>,
    completion: Arc<std::sync::atomic::AtomicBool>,
    reductions: Arc<AtomicUsize>,
}
impl Feature for RootRequestFeature {
    type State = usize;
    type Action = RootRequestAction;

    fn reduce(
        &mut self,
        state: &mut usize,
        action: &RootRequestAction,
        context: &mut FeatureContext<'_, usize, RootRequestAction>,
    ) {
        match action {
            RootRequestAction::Start => {
                let export = Arc::clone(&self.export);
                let completion = Arc::clone(&self.completion);
                context.task(Task::run_named(
                    "root-request",
                    crate::OperationPolicy::Coexist,
                    move |task_context| async move {
                        *export.lock() = Some(task_context);
                        std::future::poll_fn(move |_| {
                            if completion.load(Ordering::SeqCst) {
                                Poll::Ready(())
                            } else {
                                Poll::Pending
                            }
                        })
                        .await;
                    },
                ));
            }
            RootRequestAction::Value(value) => {
                self.reductions.fetch_add(1, Ordering::SeqCst);
                *state += value.value.get();
            }
        }
    }
}

/// Holds the actor and named task body separately from their completion handles.
///
/// A test can finish the body before the actor publishes task completion.
struct RootRequestHost {
    driver: Driver,
    store: Store<usize, RootRequestAction>,
    actor: RuntimeFuture,
    run: RuntimeFuture,
    context: TaskContext<usize, RootRequestAction>,
    task: crate::ActionTask,
    wait: Pin<Box<crate::OperationWait>>,
    completion: Arc<std::sync::atomic::AtomicBool>,
    reductions: Arc<AtomicUsize>,
    export: Arc<Mutex<Option<TaskContext<usize, RootRequestAction>>>>,
}
impl RootRequestHost {
    /// Captures a registered generation before exporting its live task context.
    fn new() -> Self {
        let driver = Driver::default();
        let export = Arc::new(Mutex::new(None));
        let completion = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let reductions = Arc::new(AtomicUsize::new(0));
        let store = Store::feature_builder(
            0,
            RootRequestFeature {
                export: Arc::clone(&export),
                completion: Arc::clone(&completion),
                reductions: Arc::clone(&reductions),
            },
            Runtime::new(driver.clone()),
        )
        .build()
        .unwrap();
        driver.finish(store.ready()).unwrap();
        let mut actor = driver.0.lock().pop().unwrap();
        let mut start = Box::pin(store.send(RootRequestAction::Start));
        assert!(poll(start.as_mut()).is_pending());
        assert!(poll(actor.as_mut()).is_pending());
        let Poll::Ready(Ok(task)) = poll(start.as_mut()) else {
            panic!("root named task must be registered before its context is exported");
        };
        drop(start);
        let mut capture = Box::pin(store.operation("root-request").capture());
        assert!(poll(capture.as_mut()).is_pending());
        assert!(poll(actor.as_mut()).is_pending());
        let Poll::Ready(Ok(wait)) = poll(capture.as_mut()) else {
            panic!("root named generation must be captured while live");
        };
        drop(capture);
        let mut run = driver.0.lock().pop().unwrap();
        assert!(poll(run.as_mut()).is_pending());
        let context = export
            .lock()
            .take()
            .expect("real root task exports its context");
        Self {
            driver,
            store,
            actor,
            run,
            context,
            task,
            wait: Box::pin(wait),
            completion,
            reductions,
            export,
        }
    }
}

#[test]
fn root_task_cancelled_before_request_retention_suppresses_read() {
    let mut host = RootRequestHost::new();
    let calls = Arc::new(AtomicUsize::new(0));
    let queried = Arc::clone(&calls);
    host.task.cancel();
    let mut read = Box::pin(host.context.read(move |_| {
        queried.fetch_add(1, Ordering::SeqCst);
    }));
    let Poll::Ready(Err(error)) = poll(read.as_mut()) else {
        panic!("cancelled root producer must reject its unentered read");
    };
    assert_eq!(error.reason(), crate::TaskAccessReason::Cancelled);
    assert_eq!(error.progress(), crate::TaskProgress::NotStarted);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert!(poll(host.wait.as_mut()).is_pending());
    host.completion.store(true, Ordering::SeqCst);
    assert!(poll(host.run.as_mut()).is_ready());
    assert!(matches!(poll(host.wait.as_mut()), Poll::Ready(Ok(()))));
    assert!(poll(host.actor.as_mut()).is_pending());
    assert!(host.task.is_finished());
    drop(host.store);
    assert!(poll(host.actor.as_mut()).is_ready());
    drop((
        read,
        host.actor,
        host.run,
        host.wait,
        host.context,
        host.task,
    ));
    assert!(host.driver.0.lock().is_empty());
    assert_eq!(host.reductions.load(Ordering::SeqCst), 0);
    assert_eq!(Arc::strong_count(&host.export), 1);
    assert_eq!(Arc::strong_count(&host.completion), 1);
    assert_eq!(Arc::strong_count(&host.reductions), 1);
}

#[test]
fn root_task_cancelled_after_request_retention_returns_original_action() {
    use crate::store::mounted::mounted_admission_test_hook;

    let mut host = RootRequestHost::new();
    let drops = Arc::new(AtomicUsize::new(0));
    let payload = Box::new(Cell::new(53));
    let identity = (&raw const *payload).addr();
    let mut send = Box::pin(host.context.send(RootRequestAction::Value(Outcome {
        value: payload,
        drops: Arc::clone(&drops),
    })));
    let cancellation = host.task.clone();
    let hook_calls = Arc::new(AtomicUsize::new(0));
    let called = Arc::clone(&hook_calls);
    let outcome = mounted_admission_test_hook::with_after_retain(
        move || {
            called.fetch_add(1, Ordering::SeqCst);
            std::thread::scope(|scope| scope.spawn(move || cancellation.cancel()).join().unwrap());
        },
        || poll(send.as_mut()),
    );
    let Poll::Ready(Err(crate::TaskSendError::Rejected { action, error })) = outcome else {
        panic!("post-retention cancellation must return the unentered action");
    };
    assert_eq!(error.reason(), crate::TaskAccessReason::Cancelled);
    assert_eq!(error.progress(), crate::TaskProgress::NotStarted);
    let RootRequestAction::Value(returned) = action else {
        panic!("rejection must preserve the action variant");
    };
    assert_eq!((&raw const *returned.value).addr(), identity);
    assert_eq!(returned.value.get(), 53);
    assert_eq!(hook_calls.load(Ordering::SeqCst), 1);
    assert_eq!(host.reductions.load(Ordering::SeqCst), 0);
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    assert!(poll(host.wait.as_mut()).is_pending());
    drop(returned);
    drop(send);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    host.completion.store(true, Ordering::SeqCst);
    assert!(poll(host.run.as_mut()).is_ready());
    assert!(matches!(poll(host.wait.as_mut()), Poll::Ready(Ok(()))));
    assert!(poll(host.actor.as_mut()).is_pending());
    assert!(host.task.is_finished());
    drop(host.store);
    assert!(poll(host.actor.as_mut()).is_ready());
    drop((host.actor, host.run, host.wait, host.context, host.task));
    assert!(host.driver.0.lock().is_empty());
    assert_eq!(host.reductions.load(Ordering::SeqCst), 0);
    assert_eq!(Arc::strong_count(&drops), 1);
    assert_eq!(Arc::strong_count(&host.export), 1);
    assert_eq!(Arc::strong_count(&host.completion), 1);
    assert_eq!(Arc::strong_count(&host.reductions), 1);
}

#[test]
fn root_task_final_owner_loss_before_ingress_suppresses_update() {
    use crate::store::mounted::mounted_admission_test_hook;

    let mut host = RootRequestHost::new();
    let calls = Arc::new(AtomicUsize::new(0));
    let updated = Arc::clone(&calls);
    let mut update = Box::pin(host.context.update(move |state| {
        updated.fetch_add(1, Ordering::SeqCst);
        *state += 17;
    }));
    let hook_calls = Arc::new(AtomicUsize::new(0));
    let called = Arc::clone(&hook_calls);
    let outcome = mounted_admission_test_hook::with_before_ingress(
        move || {
            called.fetch_add(1, Ordering::SeqCst);
            std::thread::scope(|scope| scope.spawn(move || drop(host.store)).join().unwrap());
        },
        || poll(update.as_mut()),
    );
    let Poll::Ready(Err(error)) = outcome else {
        panic!("last-owner loss must reject the unentered root update");
    };
    assert_eq!(error.reason(), crate::TaskAccessReason::StoreClosed);
    assert_eq!(error.progress(), crate::TaskProgress::NotStarted);
    assert_eq!(hook_calls.load(Ordering::SeqCst), 1);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(host.reductions.load(Ordering::SeqCst), 0);
    assert!(poll(host.wait.as_mut()).is_pending());
    host.completion.store(true, Ordering::SeqCst);
    assert!(poll(host.run.as_mut()).is_ready());
    assert!(matches!(poll(host.wait.as_mut()), Poll::Ready(Ok(()))));
    assert!(poll(host.actor.as_mut()).is_ready());
    drop((
        update,
        host.actor,
        host.run,
        host.wait,
        host.context,
        host.task,
    ));
    assert!(host.driver.0.lock().is_empty());
    assert_eq!(Arc::strong_count(&host.export), 1);
    assert_eq!(Arc::strong_count(&host.completion), 1);
    assert_eq!(Arc::strong_count(&host.reductions), 1);
}

#[test]
fn completed_root_named_generation_rejects_update_before_task_publication() {
    let mut host = RootRequestHost::new();
    let callbacks = Arc::new(AtomicUsize::new(0));
    let observed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let context = host.context.clone();
    let task = host.task.clone();
    let calls = Arc::clone(&callbacks);
    let seen = Arc::clone(&observed);
    let wake = Arc::new(ObserverWake {
        callback: Mutex::new(Some(Box::new(move || {
            assert!(
                !task.is_finished(),
                "the actor has not harvested task completion"
            );
            assert!(!task.is_cancelled());
            let mut update = Box::pin(context.update(move |state| {
                calls.fetch_add(1, Ordering::SeqCst);
                *state += 23;
            }));
            let Poll::Ready(Err(error)) = poll(update.as_mut()) else {
                panic!("completed named ancestry must reject update before entry");
            };
            assert_eq!(error.reason(), crate::TaskAccessReason::Cancelled);
            assert_eq!(error.progress(), crate::TaskProgress::NotStarted);
            seen.store(true, Ordering::SeqCst);
        }))),
        calls: AtomicUsize::new(0),
    });
    let waker = Waker::from(Arc::clone(&wake));
    assert!(
        host.wait
            .as_mut()
            .poll(&mut Context::from_waker(&waker))
            .is_pending()
    );
    assert!(!observed.load(Ordering::SeqCst));
    host.completion.store(true, Ordering::SeqCst);
    assert!(poll(host.run.as_mut()).is_ready());
    assert_eq!(wake.calls.load(Ordering::SeqCst), 1);
    assert!(wake.callback.lock().is_none());
    assert!(observed.load(Ordering::SeqCst));
    assert_eq!(callbacks.load(Ordering::SeqCst), 0);
    assert_eq!(host.reductions.load(Ordering::SeqCst), 0);
    assert!(matches!(poll(host.wait.as_mut()), Poll::Ready(Ok(()))));
    assert!(!host.task.is_finished());
    assert!(poll(host.actor.as_mut()).is_pending());
    assert!(host.task.is_finished());
    drop(host.store);
    assert!(poll(host.actor.as_mut()).is_ready());
    drop((host.actor, host.run, host.wait, host.context, host.task));
    assert!(host.driver.0.lock().is_empty());
    assert_eq!(Arc::strong_count(&callbacks), 1);
    assert_eq!(Arc::strong_count(&host.export), 1);
    assert_eq!(Arc::strong_count(&host.completion), 1);
    assert_eq!(Arc::strong_count(&host.reductions), 1);
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "Actual last-owner races cover retained and destroyed actor ingress without entering callbacks"
)]
fn removal_last_store_close_before_ingress_rejects_unentered_callbacks() {
    use crate::store::mounted::mounted_admission_test_hook;

    struct Parent;
    impl crate::Feature for Parent {
        type State = Option<Arc<AtomicUsize>>;
        type Action = ();
        fn reduce(
            &mut self,
            _: &mut Self::State,
            (): &(),
            _: &mut FeatureContext<'_, Self::State, ()>,
        ) {
        }
    }
    struct Child;
    impl crate::Feature for Child {
        type State = Arc<AtomicUsize>;
        type Action = ();
        fn reduce(
            &mut self,
            _: &mut Self::State,
            (): &(),
            _: &mut FeatureContext<'_, Self::State, ()>,
        ) {
        }
    }
    struct Capture(Arc<AtomicUsize>);
    impl Drop for Capture {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    for drain_actor in [false, true] {
        let driver = Driver::default();
        let value = Arc::new(AtomicUsize::new(7));
        let exported = Arc::new(Mutex::new(None));
        let export = Arc::clone(&exported);
        let (builder, site, removal) = crate::RootMount::new(Parent)
            .on_initial(
                |_| (),
                move |_, _| {
                    let export = Arc::clone(&export);
                    crate::Task::run(move |context| async move {
                        *export.lock() = Some(context);
                        std::future::pending::<()>().await;
                    })
                },
            )
            .builder(Some(Arc::clone(&value)), Runtime::new(driver.clone()))
            .mount_removable(
                |state| state,
                |state| state,
                |state: &Option<Arc<AtomicUsize>>| {
                    state.as_ref().map(|_| crate::MountIdentity {
                        key: (),
                        replacement: 0,
                        configuration: 0,
                    })
                },
                |_| Child,
            );
        let store = builder.build().unwrap();
        driver.finish(store.ready()).unwrap();
        driver.turn();
        let parent = exported.lock().take().expect("live root producer");
        let child = driver.finish(store.acquire(&site)).unwrap().unwrap();

        let callback_calls = Arc::new(AtomicUsize::new(0));
        let capture_drops = Arc::new(AtomicUsize::new(0));
        let before_capture = Capture(Arc::clone(&capture_drops));
        let after_capture = Capture(Arc::clone(&capture_drops));
        let before_calls = Arc::clone(&callback_calls);
        let after_calls = Arc::clone(&callback_calls);
        let mut remove = Box::pin(parent.remove_child(
            &removal,
            &child,
            move |_| {
                before_calls.fetch_add(1, Ordering::SeqCst);
                drop(before_capture);
            },
            move |_, _, _| {
                after_calls.fetch_add(1, Ordering::SeqCst);
                drop(after_capture);
            },
        ));
        let hook_calls = Arc::new(AtomicUsize::new(0));
        let called = Arc::clone(&hook_calls);
        let closing = driver.clone();
        let result = mounted_admission_test_hook::with_before_ingress(
            move || {
                called.fetch_add(1, Ordering::SeqCst);
                std::thread::scope(|scope| {
                    scope
                        .spawn(move || {
                            drop(store);
                            if drain_actor {
                                for _ in 0..4 {
                                    closing.turn();
                                }
                                assert!(closing.0.lock().is_empty());
                            }
                        })
                        .join()
                        .unwrap();
                });
            },
            || poll(remove.as_mut()),
        );
        let Poll::Ready(Err(error)) = result else {
            panic!("closed ingress must reject removal before either callback");
        };
        assert_eq!(error.reason(), crate::TaskAccessReason::StoreClosed);
        assert_eq!(error.progress(), crate::TaskProgress::NotStarted);
        assert_eq!(hook_calls.load(Ordering::SeqCst), 1);
        assert_eq!(callback_calls.load(Ordering::SeqCst), 0);
        assert_eq!(capture_drops.load(Ordering::SeqCst), 2);
        assert_eq!(value.load(Ordering::SeqCst), 7);
        drop((remove, parent, child, site, removal));
        for _ in 0..4 {
            driver.turn();
        }
        assert!(driver.0.lock().is_empty());
        assert_eq!(Arc::strong_count(&value), 1);
        assert_eq!(Arc::strong_count(&capture_drops), 1);
    }
}

#[test]
fn named_task_completion_observer_rejects_shared_endpoint_requests() {
    shared_endpoint_admission_control(SharedEndpointAdmission::CompletedOperation);
}

#[test]
fn shared_endpoint_cancellation_after_retention_returns_original_action() {
    shared_endpoint_admission_control(SharedEndpointAdmission::CancelAfterRetain);
}

#[test]
fn shared_endpoint_last_store_close_before_ingress_returns_original_action() {
    shared_endpoint_admission_control(SharedEndpointAdmission::CloseBeforeIngress);
}

#[test]
fn shared_output_host_teardown_before_lifecycle_returns_original_value() {
    shared_endpoint_admission_control(SharedEndpointAdmission::OutputCloseBeforeLifecycle);
}

#[test]
fn shared_output_cancellation_after_retention_returns_original_value() {
    shared_endpoint_admission_control(SharedEndpointAdmission::OutputCancelAfterRetain);
}

#[test]
fn shared_output_last_store_close_before_ingress_returns_original_value() {
    shared_endpoint_admission_control(SharedEndpointAdmission::OutputCloseBeforeIngress);
}

/// Selects the real request boundary interrupted by completion or owner loss.
#[derive(Clone, Copy)]
enum SharedEndpointAdmission {
    CompletedOperation,
    CancelAfterRetain,
    CloseBeforeIngress,
    OutputCloseBeforeLifecycle,
    OutputCancelAfterRetain,
    OutputCloseBeforeIngress,
}

#[test]
fn operation_last_store_close_before_ingress_rejects_unentered_metadata() {
    use crate::store::mounted::mounted_admission_test_hook;

    let driver = Driver::default();
    let reductions = Arc::new(AtomicUsize::new(0));
    let calls = Arc::clone(&reductions);
    let store = Store::new(
        (),
        crate::Reduce::<(), (), _>::new(move |(), (), _| {
            calls.fetch_add(1, Ordering::SeqCst);
            Effect::none()
        }),
        Runtime::new(driver.clone()),
    );
    driver.finish(store.ready()).unwrap();
    let operation = store.operation("work");
    let mut actor = driver.0.lock().pop().unwrap();
    let mut status = Box::pin(operation.status());
    let hook_calls = Arc::new(AtomicUsize::new(0));
    let callback_calls = Arc::clone(&hook_calls);
    let outcome = mounted_admission_test_hook::with_before_ingress(
        move || {
            callback_calls.fetch_add(1, Ordering::SeqCst);
            // The final public owner disappears on another thread after admission.
            std::thread::scope(|scope| scope.spawn(move || drop(store)).join().unwrap());
        },
        || poll(status.as_mut()),
    );
    let Poll::Ready(Err(error)) = outcome else {
        panic!("unentered operation metadata must be rejected without an actor turn");
    };
    assert_eq!(error.reason(), crate::TaskAccessReason::StoreClosed);
    assert_eq!(error.progress(), crate::TaskProgress::NotStarted);
    assert_eq!(hook_calls.load(Ordering::SeqCst), 1);
    assert_eq!(reductions.load(Ordering::SeqCst), 0);
    drop((status, operation));
    assert!(poll(actor.as_mut()).is_ready());
    drop(actor);
    assert!(driver.0.lock().is_empty());
    assert_eq!(Arc::strong_count(&reductions), 1);
}

#[test]
fn command_port_last_store_close_before_ingress_returns_unmapped_command() {
    use crate::store::mounted::mounted_admission_test_hook;

    struct Parent;
    impl Feature for Parent {
        type State = Option<Arc<AtomicUsize>>;
        type Action = ();

        fn reduce(
            &mut self,
            _: &mut Self::State,
            (): &(),
            _: &mut FeatureContext<'_, Self::State, ()>,
        ) {
        }
    }
    struct Child;
    impl Feature for Child {
        type State = Arc<AtomicUsize>;
        type Action = Outcome;

        fn reduce(
            &mut self,
            state: &mut Self::State,
            action: &Outcome,
            _: &mut FeatureContext<'_, Self::State, Outcome>,
        ) {
            state.fetch_add(action.value.get(), Ordering::SeqCst);
        }
    }

    let driver = Driver::default();
    let value = Arc::new(AtomicUsize::new(0));
    let maps = Arc::new(AtomicUsize::new(0));
    let drops = Arc::new(AtomicUsize::new(0));
    let descriptor = OptionalMount::new(
        Option::as_ref,
        Option::as_mut,
        |state: &Option<Arc<AtomicUsize>>| {
            state.as_ref().map(|_| MountIdentity {
                key: (),
                replacement: 0,
                configuration: 0,
            })
        },
        |_| Child,
    );
    let (builder, mount, site) = Store::feature_builder(
        Some(Arc::clone(&value)),
        Parent,
        Runtime::new(driver.clone()),
    )
    .mount_with_command(descriptor, {
        let maps = Arc::clone(&maps);
        move |command: Outcome| {
            maps.fetch_add(1, Ordering::SeqCst);
            command
        }
    });
    let store = builder.build().unwrap();
    driver.finish(store.ready()).unwrap();
    let port = driver.finish(store.command_port(&site)).unwrap().unwrap();
    drop(mount);
    drop(site);
    let mut actor = driver.0.lock().pop().unwrap();
    let payload = Box::new(Cell::new(29));
    let identity = (&raw const *payload).addr();
    let mut send = Box::pin(port.send(Outcome {
        value: payload,
        drops: Arc::clone(&drops),
    }));
    let outcome = mounted_admission_test_hook::with_before_ingress(
        move || {
            // Drop the actual final owner after admission, before the weak ingress upgrade.
            std::thread::scope(|scope| scope.spawn(move || drop(store)).join().unwrap());
        },
        || poll(send.as_mut()),
    );
    let Poll::Ready(Err(crate::TaskSendError::Rejected { action, error })) = outcome else {
        panic!("closing admission must return the original unmapped command");
    };
    assert_eq!(error.reason(), crate::TaskAccessReason::StoreClosed);
    assert_eq!(error.progress(), crate::TaskProgress::NotStarted);
    assert_eq!((&raw const *action.value).addr(), identity);
    assert_eq!(action.value.get(), 29);
    assert_eq!(maps.load(Ordering::SeqCst), 0);
    assert_eq!(value.load(Ordering::SeqCst), 0);
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    drop(action);
    drop(send);
    drop(port);
    assert!(poll(actor.as_mut()).is_ready());
    drop(actor);
    assert!(driver.0.lock().is_empty());
    assert_eq!(maps.load(Ordering::SeqCst), 0);
    assert_eq!(value.load(Ordering::SeqCst), 0);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    assert_eq!(Arc::strong_count(&maps), 1);
    assert_eq!(Arc::strong_count(&value), 1);
    assert_eq!(Arc::strong_count(&drops), 1);
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one real mounted port compares a lost weak ingress with a retained but closed transport"
)]
fn mounted_try_command_returns_original_after_admission_owner_or_actor_loss() {
    use crate::store::mounted::mounted_admission_test_hook;

    struct Parent;
    impl Feature for Parent {
        type State = Option<Arc<AtomicUsize>>;
        type Action = ();
        fn reduce(
            &mut self,
            _: &mut Self::State,
            (): &(),
            _: &mut FeatureContext<'_, Self::State, ()>,
        ) {
        }
    }
    struct Child;
    impl Feature for Child {
        type State = Arc<AtomicUsize>;
        type Action = Outcome;
        fn reduce(
            &mut self,
            state: &mut Self::State,
            action: &Outcome,
            _: &mut FeatureContext<'_, Self::State, Outcome>,
        ) {
            state.fetch_add(action.value.get(), Ordering::SeqCst);
        }
    }

    for retain_ingress in [false, true] {
        let driver = Driver::default();
        let value = Arc::new(AtomicUsize::new(0));
        let maps = Arc::new(AtomicUsize::new(0));
        let drops = Arc::new(AtomicUsize::new(0));
        let descriptor = OptionalMount::new(
            Option::as_ref,
            Option::as_mut,
            |state: &Option<Arc<AtomicUsize>>| {
                state.as_ref().map(|_| MountIdentity {
                    key: (),
                    replacement: 0,
                    configuration: 0,
                })
            },
            |_| Child,
        );
        let (builder, mount, site) = Store::feature_builder(
            Some(Arc::clone(&value)),
            Parent,
            Runtime::new(driver.clone()),
        )
        .mount_with_command(descriptor, {
            let maps = Arc::clone(&maps);
            move |command: Outcome| {
                maps.fetch_add(1, Ordering::SeqCst);
                command
            }
        });
        let store = builder.build().unwrap();
        driver.finish(store.ready()).unwrap();
        let port = driver.finish(store.command_port(&site)).unwrap().unwrap();
        drop((mount, site));
        let actor = driver.0.lock().pop().unwrap();
        let payload = Outcome {
            value: Box::new(Cell::new(31)),
            drops: Arc::clone(&drops),
        };
        let identity = (&raw const *payload.value).addr();
        let (result, store, actor) = if retain_ingress {
            let result = mounted_admission_test_hook::with_after_ingress(
                move || {
                    // The upgraded sender survives actual actor abandonment.
                    std::thread::scope(|scope| scope.spawn(move || drop(actor)).join().unwrap());
                },
                || port.try_send(payload),
            );
            (result, Some(store), None)
        } else {
            let result = mounted_admission_test_hook::with_before_ingress(
                move || {
                    std::thread::scope(|scope| scope.spawn(move || drop(store)).join().unwrap());
                },
                || port.try_send(payload),
            );
            (result, None, Some(actor))
        };
        let Err(crate::TryCommandError::Rejected { command, error }) = result else {
            panic!("closed mounted admission must return the original command");
        };
        assert_eq!(error.reason(), crate::TaskAccessReason::StoreClosed);
        assert_eq!(error.progress(), crate::TaskProgress::NotStarted);
        assert_eq!((&raw const *command.value).addr(), identity);
        assert_eq!(command.value.get(), 31);
        assert_eq!(maps.load(Ordering::SeqCst), 0);
        assert_eq!(value.load(Ordering::SeqCst), 0);
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        drop(command);
        drop(port);
        if let Some(store) = store {
            drop(store);
        }
        if let Some(mut actor) = actor {
            assert!(poll(actor.as_mut()).is_ready());
            drop(actor);
        }
        assert!(driver.0.lock().is_empty());
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert_eq!(Arc::strong_count(&maps), 1);
        assert_eq!(Arc::strong_count(&value), 1);
        assert_eq!(Arc::strong_count(&drops), 1);
    }
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "the real gated command and actor abandonment must remain one inspection schedule"
)]
fn mounted_command_inspection_rejects_after_actor_abandons_mapper() {
    use crate::store::mounted::mounted_admission_test_hook;

    struct Parent;
    impl Feature for Parent {
        type State = Option<Arc<AtomicUsize>>;
        type Action = ();
        fn reduce(
            &mut self,
            _: &mut Self::State,
            (): &(),
            _: &mut FeatureContext<'_, Self::State, ()>,
        ) {
        }
    }
    struct Child;
    impl Feature for Child {
        type State = Arc<AtomicUsize>;
        type Action = Outcome;
        fn reduce(
            &mut self,
            state: &mut Self::State,
            action: &Outcome,
            _: &mut FeatureContext<'_, Self::State, Outcome>,
        ) {
            state.fetch_add(action.value.get(), Ordering::SeqCst);
        }
    }

    let driver = Driver::default();
    let gate = crate::testing::TransactionGate::<Option<Arc<AtomicUsize>>, ()>::new();
    let value = Arc::new(AtomicUsize::new(0));
    let maps = Arc::new(AtomicUsize::new(0));
    let drops = Arc::new(AtomicUsize::new(0));
    let descriptor = OptionalMount::new(
        Option::as_ref,
        Option::as_mut,
        |state: &Option<Arc<AtomicUsize>>| {
            state.as_ref().map(|_| MountIdentity {
                key: (),
                replacement: 0,
                configuration: 0,
            })
        },
        |_| Child,
    );
    let (builder, mount, site) = Store::feature_builder(
        Some(Arc::clone(&value)),
        Parent,
        Runtime::new(driver.clone()),
    )
    .mount_with_command(descriptor, {
        let maps = Arc::clone(&maps);
        move |command: Outcome| {
            maps.fetch_add(1, Ordering::SeqCst);
            command
        }
    });
    let store = builder.transaction_gate(gate.clone()).build().unwrap();
    driver.turn();
    let structural = gate.boundary().unwrap();
    let receipt = gate.release_pending_with_receipt().unwrap();
    drop(driver.finish(receipt.finish()).unwrap());
    assert!(gate.acknowledge(structural));
    driver.finish(store.ready()).unwrap();
    let port = driver.finish(store.command_port(&site)).unwrap().unwrap();
    drop((mount, site));
    let payload = Outcome {
        value: Box::new(Cell::new(47)),
        drops: Arc::clone(&drops),
    };
    let identity = (&raw const *payload.value).addr();
    let Ok(receipt) = port.try_send(payload) else {
        panic!("the live mounted port must admit the original command");
    };
    driver.turn();
    assert!(gate.has_pending());
    assert_eq!(
        gate.inspect_command(&port, |command| (&raw const *command.value).addr())
            .unwrap(),
        Some(identity),
    );
    let actor = driver.0.lock().pop().unwrap();
    let matcher_calls = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&matcher_calls);
    let result = mounted_admission_test_hook::with_before_mapper(
        move || {
            // The held inspection prevents graceful close; abandon the actual host future.
            std::thread::scope(|scope| scope.spawn(move || drop(actor)).join().unwrap());
        },
        || {
            gate.match_command(&port, |_| {
                counted.fetch_add(1, Ordering::SeqCst);
                true
            })
        },
    );
    let Err(error) = result else {
        panic!("actor abandonment must expire the mounted mapper during inspection");
    };
    assert_eq!(error.reason(), crate::TaskAccessReason::ScopeEnded);
    assert_eq!(error.progress(), crate::TaskProgress::NotStarted);
    assert_eq!(matcher_calls.load(Ordering::SeqCst), 0);
    assert_eq!(maps.load(Ordering::SeqCst), 0);
    assert_eq!(value.load(Ordering::SeqCst), 0);
    // The receipt retains the rejected payload after deferred inspection retirement.
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    drop((receipt, port, gate, store));
    assert!(driver.0.lock().is_empty());
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    assert_eq!(Arc::strong_count(&maps), 1);
    assert_eq!(Arc::strong_count(&value), 1);
    assert_eq!(Arc::strong_count(&drops), 1);
}

/// Exercises endpoint and output admission with a real named task context.
#[expect(
    clippy::too_many_lines,
    reason = "endpoint and output admission schedules share one real named mounted-task lifetime and manual driver"
)]
fn shared_endpoint_admission_control(schedule: SharedEndpointAdmission) {
    use crate::store::mounted::mounted_admission_test_hook;
    use std::sync::atomic::AtomicBool;

    enum Action {
        Start,
        ExportUnnamed,
        Value(Outcome),
    }
    struct Parent;
    impl Feature for Parent {
        type State = Option<Arc<AtomicUsize>>;
        type Action = ();

        fn reduce(
            &mut self,
            _: &mut Self::State,
            (): &(),
            _: &mut FeatureContext<'_, Self::State, ()>,
        ) {
        }
    }
    type ChildContext = TaskContext<Arc<AtomicUsize>, Action>;
    struct Child {
        export: Arc<Mutex<Option<ChildContext>>>,
        completion: Arc<AtomicBool>,
    }
    impl Feature for Child {
        type State = Arc<AtomicUsize>;
        type Action = Action;

        fn reduce(
            &mut self,
            state: &mut Self::State,
            action: &Action,
            context: &mut FeatureContext<'_, Self::State, Action>,
        ) {
            match action {
                Action::Start => {
                    let export = Arc::clone(&self.export);
                    let completion = Arc::clone(&self.completion);
                    context.task(Task::run_named(
                        "endpoint",
                        crate::OperationPolicy::Coexist,
                        move |task_context| async move {
                            *export.lock() = Some(task_context);
                            std::future::poll_fn(move |_| {
                                if completion.load(Ordering::SeqCst) {
                                    Poll::Ready(())
                                } else {
                                    Poll::Pending
                                }
                            })
                            .await;
                        },
                    ));
                }
                Action::ExportUnnamed => {
                    let export = Arc::clone(&self.export);
                    context.task(Task::run(move |task_context| async move {
                        *export.lock() = Some(task_context);
                    }));
                }
                Action::Value(value) => {
                    state.fetch_add(value.value.get(), Ordering::SeqCst);
                }
            }
        }
    }
    let driver = Driver::default();
    let value = Arc::new(AtomicUsize::new(0));
    let drops = Arc::new(AtomicUsize::new(0));
    let export = Arc::new(Mutex::new(None));
    let completion = Arc::new(AtomicBool::new(false));
    let descriptor = OptionalMount::new(
        Option::as_ref,
        Option::as_mut,
        |state: &Option<Arc<AtomicUsize>>| {
            state.as_ref().map(|_| MountIdentity {
                key: (),
                replacement: 0,
                configuration: 0,
            })
        },
        {
            let export = Arc::clone(&export);
            let completion = Arc::clone(&completion);
            move |_| Child {
                export: Arc::clone(&export),
                completion: Arc::clone(&completion),
            }
        },
    );
    let (builder, site) = Store::feature_builder(
        Some(Arc::clone(&value)),
        Parent,
        Runtime::new(driver.clone()),
    )
    .mailbox_capacity(1)
    .mount_with_site(descriptor);
    let output_calls = Arc::new(AtomicUsize::new(0));
    let handler_calls = Arc::clone(&output_calls);
    let (builder, output) = builder.output::<Outcome>(&site, |handlers| {
        handlers.at_root(move |_, value, _| {
            handler_calls.fetch_add(1, Ordering::SeqCst);
            OutputFlow::Continue(value)
        });
    });
    let store = builder.build().unwrap();
    driver.finish(store.ready()).unwrap();
    let child = driver.finish(store.acquire(&site)).unwrap().unwrap();
    let mut actor = driver.0.lock().pop().unwrap();
    let mut start = Box::pin(child.send(Action::Start));
    assert!(poll(start.as_mut()).is_pending());
    assert!(poll(actor.as_mut()).is_pending());
    let Poll::Ready(Ok(task)) = poll(start.as_mut()) else {
        panic!("named mounted task must be registered before observing its completion");
    };
    drop(start);
    let mut capture = Box::pin(child.operation("endpoint").capture());
    assert!(poll(capture.as_mut()).is_pending());
    assert!(poll(actor.as_mut()).is_pending());
    let Poll::Ready(Ok(wait)) = poll(capture.as_mut()) else {
        panic!("named mounted task must be captured while live");
    };
    drop(capture);
    drop(site);
    let mut run = driver.0.lock().pop().unwrap();
    assert!(poll(run.as_mut()).is_pending());
    let context = export.lock().take().unwrap();

    if matches!(
        schedule,
        SharedEndpointAdmission::OutputCloseBeforeLifecycle
            | SharedEndpointAdmission::OutputCancelAfterRetain
            | SharedEndpointAdmission::OutputCloseBeforeIngress
    ) {
        drop(child);
        let mut store = Some(store);
        let mut actor = Some(actor);
        let mut run = Some(run);
        let wait = Arc::new(Mutex::new(Box::pin(wait)));
        assert!(poll(wait.lock().as_mut()).is_pending());
        assert!(!task.is_finished());
        assert!(!task.is_cancelled());
        let payload = Box::new(Cell::new(67));
        let identity = (&raw const *payload).addr();
        let mut delivery = Box::pin(context.output(
            &output,
            Outcome {
                value: payload,
                drops: Arc::clone(&drops),
            },
        ));
        let calls = Arc::new(AtomicUsize::new(0));
        let callback_calls = Arc::clone(&calls);
        let outcome = match schedule {
            SharedEndpointAdmission::OutputCloseBeforeLifecycle => {
                let final_owner = store.take().unwrap();
                let actor = actor.take().unwrap();
                let run = run.take().unwrap();
                mounted_admission_test_hook::with_before_lifecycle(
                    move || {
                        callback_calls.fetch_add(1, Ordering::SeqCst);
                        // Release the actual host futures as well as the last Store owner.
                        std::thread::scope(|scope| {
                            scope
                                .spawn(move || drop((final_owner, run, actor)))
                                .join()
                                .unwrap();
                        });
                    },
                    || poll(delivery.as_mut()),
                )
            }
            SharedEndpointAdmission::OutputCancelAfterRetain => {
                let cancellation = task.clone();
                let mut run = run.take().unwrap();
                let completion = Arc::clone(&completion);
                let retained_wait = Arc::clone(&wait);
                mounted_admission_test_hook::with_after_retain(
                    move || {
                        callback_calls.fetch_add(1, Ordering::SeqCst);
                        std::thread::scope(|scope| {
                            scope
                                .spawn(move || {
                                    cancellation.cancel();
                                    completion.store(true, Ordering::SeqCst);
                                    assert!(poll(run.as_mut()).is_ready());
                                    drop(run);
                                })
                                .join()
                                .unwrap();
                        });
                        // The task body is gone, but this output still owns causal work.
                        assert!(poll(retained_wait.lock().as_mut()).is_pending());
                    },
                    || poll(delivery.as_mut()),
                )
            }
            SharedEndpointAdmission::OutputCloseBeforeIngress => {
                let final_owner = store.take().unwrap();
                let run = run.take().unwrap();
                let retained_wait = Arc::clone(&wait);
                mounted_admission_test_hook::with_before_ingress(
                    move || {
                        callback_calls.fetch_add(1, Ordering::SeqCst);
                        std::thread::scope(|scope| {
                            scope
                                .spawn(move || drop((final_owner, run)))
                                .join()
                                .unwrap();
                        });
                        assert!(poll(retained_wait.lock().as_mut()).is_pending());
                    },
                    || poll(delivery.as_mut()),
                )
            }
            _ => unreachable!("only output admission schedules enter this branch"),
        };
        let Poll::Ready(Err(crate::OutputError::Returned {
            value: returned,
            error,
        })) = outcome
        else {
            panic!("the output race must immediately return the unentered original value");
        };
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            error.reason(),
            if matches!(schedule, SharedEndpointAdmission::OutputCancelAfterRetain) {
                crate::TaskAccessReason::Cancelled
            } else {
                crate::TaskAccessReason::StoreClosed
            }
        );
        assert_eq!(error.progress(), crate::TaskProgress::NotStarted);
        assert_eq!((&raw const *returned.value).addr(), identity);
        assert_eq!(returned.value.get(), 67);
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        assert_eq!(output_calls.load(Ordering::SeqCst), 0);
        assert_eq!(value.load(Ordering::SeqCst), 0);
        // Rejecting the output releases its lease; no actor turn is needed for the wait.
        assert!(matches!(poll(wait.lock().as_mut()), Poll::Ready(Ok(()))));
        drop(returned);
        drop(delivery);
        drop(context);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        if let Some(mut actor) = actor {
            if store.is_some() {
                assert!(task.is_cancelled());
                assert!(poll(actor.as_mut()).is_pending());
                drop(store);
            }
            assert!(poll(actor.as_mut()).is_ready());
        }
        assert!(task.is_finished());
        drop(run);
        drop(wait);
        drop(task);
        assert!(driver.0.lock().is_empty());
        assert_eq!(Arc::strong_count(&output_calls), 1);
        assert_eq!(Arc::strong_count(&value), 1);
        assert_eq!(Arc::strong_count(&drops), 1);
        assert_eq!(Arc::strong_count(&export), 1);
        assert_eq!(Arc::strong_count(&completion), 1);
        return;
    }

    if !matches!(schedule, SharedEndpointAdmission::CompletedOperation) {
        drop(child);
        let closes_store = matches!(schedule, SharedEndpointAdmission::CloseBeforeIngress);
        let mut store = Some(store);
        let mut wait = Box::pin(wait);
        assert!(poll(wait.as_mut()).is_pending());
        assert!(!task.is_finished());
        assert!(!task.is_cancelled());
        let payload = Box::new(Cell::new(53));
        let identity = (&raw const *payload).addr();
        let mut send = Box::pin(context.send(Action::Value(Outcome {
            value: payload,
            drops: Arc::clone(&drops),
        })));
        let calls = Arc::new(AtomicUsize::new(0));
        let callback_calls = Arc::clone(&calls);
        let outcome = if closes_store {
            let final_owner = store.take().unwrap();
            mounted_admission_test_hook::with_before_ingress(
                move || {
                    callback_calls.fetch_add(1, Ordering::SeqCst);
                    // The worker drops the actual last owner after admission, not private state.
                    std::thread::scope(|scope| {
                        scope.spawn(move || drop(final_owner)).join().unwrap();
                    });
                },
                || poll(send.as_mut()),
            )
        } else {
            let cancellation = task.clone();
            mounted_admission_test_hook::with_after_retain(
                move || {
                    callback_calls.fetch_add(1, Ordering::SeqCst);
                    std::thread::scope(|scope| {
                        scope.spawn(move || cancellation.cancel()).join().unwrap();
                    });
                },
                || poll(send.as_mut()),
            )
        };
        let Poll::Ready(Err(crate::TaskSendError::Rejected { action, error })) = outcome else {
            panic!("the admission race must return the unentered original action immediately");
        };
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            error.reason(),
            if closes_store {
                crate::TaskAccessReason::StoreClosed
            } else {
                crate::TaskAccessReason::Cancelled
            }
        );
        assert_eq!(error.progress(), crate::TaskProgress::NotStarted);
        let Action::Value(payload) = action else {
            panic!("the rejected action must retain its original discriminant");
        };
        assert_eq!((&raw const *payload.value).addr(), identity);
        assert_eq!(payload.value.get(), 53);
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        assert_eq!(value.load(Ordering::SeqCst), 0);
        drop(payload);
        drop(send);
        drop(context);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        if closes_store {
            assert!(poll(actor.as_mut()).is_ready());
        } else {
            assert!(task.is_cancelled());
            assert!(poll(wait.as_mut()).is_pending());
            assert!(poll(actor.as_mut()).is_pending());
        }
        assert!(poll(run.as_mut()).is_ready());
        assert!(matches!(poll(wait.as_mut()), Poll::Ready(Ok(()))));
        if !closes_store {
            assert!(poll(actor.as_mut()).is_pending());
            assert!(task.is_finished());
            drop(store);
            assert!(poll(actor.as_mut()).is_ready());
        }
        drop(actor);
        drop(run);
        drop(wait);
        drop(task);
        assert!(driver.0.lock().is_empty());
        assert_eq!(value.load(Ordering::SeqCst), 0);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert_eq!(Arc::strong_count(&value), 1);
        assert_eq!(Arc::strong_count(&drops), 1);
        assert_eq!(Arc::strong_count(&export), 1);
        assert_eq!(Arc::strong_count(&completion), 1);
        return;
    }

    let callbacks = Arc::new(AtomicUsize::new(0));
    let observed = Arc::new(AtomicBool::new(false));
    let task_in_wake = task.clone();
    let value_in_wake = Arc::clone(&value);
    let drops_in_wake = Arc::clone(&drops);
    let callbacks_in_wake = Arc::clone(&callbacks);
    let observed_in_wake = Arc::clone(&observed);
    let output_in_wake = output.clone();
    let output_drops = Arc::new(AtomicUsize::new(0));
    let output_drops_in_wake = Arc::clone(&output_drops);
    let output_calls_in_wake = Arc::clone(&output_calls);
    let payload = Box::new(Cell::new(53));
    let identity = (&raw const *payload).addr();
    let wake = Arc::new(ObserverWake {
        calls: AtomicUsize::new(0),
        callback: Mutex::new(Some(Box::new(move || {
            // A passive observer polls only escaped access, never the actor or completing task.
            assert!(!task_in_wake.is_finished());
            let calls = Arc::clone(&callbacks_in_wake);
            let mut read = Box::pin(context.read(move |_| {
                calls.fetch_add(1, Ordering::SeqCst);
            }));
            let Poll::Ready(Err(error)) = poll(read.as_mut()) else {
                panic!("terminal operation ancestry must reject a read before publication");
            };
            assert_eq!(error.reason(), crate::TaskAccessReason::Cancelled);
            assert_eq!(error.progress(), crate::TaskProgress::NotStarted);
            let mut update = Box::pin(context.update(move |state| {
                callbacks_in_wake.fetch_add(1, Ordering::SeqCst);
                state.fetch_add(11, Ordering::SeqCst);
            }));
            let Poll::Ready(Err(error)) = poll(update.as_mut()) else {
                panic!("terminal operation ancestry must reject an update before publication");
            };
            assert_eq!(error.reason(), crate::TaskAccessReason::Cancelled);
            assert_eq!(error.progress(), crate::TaskProgress::NotStarted);
            let mut send = Box::pin(context.send(Action::Value(Outcome {
                value: payload,
                drops: Arc::clone(&drops_in_wake),
            })));
            let Poll::Ready(Err(crate::TaskSendError::Rejected { action, error })) =
                poll(send.as_mut())
            else {
                panic!("terminal operation ancestry must return the unentered original action");
            };
            assert_eq!(error.reason(), crate::TaskAccessReason::Cancelled);
            assert_eq!(error.progress(), crate::TaskProgress::NotStarted);
            let Action::Value(payload) = action else {
                panic!("the rejected action must retain its original discriminant");
            };
            assert_eq!((&raw const *payload.value).addr(), identity);
            assert_eq!(payload.value.get(), 53);
            assert_eq!(drops_in_wake.load(Ordering::SeqCst), 0);
            drop(payload);
            assert_eq!(drops_in_wake.load(Ordering::SeqCst), 1);
            let value = Box::new(Cell::new(59));
            let identity = (&raw const *value).addr();
            let mut delivery = Box::pin(context.output(
                &output_in_wake,
                Outcome {
                    value,
                    drops: Arc::clone(&output_drops_in_wake),
                },
            ));
            let Poll::Ready(Err(crate::OutputError::Returned { value, error })) =
                poll(delivery.as_mut())
            else {
                panic!("terminal operation ancestry must return the unentered original output");
            };
            assert_eq!(error.reason(), crate::TaskAccessReason::Cancelled);
            assert_eq!(error.progress(), crate::TaskProgress::NotStarted);
            assert_eq!((&raw const *value.value).addr(), identity);
            assert_eq!(value.value.get(), 59);
            assert_eq!(output_calls_in_wake.load(Ordering::SeqCst), 0);
            assert_eq!(output_drops_in_wake.load(Ordering::SeqCst), 0);
            drop(value);
            drop(delivery);
            assert_eq!(output_drops_in_wake.load(Ordering::SeqCst), 1);
            assert_eq!(value_in_wake.load(Ordering::SeqCst), 0);
            observed_in_wake.store(true, Ordering::SeqCst);
        }))),
    });
    let waker = Waker::from(Arc::clone(&wake));
    let mut wait = Box::pin(wait);
    assert!(
        wait.as_mut()
            .poll(&mut Context::from_waker(&waker))
            .is_pending()
    );
    assert!(!observed.load(Ordering::SeqCst));
    assert!(!task.is_finished());
    completion.store(true, Ordering::SeqCst);
    assert!(poll(run.as_mut()).is_ready());
    assert_eq!(wake.calls.load(Ordering::SeqCst), 1);
    assert!(wake.callback.lock().is_none());
    assert!(observed.load(Ordering::SeqCst));
    assert!(matches!(poll(wait.as_mut()), Poll::Ready(Ok(()))));
    assert!(
        !task.is_finished(),
        "causal completion awaits actor harvest"
    );
    assert_eq!(callbacks.load(Ordering::SeqCst), 0);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    assert_eq!(value.load(Ordering::SeqCst), 0);
    assert!(poll(actor.as_mut()).is_pending());
    assert!(task.is_finished());
    assert_eq!(value.load(Ordering::SeqCst), 0);

    // An unnamed task has no terminal operation ancestry to reject retention;
    // its completed task provenance must independently prevent output admission.
    let mut unnamed = Box::pin(child.send(Action::ExportUnnamed));
    assert!(poll(unnamed.as_mut()).is_pending());
    assert!(poll(actor.as_mut()).is_pending());
    let Poll::Ready(Ok(unnamed_task)) = poll(unnamed.as_mut()) else {
        panic!("the live child must start an unnamed task");
    };
    drop(unnamed);
    let mut unnamed_run = driver.0.lock().pop().unwrap();
    assert!(poll(unnamed_run.as_mut()).is_ready());
    assert!(poll(actor.as_mut()).is_pending());
    assert!(unnamed_task.is_finished());
    let context = export.lock().take().unwrap();
    let payload = Box::new(Cell::new(61));
    let identity = (&raw const *payload).addr();
    let mut delivery = Box::pin(context.output(
        &output,
        Outcome {
            value: payload,
            drops: Arc::clone(&output_drops),
        },
    ));
    let Poll::Ready(Err(crate::OutputError::Returned {
        value: returned,
        error,
    })) = poll(delivery.as_mut())
    else {
        panic!("completed unnamed task provenance must return the original output");
    };
    assert_eq!(error.reason(), crate::TaskAccessReason::Cancelled);
    assert_eq!(error.progress(), crate::TaskProgress::NotStarted);
    assert_eq!((&raw const *returned.value).addr(), identity);
    assert_eq!(returned.value.get(), 61);
    assert_eq!(output_calls.load(Ordering::SeqCst), 0);
    assert_eq!(output_drops.load(Ordering::SeqCst), 1);
    drop(returned);
    drop(delivery);
    drop(context);
    drop(child);
    assert_eq!(output_drops.load(Ordering::SeqCst), 2);
    assert_eq!(value.load(Ordering::SeqCst), 0);
    drop(store);
    assert!(poll(actor.as_mut()).is_ready());
    assert!(driver.0.lock().is_empty());
    assert_eq!(Arc::strong_count(&output_calls), 1);
    assert_eq!(Arc::strong_count(&output_drops), 1);
    assert_eq!(Arc::strong_count(&value), 1);
    assert_eq!(Arc::strong_count(&export), 1);
    assert_eq!(Arc::strong_count(&completion), 1);
}

#[test]
fn root_effect_send_cancelled_after_retaining_work_never_reaches_reducer() {
    use crate::store::mounted::mounted_admission_test_hook;

    enum Action {
        Start,
        Value(Outcome),
    }

    let driver = Driver::default();
    let reductions = Arc::new(AtomicUsize::new(0));
    let reducer_calls = Arc::clone(&reductions);
    let drops = Arc::new(AtomicUsize::new(0));
    let export = Arc::new(Mutex::new(None::<crate::EffectSender<Action>>));
    let reducer_export = Arc::clone(&export);
    let store = Store::new(
        0usize,
        crate::Reduce::new(move |state: &mut usize, action: &Action, _| match action {
            Action::Start => {
                let export = Arc::clone(&reducer_export);
                Effect::run(move |sender| async move {
                    *export.lock() = Some(sender);
                    std::future::pending::<()>().await;
                })
                .named("sender", crate::OperationPolicy::Coexist)
            }
            Action::Value(value) => {
                reducer_calls.fetch_add(1, Ordering::SeqCst);
                *state += value.value.get();
                Effect::none()
            }
        }),
        Runtime::new(driver.clone()),
    );
    driver.finish(store.ready()).unwrap();
    let mut actor = driver.0.lock().pop().unwrap();
    let mut start = Box::pin(store.send(Action::Start));
    assert!(poll(start.as_mut()).is_pending());
    assert!(poll(actor.as_mut()).is_pending());
    let Poll::Ready(Ok(task)) = poll(start.as_mut()) else {
        panic!("root effect must be registered before exporting its sender");
    };
    drop(start);
    let mut capture = Box::pin(store.operation("sender").capture());
    assert!(poll(capture.as_mut()).is_pending());
    assert!(poll(actor.as_mut()).is_pending());
    let Poll::Ready(Ok(wait)) = poll(capture.as_mut()) else {
        panic!("root operation must be captured while live");
    };
    drop(capture);
    let mut run = driver.0.lock().pop().unwrap();
    assert!(poll(run.as_mut()).is_pending());
    let sender = export.lock().take().expect("running root sender");
    let mut wait = Box::pin(wait);
    assert!(poll(wait.as_mut()).is_pending());
    assert!(!task.is_finished());
    let action = Action::Value(Outcome {
        value: Box::new(Cell::new(53)),
        drops: Arc::clone(&drops),
    });
    let calls = Arc::new(AtomicUsize::new(0));
    let hook_calls = Arc::clone(&calls);
    let cancellation = task.clone();
    let mut send = Box::pin(sender.send(action));
    let outcome = mounted_admission_test_hook::with_after_retain(
        move || {
            hook_calls.fetch_add(1, Ordering::SeqCst);
            // The worker cancels only; it never polls the Store actor or effect.
            std::thread::scope(|scope| {
                scope.spawn(move || cancellation.cancel()).join().unwrap();
            });
        },
        || poll(send.as_mut()),
    );
    let Poll::Ready(Err(error)) = outcome else {
        panic!("cancellation after operation retention must reject the root action");
    };
    assert!(error.is_cancelled());
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    assert_eq!(reductions.load(Ordering::SeqCst), 0);
    assert!(task.is_cancelled());
    assert!(poll(wait.as_mut()).is_pending());
    assert!(!task.is_finished());
    drop(send);
    // Lineage cancellation wakes the actor; its control pass aborts the body.
    assert!(poll(actor.as_mut()).is_pending());
    assert!(poll(wait.as_mut()).is_pending());
    assert!(!task.is_finished());
    assert!(poll(run.as_mut()).is_ready());
    assert!(matches!(poll(wait.as_mut()), Poll::Ready(Ok(()))));
    assert!(poll(actor.as_mut()).is_pending());
    assert!(task.is_finished());
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    assert_eq!(reductions.load(Ordering::SeqCst), 0);
    drop(sender);
    drop(store);
    assert!(poll(actor.as_mut()).is_ready());
    drop((actor, run, wait, task));
    assert!(driver.0.lock().is_empty());
    assert_eq!(Arc::strong_count(&drops), 1);
    assert_eq!(Arc::strong_count(&reductions), 1);
    assert_eq!(Arc::strong_count(&export), 1);
}

#[test]
fn operation_completion_observer_rejects_shared_effect_send_before_task_terminal_publication() {
    shared_effect_sender_retention_control(false);
}

#[test]
fn shared_effect_send_cancelled_after_retaining_work_never_reaches_reducer() {
    shared_effect_sender_retention_control(true);
}

/// Interrupts an effect sender after retention, or observes its completed owner.
#[expect(
    clippy::too_many_lines,
    reason = "two admission boundaries share one real named mounted-effect lifetime and manual driver"
)]
fn shared_effect_sender_retention_control(cancel_after_retain: bool) {
    use crate::store::mounted::mounted_admission_test_hook;
    use std::sync::atomic::AtomicBool;

    enum Action {
        Start,
        Value(Outcome),
    }
    struct Parent;
    impl Feature for Parent {
        type State = Option<Arc<AtomicUsize>>;
        type Action = ();

        fn reduce(
            &mut self,
            _: &mut Self::State,
            (): &(),
            _: &mut FeatureContext<'_, Self::State, ()>,
        ) {
        }
    }
    type SenderSlot = Arc<Mutex<Option<crate::EffectSender<Action>>>>;
    struct Child {
        export: SenderSlot,
        completion: Arc<AtomicBool>,
    }
    impl Feature for Child {
        type State = Arc<AtomicUsize>;
        type Action = Action;

        fn reduce(
            &mut self,
            state: &mut Self::State,
            action: &Action,
            context: &mut FeatureContext<'_, Self::State, Action>,
        ) {
            match action {
                Action::Start => {
                    let export = Arc::clone(&self.export);
                    let completion = Arc::clone(&self.completion);
                    context.effect(
                        Effect::run(move |sender| async move {
                            *export.lock() = Some(sender);
                            std::future::poll_fn(move |_| {
                                if completion.load(Ordering::SeqCst) {
                                    Poll::Ready(())
                                } else {
                                    Poll::Pending
                                }
                            })
                            .await;
                        })
                        .named("sender", crate::OperationPolicy::Coexist),
                    );
                }
                Action::Value(value) => {
                    state.fetch_add(value.value.get(), Ordering::SeqCst);
                }
            }
        }
    }
    let driver = Driver::default();
    let value = Arc::new(AtomicUsize::new(0));
    let drops = Arc::new(AtomicUsize::new(0));
    let export = Arc::new(Mutex::new(None));
    let completion = Arc::new(AtomicBool::new(false));
    let descriptor = OptionalMount::new(
        Option::as_ref,
        Option::as_mut,
        |state: &Option<Arc<AtomicUsize>>| {
            state.as_ref().map(|_| MountIdentity {
                key: (),
                replacement: 0,
                configuration: 0,
            })
        },
        {
            let export = Arc::clone(&export);
            let completion = Arc::clone(&completion);
            move |_| Child {
                export: Arc::clone(&export),
                completion: Arc::clone(&completion),
            }
        },
    );
    let (builder, site) = Store::feature_builder(
        Some(Arc::clone(&value)),
        Parent,
        Runtime::new(driver.clone()),
    )
    .mailbox_capacity(1)
    .mount_with_site(descriptor);
    let store = builder.build().unwrap();
    driver.finish(store.ready()).unwrap();
    let child = driver.finish(store.acquire(&site)).unwrap().unwrap();
    let mut actor = driver.0.lock().pop().unwrap();
    let mut start = Box::pin(child.send(Action::Start));
    assert!(poll(start.as_mut()).is_pending());
    assert!(poll(actor.as_mut()).is_pending());
    let Poll::Ready(Ok(task)) = poll(start.as_mut()) else {
        panic!("named mounted effect must be registered before exporting its sender");
    };
    drop(start);
    let mut capture = Box::pin(child.operation("sender").capture());
    assert!(poll(capture.as_mut()).is_pending());
    assert!(poll(actor.as_mut()).is_pending());
    let Poll::Ready(Ok(wait)) = poll(capture.as_mut()) else {
        panic!("named mounted operation must be captured while live");
    };
    drop(capture);
    drop(child);
    drop(site);
    let mut run = driver.0.lock().pop().unwrap();
    assert!(poll(run.as_mut()).is_pending());
    let sender = export.lock().take().unwrap();
    let mut wait = Box::pin(wait);
    assert!(!task.is_finished());
    assert!(!task.is_cancelled());
    let action = Action::Value(Outcome {
        value: Box::new(Cell::new(53)),
        drops: Arc::clone(&drops),
    });

    if cancel_after_retain {
        assert!(poll(wait.as_mut()).is_pending());
        let calls = Arc::new(AtomicUsize::new(0));
        let callback_calls = Arc::clone(&calls);
        let cancellation = task.clone();
        let mut send = Box::pin(sender.send(action));
        let outcome = mounted_admission_test_hook::with_after_retain(
            move || {
                callback_calls.fetch_add(1, Ordering::SeqCst);
                // The worker cancels only; it never polls the actor or mounted effect.
                std::thread::scope(|scope| {
                    scope.spawn(move || cancellation.cancel()).join().unwrap();
                });
            },
            || poll(send.as_mut()),
        );
        let Poll::Ready(Err(error)) = outcome else {
            panic!("cancellation after retention must reject before publishing the action");
        };
        assert!(error.is_cancelled());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(task.is_cancelled());
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert_eq!(value.load(Ordering::SeqCst), 0);
        drop(send);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert!(poll(wait.as_mut()).is_pending());
        assert!(poll(actor.as_mut()).is_pending());
        assert!(poll(run.as_mut()).is_ready());
    } else {
        let observed = Arc::new(AtomicBool::new(false));
        let observed_in_wake = Arc::clone(&observed);
        let sender = sender.clone();
        let task_in_wake = task.clone();
        let value_in_wake = Arc::clone(&value);
        let drops_in_wake = Arc::clone(&drops);
        let wake = Arc::new(ObserverWake {
            calls: AtomicUsize::new(0),
            callback: Mutex::new(Some(Box::new(move || {
                // Passive observation cannot re-enter the actor or completing effect.
                assert!(!task_in_wake.is_finished());
                assert!(!task_in_wake.is_cancelled());
                let mut send = Box::pin(sender.send(action));
                let Poll::Ready(Err(error)) = poll(send.as_mut()) else {
                    panic!("terminal operation ownership must not be resurrected by its sender");
                };
                assert!(error.is_cancelled());
                assert_eq!(drops_in_wake.load(Ordering::SeqCst), 1);
                assert_eq!(value_in_wake.load(Ordering::SeqCst), 0);
                observed_in_wake.store(true, Ordering::SeqCst);
            }))),
        });
        let waker = Waker::from(Arc::clone(&wake));
        assert!(
            wait.as_mut()
                .poll(&mut Context::from_waker(&waker))
                .is_pending()
        );
        assert!(!observed.load(Ordering::SeqCst));
        completion.store(true, Ordering::SeqCst);
        assert!(poll(run.as_mut()).is_ready());
        assert_eq!(wake.calls.load(Ordering::SeqCst), 1);
        assert!(wake.callback.lock().is_none());
        assert!(observed.load(Ordering::SeqCst));
        assert!(
            !task.is_finished(),
            "causal completion awaits actor harvest"
        );
    }
    assert!(matches!(poll(wait.as_mut()), Poll::Ready(Ok(()))));
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    assert_eq!(value.load(Ordering::SeqCst), 0);
    assert!(poll(actor.as_mut()).is_pending());
    assert!(task.is_finished());
    assert_eq!(value.load(Ordering::SeqCst), 0);
    drop(sender);
    drop(store);
    assert!(poll(actor.as_mut()).is_ready());
    drop(actor);
    drop(run);
    drop(wait);
    drop(task);
    assert!(driver.0.lock().is_empty());
    assert_eq!(Arc::strong_count(&value), 1);
    assert_eq!(Arc::strong_count(&drops), 1);
    assert_eq!(Arc::strong_count(&export), 1);
    assert_eq!(Arc::strong_count(&completion), 1);
}

/// Replaces a row at the same key so its old output authority must expire.
struct RootFeature;
impl Feature for RootFeature {
    type State = State;
    type Action = RootAction;
    fn reduce(
        &mut self,
        state: &mut State,
        action: &RootAction,
        _: &mut FeatureContext<'_, State, RootAction>,
    ) {
        if matches!(action, RootAction::Replace) {
            state.rows.remove(&"one".to_owned());
            state.rows.insert(
                "one".to_owned(),
                Row {
                    child: Some(()),
                    handled: 0,
                    effects: 0,
                    tasks: 0,
                },
            );
        }
    }
}

#[test]
fn nested_output_retirement_drops_handler_capture_before_successor_and_rejects_queued_value() {
    nested_output_retirement_control(false);
}

#[test]
fn gated_nested_retirement_rejects_stale_output_before_publishing_uninspectable_frontier() {
    nested_output_retirement_control(true);
}

/// Replaces an output owner with an earlier queued action, with or without a gate.
#[expect(
    clippy::too_many_lines,
    reason = "keep queued output, replacement and capture destruction in one retirement scenario"
)]
fn nested_output_retirement_control(gated: bool) {
    let driver = Driver::default();
    let gate = gated.then(crate::testing::TransactionGate::<State, RootAction>::new);
    let captures = Arc::new(Mutex::new(Vec::<Weak<()>>::new()));
    let outputs = Arc::new(Mutex::new(Vec::<Output<Outcome>>::new()));
    let sites = Arc::new(Mutex::new(Vec::new()));
    let factory_captures = std::sync::Arc::clone(&captures);
    let recipe_captures = std::sync::Arc::clone(&captures);
    let recipe_outputs = std::sync::Arc::clone(&outputs);
    let recipe_sites = std::sync::Arc::clone(&sites);
    let (owner, _) = super::nested::attach_selected_key::<State, RootAction, RowFeature, _, _>(
        |state| &state.rows,
        |state| &mut state.rows,
        "one".to_owned(),
        move |_| {
            assert!(
                factory_captures
                    .lock()
                    .iter()
                    .all(|capture| capture.upgrade().is_none()),
                "old output handlers retire before successor construction"
            );
            RowFeature
        },
        move |_, children| {
            let source = children.optional(OptionalMount::new(
                |row: &Row| row.child.as_ref(),
                |row: &mut Row| row.child.as_mut(),
                |row: &Row| {
                    row.child.as_ref().map(|()| MountIdentity {
                        key: (),
                        replacement: 0,
                        configuration: 0,
                    })
                },
                |()| SourceFeature,
            ));
            let capture = Arc::new(());
            recipe_captures.lock().push(Arc::downgrade(&capture));
            let output = children.output::<Outcome>(&source, |handlers| {
                handlers.at_root(move |_, value, _| {
                    let _ = &capture;
                    OutputFlow::Continue(value)
                });
            });
            recipe_sites.lock().push(source);
            recipe_outputs.lock().push(output);
        },
    );
    let mut builder = Store::feature_builder(
        State {
            rows: [(
                "one".to_owned(),
                Row {
                    child: Some(()),
                    handled: 0,
                    effects: 0,
                    tasks: 0,
                },
            )]
            .into_iter()
            .collect(),
            received: None,
        },
        RootFeature,
        Runtime::new(driver.clone()),
    )
    .with_mount(owner);
    if let Some(gate) = &gate {
        builder = builder.transaction_gate(gate.clone());
    }
    let store = builder.build().unwrap();
    if let Some(gate) = &gate {
        driver.turn();
        let boundary = gate.boundary().unwrap();
        let receipt = gate.release_pending_with_receipt().unwrap();
        drop(driver.finish(receipt.finish()).unwrap());
        assert!(gate.acknowledge(boundary));
    }
    driver.finish(store.ready()).unwrap();
    let old_site = sites.lock()[0].clone();
    let context = driver.finish(store.acquire(&old_site)).unwrap().unwrap();
    let output = outputs.lock()[0].clone();
    let drops = Arc::new(AtomicUsize::new(0));
    let value = Outcome {
        value: Box::new(Cell::new(9)),
        drops: std::sync::Arc::clone(&drops),
    };
    let identity = &raw const *value.value as usize;
    let mut replace = Box::pin(store.send(RootAction::Replace));
    assert!(poll(replace.as_mut()).is_pending());
    let mut queued = Box::pin(context.output(&output, value));
    assert!(poll(queued.as_mut()).is_pending());
    driver.turn();
    if let Some(gate) = &gate {
        let boundary = gate.boundary().unwrap();
        let receipt = gate.release_pending_with_receipt().unwrap();
        drop(driver.finish(receipt.finish()).unwrap());
        assert!(gate.acknowledge(boundary));
        driver.turn();
    }
    assert!(matches!(poll(replace.as_mut()), Poll::Ready(Ok(_))));
    drop(replace);
    let error = driver.finish(queued).unwrap_err();
    let crate::OutputError::Returned { value, error } = error else {
        panic!("stale output returns owned value")
    };
    assert_eq!(error.reason(), TaskAccessReason::ScopeEnded);
    assert_eq!(error.progress(), TaskProgress::NotStarted);
    assert_eq!(&raw const *value.value as usize, identity);
    assert_eq!(outputs.lock().len(), 2);
    assert!(captures.lock()[0].upgrade().is_none());
    assert!(captures.lock()[1].upgrade().is_some());
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    drop(value);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    drop(store);
    driver.turn();
    assert!(driver.0.lock().is_empty());
    assert!(
        captures
            .lock()
            .iter()
            .all(|capture| capture.upgrade().is_none())
    );
}

/// Records only ancestor effects; handler and task counts change elsewhere.
struct RowFeature;
impl Feature for RowFeature {
    type State = Row;
    type Action = RowAction;
    fn reduce(
        &mut self,
        state: &mut Row,
        _: &RowAction,
        _: &mut FeatureContext<'_, Row, RowAction>,
    ) {
        state.effects += 1;
    }
}
/// Provides the leaf's distinct action type without handling root actions.
struct SourceFeature;
impl Feature for SourceFeature {
    type State = ();
    type Action = SourceAction;
    fn reduce(
        &mut self,
        (): &mut (),
        _: &SourceAction,
        _: &mut FeatureContext<'_, (), SourceAction>,
    ) {
    }
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "keep descendant output, ancestor effect and task routing in one scenario"
)]
fn descendant_output_lowers_ancestor_effect_and_task_with_distinct_root_action() {
    let driver = Driver::default();
    let drops = Arc::new(AtomicUsize::new(0));
    let output_cell = Arc::new(OnceLock::<Output<Outcome>>::new());
    let parent_cell = Arc::new(OnceLock::<MountSite<State, Row, RowAction, RootAction>>::new());
    let recipe_parent = std::sync::Arc::clone(&parent_cell);
    let recipe_output = std::sync::Arc::clone(&output_cell);
    let recipe_drops = std::sync::Arc::clone(&drops);
    let (owner, parent) = super::nested::attach_selected_key::<State, RootAction, RowFeature, _, _>(
        |state| &state.rows,
        |state| &mut state.rows,
        "one".to_owned(),
        |_| RowFeature,
        move |_, children| {
            let initial_output = std::sync::Arc::clone(&recipe_output);
            let initial_drops = std::sync::Arc::clone(&recipe_drops);
            let source = children.optional(
                OptionalMount::new(
                    |row: &Row| row.child.as_ref(),
                    |row: &mut Row| row.child.as_mut(),
                    |row: &Row| {
                        row.child.as_ref().map(|()| MountIdentity {
                            key: (),
                            replacement: 0,
                            configuration: 0,
                        })
                    },
                    |()| SourceFeature,
                )
                .on_initial(
                    |()| (),
                    move |_, _| {
                        let output = initial_output.get().unwrap().clone();
                        let drops = std::sync::Arc::clone(&initial_drops);
                        Task::run(move |context| async move {
                            context
                                .output(
                                    &output,
                                    Outcome {
                                        value: Box::new(Cell::new(7)),
                                        drops,
                                    },
                                )
                                .await
                                .unwrap();
                        })
                    },
                ),
            );
            let output = children.output::<Outcome>(&source, |handlers| {
                handlers.at(recipe_parent.get().unwrap(), |row, value, context| {
                    row.handled += value.value.get();
                    context.effect(Effect::send(RowAction::Effect));
                    context.task(Task::run(|context| async move {
                        context
                            .update(|row: &mut Row| row.tasks += 1)
                            .await
                            .unwrap();
                    }));
                    OutputFlow::Continue(value)
                });
                handlers.at_root(|root, value, _| {
                    root.received = Some(value);
                    OutputFlow::Consumed
                });
            });
            recipe_output
                .set(output)
                .as_ref()
                .expect("output is installed once");
        },
    );
    parent_cell
        .set(parent)
        .as_ref()
        .expect("parent site is installed once");
    let store = Store::feature_builder(
        State {
            rows: [(
                "one".to_owned(),
                Row {
                    child: Some(()),
                    handled: 0,
                    effects: 0,
                    tasks: 0,
                },
            )]
            .into_iter()
            .collect(),
            received: None,
        },
        RootFeature,
        Runtime::new(driver.clone()),
    )
    .with_mount(owner)
    .build()
    .unwrap();
    driver.finish(store.ready()).unwrap();
    for _ in 0..8 {
        driver.turn();
    }
    let values = driver
        .finish(store.read(|state| {
            let row = state.rows.get(&"one".to_owned()).unwrap();
            (
                row.handled,
                row.effects,
                row.tasks,
                state.received.as_ref().unwrap().value.get(),
            )
        }))
        .unwrap();
    assert_eq!(values, (7, 1, 1, 7));
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    drop(store);
    driver.turn();
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    assert!(driver.0.lock().is_empty());
}

#[test]
fn driver_honours_frozen_clock() {
    let driver = Driver::default();
    let mut elapsed = driver.sleep_until(crate::MonotonicInstant::ZERO);
    assert!(
        poll(elapsed.as_mut()).is_ready(),
        "elapsed deadline must be ready"
    );

    for deadline in [
        crate::MonotonicInstant::from_elapsed(std::time::Duration::from_nanos(1)),
        crate::MonotonicInstant::MAX,
    ] {
        let mut future = driver.sleep_until(deadline);
        assert!(poll(future.as_mut()).is_pending());
        assert_eq!(driver.now(), crate::MonotonicInstant::ZERO);
    }
}

#[test]
fn driver_yields_and_wakes_before_completing() {
    let driver = Driver::default();
    let wake = Arc::new(ObserverWake {
        callback: Mutex::new(None),
        calls: AtomicUsize::new(0),
    });
    let waker = Waker::from(Arc::clone(&wake));
    let context = &mut Context::from_waker(&waker);
    let mut yielding = driver.yield_now();

    assert_eq!(wake.calls.load(Ordering::SeqCst), 0);
    assert!(
        yielding.as_mut().poll(context).is_pending(),
        "yield must offer a scheduling turn"
    );
    assert_eq!(wake.calls.load(Ordering::SeqCst), 1);
    assert!(yielding.as_mut().poll(context).is_ready());
    assert_eq!(wake.calls.load(Ordering::SeqCst), 1);
}
