//! Explicit provider captures and current reads through public mounted Features.

use std::convert::Infallible;
use std::pin::Pin;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

use futures_channel::oneshot;
use futures_executor::block_on;
use rca_core::{
    DependencyKey, DependencyValues, Feature, FeatureContext, MonotonicInstant, MountIdentity,
    OptionalMount, RootMount, Runtime, RuntimeDriver, RuntimeFuture, SharedKeyCache,
    SharedKeyIdentity, SharedLoadContext, SharedReader, SharedReaderKey, SharedSubscribeContext,
    SharedSubscriber, Task, TaskAccessReason, TaskProgress,
};

/// Manually polls a frozen-clock actor; yielding still suspends for one turn.
#[derive(Clone, Default)]
struct Driver(Arc<Mutex<Vec<RuntimeFuture>>>);

impl std::fmt::Debug for Driver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ProviderDriver")
    }
}

impl RuntimeDriver for Driver {
    fn spawn(&self, task: RuntimeFuture) {
        self.0.lock().unwrap().push(task);
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
        let pending = std::mem::take(&mut *self.0.lock().unwrap());
        for mut future in pending {
            if poll(future.as_mut()).is_pending() {
                self.0.lock().unwrap().push(future);
            }
        }
    }

    /// Bounds progress for a finite operation rather than waiting on wall-clock time.
    fn finish<F: Future>(&self, future: F) -> F::Output {
        let mut future = Box::pin(future);
        for _ in 0..16 {
            if let Poll::Ready(value) = poll(future.as_mut()) {
                return value;
            }
            self.turn();
        }
        panic!("finite actor operation did not finish");
    }
}

/// Polls once without driving queued actors or advancing the frozen clock.
fn poll<F: Future + ?Sized>(future: Pin<&mut F>) -> Poll<F::Output> {
    future.poll(&mut Context::from_waker(Waker::noop()))
}

/// Retains the installed producer and counts subscription lifetime independently.
struct Backend {
    value: AtomicU32,
    subscriber: Mutex<Option<SharedSubscriber<u32>>>,
    subscriptions: AtomicUsize,
    unsubscriptions: AtomicUsize,
}

#[derive(Clone)]
struct Provider(Arc<Backend>);

/// Removes the producer when the reader's final subscription owner goes away.
struct Subscription(Arc<Backend>);

impl Drop for Subscription {
    fn drop(&mut self) {
        self.0.subscriber.lock().unwrap().take();
        self.0.unsubscriptions.fetch_add(1, Ordering::SeqCst);
    }
}

impl SharedReaderKey for Provider {
    type Value = u32;
    type Error = Infallible;
    type Subscription = Subscription;

    fn identity(&self) -> SharedKeyIdentity {
        SharedKeyIdentity::new::<u32>("feature-provider", "v1")
    }

    fn load(
        &self,
        _: SharedLoadContext<'_, u32>,
    ) -> impl Future<Output = Result<Option<u32>, Infallible>> + Send {
        std::future::ready(Ok(Some(self.0.value.load(Ordering::SeqCst))))
    }

    fn subscribe(
        &self,
        _: SharedSubscribeContext,
        subscriber: SharedSubscriber<u32>,
    ) -> Result<Subscription, Infallible> {
        self.0.subscriptions.fetch_add(1, Ordering::SeqCst);
        assert!(
            self.0
                .subscriber
                .lock()
                .unwrap()
                .replace(subscriber)
                .is_none()
        );
        Ok(Subscription(Arc::clone(&self.0)))
    }
}

/// Loads the same logical key in a fresh cache so independent fixtures cannot alias.
fn provider(value: u32) -> (Provider, SharedReader<u32>) {
    let provider = Provider(Arc::new(Backend {
        value: AtomicU32::new(value),
        subscriber: Mutex::new(None),
        subscriptions: AtomicUsize::new(0),
        unsubscriptions: AtomicUsize::new(0),
    }));
    // One cache namespace per fresh fixture; the cache retains only weak owners.
    let reader = block_on(SharedKeyCache::new().reader(provider.clone(), || 0)).unwrap();
    (provider, reader)
}

#[test]
fn duplicate_provider_subscription_panics_without_poisoning_original_cleanup() {
    let (key, reader) = provider(3);
    let producer = key.0.subscriber.lock().unwrap().clone().unwrap();
    let registration = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        block_on(SharedKeyCache::new().reader(key.clone(), || 0)).unwrap()
    }));
    // Catch disposal separately: a poisoned fixture lock must fail this test,
    // not turn the expected registration panic into a double-panic abort.
    let cleanup = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(reader)));

    let panic = registration.expect_err("the backend permits only one reader cohort");
    assert_eq!(
        panic.downcast_ref::<&str>().copied(),
        Some("assertion failed: self.0.subscriber.lock().unwrap().replace(subscriber).is_none()")
    );
    assert!(
        cleanup.is_ok(),
        "original subscription cleanup must not panic"
    );
    assert_eq!(key.0.subscriptions.load(Ordering::SeqCst), 2);
    assert_eq!(key.0.unsubscriptions.load(Ordering::SeqCst), 1);
    assert!(key.0.subscriber.lock().unwrap().is_none());
    assert!(!producer.yield_value(99).unwrap());
    assert_eq!(Arc::strong_count(&key.0), 1);
}

struct Client;
impl DependencyKey for Client {
    type Value = SharedReader<u32>;
}

/// Freezes one reader into a dependency container before state or feature construction.
fn prepared(reader: SharedReader<u32>) -> DependencyValues {
    let mut values = DependencyValues::new();
    values
        .prepare(|values| values.set::<Client>(reader))
        .unwrap();
    values
}

struct DropCount(Arc<AtomicUsize>);
impl Drop for DropCount {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

/// Keeps the state-time capture separate from the parent-owned current reader.
struct ChildState {
    captured: SharedReader<u32>,
    parent: Arc<Mutex<SharedReader<u32>>>,
    results: Vec<(u32, u32, u32)>,
}

struct RootState {
    child: Option<ChildState>,
    parent: Arc<Mutex<SharedReader<u32>>>,
    replacement: u64,
}

enum RootAction {
    Current(SharedReader<u32>),
    Replace,
}

enum ChildAction {
    Wait,
}

struct Parent;
impl Feature for Parent {
    type State = RootState;
    type Action = RootAction;

    fn reduce(
        &mut self,
        state: &mut RootState,
        action: &RootAction,
        _: &mut FeatureContext<'_, RootState, RootAction>,
    ) {
        match action {
            RootAction::Current(reader) => *state.parent.lock().unwrap() = reader.clone(),
            RootAction::Replace => state.replacement += 1,
        }
    }
}

struct Child {
    _captured: SharedReader<u32>,
    _owner: DropCount,
    pending: Arc<Mutex<Option<oneshot::Receiver<()>>>>,
    task_drops: Arc<AtomicUsize>,
    task_polls: Arc<AtomicUsize>,
}

impl Feature for Child {
    type State = ChildState;
    type Action = ChildAction;

    fn reduce(
        &mut self,
        _: &mut ChildState,
        _: &ChildAction,
        context: &mut FeatureContext<'_, ChildState, ChildAction>,
    ) {
        let pending = self.pending.lock().unwrap().take().unwrap();
        let capture = DropCount(Arc::clone(&self.task_drops));
        let polls = Arc::clone(&self.task_polls);
        context.task(Task::run(move |_| async move {
            let _capture = capture;
            polls.fetch_add(1, Ordering::SeqCst);
            let _ = pending.await;
        }));
    }
}

// Catches capture refresh, stale current reads, early task polling, result
// retargeting, absent child cancellation and retained subscription ownership.
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "One dependency handover sequence preserves initial-task capture, current reads and stale-result rejection"
)]
fn mounted_initial_task_keeps_a_and_b_reads_current_c_and_rejects_old_results() {
    let driver = Driver::default();
    let trace = Arc::new(Mutex::new(Vec::new()));
    let (a, a_reader) = provider(3);
    let (b, b_reader) = provider(7);
    let (c, c_reader) = provider(11);
    let stale_producers = [&a, &b, &c].map(|key| key.0.subscriber.lock().unwrap().clone().unwrap());
    let mut state_values = prepared(a_reader);
    trace.lock().unwrap().push("prepare A");
    let parent_data = Arc::new(Mutex::new(state_values.get_or_insert::<Client>().clone()));
    let state = RootState {
        child: Some(ChildState {
            captured: state_values.get_or_insert::<Client>().clone(),
            parent: Arc::clone(&parent_data),
            results: Vec::new(),
        }),
        parent: parent_data,
        replacement: 0,
    };
    trace.lock().unwrap().push("State");
    drop(state_values);
    let values = prepared(b_reader);
    trace.lock().unwrap().push("prepare B");
    let (release, suspended) = oneshot::channel();
    let mut suspended = Some(suspended);
    let (task_release, task_pending) = oneshot::channel();
    let pending = Arc::new(Mutex::new(Some(task_pending)));
    let feature_drops = Arc::new(AtomicUsize::new(0));
    let task_drops = Arc::new(AtomicUsize::new(0));
    let task_polls = Arc::new(AtomicUsize::new(0));
    let body_polls = Arc::new(AtomicUsize::new(0));
    let completion = Arc::new(AtomicUsize::new(0));
    let make_trace = Arc::clone(&trace);
    let make_drops = Arc::clone(&feature_drops);
    let make_pending = Arc::clone(&pending);
    let make_task_drops = Arc::clone(&task_drops);
    let make_task_polls = Arc::clone(&task_polls);
    let factory_trace = Arc::clone(&trace);
    let body = Arc::clone(&body_polls);
    let completed = Arc::clone(&completion);
    let mount = OptionalMount::new(
        |root: &RootState| root.child.as_ref(),
        |root: &mut RootState| root.child.as_mut(),
        |root: &RootState| {
            root.child.as_ref().map(|_| MountIdentity {
                key: (),
                replacement: root.replacement,
                configuration: 0,
            })
        },
        move |state: &ChildState| {
            assert_eq!(state.captured.read(|value| *value).unwrap(), 3);
            make_trace.lock().unwrap().push("Feature");
            Child {
                _captured: state.captured.clone(),
                _owner: DropCount(Arc::clone(&make_drops)),
                pending: Arc::clone(&make_pending),
                task_drops: Arc::clone(&make_task_drops),
                task_polls: Arc::clone(&make_task_polls),
            }
        },
    )
    .on_initial(
        |state| state.captured.clone(),
        move |change, mut capture| {
            let rca_core::ReactionChange::Initial { current: a } = change else {
                panic!("Initial hook only");
            };
            let b = capture.dependency::<Client>().clone();
            factory_trace.lock().unwrap().push("Initial");
            let release = suspended.take();
            let body = Arc::clone(&body);
            let completed = Arc::clone(&completed);
            Task::<ChildState, ChildAction>::run(move |context| async move {
                let Some(release) = release else {
                    return;
                };
                body.fetch_add(1, Ordering::SeqCst);
                release.await.unwrap();
                let current = context
                    .read(|state| state.parent.lock().unwrap().clone())
                    .await
                    .unwrap();
                let result = (
                    a.read(|v| *v).unwrap(),
                    b.read(|v| *v).unwrap(),
                    current.read(|v| *v).unwrap(),
                );
                let _ = context
                    .update(move |state| state.results.push(result))
                    .await
                    .unwrap();
                completed.fetch_add(1, Ordering::SeqCst);
            })
        },
    );
    let (builder, site) = RootMount::new(Parent)
        .builder(state, Runtime::new(driver.clone()))
        .with_dependency_values(values)
        .mount_with_site(mount);
    let store = builder.build().unwrap();
    assert_eq!(*trace.lock().unwrap(), ["prepare A", "State", "prepare B"]);
    driver.finish(store.ready()).unwrap();
    assert_eq!(
        *trace.lock().unwrap(),
        ["prepare A", "State", "prepare B", "Feature", "Initial"]
    );
    assert_eq!(body_polls.load(Ordering::SeqCst), 0);
    driver.turn();
    assert_eq!(body_polls.load(Ordering::SeqCst), 1);
    assert_eq!(completion.load(Ordering::SeqCst), 0);
    let child = driver.finish(store.acquire(&site)).unwrap().unwrap();
    let _ = driver
        .finish(store.send(RootAction::Current(c_reader)))
        .unwrap();
    assert_eq!(completion.load(Ordering::SeqCst), 0);
    release.send(()).unwrap();
    for _ in 0..8 {
        driver.turn();
    }
    assert_eq!(completion.load(Ordering::SeqCst), 1);
    assert_eq!(
        driver
            .finish(child.read(|state| state.results.clone()))
            .unwrap(),
        [(3, 7, 11)]
    );

    let _ = driver.finish(child.send(ChildAction::Wait)).unwrap();
    driver.turn();
    assert_eq!(task_polls.load(Ordering::SeqCst), 1);
    assert_eq!(task_drops.load(Ordering::SeqCst), 0);
    let (result_ready, result_pending) = oneshot::channel();
    let writes = Arc::new(AtomicUsize::new(0));
    let result_writes = Arc::clone(&writes);
    let old_context = child.clone();
    let mut result = Box::pin(async move {
        let value = result_pending.await.unwrap();
        old_context
            .update(move |state| {
                result_writes.fetch_add(1, Ordering::SeqCst);
                state.results.push(value);
            })
            .await
    });
    assert!(poll(result.as_mut()).is_pending());
    let _ = driver.finish(store.send(RootAction::Replace)).unwrap();
    for _ in 0..4 {
        driver.turn();
    }
    assert_eq!(feature_drops.load(Ordering::SeqCst), 1);
    assert_eq!(task_drops.load(Ordering::SeqCst), 1);
    assert!(task_release.send(()).is_err());
    result_ready.send((99, 99, 99)).unwrap();
    let error = driver.finish(result).unwrap_err();
    assert_eq!(error.reason(), TaskAccessReason::ScopeEnded);
    assert_eq!(error.progress(), TaskProgress::NotStarted);
    assert_eq!(writes.load(Ordering::SeqCst), 0);
    let replacement = driver.finish(store.acquire(&site)).unwrap().unwrap();
    assert_eq!(
        driver
            .finish(replacement.read(|state| state.results.clone()))
            .unwrap(),
        [(3, 7, 11)]
    );

    // Retained sites, contexts and producer callbacks cannot own the actor or readers.
    drop(store);
    for _ in 0..4 {
        driver.turn();
    }
    assert_eq!(feature_drops.load(Ordering::SeqCst), 2);
    assert_eq!(task_drops.load(Ordering::SeqCst), 1);
    for (key, producer) in [&a, &b, &c].into_iter().zip(stale_producers) {
        assert_eq!(key.0.subscriptions.load(Ordering::SeqCst), 1);
        assert_eq!(key.0.unsubscriptions.load(Ordering::SeqCst), 1);
        assert!(key.0.subscriber.lock().unwrap().is_none());
        assert!(!producer.yield_value(99).unwrap());
    }
}

struct ReaderFeature;
impl Feature for ReaderFeature {
    type State = SharedReader<u32>;
    type Action = ();

    fn reduce(
        &mut self,
        _: &mut Self::State,
        (): &(),
        _: &mut FeatureContext<'_, Self::State, ()>,
    ) {
    }
}

// Catches cloned dependency containers being misrepresented as fresh providers.
#[test]
fn public_features_from_cloned_containers_share_but_fresh_fixtures_do_not() {
    let driver = Driver::default();
    let (first, first_reader) = provider(3);
    let (fresh, fresh_reader) = provider(3);
    assert!(!Arc::ptr_eq(&first.0, &fresh.0));
    let first_values = prepared(first_reader);
    let shared_values = first_values.clone();
    let fresh_values = prepared(fresh_reader);
    let build = |mut values: DependencyValues| {
        let state = values.get_or_insert::<Client>().clone();
        RootMount::new(ReaderFeature)
            .builder(state, Runtime::new(driver.clone()))
            .with_dependency_values(values)
            .build()
            .unwrap()
    };
    let first_store = build(first_values);
    let shared_store = build(shared_values);
    let fresh_store = build(fresh_values);
    for store in [&first_store, &shared_store, &fresh_store] {
        driver.finish(store.ready()).unwrap();
    }
    let producer = first.0.subscriber.lock().unwrap().clone().unwrap();
    assert!(producer.yield_value(7).unwrap());
    assert_eq!(
        driver
            .finish(first_store.read(|state| state.read(|value| *value).unwrap()))
            .unwrap(),
        7
    );
    assert_eq!(
        driver
            .finish(shared_store.read(|state| state.read(|value| *value).unwrap()))
            .unwrap(),
        7
    );
    assert_eq!(
        driver
            .finish(fresh_store.read(|state| state.read(|value| *value).unwrap()))
            .unwrap(),
        3
    );
    drop(first_store);
    driver.turn();
    assert_eq!(first.0.unsubscriptions.load(Ordering::SeqCst), 0);
    drop(shared_store);
    drop(fresh_store);
    for _ in 0..4 {
        driver.turn();
    }
    assert_eq!(first.0.unsubscriptions.load(Ordering::SeqCst), 1);
    assert_eq!(fresh.0.unsubscriptions.load(Ordering::SeqCst), 1);
    assert!(!producer.yield_value(99).unwrap());
}
