//! Five actual mounted edges route private commands without ancestor actions.

use std::cell::Cell;
use std::marker::PhantomData;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

use futures_channel::oneshot;
use rca_core::{
    DependencyKey, DependencyValues, Feature, FeatureContext, MountIdentity, OptionalMount, Output,
    OutputFlow, Runtime, RuntimeDriver, RuntimeFuture, Store, Task, TaskAccessReason,
    TaskSendError,
};

/// Exercises sustained routing normally, with the same ownership schedule at Miri scale.
const INPUT_COUNT: usize = if cfg!(miri) { 3 } else { 10_000 };

/// Manually polls a frozen-clock actor; yielding still suspends for one turn.
#[derive(Clone, Default)]
struct Driver(Arc<Mutex<Vec<RuntimeFuture>>>);
impl std::fmt::Debug for Driver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PluginDriver")
    }
}
impl RuntimeDriver for Driver {
    fn spawn(&self, task: RuntimeFuture) {
        self.0.lock().unwrap().push(task);
    }
    fn now(&self) -> rca_core::MonotonicInstant {
        rca_core::MonotonicInstant::ZERO
    }
    fn sleep_until(&self, deadline: rca_core::MonotonicInstant) -> RuntimeFuture {
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
fn manual_driver_clock_waits_respect_the_fixed_origin() {
    let driver = Driver::default();
    assert_eq!(
        rca_core::RuntimeDriver::now(&driver),
        rca_core::MonotonicInstant::ZERO
    );
    let mut elapsed =
        rca_core::RuntimeDriver::sleep_until(&driver, rca_core::MonotonicInstant::ZERO);
    let mut context = Context::from_waker(Waker::noop());
    assert!(elapsed.as_mut().poll(&mut context).is_ready());

    for deadline in [
        rca_core::MonotonicInstant::from_elapsed(std::time::Duration::from_nanos(1)),
        rca_core::MonotonicInstant::MAX,
    ] {
        let mut future = rca_core::RuntimeDriver::sleep_until(&driver, deadline);
        assert!(future.as_mut().poll(&mut context).is_pending());
    }
    assert!(driver.0.lock().unwrap().is_empty());
}

#[test]
fn manual_driver_yield_suspends_and_wakes_before_completion() {
    /// Counts scheduling requests without driving the actor from the waker.
    struct WakeCount(std::sync::atomic::AtomicUsize);
    impl std::task::Wake for WakeCount {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    }

    let driver = Driver::default();
    let notifications = Arc::new(WakeCount(std::sync::atomic::AtomicUsize::new(0)));
    let waker = Waker::from(Arc::clone(&notifications));
    let mut context = Context::from_waker(&waker);
    let mut future = rca_core::RuntimeDriver::yield_now(&driver);
    assert_eq!(notifications.0.load(std::sync::atomic::Ordering::SeqCst), 0);
    assert!(future.as_mut().poll(&mut context).is_pending());
    assert_eq!(notifications.0.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert!(future.as_mut().poll(&mut context).is_ready());
    assert_eq!(notifications.0.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert!(driver.0.lock().unwrap().is_empty());
}

impl Driver {
    /// Polls the captured queue once, outside its lock; new work waits for a later turn.
    fn turn(&self) {
        let tasks = std::mem::take(&mut *self.0.lock().unwrap());
        for mut task in tasks {
            if task
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending()
            {
                self.0.lock().unwrap().push(task);
            }
        }
    }
    /// Bounds progress for a finite operation rather than waiting on wall-clock time.
    fn finish<F: Future>(&self, future: F) -> F::Output {
        let mut future = std::pin::pin!(future);
        for _ in 0..32 {
            if let Poll::Ready(value) = future
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
            {
                return value;
            }
            self.turn();
        }
        panic!("bounded plugin actor progress")
    }
}

/// Separates the command count, captured dependency and later live-state client.
#[derive(Debug, PartialEq)]
struct Outcome {
    inputs: usize,
    captured_client: usize,
    current_client: usize,
}
struct Backend(usize);
struct Client;
impl DependencyKey for Client {
    type Value = Arc<Backend>;
}

/// An actual mounted ancestor; private leaf input must not increment its reductions.
struct Layer<T> {
    child: Option<T>,
    replacement: u64,
    reductions: usize,
    outcomes: Vec<Outcome>,
}
fn layer<T>(child: T) -> Layer<T> {
    Layer {
        child: Some(child),
        replacement: 0,
        reductions: 0,
        outcomes: Vec::new(),
    }
}
struct LayerAction;
struct LayerFeature<T>(PhantomData<fn() -> T>);
impl<T: 'static> Feature for LayerFeature<T> {
    type State = Layer<T>;
    type Action = LayerAction;
    fn reduce(
        &mut self,
        state: &mut Layer<T>,
        _: &LayerAction,
        _: &mut FeatureContext<'_, Layer<T>, LayerAction>,
    ) {
        state.reductions += 1;
    }
}
/// Gives each nested edge its own replacement identity and real child projection.
fn child<T: Send + 'static, F: Feature<State = T> + Send + 'static>(
    make: impl FnMut(&T) -> F + Send + 'static,
) -> OptionalMount<Layer<T>, F, ()>
where
    F::Action: Send,
{
    OptionalMount::new(
        |state: &Layer<T>| state.child.as_ref(),
        |state: &mut Layer<T>| state.child.as_mut(),
        |state: &Layer<T>| {
            state.child.as_ref().map(|_| MountIdentity {
                key: (),
                replacement: state.replacement,
                configuration: 0,
            })
        },
        make,
    )
}

struct Leaf {
    reductions: usize,
    current_client: Arc<Backend>,
}
struct LeafAction;
/// Transfers a non-cloneable command through the leaf's private port.
struct PrivateInput(Box<Cell<usize>>);
/// Suspends the last input's task so capture, live reads and replacement stay distinct.
struct Plugin {
    output: Arc<Mutex<Option<Output<Outcome>>>>,
    suspended: Arc<Mutex<Option<oneshot::Receiver<()>>>>,
    started: Arc<AtomicUsize>,
}
impl Feature for Plugin {
    type State = Leaf;
    type Action = LeafAction;
    fn reduce(
        &mut self,
        state: &mut Leaf,
        _: &LeafAction,
        context: &mut FeatureContext<'_, Leaf, LeafAction>,
    ) {
        state.reductions += 1;
        if state.reductions == INPUT_COUNT {
            let captured = Arc::clone(context.reducer_context().dependency::<Client>());
            let output = self.output.lock().unwrap().as_ref().unwrap().clone();
            let suspended = self.suspended.lock().unwrap().take().unwrap();
            let started = Arc::clone(&self.started);
            let inputs = state.reductions;
            context.task(Task::<Leaf, LeafAction>::run(move |task| async move {
                started.fetch_add(1, Ordering::SeqCst);
                suspended.await.unwrap();
                let current = task
                    .read(|state| Arc::clone(&state.current_client))
                    .await
                    .unwrap();
                drop(
                    task.output(
                        &output,
                        Outcome {
                            inputs,
                            captured_client: captured.0,
                            current_client: current.0,
                        },
                    )
                    .await
                    .unwrap(),
                );
            }));
        }
    }
}
type Level4 = Layer<Leaf>;
type Level3 = Layer<Level4>;
type Level2 = Layer<Level3>;
type Level1 = Layer<Level2>;
type Root = Layer<Level1>;
fn subtree() -> Level2 {
    layer(layer(layer(Leaf {
        reductions: 0,
        current_client: Arc::new(Backend(1)),
    })))
}
/// Reads root-to-leaf reducer counts without deriving the expected routing result.
fn counts(root: &Root) -> [usize; 6] {
    let one = root.child.as_ref().unwrap();
    let two = one.child.as_ref().unwrap();
    let three = two.child.as_ref().unwrap();
    let four = three.child.as_ref().unwrap();
    [
        root.reductions,
        one.reductions,
        two.reductions,
        three.reductions,
        four.reductions,
        four.child.as_ref().unwrap().reductions,
    ]
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "One depth-five workload preserves ancestor bypass, capture lifetime and teardown checks"
)]
fn ten_thousand_depth_five_inputs_bypass_ancestors_and_keep_capture_and_lifetime_boundaries() {
    let driver = Driver::default();
    let sites = Arc::new(Mutex::new(Vec::new()));
    let output = Arc::new(Mutex::new(None));
    let maps = Arc::new(AtomicUsize::new(0));
    let started = Arc::new(AtomicUsize::new(0));
    let (release, suspended) = oneshot::channel();
    let suspended = Arc::new(Mutex::new(Some(suspended)));
    let mut dependencies = DependencyValues::new();
    dependencies
        .prepare(|values| values.set::<Client>(Arc::new(Backend(2))))
        .unwrap();
    let declared = Arc::clone(&sites);
    let mapped = Arc::clone(&maps);
    let task_started = Arc::clone(&started);
    let (builder, level_one) = Store::feature_builder(
        layer(layer(subtree())),
        LayerFeature::<Level1>(PhantomData),
        Runtime::new(driver.clone()),
    )
    .with_dependency_values(dependencies)
    .mount_with_children(
        child(|_| LayerFeature::<Level2>(PhantomData)),
        move |_, first| {
            let declared = Arc::clone(&declared);
            let output = Arc::clone(&output);
            let mapped = Arc::clone(&mapped);
            let suspended = Arc::clone(&suspended);
            let started = Arc::clone(&task_started);
            first.optional_with_children(
                child(|_| LayerFeature::<Level3>(PhantomData)),
                move |_, second| {
                    let declared = Arc::clone(&declared);
                    let output = Arc::clone(&output);
                    let mapped = Arc::clone(&mapped);
                    let suspended = Arc::clone(&suspended);
                    let started = Arc::clone(&started);
                    second.optional_with_children(
                        child(|_| LayerFeature::<Level4>(PhantomData)),
                        move |_, third| {
                            let declared = Arc::clone(&declared);
                            let output = Arc::clone(&output);
                            let mapped = Arc::clone(&mapped);
                            let suspended = Arc::clone(&suspended);
                            let started = Arc::clone(&started);
                            third.optional_with_children(
                                child(|_| LayerFeature::<Leaf>(PhantomData)),
                                move |_, fourth| {
                                    let declared = Arc::clone(&declared);
                                    let configured_output = Arc::clone(&output);
                                    let mapped = Arc::clone(&mapped);
                                    let output = Arc::clone(&output);
                                    let suspended = Arc::clone(&suspended);
                                    let started = Arc::clone(&started);
                                    fourth.optional_with_children(
                                        child(move |_| Plugin {
                                            output: Arc::clone(&output),
                                            suspended: Arc::clone(&suspended),
                                            started: Arc::clone(&started),
                                        }),
                                        move |_, fifth| {
                                            let site = fifth.site().clone();
                                            let output = fifth.output(&site, |handlers| {
                                                handlers.at_root(|root: &mut Root, value, _| {
                                                    root.outcomes.push(value);
                                                    OutputFlow::Consumed
                                                });
                                            });
                                            *configured_output.lock().unwrap() = Some(output);
                                            let mapped = Arc::clone(&mapped);
                                            let command =
                                                fifth.command(move |input: PrivateInput| {
                                                    assert_eq!(input.0.get(), 1);
                                                    mapped.fetch_add(1, Ordering::SeqCst);
                                                    LeafAction
                                                });
                                            declared.lock().unwrap().push((site, command));
                                        },
                                    );
                                },
                            );
                        },
                    );
                },
            );
        },
    );
    let store = builder.build().unwrap();
    driver.finish(store.ready()).unwrap();
    let (leaf_site, command_site) = sites.lock().unwrap()[0].clone();
    let port = driver
        .finish(store.command_port(&command_site))
        .unwrap()
        .unwrap();
    for _ in 0..INPUT_COUNT - 1 {
        drop(
            driver
                .finish(port.send(PrivateInput(Box::new(Cell::new(1)))))
                .unwrap(),
        );
    }
    let last = driver
        .finish(port.send(PrivateInput(Box::new(Cell::new(1)))))
        .unwrap();
    driver.turn();
    assert_eq!(started.load(Ordering::SeqCst), 1);
    assert_eq!(maps.load(Ordering::SeqCst), INPUT_COUNT);
    assert_eq!(
        driver.finish(store.read(counts)).unwrap(),
        [0, 0, 0, 0, 0, INPUT_COUNT]
    );
    let leaf = driver.finish(store.acquire(&leaf_site)).unwrap().unwrap();
    drop(
        driver
            .finish(leaf.update(|state| state.current_client = Arc::new(Backend(3))))
            .unwrap(),
    );
    release.send(()).unwrap();
    driver.finish(last.finish());
    assert_eq!(
        driver
            .finish(store.read(|root| {
                root.outcomes
                    .iter()
                    .map(|outcome| {
                        (
                            outcome.inputs,
                            outcome.captured_client,
                            outcome.current_client,
                        )
                    })
                    .collect::<Vec<_>>()
            }))
            .unwrap(),
        [(INPUT_COUNT, 2, 3)]
    );
    assert_eq!(
        driver.finish(store.read(counts)).unwrap(),
        [0, 0, 0, 0, 0, INPUT_COUNT]
    );
    let ancestor = driver.finish(store.acquire(&level_one)).unwrap().unwrap();
    drop(
        driver
            .finish(ancestor.update(|one| {
                one.replacement += 1;
                one.child = Some(subtree());
            }))
            .unwrap(),
    );
    match driver.finish(port.send(PrivateInput(Box::new(Cell::new(7))))) {
        Err(TaskSendError::Rejected { action, error }) => {
            assert_eq!(error.reason(), TaskAccessReason::ScopeEnded);
            assert_eq!(action.0.get(), 7);
        }
        _ => panic!("old depth-five port must not retarget the equal-ID replacement"),
    }
    assert_eq!(maps.load(Ordering::SeqCst), INPUT_COUNT);
    assert_eq!(sites.lock().unwrap().len(), 2);
    let fresh_site = sites.lock().unwrap()[1].1.clone();
    let fresh = driver
        .finish(store.command_port(&fresh_site))
        .unwrap()
        .unwrap();
    drop(
        driver
            .finish(fresh.send(PrivateInput(Box::new(Cell::new(1)))))
            .unwrap(),
    );
    assert_eq!(
        driver.finish(store.read(counts)).unwrap(),
        [0, 0, 0, 0, 0, 1]
    );
    driver.finish(store.shutdown()).unwrap();
}
