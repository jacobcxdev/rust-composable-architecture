//! Checks timer ordering, cancellation, fairness and cleanup with controlled time.

use std::collections::HashMap;
use std::fmt;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::task::{Context, Poll, Wake, Waker};
use std::thread::JoinHandle;
use std::time::Duration;

use rca_core::{
    ClockDriver as _, Effect, MonotonicInstant, ReduceContext, Reducer, Runtime, RuntimeDriver,
    RuntimeFuture, Store,
};

/// Shares a controlled clock while each spawned task runs on its own joined thread.
#[derive(Clone)]
struct VirtualDriver {
    /// Retains the clock, registered sleepers and task handles.
    shared: Arc<VirtualShared>,
}

/// Owns thread cleanup; driver clones cannot join or stop another owner's tasks.
struct VirtualRuntime {
    /// Remains alive until every captured driver and sleep releases it.
    shared: Arc<VirtualShared>,
}

/// Separates clock access, sleeper registration and task ownership into short locks.
struct VirtualShared {
    /// Advances only through explicit test commands.
    now: Mutex<MonotonicInstant>,
    /// Retains each registered sleep until its future unregisters.
    sleepers: Mutex<HashMap<u64, Sleeper>>,
    /// Holds join handles until the owner takes and joins each batch.
    handles: Mutex<Vec<JoinHandle<()>>>,
    /// Assigns distinct identities to the finite sleeps in each test.
    next_sleep: AtomicU64,
    /// Counts wait construction independently of poll and registration.
    sleep_calls: AtomicUsize,
    /// Records peak simultaneous registrations, including already-notified waits.
    max_active_sleeps: AtomicUsize,
}

/// Stores one registered deadline and, until notification, its owned waker.
struct Sleeper {
    /// Selects this entry when the clock reaches its requested time.
    deadline: MonotonicInstant,
    /// Becomes empty after waking, while the registration remains counted.
    waker: Option<Waker>,
}

/// Registers lazily and removes its exact entry on readiness or cancellation.
struct VirtualSleep {
    /// Keeps the registry alive without retaining its runtime owner.
    shared: Arc<VirtualShared>,
    /// Prevents one sleep's removal from affecting another.
    id: u64,
    /// Remains fixed for this future's lifetime.
    deadline: MonotonicInstant,
    /// Distinguishes an unpolled future from a live registry entry.
    registered: bool,
}

impl fmt::Debug for VirtualDriver {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("VirtualDriver")
            .field("sleep_calls", &self.shared.sleep_calls)
            .finish_non_exhaustive()
    }
}

impl RuntimeDriver for VirtualDriver {
    fn spawn(&self, task: RuntimeFuture) {
        self.shared
            .handles
            .lock()
            .expect("runtime handle lock")
            .push(std::thread::spawn(move || futures_executor::block_on(task)));
    }

    fn now(&self) -> MonotonicInstant {
        *self.shared.now.lock().expect("virtual clock lock")
    }

    fn sleep_until(&self, deadline: MonotonicInstant) -> RuntimeFuture {
        self.shared.sleep_calls.fetch_add(1, Ordering::Relaxed);
        let id = self.shared.next_sleep.fetch_add(1, Ordering::Relaxed);
        Box::pin(VirtualSleep {
            shared: Arc::clone(&self.shared),
            id,
            deadline,
            registered: false,
        })
    }

    fn yield_now(&self) -> RuntimeFuture {
        let mut yielded = false;
        Box::pin(futures_util::future::poll_fn(move |context| {
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

impl Future for VirtualSleep {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if *self.shared.now.lock().expect("virtual clock lock") >= self.deadline {
            self.unregister();
            return Poll::Ready(());
        }

        self.register(cx)
    }
}

impl VirtualSleep {
    /// Registers after the initial clock check, allowing a concurrent advance.
    fn register(&mut self, cx: &Context<'_>) -> Poll<()> {
        let was_registered = self.registered;
        let waker = cx.waker().clone();
        let previous = {
            let mut sleepers = self.shared.sleepers.lock().expect("sleeper lock");
            let previous = sleepers.insert(
                self.id,
                Sleeper {
                    deadline: self.deadline,
                    waker: Some(waker),
                },
            );
            if !was_registered {
                let active = sleepers.len();
                self.shared
                    .max_active_sleeps
                    .fetch_max(active, Ordering::Relaxed);
            }
            previous
        };
        self.registered = true;
        drop(previous);
        if *self.shared.now.lock().expect("virtual clock lock") >= self.deadline {
            self.unregister();
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    }

    /// Removes the entry before releasing its waker outside the registry lock.
    fn unregister(&mut self) {
        if self.registered {
            let previous = self
                .shared
                .sleepers
                .lock()
                .expect("sleeper lock")
                .remove(&self.id);
            self.registered = false;
            drop(previous);
        }
    }
}

impl Drop for VirtualSleep {
    fn drop(&mut self) {
        self.unregister();
    }
}

impl VirtualRuntime {
    /// Creates an independent clock and empty task/sleeper registries.
    fn new() -> Self {
        Self {
            shared: Arc::new(VirtualShared {
                now: Mutex::new(MonotonicInstant::ZERO),
                sleepers: Mutex::new(HashMap::new()),
                handles: Mutex::new(Vec::new()),
                next_sleep: AtomicU64::new(0),
                sleep_calls: AtomicUsize::new(0),
                max_active_sleeps: AtomicUsize::new(0),
            }),
        }
    }

    /// Gives a store a shared driver without transferring responsibility for joins.
    fn runtime(&self) -> Runtime {
        Runtime::new(VirtualDriver {
            shared: Arc::clone(&self.shared),
        })
    }

    /// Waits a finite number of scheduler turns for an actual sleep registration.
    fn wait_for_sleep(&self) {
        for _ in 0..100_000 {
            if self.shared.sleep_calls.load(Ordering::Acquire) != 0 && self.active_sleeps() != 0 {
                return;
            }
            std::thread::yield_now();
        }
        panic!("runtime never registered timer sleep");
    }

    /// Waits until the driver constructs the requested number of sleeps.
    fn wait_for_sleep_calls(&self, expected: usize) {
        Self::wait_until(|| self.sleep_calls() >= expected);
    }

    /// Reads total wait constructions, including waits already destroyed.
    fn sleep_calls(&self) -> usize {
        self.shared.sleep_calls.load(Ordering::Acquire)
    }

    /// Reads the peak registration count rather than a wall-time measurement.
    fn max_active_sleeps(&self) -> usize {
        self.shared.max_active_sleeps.load(Ordering::Acquire)
    }

    /// Counts registrations until their waits complete or are destroyed.
    fn active_sleeps(&self) -> usize {
        self.shared.sleepers.lock().expect("sleeper lock").len()
    }

    /// Bounds scheduler retries; this is a progress watchdog, not a speed threshold.
    fn wait_until(mut condition: impl FnMut() -> bool) {
        for _ in 0..100_000 {
            if condition() {
                return;
            }
            std::thread::yield_now();
        }
        panic!("virtual runtime condition did not become true");
    }

    /// Moves test time without notification for explicit actor-poll schedules.
    fn set_now(&self, now: MonotonicInstant) {
        *self.shared.now.lock().expect("virtual clock lock") = now;
    }

    /// Takes due notifications under lock, then consumes the wakers outside it.
    fn wake_due(&self) {
        let now = *self.shared.now.lock().expect("virtual clock lock");
        let wakers: Vec<_> = self
            .shared
            .sleepers
            .lock()
            .expect("sleeper lock")
            .values_mut()
            .filter(|sleeper| sleeper.deadline <= now)
            .filter_map(|sleeper| sleeper.waker.take())
            .collect();
        for waker in wakers {
            waker.wake();
        }
    }

    /// Advances time and notifies every registered wait whose deadline is due.
    fn advance_to(&self, now: MonotonicInstant) {
        self.set_now(now);
        self.wake_due();
    }

    /// Joins each batch without holding the registry lock while tasks finish.
    fn join_all(&self) {
        loop {
            let handles =
                std::mem::take(&mut *self.shared.handles.lock().expect("runtime handle lock"));
            if handles.is_empty() {
                return;
            }
            for handle in handles {
                handle.join().expect("runtime task must finish cleanly");
            }
        }
    }
}

impl Drop for VirtualRuntime {
    fn drop(&mut self) {
        self.join_all();
    }
}

/// Checks the sleeper lock during final waker destruction without blocking.
struct SleeperWakerDrop {
    /// Avoids a cycle between the stored waker and its registry.
    shared: Weak<VirtualShared>,
    /// Distinguishes one callback from a callback that never ran.
    drops: Arc<AtomicUsize>,
    /// Records lock availability without asserting or blocking in destruction.
    unlocked: Arc<AtomicBool>,
}

#[expect(
    clippy::manual_noop_waker,
    reason = "the waker's destructor records whether the sleeper lock is available"
)]
impl Wake for SleeperWakerDrop {
    fn wake(self: Arc<Self>) {}
}

impl Drop for SleeperWakerDrop {
    fn drop(&mut self) {
        let shared = self.shared.upgrade().expect("clock remains alive");
        let unlocked = shared.sleepers.try_lock().is_ok();
        self.unlocked.store(unlocked, Ordering::SeqCst);
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}

/// Exercises either stored-waker replacement or cancellation with a live clock.
fn assert_sleep_waker_dropped_unlocked(replace: bool) {
    let clock = VirtualRuntime::new();
    let mut wait = clock
        .runtime()
        .sleep_until(MonotonicInstant::from_elapsed(Duration::from_secs(1)));
    let drops = Arc::new(AtomicUsize::new(0));
    let unlocked = Arc::new(AtomicBool::new(false));
    let waker = Waker::from(Arc::new(SleeperWakerDrop {
        shared: Arc::downgrade(&clock.shared),
        drops: Arc::clone(&drops),
        unlocked: Arc::clone(&unlocked),
    }));
    assert!(
        wait.as_mut()
            .poll(&mut Context::from_waker(&waker))
            .is_pending()
    );
    drop(waker);

    if replace {
        assert!(
            wait.as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending()
        );
    }
    drop(wait);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    assert!(
        unlocked.load(Ordering::SeqCst),
        "stored waker must be dropped outside the sleeper lock"
    );
    assert_eq!(clock.active_sleeps(), 0);
}

/// A repoll releases the old notification after publishing the new one.
#[test]
fn virtual_sleep_replacement_drops_old_waker_outside_lock() {
    assert_sleep_waker_dropped_unlocked(true);
}

/// Cancelling a pending sleep releases its notification outside the registry lock.
#[test]
fn virtual_sleep_cancellation_drops_old_waker_outside_lock() {
    assert_sleep_waker_dropped_unlocked(false);
}

/// A clock advance between the initial check and registration cannot lose its wake.
#[test]
fn virtual_sleep_registration_rechecks_a_concurrent_clock_advance() {
    /// Records notifications without running the waiting future.
    #[derive(Default)]
    struct Notification(AtomicUsize);

    impl Wake for Notification {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    let clock = VirtualRuntime::new();
    let deadline = MonotonicInstant::from_elapsed(Duration::from_secs(1));
    let mut wait = VirtualSleep {
        shared: Arc::clone(&clock.shared),
        id: 0,
        deadline,
        registered: false,
    };
    let notification = Arc::new(Notification::default());
    let waker = Waker::from(Arc::clone(&notification));

    // Reproduce the two real poll phases with an advance in their unlocked gap.
    assert!(*clock.shared.now.lock().unwrap() < deadline);
    clock.advance_to(deadline);
    let result = wait.register(&Context::from_waker(&waker));
    let notified = notification.0.load(Ordering::SeqCst);
    drop(wait);
    assert_eq!(clock.active_sleeps(), 0);
    assert!(
        result.is_ready() || notified != 0,
        "elapsed sleep must complete or receive a wake after registration"
    );
}

/// Due delivery transfers the stored waker without cloning it under the registry lock.
#[test]
fn virtual_sleep_due_wake_transfers_its_stored_waker() {
    /// Records wake ownership and nonblocking access to the sleeper registry.
    struct Notification {
        /// Allows a nonblocking lock check without retaining the registry.
        shared: Weak<VirtualShared>,
        /// Captures the number of owners at the actual wake callback.
        owners: Arc<AtomicUsize>,
        /// Records whether the callback runs outside the registry lock.
        unlocked: Arc<AtomicBool>,
    }

    impl Wake for Notification {
        fn wake(self: Arc<Self>) {
            self.owners
                .store(Arc::strong_count(&self), Ordering::SeqCst);
            let shared = self.shared.upgrade().expect("clock remains alive");
            self.unlocked
                .store(shared.sleepers.try_lock().is_ok(), Ordering::SeqCst);
        }
    }

    let clock = VirtualRuntime::new();
    let deadline = MonotonicInstant::from_elapsed(Duration::from_secs(1));
    let mut wait = clock.runtime().sleep_until(deadline);
    let owners = Arc::new(AtomicUsize::new(0));
    let unlocked = Arc::new(AtomicBool::new(false));
    let waker = Waker::from(Arc::new(Notification {
        shared: Arc::downgrade(&clock.shared),
        owners: Arc::clone(&owners),
        unlocked: Arc::clone(&unlocked),
    }));
    assert!(
        wait.as_mut()
            .poll(&mut Context::from_waker(&waker))
            .is_pending()
    );
    drop(waker);

    clock.advance_to(deadline);
    drop(wait);
    assert!(unlocked.load(Ordering::SeqCst));
    assert_eq!(
        owners.load(Ordering::SeqCst),
        1,
        "due wake must transfer its sole stored owner instead of cloning under the lock"
    );
    assert_eq!(clock.active_sleeps(), 0);
}

/// Cancelling the first due action rejects its siblings and its returned timer.
#[test]
fn timer_lineage_cancellation_rejects_due_siblings_and_returned_timer() {
    use futures_util::FutureExt as _;

    for cancel in [false, true] {
        let clock = VirtualRuntime::new();
        let queued = Arc::new(Mutex::new(Vec::new()));
        let root_slot = Arc::new(Mutex::new(None::<rca_core::ActionTask>));
        let reducer_root = Arc::clone(&root_slot);
        let calls = Arc::new(AtomicUsize::new(0));
        let callback_calls = Arc::clone(&calls);
        let reducer = rca_core::Reduce::new(move |state: &mut Vec<u8>, action: &u8, _| {
            state.push(*action);
            match action {
                0 => {
                    let calls = Arc::clone(&callback_calls);
                    Effect::merge([
                        Effect::send_after(Duration::from_secs(1), 1),
                        Effect::send_after(Duration::ZERO, 2).delay(Duration::from_secs(1)),
                        Effect::send_after(Duration::from_secs(1), 3),
                        Effect::interval(Duration::from_secs(1), move || {
                            calls.fetch_add(1, Ordering::SeqCst);
                            4
                        }),
                    ])
                }
                1 => {
                    if cancel {
                        reducer_root.lock().unwrap().as_ref().unwrap().cancel();
                    }
                    Effect::send_after(Duration::from_secs(1), 5)
                }
                _ => Effect::none(),
            }
        });
        let store = Store::new(
            Vec::new(),
            reducer,
            Runtime::new(TimerConstructionDriver {
                clock: VirtualDriver {
                    shared: Arc::clone(&clock.shared),
                },
                queued: Arc::clone(&queued),
                hook: Arc::new(Mutex::new(None)),
                wait_drops: Arc::new(AtomicUsize::new(0)),
            }),
        );
        let mut actor = queued.lock().unwrap().pop().expect("one actor");
        let mut cx = Context::from_waker(Waker::noop());
        let receipt = store.try_send(0).unwrap();
        assert!(actor.as_mut().poll(&mut cx).is_pending());
        let root = receipt.finish().now_or_never().unwrap().unwrap();
        *root_slot.lock().unwrap() = Some(root.clone());

        for second in [1, 2] {
            clock.set_now(MonotonicInstant::from_elapsed(Duration::from_secs(second)));
            // Two due batches and their cooperative yields fit within this finite drive.
            for _ in 0..8 {
                assert!(actor.as_mut().poll(&mut cx).is_pending());
            }
        }
        let mut state = {
            let mut read = std::pin::pin!(store.read(Clone::clone));
            assert!(read.as_mut().poll(&mut cx).is_pending());
            assert!(actor.as_mut().poll(&mut cx).is_pending());
            let Poll::Ready(Ok(state)) = read.as_mut().poll(&mut cx) else {
                panic!("actor must answer admitted read");
            };
            state
        };
        if cancel {
            assert_eq!(state, [0, 1]);
            assert_eq!(calls.load(Ordering::SeqCst), 0);
            assert!(root.is_finished());
        } else {
            state.sort_unstable();
            assert_eq!(state, [0, 1, 2, 3, 4, 4, 5]);
            assert_eq!(calls.load(Ordering::SeqCst), 2);
            assert!(!root.is_finished());
            root.cancel();
            assert!(actor.as_mut().poll(&mut cx).is_pending());
        }
        root.finish()
            .now_or_never()
            .expect("all causal work retired");
        assert!(queued.lock().unwrap().is_empty());
        assert_eq!(clock.active_sleeps(), 0);
        let mut closed = std::pin::pin!(store.closed());
        drop(store);
        assert!(actor.as_mut().poll(&mut cx).is_ready());
        assert!(closed.as_mut().poll(&mut cx).is_ready());
    }
}

/// Factory-side and reducer-side cancellation both stop interval renewal immediately.
#[test]
fn interval_cancellation_inside_factory_or_reducer_retires_without_rescheduling() {
    use futures_util::FutureExt as _;

    for in_factory in [true, false] {
        let clock = VirtualRuntime::new();
        let queued = Arc::new(Mutex::new(Vec::new()));
        let root_slot = Arc::new(Mutex::new(None::<rca_core::ActionTask>));
        let callback_root = Arc::clone(&root_slot);
        let calls = Arc::new(AtomicUsize::new(0));
        let callback_calls = Arc::clone(&calls);
        let capture = Arc::new(());
        let lifetime = Arc::downgrade(&capture);
        let mut capture = Some(capture);
        let reducer = rca_core::Reduce::new(move |state: &mut usize, action: &bool, _| {
            if *action {
                *state += 1;
                Effect::cancel("interval")
            } else {
                let capture = capture.take().expect("one interval");
                let calls = Arc::clone(&callback_calls);
                let root = Arc::clone(&callback_root);
                Effect::interval(Duration::from_secs(1), move || {
                    assert_eq!(Arc::strong_count(&capture), 1);
                    calls.fetch_add(1, Ordering::SeqCst);
                    if in_factory {
                        root.lock().unwrap().as_ref().unwrap().cancel();
                    }
                    true
                })
                .cancellable("interval")
            }
        });
        let store = Store::new(
            0,
            reducer,
            Runtime::new(TimerConstructionDriver {
                clock: VirtualDriver {
                    shared: Arc::clone(&clock.shared),
                },
                queued: Arc::clone(&queued),
                hook: Arc::new(Mutex::new(None)),
                wait_drops: Arc::new(AtomicUsize::new(0)),
            }),
        );
        let mut actor = queued.lock().unwrap().pop().expect("one actor");
        let mut cx = Context::from_waker(Waker::noop());
        let receipt = store.try_send(false).unwrap();
        assert!(actor.as_mut().poll(&mut cx).is_pending());
        let root = receipt.finish().now_or_never().unwrap().unwrap();
        *root_slot.lock().unwrap() = Some(root.clone());
        clock.set_now(MonotonicInstant::from_elapsed(Duration::from_secs(1)));
        for _ in 0..4 {
            assert!(actor.as_mut().poll(&mut cx).is_pending());
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(root.is_finished());
        assert_eq!(root.is_cancelled(), in_factory);
        root.finish().now_or_never().expect("interval work retired");
        assert!(lifetime.upgrade().is_none());
        assert_eq!(clock.active_sleeps(), 0);
        clock.set_now(MonotonicInstant::from_elapsed(Duration::from_secs(5)));
        assert!(actor.as_mut().poll(&mut cx).is_pending());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let state = {
            let mut read = std::pin::pin!(store.read(|state| *state));
            assert!(read.as_mut().poll(&mut cx).is_pending());
            assert!(actor.as_mut().poll(&mut cx).is_pending());
            let Poll::Ready(Ok(state)) = read.as_mut().poll(&mut cx) else {
                panic!("actor must answer admitted read");
            };
            state
        };
        assert_eq!(state, usize::from(!in_factory));
        assert!(queued.lock().unwrap().is_empty());
        let mut closed = std::pin::pin!(store.closed());
        drop(store);
        assert!(actor.as_mut().poll(&mut cx).is_ready());
        assert!(closed.as_mut().poll(&mut cx).is_ready());
    }
}

/// Mounted intervals stop after cancellation, route retirement or configuration change.
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "keeps public mount acquisition, cancellation, retirement or configuration refresh and exact timer cleanup in one deterministic schedule"
)]
fn mounted_interval_cancellation_or_route_retirement_prevents_renewal() {
    use futures_util::FutureExt as _;
    use rca_core::{Feature, FeatureContext, MountIdentity, OptionalMount};

    /// Starts one interval, then changes state so the mount can invalidate its route.
    struct Child(Option<Effect<bool>>);

    impl Feature for Child {
        type State = usize;
        type Action = bool;

        fn update(
            &mut self,
            state: &mut usize,
            action: &bool,
            context: &mut FeatureContext<'_, usize, bool>,
        ) {
            if *action {
                *state += 1;
            } else {
                context.effect(self.0.take().expect("one mounted interval"));
            }
        }
    }

    for (in_factory, config_refresh) in [(true, false), (false, false), (false, true)] {
        let clock = VirtualRuntime::new();
        let queued = Arc::new(Mutex::new(Vec::new()));
        let task_slot = Arc::new(Mutex::new(None::<rca_core::ActionTask>));
        let callback_task = Arc::clone(&task_slot);
        let calls = Arc::new(AtomicUsize::new(0));
        let callback_calls = Arc::clone(&calls);
        let capture = Arc::new(());
        let lifetime = Arc::downgrade(&capture);
        let mut effect = Some(Effect::interval(Duration::from_secs(1), move || {
            assert_eq!(Arc::strong_count(&capture), 1);
            callback_calls.fetch_add(1, Ordering::SeqCst);
            if in_factory {
                callback_task.lock().unwrap().as_ref().unwrap().cancel();
            }
            true
        }));
        let read: fn(&Option<usize>) -> Option<&usize> = if config_refresh {
            Option::as_ref
        } else {
            |root| root.as_ref().filter(|value| **value == 0)
        };
        let write: fn(&mut Option<usize>) -> Option<&mut usize> = if config_refresh {
            Option::as_mut
        } else {
            |root| root.as_mut().filter(|value| **value == 0)
        };
        let identity: fn(&Option<usize>) -> Option<MountIdentity<()>> = if config_refresh {
            |root| {
                root.as_ref().map(|value| MountIdentity {
                    key: (),
                    replacement: 0,
                    configuration: u64::try_from(*value).unwrap(),
                })
            }
        } else {
            |root| {
                root.filter(|value| *value == 0).map(|_| MountIdentity {
                    key: (),
                    replacement: 0,
                    configuration: 0,
                })
            }
        };
        let mount = OptionalMount::new(read, write, identity, move |_| Child(effect.take()));
        let root = rca_core::Reduce::new(|_: &mut Option<usize>, (): &(), _| Effect::none());
        let (builder, site) = Store::builder(
            Some(0),
            root,
            Runtime::new(TimerConstructionDriver {
                clock: VirtualDriver {
                    shared: Arc::clone(&clock.shared),
                },
                queued: Arc::clone(&queued),
                hook: Arc::new(Mutex::new(None)),
                wait_drops: Arc::new(AtomicUsize::new(0)),
            }),
        )
        .mount_with_site(mount);
        let store = builder.build().unwrap();
        let mut actor = queued.lock().unwrap().pop().expect("one actor");
        let mut cx = Context::from_waker(Waker::noop());
        assert!(actor.as_mut().poll(&mut cx).is_pending());
        store.ready().now_or_never().unwrap().unwrap();
        let child = {
            let mut acquisition = std::pin::pin!(store.acquire(&site));
            assert!(acquisition.as_mut().poll(&mut cx).is_pending());
            assert!(actor.as_mut().poll(&mut cx).is_pending());
            acquisition.now_or_never().unwrap().unwrap().unwrap()
        };
        let task = {
            let mut start = std::pin::pin!(child.send(false));
            assert!(start.as_mut().poll(&mut cx).is_pending());
            assert!(actor.as_mut().poll(&mut cx).is_pending());
            start.now_or_never().unwrap().unwrap()
        };
        *task_slot.lock().unwrap() = Some(task.clone());
        assert!(!task.is_finished());
        assert_eq!(clock.active_sleeps(), 1);
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert!(lifetime.upgrade().is_some());

        clock.set_now(MonotonicInstant::from_elapsed(Duration::from_secs(1)));
        for _ in 0..4 {
            assert!(actor.as_mut().poll(&mut cx).is_pending());
        }
        // Retirement and configuration refresh end owned work without cancelling its parent lineage.
        assert_eq!(task.is_cancelled(), in_factory);
        assert!(task.is_finished());
        task.finish().now_or_never().expect("mounted timer retired");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(lifetime.upgrade().is_none());
        assert_eq!(Arc::strong_count(&calls), 1);
        assert_eq!(clock.active_sleeps(), 0);
        let sleep_calls = clock.sleep_calls();
        clock.set_now(MonotonicInstant::from_elapsed(Duration::from_secs(5)));
        assert!(actor.as_mut().poll(&mut cx).is_pending());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(clock.sleep_calls(), sleep_calls);
        assert!(queued.lock().unwrap().is_empty());

        if config_refresh {
            let reads = Arc::new(AtomicUsize::new(0));
            let old_reads = Arc::clone(&reads);
            let mut stale = std::pin::pin!(child.read(move |_| {
                old_reads.fetch_add(1, Ordering::SeqCst);
            }));
            assert!(stale.as_mut().poll(&mut cx).is_pending());
            assert!(actor.as_mut().poll(&mut cx).is_pending());
            let Poll::Ready(Err(error)) = stale.as_mut().poll(&mut cx) else {
                panic!("configuration refresh must retire the old child authority");
            };
            assert_eq!(error.reason(), rca_core::TaskAccessReason::ScopeEnded);
            assert_eq!(error.progress(), rca_core::TaskProgress::NotStarted);
            assert_eq!(reads.load(Ordering::SeqCst), 0);
            let fresh = {
                let mut acquisition = std::pin::pin!(store.acquire(&site));
                assert!(acquisition.as_mut().poll(&mut cx).is_pending());
                assert!(actor.as_mut().poll(&mut cx).is_pending());
                acquisition.now_or_never().unwrap().unwrap().unwrap()
            };
            let mut read = std::pin::pin!(fresh.read(|state| *state));
            assert!(read.as_mut().poll(&mut cx).is_pending());
            assert!(actor.as_mut().poll(&mut cx).is_pending());
            assert_eq!(read.now_or_never().unwrap().unwrap(), 1);
        }

        drop(child);
        let mut shutdown = std::pin::pin!(store.shutdown_into_state());
        assert!(shutdown.as_mut().poll(&mut cx).is_pending());
        assert!(actor.as_mut().poll(&mut cx).is_ready());
        assert_eq!(
            shutdown.now_or_never().unwrap().unwrap(),
            Some(usize::from(!in_factory))
        );
    }
}

/// Owner loss inside a mounted interval factory rejects its just-produced action.
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one mounted timer schedule keeps final-owner loss, rejected delivery and cleanup in one place"
)]
fn mounted_interval_factory_owner_close_stops_routed_delivery() {
    use futures_util::FutureExt as _;
    use rca_core::{Feature, FeatureContext, MountIdentity, OptionalMount};

    /// Counts child reduction separately from interval-factory entry.
    struct Child {
        /// Moves the prebuilt interval into the first start action.
        effect: Option<Effect<bool>>,
        /// Detects delivery after the factory closed the final owner.
        entered: Arc<AtomicUsize>,
    }

    impl Feature for Child {
        type State = usize;
        type Action = bool;

        fn update(
            &mut self,
            state: &mut usize,
            action: &bool,
            context: &mut FeatureContext<'_, usize, bool>,
        ) {
            if *action {
                self.entered.fetch_add(1, Ordering::SeqCst);
                *state += 1;
            } else {
                context.effect(self.effect.take().expect("one mounted interval"));
            }
        }
    }

    let clock = VirtualRuntime::new();
    let queued = Arc::new(Mutex::new(Vec::new()));
    let owner = Arc::new(Mutex::new(None::<Store<Option<usize>, ()>>));
    let factory_owner = Arc::clone(&owner);
    let entered = Arc::new(AtomicUsize::new(0));
    let child_entered = Arc::clone(&entered);
    let calls = Arc::new(AtomicUsize::new(0));
    let factory_calls = Arc::clone(&calls);
    let capture = Arc::new(());
    let lifetime = Arc::downgrade(&capture);
    let mut effect = Some(Effect::interval(Duration::from_secs(1), move || {
        let _ = &capture;
        factory_calls.fetch_add(1, Ordering::SeqCst);
        let last_owner = factory_owner
            .lock()
            .unwrap()
            .take()
            .expect("sole Store owner");
        drop(last_owner);
        true
    }));
    let mount = OptionalMount::new(
        Option::as_ref,
        Option::as_mut,
        |root: &Option<usize>| {
            root.as_ref().map(|_| MountIdentity {
                key: (),
                replacement: 0,
                configuration: 0,
            })
        },
        move |_| Child {
            effect: effect.take(),
            entered: Arc::clone(&child_entered),
        },
    );
    let root = rca_core::Reduce::new(|_: &mut Option<usize>, (): &(), _| Effect::none());
    let (builder, site) = Store::builder(
        Some(0),
        root,
        Runtime::new(TimerConstructionDriver {
            clock: VirtualDriver {
                shared: Arc::clone(&clock.shared),
            },
            queued: Arc::clone(&queued),
            hook: Arc::new(Mutex::new(None)),
            wait_drops: Arc::new(AtomicUsize::new(0)),
        }),
    )
    .mount_with_site(mount);
    let store = builder.build().unwrap();
    let mut actor = queued.lock().unwrap().pop().expect("one actor");
    let mut cx = Context::from_waker(Waker::noop());
    assert!(actor.as_mut().poll(&mut cx).is_pending());
    store.ready().now_or_never().unwrap().unwrap();
    let child = {
        let mut acquisition = std::pin::pin!(store.acquire(&site));
        assert!(acquisition.as_mut().poll(&mut cx).is_pending());
        assert!(actor.as_mut().poll(&mut cx).is_pending());
        acquisition.now_or_never().unwrap().unwrap().unwrap()
    };
    let task = {
        let mut start = std::pin::pin!(child.send(false));
        assert!(start.as_mut().poll(&mut cx).is_pending());
        assert!(actor.as_mut().poll(&mut cx).is_pending());
        start.now_or_never().unwrap().unwrap()
    };
    assert_eq!(clock.active_sleeps(), 1);
    assert!(lifetime.upgrade().is_some());
    let mut closed = std::pin::pin!(store.closed());
    *owner.lock().unwrap() = Some(store);

    clock.set_now(MonotonicInstant::from_elapsed(Duration::from_secs(1)));
    assert!(actor.as_mut().poll(&mut cx).is_ready());
    assert!(closed.as_mut().poll(&mut cx).is_ready());
    assert!(task.is_finished());
    assert!(task.is_cancelled());
    task.finish().now_or_never().expect("closed routed timer");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(entered.load(Ordering::SeqCst), 0);
    assert!(owner.lock().unwrap().is_none());
    assert!(lifetime.upgrade().is_none());
    assert_eq!(clock.active_sleeps(), 0);
    assert!(queued.lock().unwrap().is_empty());
    drop(child);
}

/// A one-shot timer can retire its own route without completing its work twice.
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one mounted timer schedule keeps child retirement, stale authority and cleanup in one place"
)]
fn mounted_one_shot_self_retirement_does_not_recomplete_timer() {
    use futures_util::FutureExt as _;
    use rca_core::{Feature, FeatureContext, MountIdentity, OptionalMount};

    /// Counts destruction of the actual one-shot action payload.
    struct Payload(Arc<AtomicUsize>);

    impl Drop for Payload {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
    /// Separates timer registration from the route-invalidating delivery.
    enum ChildAction {
        /// Installs the prebuilt timer once.
        Start,
        /// Retains the drop witness until the timer action is disposed.
        Tick(Payload),
    }

    /// Changes its state on the tick so its own selector stops matching.
    struct Child(Option<Effect<ChildAction>>);

    impl Feature for Child {
        type State = usize;
        type Action = ChildAction;

        fn update(
            &mut self,
            state: &mut usize,
            action: &ChildAction,
            context: &mut FeatureContext<'_, usize, ChildAction>,
        ) {
            match action {
                ChildAction::Start => {
                    context.effect(self.0.take().expect("one mounted one-shot"));
                }
                ChildAction::Tick(payload) => {
                    let _ = &payload.0;
                    *state = 1;
                }
            }
        }
    }

    let clock = VirtualRuntime::new();
    let queued = Arc::new(Mutex::new(Vec::new()));
    let drops = Arc::new(AtomicUsize::new(0));
    let mut effect = Some(Effect::send_after(
        Duration::from_secs(1),
        ChildAction::Tick(Payload(Arc::clone(&drops))),
    ));
    let mount = OptionalMount::new(
        |root: &Option<usize>| root.as_ref().filter(|value| **value == 0),
        |root: &mut Option<usize>| root.as_mut().filter(|value| **value == 0),
        |root: &Option<usize>| {
            root.filter(|value| *value == 0).map(|_| MountIdentity {
                key: (),
                replacement: 0,
                configuration: 0,
            })
        },
        move |_| Child(effect.take()),
    );
    let root = rca_core::Reduce::new(|_: &mut Option<usize>, (): &(), _| Effect::none());
    let (builder, site) = Store::builder(
        Some(0),
        root,
        Runtime::new(TimerConstructionDriver {
            clock: VirtualDriver {
                shared: Arc::clone(&clock.shared),
            },
            queued: Arc::clone(&queued),
            hook: Arc::new(Mutex::new(None)),
            wait_drops: Arc::new(AtomicUsize::new(0)),
        }),
    )
    .mount_with_site(mount);
    let store = builder.build().unwrap();
    let mut actor = queued.lock().unwrap().pop().expect("one actor");
    let mut cx = Context::from_waker(Waker::noop());
    assert!(actor.as_mut().poll(&mut cx).is_pending());
    store.ready().now_or_never().unwrap().unwrap();
    let child = {
        let mut acquisition = std::pin::pin!(store.acquire(&site));
        assert!(acquisition.as_mut().poll(&mut cx).is_pending());
        assert!(actor.as_mut().poll(&mut cx).is_pending());
        acquisition.now_or_never().unwrap().unwrap().unwrap()
    };
    let task = {
        let mut start = std::pin::pin!(child.send(ChildAction::Start));
        assert!(start.as_mut().poll(&mut cx).is_pending());
        assert!(actor.as_mut().poll(&mut cx).is_pending());
        start.now_or_never().unwrap().unwrap()
    };
    assert_eq!(clock.active_sleeps(), 1);
    assert_eq!(drops.load(Ordering::SeqCst), 0);

    clock.set_now(MonotonicInstant::from_elapsed(Duration::from_secs(1)));
    for _ in 0..4 {
        assert!(actor.as_mut().poll(&mut cx).is_pending());
    }
    assert!(task.is_finished());
    assert!(!task.is_cancelled());
    task.finish()
        .now_or_never()
        .expect("retired one-shot finished");
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    assert_eq!(clock.active_sleeps(), 0);
    let mut stale = std::pin::pin!(child.read(|_| panic!("retired child callback")));
    assert!(stale.as_mut().poll(&mut cx).is_pending());
    assert!(actor.as_mut().poll(&mut cx).is_pending());
    let Poll::Ready(Err(error)) = stale.as_mut().poll(&mut cx) else {
        panic!("retired child authority must reject the read");
    };
    assert_eq!(error.reason(), rca_core::TaskAccessReason::ScopeEnded);
    assert_eq!(error.progress(), rca_core::TaskProgress::NotStarted);
    assert!(queued.lock().unwrap().is_empty());
    drop(child);
    let mut shutdown = std::pin::pin!(store.shutdown_into_state());
    assert!(shutdown.as_mut().poll(&mut cx).is_pending());
    assert!(actor.as_mut().poll(&mut cx).is_ready());
    assert_eq!(shutdown.now_or_never().unwrap().unwrap(), Some(1));
    assert_eq!(drops.load(Ordering::SeqCst), 1);
}

/// The first mounted due timer cancels later popped payloads and interval factories.
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one mounted due batch proves cancellation precedes sibling payload delivery and interval factory entry"
)]
fn mounted_due_sibling_cancellation_rejects_popped_timer_payload_and_interval_factory() {
    use futures_util::FutureExt as _;
    use rca_core::{Feature, FeatureContext, MountIdentity, OptionalMount};

    /// Counts destruction of both the one-shot payload and interval capture.
    struct Payload(Arc<AtomicUsize>);

    impl Drop for Payload {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
    /// Identifies each member of one equal-deadline batch.
    enum Action {
        /// Installs all three timers.
        Start,
        /// Cancels the shared task before later siblings can enter reduction.
        Cancel,
        /// Carries the rejected one-shot payload.
        Later(Payload),
        /// Detects any interval action admitted after cancellation.
        Tick,
    }

    /// Cancels the real parent task from the first timer reduction.
    struct Child {
        /// Moves the entire prebuilt batch into the actor once.
        effect: Option<Effect<Action>>,
        /// Receives the parent handle after the start request commits.
        task: Arc<Mutex<Option<rca_core::ActionTask>>>,
    }

    impl Feature for Child {
        type State = Vec<u8>;
        type Action = Action;

        fn update(
            &mut self,
            state: &mut Vec<u8>,
            action: &Action,
            context: &mut FeatureContext<'_, Vec<u8>, Action>,
        ) {
            match action {
                Action::Start => context.effect(self.effect.take().expect("one due batch")),
                Action::Cancel => {
                    state.push(1);
                    self.task.lock().unwrap().as_ref().unwrap().cancel();
                }
                Action::Later(payload) => {
                    let _ = &payload.0;
                    state.push(2);
                }
                Action::Tick => state.push(3),
            }
        }
    }

    let clock = VirtualRuntime::new();
    let queued = Arc::new(Mutex::new(Vec::new()));
    let task_slot = Arc::new(Mutex::new(None::<rca_core::ActionTask>));
    let drops = Arc::new(AtomicUsize::new(0));
    let calls = Arc::new(AtomicUsize::new(0));
    let factory_calls = Arc::clone(&calls);
    let capture = Payload(Arc::clone(&drops));
    let mut effect = Some(Effect::merge([
        Effect::send_after(Duration::from_secs(1), Action::Cancel),
        Effect::send_after(
            Duration::from_secs(1),
            Action::Later(Payload(Arc::clone(&drops))),
        ),
        Effect::interval(Duration::from_secs(1), move || {
            let _owned_capture = &capture;
            factory_calls.fetch_add(1, Ordering::SeqCst);
            Action::Tick
        }),
    ]));
    let child_task = Arc::clone(&task_slot);
    let mount = OptionalMount::new(
        Option::as_ref,
        Option::as_mut,
        |root: &Option<Vec<u8>>| {
            root.as_ref().map(|_| MountIdentity {
                key: (),
                replacement: 0,
                configuration: 0,
            })
        },
        move |_| Child {
            effect: effect.take(),
            task: Arc::clone(&child_task),
        },
    );
    let root = rca_core::Reduce::new(|_: &mut Option<Vec<u8>>, (): &(), _| Effect::none());
    let (builder, site) = Store::builder(
        Some(Vec::new()),
        root,
        Runtime::new(TimerConstructionDriver {
            clock: VirtualDriver {
                shared: Arc::clone(&clock.shared),
            },
            queued: Arc::clone(&queued),
            hook: Arc::new(Mutex::new(None)),
            wait_drops: Arc::new(AtomicUsize::new(0)),
        }),
    )
    .mount_with_site(mount);
    let store = builder.build().unwrap();
    let mut actor = queued.lock().unwrap().pop().expect("one actor");
    let mut cx = Context::from_waker(Waker::noop());
    assert!(actor.as_mut().poll(&mut cx).is_pending());
    store.ready().now_or_never().unwrap().unwrap();
    let child = {
        let mut acquisition = std::pin::pin!(store.acquire(&site));
        assert!(acquisition.as_mut().poll(&mut cx).is_pending());
        assert!(actor.as_mut().poll(&mut cx).is_pending());
        acquisition.now_or_never().unwrap().unwrap().unwrap()
    };
    let task = {
        let mut start = std::pin::pin!(child.send(Action::Start));
        assert!(start.as_mut().poll(&mut cx).is_pending());
        assert!(actor.as_mut().poll(&mut cx).is_pending());
        start.now_or_never().unwrap().unwrap()
    };
    *task_slot.lock().unwrap() = Some(task.clone());
    assert!(!task.is_finished());
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(clock.active_sleeps(), 1);

    // All three equal deadlines are popped together in source insertion order.
    // The first reduction cancels their shared lineage without retiring this mount.
    clock.set_now(MonotonicInstant::from_elapsed(Duration::from_secs(1)));
    for _ in 0..4 {
        assert!(actor.as_mut().poll(&mut cx).is_pending());
    }
    assert!(task.is_cancelled());
    assert!(task.is_finished());
    task.finish().now_or_never().expect("due siblings retired");
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(drops.load(Ordering::SeqCst), 2);
    assert_eq!(Arc::strong_count(&calls), 1);
    assert_eq!(clock.active_sleeps(), 0);
    let sleep_calls = clock.sleep_calls();
    clock.set_now(MonotonicInstant::from_elapsed(Duration::from_secs(5)));
    assert!(actor.as_mut().poll(&mut cx).is_pending());
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(drops.load(Ordering::SeqCst), 2);
    assert_eq!(clock.sleep_calls(), sleep_calls);
    assert!(queued.lock().unwrap().is_empty());

    drop(child);
    let mut shutdown = std::pin::pin!(store.shutdown_into_state());
    assert!(shutdown.as_mut().poll(&mut cx).is_pending());
    assert!(actor.as_mut().poll(&mut cx).is_ready());
    assert_eq!(shutdown.now_or_never().unwrap().unwrap(), Some(vec![1]));
}

/// Wraps driver waits so their destruction cancels the parent timer task.
#[derive(Debug)]
struct CancelOnSleepDropDriver {
    /// Supplies the controlled clock and manually captured actor.
    inner: TimerConstructionDriver,
    /// Receives the timer's parent after registration commits.
    root: Arc<Mutex<Option<rca_core::ActionTask>>>,
    /// Counts each cancellation callback from wait destruction.
    drops: Arc<AtomicUsize>,
}

/// Forwards polling and cancels the parent before disposing its inner wait.
struct CancelOnSleepDrop {
    /// Retains the provider wait until the wrapper is destroyed.
    future: RuntimeFuture,
    /// Points to the real parent task that the callback cancels.
    root: Arc<Mutex<Option<rca_core::ActionTask>>>,
    /// Separately observes destructor entry.
    drops: Arc<AtomicUsize>,
}

impl Future for CancelOnSleepDrop {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        self.future.as_mut().poll(cx)
    }
}

impl Drop for CancelOnSleepDrop {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
        self.root.lock().unwrap().as_ref().unwrap().cancel();
    }
}

impl RuntimeDriver for CancelOnSleepDropDriver {
    fn spawn(&self, task: RuntimeFuture) {
        self.inner.spawn(task);
    }

    fn now(&self) -> MonotonicInstant {
        self.inner.now()
    }

    fn sleep_until(&self, deadline: MonotonicInstant) -> RuntimeFuture {
        Box::pin(CancelOnSleepDrop {
            future: self.inner.sleep_until(deadline),
            root: Arc::clone(&self.root),
            drops: Arc::clone(&self.drops),
        })
    }

    fn yield_now(&self) -> RuntimeFuture {
        self.inner.yield_now()
    }
}

/// A completed sleep's destructor cannot cancel a due action before it commits.
#[test]
fn completed_sleep_is_dropped_after_due_timer_commit() {
    use futures_util::FutureExt as _;

    let clock = VirtualRuntime::new();
    let queued = Arc::new(Mutex::new(Vec::new()));
    let root_slot = Arc::new(Mutex::new(None));
    let drops = Arc::new(AtomicUsize::new(0));
    let reducer = rca_core::Reduce::new(|state: &mut usize, tick: &bool, _| {
        if *tick {
            *state += 1;
            Effect::none()
        } else {
            Effect::interval(Duration::from_secs(1), || true)
        }
    });
    let store = Store::new(
        0,
        reducer,
        Runtime::new(CancelOnSleepDropDriver {
            inner: TimerConstructionDriver {
                clock: VirtualDriver {
                    shared: Arc::clone(&clock.shared),
                },
                queued: Arc::clone(&queued),
                hook: Arc::new(Mutex::new(None)),
                wait_drops: Arc::new(AtomicUsize::new(0)),
            },
            root: Arc::clone(&root_slot),
            drops: Arc::clone(&drops),
        }),
    );
    let mut actor = queued.lock().unwrap().pop().expect("one actor");
    let mut cx = Context::from_waker(Waker::noop());
    let receipt = store.try_send(false).unwrap();
    assert!(actor.as_mut().poll(&mut cx).is_pending());
    let root = receipt.finish().now_or_never().unwrap().unwrap();
    *root_slot.lock().unwrap() = Some(root.clone());
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    clock.set_now(MonotonicInstant::from_elapsed(Duration::from_secs(1)));
    assert!(actor.as_mut().poll(&mut cx).is_pending());
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    assert!(root.is_cancelled());
    root.finish()
        .now_or_never()
        .expect("drop cancelled interval");
    let state = {
        let mut read = std::pin::pin!(store.read(|state| *state));
        assert!(read.as_mut().poll(&mut cx).is_pending());
        assert!(actor.as_mut().poll(&mut cx).is_pending());
        let Poll::Ready(Ok(state)) = read.as_mut().poll(&mut cx) else {
            panic!("actor must answer admitted read");
        };
        state
    };
    assert_eq!(
        state, 1,
        "the due action commits before sleep-drop cancellation"
    );
    assert_eq!(clock.active_sleeps(), 0);
    assert!(queued.lock().unwrap().is_empty());
    let mut closed = std::pin::pin!(store.closed());
    drop(store);
    assert!(actor.as_mut().poll(&mut cx).is_ready());
    assert!(closed.as_mut().poll(&mut cx).is_ready());
    assert_eq!(drops.load(Ordering::SeqCst), 1);
}

/// Schedules an insertion-ordered batch, then records each delivered value.
#[derive(Clone, Copy)]
enum Action {
    /// Starts eight timers at the same deadline.
    ScheduleMany,
    /// Records one timer's insertion index.
    Fired(u8),
}

/// Keeps many logical timers behind one actor-owned driver sleep.
struct Timers;

impl Reducer for Timers {
    type State = Vec<u8>;
    type Action = Action;

    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &Self::Action,
        _context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        match action {
            Action::ScheduleMany => Effect::merge(
                (0..8)
                    .map(|value| Effect::send_after(Duration::from_secs(1), Action::Fired(value))),
            ),
            Action::Fired(value) => {
                state.push(*value);
                Effect::none()
            }
        }
    }
}

/// Eight same-deadline timers preserve insertion order with one sleep registration.
#[test]
fn store_uses_one_sleep_authority_for_many_timers() {
    let runtime = VirtualRuntime::new();
    let store = Store::new(Vec::new(), Timers, runtime.runtime());

    futures_executor::block_on(async {
        let timers = store
            .send(Action::ScheduleMany)
            .await
            .expect("timer commit");
        runtime.wait_for_sleep();
        VirtualRuntime::wait_until(|| runtime.active_sleeps() == 1);

        assert_eq!(runtime.sleep_calls(), 1);
        assert_eq!(runtime.max_active_sleeps(), 1);

        runtime.advance_to(MonotonicInstant::from_elapsed(Duration::from_secs(1)));
        timers.finish().await;
        assert_eq!(
            store.read(Clone::clone).await.expect("state"),
            [0, 1, 2, 3, 4, 5, 6, 7]
        );
        store.shutdown().await.expect("clean shutdown");
    });
    runtime.join_all();
}

/// Drives the shared timer, interval, sequence and mailbox-fairness scenarios.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TimerAction {
    /// Starts a delayed action followed by an immediate suffix.
    ScheduleSequence,
    /// Starts a cancellable timer one hundred seconds away.
    ScheduleFar,
    /// Starts a one-second timer.
    ScheduleOne,
    /// Starts a one-second fixed-rate interval.
    StartInterval,
    /// Cancels the far timer by its ordinary cancellation key.
    CancelFar,
    /// Cancels the fixed-rate interval.
    CancelInterval,
    /// Adds mailbox traffic independent of timer delivery.
    External(u16),
    /// Records one-shot timer delivery.
    Timer,
    /// Records the delayed head of the sequence.
    First,
    /// Records the sequence suffix.
    Second,
    /// Records one interval tick.
    Tick,
}

/// Keeps far-timer and interval cancellation independent.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum TimerKey {
    /// Identifies the cancellable far timer.
    Far,
    /// Identifies the interval factory and its future ticks.
    Interval,
}

/// Returns timer effects and records only delivered actions in state.
struct TimerReducer;

impl Reducer for TimerReducer {
    type State = Vec<TimerAction>;
    type Action = TimerAction;

    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &Self::Action,
        _context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        match action {
            TimerAction::ScheduleSequence => Effect::concatenate([
                Effect::send_after(Duration::from_secs(1), TimerAction::First),
                Effect::send(TimerAction::Second),
            ]),
            TimerAction::ScheduleFar => {
                Effect::send_after(Duration::from_secs(100), TimerAction::Timer)
                    .cancellable(TimerKey::Far)
            }
            TimerAction::ScheduleOne => {
                Effect::send_after(Duration::from_secs(1), TimerAction::Timer)
            }
            TimerAction::StartInterval => {
                Effect::interval(Duration::from_secs(1), || TimerAction::Tick)
                    .cancellable(TimerKey::Interval)
            }
            TimerAction::CancelFar => Effect::cancel(TimerKey::Far),
            TimerAction::CancelInterval => Effect::cancel(TimerKey::Interval),
            TimerAction::External(_)
            | TimerAction::Timer
            | TimerAction::First
            | TimerAction::Second
            | TimerAction::Tick => {
                state.push(*action);
                Effect::none()
            }
        }
    }
}

/// Uses ordered state reads to observe an exact tick count with bounded retries.
async fn wait_for_ticks(store: &Store<Vec<TimerAction>, TimerAction>, expected: usize) {
    for _ in 0..100_000 {
        let actual = store
            .read(|state| {
                state
                    .iter()
                    .filter(|action| **action == TimerAction::Tick)
                    .count()
            })
            .await
            .expect("timer state");
        if actual == expected {
            return;
        }
        std::thread::yield_now();
    }
    panic!("interval did not reach expected tick count");
}

/// A sequence cannot start its suffix before its delayed send completes.
#[test]
fn delayed_send_completes_before_concatenate_suffix_starts() {
    let runtime = VirtualRuntime::new();
    let store = Store::new(Vec::new(), TimerReducer, runtime.runtime());

    futures_executor::block_on(async {
        let sequence = store
            .send(TimerAction::ScheduleSequence)
            .await
            .expect("sequence commit");
        runtime.wait_for_sleep();
        assert!(store.read(Clone::clone).await.expect("state").is_empty());

        runtime.advance_to(MonotonicInstant::from_elapsed(Duration::from_secs(1)));
        sequence.finish().await;
        assert_eq!(
            store.read(Clone::clone).await.expect("state"),
            [TimerAction::First, TimerAction::Second]
        );
        store.shutdown().await.expect("clean shutdown");
    });
    runtime.join_all();
}

/// Cancellation unregisters a far-future wait without advancing the clock.
#[test]
fn cancelling_far_future_timer_reclaims_sleep_promptly() {
    let runtime = VirtualRuntime::new();
    let store = Store::new(Vec::new(), TimerReducer, runtime.runtime());

    futures_executor::block_on(async {
        let timer = store
            .send(TimerAction::ScheduleFar)
            .await
            .expect("far timer commit");
        runtime.wait_for_sleep();
        assert_eq!(runtime.active_sleeps(), 1);

        store
            .send(TimerAction::CancelFar)
            .await
            .expect("timer cancellation")
            .finish()
            .await;
        timer.finish().await;
        VirtualRuntime::wait_until(|| runtime.active_sleeps() == 0);

        assert!(store.read(Clone::clone).await.expect("state").is_empty());
        store.shutdown().await.expect("clean shutdown");
    });
    runtime.join_all();
}

/// Missed interval positions catch up without shifting the next fixed-rate deadline.
#[test]
fn interval_catches_up_at_fixed_rate_without_drift() {
    let runtime = VirtualRuntime::new();
    let store = Store::new(Vec::new(), TimerReducer, runtime.runtime());

    futures_executor::block_on(async {
        let interval = store
            .send(TimerAction::StartInterval)
            .await
            .expect("interval commit");
        runtime.wait_for_sleep();
        runtime.advance_to(MonotonicInstant::from_elapsed(Duration::from_secs(5)));
        wait_for_ticks(&store, 5).await;

        runtime.advance_to(MonotonicInstant::from_elapsed(Duration::from_millis(5_500)));
        assert_eq!(
            store
                .read(Clone::clone)
                .await
                .expect("state")
                .iter()
                .filter(|action| **action == TimerAction::Tick)
                .count(),
            5
        );
        runtime.advance_to(MonotonicInstant::from_elapsed(Duration::from_secs(6)));
        wait_for_ticks(&store, 6).await;

        store
            .send(TimerAction::CancelInterval)
            .await
            .expect("interval cancel")
            .finish()
            .await;
        interval.finish().await;
        store.shutdown().await.expect("clean shutdown");
    });
    runtime.join_all();
}

/// A due timer commits within one bounded command batch despite queued traffic.
#[test]
fn mailbox_flood_cannot_starve_due_timer() {
    let runtime = VirtualRuntime::new();
    let store = Store::new(Vec::new(), TimerReducer, runtime.runtime());

    futures_executor::block_on(async {
        let timer = store
            .send(TimerAction::ScheduleOne)
            .await
            .expect("one-shot commit");
        runtime.wait_for_sleep();
        runtime.set_now(MonotonicInstant::from_elapsed(Duration::from_secs(1)));
        let receipts: Vec<_> = (0..192)
            .map(|value| {
                store
                    .try_send(TimerAction::External(value))
                    .expect("mailbox admission")
            })
            .collect();
        runtime.wake_due();
        timer.finish().await;
        for receipt in receipts {
            receipt.await.expect("external commit").finish().await;
        }

        let state = store.read(Clone::clone).await.expect("state");
        let timer_index = state
            .iter()
            .position(|action| *action == TimerAction::Timer)
            .expect("timer output");
        assert!(timer_index <= 64, "due timer exceeded one command batch");
        store.shutdown().await.expect("clean shutdown");
    });
    runtime.join_all();
}

/// External work commits within one bounded timer batch despite an overdue interval.
#[test]
fn overdue_interval_cannot_starve_external_command() {
    let runtime = VirtualRuntime::new();
    let store = Store::new(Vec::new(), TimerReducer, runtime.runtime());

    futures_executor::block_on(async {
        let interval = store
            .send(TimerAction::StartInterval)
            .await
            .expect("interval commit");
        runtime.wait_for_sleep();
        runtime.set_now(MonotonicInstant::from_elapsed(Duration::from_secs(200)));
        let external = store
            .try_send(TimerAction::External(1))
            .expect("external admission");
        runtime.wake_due();
        external.await.expect("external commit").finish().await;
        store
            .send(TimerAction::CancelInterval)
            .await
            .expect("interval cancel")
            .finish()
            .await;
        interval.finish().await;

        let state = store.read(Clone::clone).await.expect("state");
        let external_index = state
            .iter()
            .position(|action| *action == TimerAction::External(1))
            .expect("external action");
        assert!(
            external_index <= 64,
            "external action exceeded one timer batch"
        );
        store.shutdown().await.expect("clean shutdown");
    });
    runtime.join_all();
}

/// Saturated timer work stays cancellable without creating a futile sleep.
#[test]
fn saturated_deadline_creates_no_sleep_and_remains_cancellable() {
    let runtime = VirtualRuntime::new();
    runtime.set_now(MonotonicInstant::MAX);
    let store = Store::new(Vec::new(), TimerReducer, runtime.runtime());

    futures_executor::block_on(async {
        let timer = store
            .send(TimerAction::ScheduleOne)
            .await
            .expect("saturated timer commit");
        assert_eq!(runtime.sleep_calls(), 0);
        timer.cancel();
        timer.finish().await;

        assert!(timer.is_cancelled());
        assert!(store.read(Clone::clone).await.expect("state").is_empty());
        store.shutdown().await.expect("clean shutdown");
    });
    runtime.join_all();
}

/// An earlier deadline replaces the actor's wake without parallel registrations.
#[test]
fn earlier_deadline_replaces_wake_without_parallel_sleep() {
    let runtime = VirtualRuntime::new();
    let store = Store::new(Vec::new(), TimerReducer, runtime.runtime());

    futures_executor::block_on(async {
        let later = store
            .send(TimerAction::ScheduleFar)
            .await
            .expect("later timer commit");
        runtime.wait_for_sleep_calls(1);
        let earlier = store
            .send(TimerAction::ScheduleOne)
            .await
            .expect("earlier timer commit");
        runtime.wait_for_sleep_calls(2);
        VirtualRuntime::wait_until(|| runtime.active_sleeps() == 1);

        assert_eq!(runtime.active_sleeps(), 1);
        assert_eq!(runtime.max_active_sleeps(), 1);

        runtime.advance_to(MonotonicInstant::from_elapsed(Duration::from_secs(1)));
        earlier.finish().await;
        later.cancel();
        later.finish().await;
        assert_eq!(
            store.read(Clone::clone).await.expect("state"),
            [TimerAction::Timer]
        );
        store.shutdown().await.expect("clean shutdown");
    });
    runtime.join_all();
}

/// Shutdown acknowledgement follows driver-wait destruction, not only cancellation.
#[test]
fn shutdown_drops_timer_wake_before_acknowledgement() {
    let runtime = VirtualRuntime::new();
    let store = Store::new(Vec::new(), TimerReducer, runtime.runtime());

    futures_executor::block_on(async {
        let timer = store
            .send(TimerAction::ScheduleFar)
            .await
            .expect("far timer commit");
        runtime.wait_for_sleep();
        VirtualRuntime::wait_until(|| runtime.active_sleeps() == 1);

        store.shutdown().await.expect("clean shutdown");
        assert_eq!(runtime.active_sleeps(), 0);
        timer.finish().await;
        assert!(timer.is_cancelled());
    });
    runtime.join_all();
}

/// Runs one external action while a real driver sleep is being constructed.
type TimerConstructionHook = Box<dyn FnOnce() + Send>;

/// Capacity notification can close the final owner before a popped command reduces.
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one causal table keeps capacity wake, final-owner loss, and timer/effect teardown ordering visible"
)]
fn final_owner_loss_during_capacity_wake_rejects_popped_command_with_timer() {
    use futures_util::FutureExt as _;

    /// Takes the final owner out of its mutex before closing the store on wake.
    struct CloseStore(Arc<Mutex<Option<Store<(), u8>>>>);

    impl std::task::Wake for CloseStore {
        fn wake(self: Arc<Self>) {
            self.wake_by_ref();
        }

        fn wake_by_ref(self: &Arc<Self>) {
            let owner = self.0.lock().unwrap().take();
            drop(owner);
        }
    }

    // Cover the ordinary batch and the post-listener receive after sleep construction.
    for during_construction in [false, true] {
        let clock = VirtualRuntime::new();
        let queued = Arc::new(Mutex::new(Vec::new()));
        let hook: Arc<Mutex<Option<TimerConstructionHook>>> = Arc::new(Mutex::new(None));
        let wait_drops = Arc::new(AtomicUsize::new(0));
        let exported = Arc::new(Mutex::new(None));
        let export = Arc::clone(&exported);
        let commits = Arc::new(AtomicUsize::new(0));
        let committed = Arc::clone(&commits);
        let store = Store::builder(
            (),
            rca_core::Reduce::new(move |(): &mut (), action: &u8, _: &mut ReduceContext<'_>| {
                match action {
                    0 => {
                        let export = Arc::clone(&export);
                        Effect::run(move |sender| async move {
                            *export.lock().unwrap() = Some(sender);
                            std::future::pending::<()>().await;
                        })
                    }
                    1 => Effect::send_after(Duration::from_secs(60), 4),
                    _ => {
                        committed.fetch_add(1, Ordering::SeqCst);
                        Effect::none()
                    }
                }
            }),
            Runtime::new(TimerConstructionDriver {
                clock: VirtualDriver {
                    shared: Arc::clone(&clock.shared),
                },
                queued: Arc::clone(&queued),
                hook: Arc::clone(&hook),
                wait_drops: Arc::clone(&wait_drops),
            }),
        )
        .mailbox_capacity(1)
        .build()
        .unwrap();
        let mut actor = queued.lock().unwrap().pop().unwrap();
        let receipt = store.try_send(0).unwrap();
        let mut cx = Context::from_waker(Waker::noop());
        assert!(actor.as_mut().poll(&mut cx).is_pending());
        let run_task = receipt
            .finish()
            .now_or_never()
            .expect("run committed")
            .unwrap();
        let mut run = queued.lock().unwrap().pop().unwrap();
        assert!(run.as_mut().poll(&mut cx).is_pending());
        let sender = exported.lock().unwrap().take().unwrap();
        let timer_receipt = store.try_send(1).unwrap();
        let mut closed = Box::pin(store.closed());
        let owner = Arc::new(Mutex::new(Some(store)));
        let popped_receipt = Arc::new(Mutex::new(None));
        let pending_send = Arc::new(Mutex::new(None));
        let delivered = Arc::new(Mutex::new(None));
        let admit: TimerConstructionHook = {
            let owner = Arc::clone(&owner);
            let popped_receipt = Arc::clone(&popped_receipt);
            let pending_send = Arc::clone(&pending_send);
            let delivered = Arc::clone(&delivered);
            Box::new(move || {
                *popped_receipt.lock().unwrap() =
                    Some(owner.lock().unwrap().as_ref().unwrap().try_send(2).unwrap());
                let mut send: RuntimeFuture = Box::pin(async move {
                    let result = sender.send(3).await;
                    *delivered.lock().unwrap() = Some(result);
                });
                let waker = Waker::from(Arc::new(CloseStore(owner)));
                assert!(
                    send.as_mut()
                        .poll(&mut Context::from_waker(&waker))
                        .is_pending()
                );
                *pending_send.lock().unwrap() = Some(send);
            })
        };
        if during_construction {
            *hook.lock().unwrap() = Some(admit);
        } else {
            assert!(actor.as_mut().poll(&mut cx).is_pending());
            admit();
        }

        assert!(actor.as_mut().poll(&mut cx).is_ready());
        assert!(owner.lock().unwrap().is_none());
        assert!(closed.as_mut().poll(&mut cx).is_ready());
        popped_receipt
            .lock()
            .unwrap()
            .take()
            .unwrap()
            .finish()
            .now_or_never()
            .expect("popped command resolved")
            .expect_err("final owner loss must reject the popped command");
        assert_eq!(commits.load(Ordering::SeqCst), 0);
        let timer = timer_receipt
            .finish()
            .now_or_never()
            .expect("timer committed")
            .unwrap();
        assert!(timer.is_finished());
        assert!(timer.is_cancelled());
        assert_eq!(clock.sleep_calls(), 1);
        assert_eq!(clock.active_sleeps(), 0);
        assert_eq!(wait_drops.load(Ordering::SeqCst), 1);
        let mut send = pending_send.lock().unwrap().take().unwrap();
        assert!(send.as_mut().poll(&mut cx).is_ready());
        assert!(
            delivered
                .lock()
                .unwrap()
                .take()
                .unwrap()
                .unwrap_err()
                .is_store_stopped()
        );
        assert!(run.as_mut().poll(&mut cx).is_ready());
        assert!(run_task.is_finished());
        assert!(run_task.is_cancelled());
        assert!(queued.lock().unwrap().is_empty());
    }
}

/// Clock-read cancellation rejects an already-popped action and releases its payload.
#[test]
fn cancellation_during_timer_commit_clock_read_rejects_owned_action() {
    use futures_util::FutureExt as _;

    let clock = VirtualRuntime::new();
    let queued = Arc::new(Mutex::new(Vec::new()));
    let cancel = Arc::new(Mutex::new(None));
    let root_slot = Arc::new(Mutex::new(None::<rca_core::ActionTask>));
    let reducer_root = Arc::clone(&root_slot);
    let arm = Arc::clone(&cancel);
    let payload = Arc::new(());
    let scheduled_payload = Arc::clone(&payload);
    let reducer = rca_core::Reduce::new(
        move |state: &mut Vec<u8>, action: &(u8, Option<Arc<()>>), _| {
            state.push(action.0);
            match action.0 {
                0 => Effect::merge([
                    Effect::send_after(Duration::from_secs(1), (1, None)),
                    Effect::send_after(
                        Duration::from_secs(1),
                        (2, Some(Arc::clone(&scheduled_payload))),
                    ),
                ]),
                1 => {
                    let root = reducer_root.lock().unwrap().as_ref().unwrap().clone();
                    assert!(!root.is_cancelled());
                    // Both timers have been popped. The second passes its live
                    // precheck before its transaction samples the runtime clock.
                    *arm.lock().unwrap() = Some(root);
                    Effect::none()
                }
                _ => Effect::none(),
            }
        },
    );
    let wait_drops = Arc::new(AtomicUsize::new(0));
    let store = Store::new(
        Vec::new(),
        reducer,
        Runtime::new(CancelOnNowDriver {
            inner: TimerConstructionDriver {
                clock: VirtualDriver {
                    shared: Arc::clone(&clock.shared),
                },
                queued: Arc::clone(&queued),
                hook: Arc::new(Mutex::new(None)),
                wait_drops: Arc::clone(&wait_drops),
            },
            cancel: Arc::clone(&cancel),
        }),
    );
    let mut actor = queued.lock().unwrap().pop().unwrap();
    let mut cx = Context::from_waker(Waker::noop());
    let receipt = store.try_send((0, None)).unwrap();
    assert!(actor.as_mut().poll(&mut cx).is_pending());
    let root = receipt.finish().now_or_never().unwrap().unwrap();
    *root_slot.lock().unwrap() = Some(root.clone());
    assert_eq!(Arc::strong_count(&payload), 3);
    assert_eq!(clock.active_sleeps(), 1);

    clock.advance_to(MonotonicInstant::from_elapsed(Duration::from_secs(1)));
    assert!(actor.as_mut().poll(&mut cx).is_pending());
    assert!(cancel.lock().unwrap().is_none());
    assert!(root.is_cancelled());
    assert!(root.is_finished());
    root.finish()
        .now_or_never()
        .expect("cancelled timer retired");
    assert_eq!(Arc::strong_count(&payload), 2, "rejected action released");
    assert_eq!(clock.active_sleeps(), 0);
    assert_eq!(clock.sleep_calls(), 1);
    assert_eq!(wait_drops.load(Ordering::SeqCst), 1);

    let mut shutdown = Box::pin(store.shutdown_into_state());
    assert!(shutdown.as_mut().poll(&mut cx).is_pending());
    assert!(actor.as_mut().poll(&mut cx).is_ready());
    assert_eq!(shutdown.now_or_never().unwrap().unwrap(), [0, 1]);
    assert_eq!(Arc::strong_count(&payload), 1, "reducer released");
    assert!(queued.lock().unwrap().is_empty());
}

/// Cancels one real task during the next runtime clock query.
#[derive(Debug)]
struct CancelOnNowDriver {
    /// Preserves the controlled clock, task capture and wait-drop counter.
    inner: TimerConstructionDriver,
    /// Is empty until the preceding action arms the exact callback boundary.
    cancel: Arc<Mutex<Option<rca_core::ActionTask>>>,
}

impl RuntimeDriver for CancelOnNowDriver {
    fn spawn(&self, task: RuntimeFuture) {
        self.inner.spawn(task);
    }

    fn now(&self) -> MonotonicInstant {
        let task = self.cancel.lock().unwrap().take();
        if let Some(task) = task {
            task.cancel();
        }
        self.inner.now()
    }

    fn sleep_until(&self, deadline: MonotonicInstant) -> RuntimeFuture {
        self.inner.sleep_until(deadline)
    }

    fn yield_now(&self) -> RuntimeFuture {
        self.inner.yield_now()
    }
}

/// Cancellation during arbitration must be processed before the actor parks again.
#[test]
fn cancellation_during_clock_read_is_harvested_before_parking() {
    use futures_util::FutureExt as _;

    let clock = VirtualRuntime::new();
    let queued = Arc::new(Mutex::new(Vec::new()));
    let cancel = Arc::new(Mutex::new(None));
    let wait_drops = Arc::new(AtomicUsize::new(0));
    let store = Store::new(
        Vec::new(),
        TimerReducer,
        Runtime::new(CancelOnNowDriver {
            inner: TimerConstructionDriver {
                clock: VirtualDriver {
                    shared: Arc::clone(&clock.shared),
                },
                queued: Arc::clone(&queued),
                hook: Arc::new(Mutex::new(None)),
                wait_drops: Arc::clone(&wait_drops),
            },
            cancel: Arc::clone(&cancel),
        }),
    );
    let mut actor = queued.lock().unwrap().pop().unwrap();
    let mut cx = Context::from_waker(Waker::noop());
    let receipt = store.try_send(TimerAction::ScheduleFar).unwrap();
    assert!(actor.as_mut().poll(&mut cx).is_pending());
    let timer = receipt
        .finish()
        .now_or_never()
        .expect("timer committed")
        .unwrap();
    assert_eq!(clock.active_sleeps(), 1);
    let cancellation = timer.clone();
    let arm = Arc::clone(&cancel);
    // Reads do not sample the clock. The next timer arbitration cancels only
    // after the actor has harvested control and captured its current epoch.
    let mut read = Box::pin(store.read(move |_| {
        *arm.lock().unwrap() = Some(cancellation);
        7
    }));
    assert!(read.as_mut().poll(&mut cx).is_pending());
    assert!(actor.as_mut().poll(&mut cx).is_pending());
    assert!(cancel.lock().unwrap().is_none());
    assert_eq!(read.now_or_never().expect("read acknowledged").unwrap(), 7);
    assert!(timer.is_cancelled());
    assert!(timer.finish().now_or_never().is_some());
    assert_eq!(clock.active_sleeps(), 0);
    assert_eq!(clock.sleep_calls(), 1);
    assert_eq!(wait_drops.load(Ordering::SeqCst), 1);

    let mut shutdown = Box::pin(store.shutdown_into_state());
    assert!(shutdown.as_mut().poll(&mut cx).is_pending());
    assert!(actor.as_mut().poll(&mut cx).is_ready());
    assert_eq!(
        shutdown
            .now_or_never()
            .expect("shutdown acknowledged")
            .unwrap(),
        Vec::<TimerAction>::new()
    );
    assert!(queued.lock().unwrap().is_empty());
}

/// Captures tasks and inserts a one-shot callback into real sleep construction.
struct TimerConstructionDriver {
    /// Supplies time and actual registered driver waits.
    clock: VirtualDriver,
    /// Lets each scenario poll the actor and effects in a chosen order.
    queued: Arc<Mutex<Vec<RuntimeFuture>>>,
    /// Is taken out of its mutex before the callback can re-enter the store.
    hook: Arc<Mutex<Option<TimerConstructionHook>>>,
    /// Counts destruction separately from provider registration and readiness.
    wait_drops: Arc<AtomicUsize>,
}

impl fmt::Debug for TimerConstructionDriver {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TimerConstructionDriver")
            .finish_non_exhaustive()
    }
}

/// Counts disposal of a constructed wait, including an unpolled wrapper.
struct TimerWaitDrop(Arc<AtomicUsize>);

impl Drop for TimerWaitDrop {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

impl RuntimeDriver for TimerConstructionDriver {
    fn spawn(&self, task: RuntimeFuture) {
        self.queued.lock().unwrap().push(task);
    }

    fn now(&self) -> MonotonicInstant {
        self.clock.now()
    }

    fn sleep_until(&self, deadline: MonotonicInstant) -> RuntimeFuture {
        let wait = self.clock.sleep_until(deadline);
        let drop = TimerWaitDrop(Arc::clone(&self.wait_drops));
        let hook = self.hook.lock().unwrap().take();
        if let Some(hook) = hook {
            hook();
        }
        Box::pin(async move {
            wait.await;
            std::mem::drop(drop);
        })
    }

    fn yield_now(&self) -> RuntimeFuture {
        self.clock.yield_now()
    }
}

/// Samples driver-wait cleanup at acknowledgement, before the test resumes it.
struct TimerAckWake {
    /// Reports the number of destroyed wait wrappers.
    wait_drops: Arc<AtomicUsize>,
    /// Keeps the earliest observed drop count across acknowledgement notifications.
    drops_at_ack: Arc<AtomicUsize>,
}

impl std::task::Wake for TimerAckWake {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.drops_at_ack
            .fetch_min(self.wait_drops.load(Ordering::SeqCst), Ordering::SeqCst);
    }
}

/// Sleep-construction callbacks can request shutdown or release the final owner safely.
#[test]
fn timer_construction_observes_ordered_shutdown_and_final_owner_loss() {
    for graceful in [true, false] {
        let clock = VirtualRuntime::new();
        let queued = Arc::new(Mutex::new(Vec::new()));
        let hook: Arc<Mutex<Option<TimerConstructionHook>>> = Arc::new(Mutex::new(None));
        let wait_drops = Arc::new(AtomicUsize::new(0));
        let drops_at_ack = Arc::new(AtomicUsize::new(usize::MAX));
        let store = Store::new(
            Vec::new(),
            TimerReducer,
            Runtime::new(TimerConstructionDriver {
                clock: VirtualDriver {
                    shared: Arc::clone(&clock.shared),
                },
                queued: Arc::clone(&queued),
                hook: Arc::clone(&hook),
                wait_drops: Arc::clone(&wait_drops),
            }),
        );
        let receipt = store
            .try_send(TimerAction::ScheduleFar)
            .expect("far timer admission");
        let mut closed = std::pin::pin!(store.closed());
        let shutdown: Arc<Mutex<Option<RuntimeFuture>>> = Arc::new(Mutex::new(None));
        let final_state = Arc::new(Mutex::new(None));
        if graceful {
            let shutdown_slot = Arc::clone(&shutdown);
            let final_state = Arc::clone(&final_state);
            let waker = Waker::from(Arc::new(TimerAckWake {
                wait_drops: Arc::clone(&wait_drops),
                drops_at_ack: Arc::clone(&drops_at_ack),
            }));
            *hook.lock().unwrap() = Some(Box::new(move || {
                let mut shutdown: RuntimeFuture = Box::pin(async move {
                    let state = store
                        .shutdown_into_state()
                        .await
                        .expect("graceful shutdown");
                    *final_state.lock().unwrap() = Some(state);
                });
                let mut cx = Context::from_waker(&waker);
                assert!(shutdown.as_mut().poll(&mut cx).is_pending());
                *shutdown_slot.lock().unwrap() = Some(shutdown);
            }));
        } else {
            *hook.lock().unwrap() = Some(Box::new(move || drop(store)));
        }

        let mut actor = {
            let mut queued = queued.lock().unwrap();
            assert_eq!(queued.len(), 1, "only the actor is runtime-owned");
            queued.pop().unwrap()
        };
        let mut cx = Context::from_waker(Waker::noop());
        assert!(actor.as_mut().poll(&mut cx).is_ready());
        assert!(hook.lock().unwrap().is_none());
        assert!(queued.lock().unwrap().is_empty());
        assert_eq!(clock.sleep_calls(), 1);
        assert_eq!(clock.active_sleeps(), 0);
        assert_eq!(wait_drops.load(Ordering::SeqCst), 1);
        assert!(closed.as_mut().poll(&mut cx).is_ready());

        let mut receipt = std::pin::pin!(receipt.finish());
        let Poll::Ready(Ok(timer)) = receipt.as_mut().poll(&mut cx) else {
            panic!("far timer must commit before sleep construction");
        };
        assert!(timer.is_cancelled());
        assert!(timer.is_finished());
        let mut finished = std::pin::pin!(timer.finish());
        assert!(finished.as_mut().poll(&mut cx).is_ready());
        if graceful {
            assert_eq!(drops_at_ack.load(Ordering::SeqCst), 1);
            let mut shutdown = shutdown.lock().unwrap().take().unwrap();
            assert!(shutdown.as_mut().poll(&mut cx).is_ready());
            assert_eq!(*final_state.lock().unwrap(), Some(Vec::new()));
        } else {
            assert_eq!(drops_at_ack.load(Ordering::SeqCst), usize::MAX);
            assert!(shutdown.lock().unwrap().is_none());
            assert!(final_state.lock().unwrap().is_none());
        }
    }
}

/// Observation disposal during sleep construction retires its queued take and value.
#[test]
fn dropping_observation_during_timer_construction_closes_queued_take() {
    let clock = VirtualRuntime::new();
    let queued = Arc::new(Mutex::new(Vec::new()));
    let hook: Arc<Mutex<Option<TimerConstructionHook>>> = Arc::new(Mutex::new(None));
    let wait_drops = Arc::new(AtomicUsize::new(0));
    let store = Store::new(
        Vec::new(),
        TimerReducer,
        Runtime::new(TimerConstructionDriver {
            clock: VirtualDriver {
                shared: Arc::clone(&clock.shared),
            },
            queued: Arc::clone(&queued),
            hook: Arc::clone(&hook),
            wait_drops: Arc::clone(&wait_drops),
        }),
    );
    let mut actor = {
        let mut queued = queued.lock().unwrap();
        assert_eq!(queued.len(), 1, "only the actor is runtime-owned");
        queued.pop().unwrap()
    };
    let mut cx = Context::from_waker(Waker::noop());
    let payload = Arc::new(());
    let selected_payload = Arc::clone(&payload);
    let selections = Arc::new(AtomicUsize::new(0));
    let selected = Arc::clone(&selections);
    let mut values = {
        let mut registration = std::pin::pin!(store.observe(move |_| {
            selected.fetch_add(1, Ordering::SeqCst);
            Arc::clone(&selected_payload)
        }));
        assert!(registration.as_mut().poll(&mut cx).is_pending());
        assert!(actor.as_mut().poll(&mut cx).is_pending());
        let Poll::Ready(Ok(values)) = registration.as_mut().poll(&mut cx) else {
            panic!("actor must acknowledge observation registration");
        };
        values
    };
    {
        let mut initial = std::pin::pin!(values.next());
        let Poll::Ready(Some(value)) = initial.as_mut().poll(&mut cx) else {
            panic!("registration must retain its initial value");
        };
        assert!(Arc::ptr_eq(&value, &payload));
    };
    assert_eq!(Arc::strong_count(&payload), 2);
    let retained_payload = Arc::downgrade(&payload);
    *hook.lock().unwrap() = Some(Box::new(move || {
        // Timer commit published a slot; the actor has not run its next cleanup.
        assert_eq!(retained_payload.strong_count(), 3);
        {
            let mut next = std::pin::pin!(values.next());
            assert!(
                next.as_mut()
                    .poll(&mut Context::from_waker(Waker::noop()))
                    .is_pending()
            );
        };
        drop(values);
        // The real take is queued while the inactive entry still owns its slot.
        assert_eq!(retained_payload.strong_count(), 3);
    }));

    let receipt = store.try_send(TimerAction::ScheduleFar).unwrap();
    assert!(actor.as_mut().poll(&mut cx).is_pending());
    assert!(hook.lock().unwrap().is_none());
    assert!(queued.lock().unwrap().is_empty());
    assert_eq!(selections.load(Ordering::SeqCst), 2);
    assert_eq!(Arc::strong_count(&payload), 1);
    assert_eq!(clock.runtime().now(), MonotonicInstant::ZERO);
    assert_eq!(clock.active_sleeps(), 1);

    let mut receipt = std::pin::pin!(receipt.finish());
    let Poll::Ready(Ok(timer)) = receipt.as_mut().poll(&mut cx) else {
        panic!("timer must commit before sleep construction");
    };
    assert!(!timer.is_finished());
    timer.cancel();
    assert!(actor.as_mut().poll(&mut cx).is_pending());
    let mut finished = std::pin::pin!(timer.finish());
    assert!(finished.as_mut().poll(&mut cx).is_ready());
    assert!(timer.is_cancelled());
    assert_eq!(clock.active_sleeps(), 0);
    assert_eq!(wait_drops.load(Ordering::SeqCst), 1);

    let mut shutdown = std::pin::pin!(store.shutdown_into_state());
    assert!(shutdown.as_mut().poll(&mut cx).is_pending());
    assert!(actor.as_mut().poll(&mut cx).is_ready());
    let Poll::Ready(Ok(state)) = shutdown.as_mut().poll(&mut cx) else {
        panic!("manual actor must acknowledge shutdown");
    };
    assert!(state.is_empty());
    assert_eq!(selections.load(Ordering::SeqCst), 2);
    assert_eq!(Arc::strong_count(&payload), 1);
}

/// Mailbox work admitted during sleep construction commits without waiting for time.
#[test]
fn command_admitted_during_timer_construction_commits_before_deadline() {
    let clock = VirtualRuntime::new();
    let queued = Arc::new(Mutex::new(Vec::new()));
    let hook: Arc<Mutex<Option<TimerConstructionHook>>> = Arc::new(Mutex::new(None));
    let wait_drops = Arc::new(AtomicUsize::new(0));
    let store = Store::new(
        Vec::new(),
        TimerReducer,
        Runtime::new(TimerConstructionDriver {
            clock: VirtualDriver {
                shared: Arc::clone(&clock.shared),
            },
            queued: Arc::clone(&queued),
            hook: Arc::clone(&hook),
            wait_drops: Arc::clone(&wait_drops),
        }),
    );
    let timer_receipt = store
        .try_send(TimerAction::ScheduleOne)
        .expect("timer admission");
    let command_receipt = Arc::new(Mutex::new(None));
    let admitted = Arc::clone(&command_receipt);
    let sender = store.clone();
    *hook.lock().unwrap() = Some(Box::new(move || {
        *admitted.lock().unwrap() = Some(
            sender
                .try_send(TimerAction::External(7))
                .expect("command admission during sleep construction"),
        );
    }));
    let mut actor = {
        let mut queued = queued.lock().unwrap();
        assert_eq!(queued.len(), 1, "only the actor is runtime-owned");
        queued.pop().unwrap()
    };
    let mut cx = Context::from_waker(Waker::noop());
    for _ in 0..16 {
        assert!(actor.as_mut().poll(&mut cx).is_pending());
    }
    assert!(hook.lock().unwrap().is_none());
    assert!(queued.lock().unwrap().is_empty());
    assert_eq!(clock.runtime().now(), MonotonicInstant::ZERO);
    assert_eq!(clock.sleep_calls(), 1);
    assert_eq!(clock.active_sleeps(), 1);
    assert_eq!(wait_drops.load(Ordering::SeqCst), 0);

    let mut receipt = std::pin::pin!(timer_receipt.finish());
    let Poll::Ready(Ok(timer)) = receipt.as_mut().poll(&mut cx) else {
        panic!("timer must commit before sleep construction");
    };
    let command_receipt = command_receipt
        .lock()
        .unwrap()
        .take()
        .expect("hook receipt");
    let mut receipt = std::pin::pin!(command_receipt.finish());
    let Poll::Ready(Ok(command)) = receipt.as_mut().poll(&mut cx) else {
        panic!("ordinary command must commit without advancing the clock");
    };
    assert!(command.is_finished());
    assert!(!command.is_cancelled());
    let mut finished = std::pin::pin!(timer.finish());
    assert!(finished.as_mut().poll(&mut cx).is_pending());
    let state = {
        let mut read = std::pin::pin!(store.read(Clone::clone));
        assert!(read.as_mut().poll(&mut cx).is_pending());
        for _ in 0..16 {
            assert!(actor.as_mut().poll(&mut cx).is_pending());
        }
        let Poll::Ready(Ok(state)) = read.as_mut().poll(&mut cx) else {
            panic!("state query must complete before the timer deadline");
        };
        state
    };
    assert_eq!(state, [TimerAction::External(7)]);
    assert_eq!(clock.runtime().now(), MonotonicInstant::ZERO);
    assert!(finished.as_mut().poll(&mut cx).is_pending());
    assert_eq!(clock.sleep_calls(), 1);
    assert_eq!(wait_drops.load(Ordering::SeqCst), 0);

    clock.advance_to(MonotonicInstant::from_elapsed(Duration::from_secs(1)));
    for _ in 0..16 {
        assert!(actor.as_mut().poll(&mut cx).is_pending());
    }
    assert!(finished.as_mut().poll(&mut cx).is_ready());
    assert!(!timer.is_cancelled());
    assert_eq!(clock.active_sleeps(), 0);
    assert_eq!(wait_drops.load(Ordering::SeqCst), 1);
    assert!(queued.lock().unwrap().is_empty());

    let mut shutdown = std::pin::pin!(store.shutdown_into_state());
    assert!(shutdown.as_mut().poll(&mut cx).is_pending());
    assert!(actor.as_mut().poll(&mut cx).is_ready());
    let Poll::Ready(Ok(state)) = shutdown.as_mut().poll(&mut cx) else {
        panic!("manual actor must acknowledge shutdown");
    };
    assert_eq!(state, [TimerAction::External(7), TimerAction::Timer]);
}

/// Final-owner loss rejects a gated mailbox effect while also cancelling a live timer.
#[cfg(feature = "test-util")]
#[test]
fn final_owner_drop_rejects_gated_mailbox_effect_with_active_timer() {
    use futures_util::FutureExt as _;

    let clock = VirtualRuntime::new();
    let queued = Arc::new(Mutex::new(Vec::new()));
    let wait_drops = Arc::new(AtomicUsize::new(0));
    let delivered = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&delivered);
    let reducer = rca_core::Reduce::new(
        move |(): &mut (), action: &(u8, Arc<()>), _| match action.0 {
            0 => Effect::send_after(Duration::from_secs(60), (3, Arc::clone(&action.1))),
            1 => Effect::send((2, Arc::clone(&action.1))),
            _ => {
                counted.fetch_add(1, Ordering::SeqCst);
                Effect::none()
            }
        },
    );
    let gate = rca_core::testing::AssertionGate::new();
    let store = Store::builder(
        (),
        reducer,
        Runtime::new(TimerConstructionDriver {
            clock: VirtualDriver {
                shared: Arc::clone(&clock.shared),
            },
            queued: Arc::clone(&queued),
            hook: Arc::new(Mutex::new(None)),
            wait_drops: Arc::clone(&wait_drops),
        }),
    )
    .assertion_gate(gate.clone())
    .build()
    .unwrap();
    let timer_payload = Arc::new(());
    let timer_receipt = store.try_send((0, Arc::clone(&timer_payload))).unwrap();
    let mut actor = {
        let mut queued = queued.lock().unwrap();
        assert_eq!(queued.len(), 1);
        queued.pop().unwrap()
    };
    let mut cx = Context::from_waker(Waker::noop());
    assert!(actor.as_mut().poll(&mut cx).is_pending());
    let timer = timer_receipt.finish().now_or_never().unwrap().unwrap();
    assert_eq!(clock.active_sleeps(), 1);
    assert_eq!(wait_drops.load(Ordering::SeqCst), 0);

    let payload = Arc::new(());
    let effect_receipt = store.try_send((1, Arc::clone(&payload))).unwrap();
    assert!(actor.as_mut().poll(&mut cx).is_pending());
    let task = effect_receipt.finish().now_or_never().unwrap().unwrap();
    let mut effect = {
        let mut queued = queued.lock().unwrap();
        assert_eq!(queued.len(), 1);
        queued.pop().unwrap()
    };
    assert!(effect.as_mut().poll(&mut cx).is_pending());
    assert!(actor.as_mut().poll(&mut cx).is_pending());
    assert_eq!(
        gate.inspect_pending(|action| {
            assert!(Arc::ptr_eq(&action.1, &payload));
            action.0
        }),
        Some(2)
    );
    assert_eq!(clock.active_sleeps(), 1);
    assert!(!task.is_finished());
    assert!(!timer.is_finished());
    assert_eq!(delivered.load(Ordering::SeqCst), 0);
    let closed = store.closed();

    drop(store);
    assert!(actor.as_mut().poll(&mut cx).is_ready());
    assert!(closed.now_or_never().is_some());
    assert!(!gate.has_pending());
    assert!(gate.pending_snapshot().is_none());
    assert!(effect.as_mut().poll(&mut cx).is_ready());
    assert!(task.is_cancelled());
    assert!(task.finish().now_or_never().is_some());
    assert!(timer.is_cancelled());
    assert!(timer.finish().now_or_never().is_some());
    assert_eq!(delivered.load(Ordering::SeqCst), 0);
    assert_eq!(clock.active_sleeps(), 0);
    assert_eq!(wait_drops.load(Ordering::SeqCst), 1);
    assert_eq!(Arc::strong_count(&payload), 1);
    assert_eq!(Arc::strong_count(&timer_payload), 1);
    assert!(queued.lock().unwrap().is_empty());
}

/// Gated one-shot and interval delivery both release observations on owner loss.
#[cfg(feature = "test-util")]
#[test]
fn final_owner_drop_rejects_gated_timer_delivery_and_releases_resources() {
    for (start, pending) in [
        (TimerAction::ScheduleOne, TimerAction::Timer),
        (TimerAction::StartInterval, TimerAction::Tick),
    ] {
        let runtime = VirtualRuntime::new();
        let gate = rca_core::testing::AssertionGate::new();
        let store = Store::builder(Vec::new(), TimerReducer, runtime.runtime())
            .assertion_gate(gate.clone())
            .build()
            .expect("gated timer Store");
        let selections = Arc::new(AtomicUsize::new(0));

        futures_executor::block_on(async {
            let timer = store.send(start).await.expect("timer commit");
            let selected = Arc::clone(&selections);
            let mut values = store
                .observe(move |state| {
                    selected.fetch_add(1, Ordering::AcqRel);
                    state.clone()
                })
                .await
                .expect("state observation");
            assert_eq!(values.next().await, Some(Vec::new()));
            runtime.wait_for_sleep();

            runtime.advance_to(MonotonicInstant::from_elapsed(Duration::from_secs(1)));
            gate.wait_for_pending().await;
            assert_eq!(gate.inspect_pending(|action| *action), Some(pending));
            gate.assert_state::<Vec<TimerAction>>(|state| assert!(state.is_empty()))
                .finish()
                .await
                .expect("timer output has not committed");
            let queued = store
                .try_send(TimerAction::External(7))
                .expect("external action admitted behind gated timer");
            assert!(!timer.is_finished());
            let closed = store.closed();

            drop(store);
            closed.await;
            queued.await.expect_err("owner loss rejects queued action");
            timer.finish().await;
            assert!(timer.is_cancelled());
            assert!(timer.is_finished());
            assert!(values.next().await.is_none());
            runtime.join_all();

            assert!(!gate.has_pending());
            assert_eq!(selections.load(Ordering::Acquire), 1);
            assert_eq!(Arc::strong_count(&selections), 1);
            assert_eq!(runtime.active_sleeps(), 0);
        });
    }
}

/// Already-due delivery obeys owner lifetime, with or without an intervening command.
#[cfg(feature = "test-util")]
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "keeps due-timer arbitration and both owner-lifetime outcomes in one deterministic schedule"
)]
fn already_due_gated_timer_obeys_owner_lifetime_across_command_arbitration() {
    use futures_util::FutureExt;

    for queued_command in [false, true] {
        for close in [false, true] {
            let clock = VirtualRuntime::new();
            let queued = Arc::new(Mutex::new(Vec::new()));
            let wait_drops = Arc::new(AtomicUsize::new(0));
            let reductions = Arc::new(AtomicUsize::new(0));
            let counted = Arc::clone(&reductions);
            let reducer = rca_core::Reduce::new(
                move |state: &mut Vec<TimerAction>, action: &TimerAction, _| {
                    if *action == TimerAction::ScheduleOne {
                        Effect::send_after(Duration::ZERO, TimerAction::Timer)
                    } else {
                        if *action == TimerAction::Timer {
                            counted.fetch_add(1, Ordering::SeqCst);
                        }
                        state.push(*action);
                        Effect::none()
                    }
                },
            );
            let gate = rca_core::testing::AssertionGate::new();
            let store = Store::builder(
                Vec::new(),
                reducer,
                Runtime::new(TimerConstructionDriver {
                    clock: VirtualDriver {
                        shared: Arc::clone(&clock.shared),
                    },
                    queued: Arc::clone(&queued),
                    hook: Arc::new(Mutex::new(None)),
                    wait_drops: Arc::clone(&wait_drops),
                }),
            )
            .assertion_gate(gate.clone())
            .build()
            .unwrap();
            let scheduled = store.try_send(TimerAction::ScheduleOne).unwrap();
            let command = queued_command.then(|| store.try_send(TimerAction::External(7)).unwrap());
            let mut actor = {
                let mut queued = queued.lock().unwrap();
                assert_eq!(queued.len(), 1);
                queued.pop().unwrap()
            };
            let mut context = Context::from_waker(Waker::noop());
            assert!(actor.as_mut().poll(&mut context).is_pending());
            let timer = scheduled.finish().now_or_never().unwrap().unwrap();
            if let Some(command) = command {
                assert!(
                    command
                        .finish()
                        .now_or_never()
                        .unwrap()
                        .unwrap()
                        .is_finished()
                );
            }
            assert_eq!(
                gate.inspect_pending(|action| *action),
                Some(TimerAction::Timer)
            );
            assert!(!timer.is_finished());
            assert_eq!(reductions.load(Ordering::SeqCst), 0);
            let inspection = gate.assert_state::<Vec<TimerAction>>(move |state| {
                if queued_command {
                    assert_eq!(state, &[TimerAction::External(7)]);
                } else {
                    assert!(state.is_empty());
                }
            });
            assert!(actor.as_mut().poll(&mut context).is_pending());
            inspection.finish().now_or_never().unwrap().unwrap();

            if close {
                let closed = store.closed();
                drop(store);
                assert!(actor.as_mut().poll(&mut context).is_ready());
                assert!(closed.now_or_never().is_some());
                assert!(timer.is_finished());
                assert!(timer.is_cancelled());
                assert_eq!(reductions.load(Ordering::SeqCst), 0);
            } else {
                let release = gate.release_pending_with_receipt().unwrap();
                assert!(actor.as_mut().poll(&mut context).is_pending());
                assert!(actor.as_mut().poll(&mut context).is_pending());
                assert!(
                    release
                        .finish()
                        .now_or_never()
                        .unwrap()
                        .unwrap()
                        .is_finished()
                );
                assert!(timer.is_finished());
                assert!(!timer.is_cancelled());
                assert_eq!(reductions.load(Ordering::SeqCst), 1);
                let mut shutdown = std::pin::pin!(store.shutdown_into_state());
                assert!(shutdown.as_mut().poll(&mut context).is_pending());
                assert!(actor.as_mut().poll(&mut context).is_ready());
                let Poll::Ready(Ok(state)) = shutdown.as_mut().poll(&mut context) else {
                    panic!("released timer store must acknowledge shutdown");
                };
                if queued_command {
                    assert_eq!(state, [TimerAction::External(7), TimerAction::Timer]);
                } else {
                    assert_eq!(state, [TimerAction::Timer]);
                }
            }
            assert!(!gate.has_pending());
            assert!(queued.lock().unwrap().is_empty());
            assert_eq!(clock.runtime().now(), MonotonicInstant::ZERO);
            assert_eq!(clock.sleep_calls(), 0);
            assert_eq!(clock.active_sleeps(), 0);
            assert_eq!(wait_drops.load(Ordering::SeqCst), 0);
        }
    }
}

/// A held later timer cannot hide cancellation of an earlier deferred completion.
#[cfg(feature = "test-util")]
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "keeps both gated timer commits and intervening cancellation in one deterministic actor schedule"
)]
fn gated_timer_batch_preserves_cancellation_while_a_later_timer_is_held() {
    use futures_util::FutureExt;

    /// Identifies timer order and independently counts payload destruction.
    struct Payload {
        /// Distinguishes the first and second timer actions.
        value: u8,
        /// Checks each payload is destroyed exactly once.
        drops: Arc<AtomicUsize>,
    }

    impl Drop for Payload {
        fn drop(&mut self) {
            self.drops.fetch_add(1, Ordering::SeqCst);
        }
    }
    /// Separates timer admission from gated delivery.
    enum Action {
        /// Creates one timer with the supplied payload value.
        Schedule(u8),
        /// Owns the payload until the gated action commits or is discarded.
        Fired(Payload),
    }

    let clock = VirtualRuntime::new();
    let queued = Arc::new(Mutex::new(Vec::new()));
    let wait_drops = Arc::new(AtomicUsize::new(0));
    let payload_drops = Arc::new(AtomicUsize::new(0));
    let reductions = Arc::new(AtomicUsize::new(0));
    let owned_drops = Arc::clone(&payload_drops);
    let counted = Arc::clone(&reductions);
    let reducer = rca_core::Reduce::new(
        move |state: &mut Vec<u8>, action: &Action, _| match action {
            Action::Schedule(value) => Effect::send_after(
                Duration::from_secs(1),
                Action::Fired(Payload {
                    value: *value,
                    drops: Arc::clone(&owned_drops),
                }),
            ),
            Action::Fired(payload) => {
                counted.fetch_add(1, Ordering::SeqCst);
                state.push(payload.value);
                Effect::none()
            }
        },
    );
    let gate = rca_core::testing::TransactionGate::new();
    let store = Store::builder(
        Vec::new(),
        reducer,
        Runtime::new(TimerConstructionDriver {
            clock: VirtualDriver {
                shared: Arc::clone(&clock.shared),
            },
            queued: Arc::clone(&queued),
            hook: Arc::new(Mutex::new(None)),
            wait_drops: Arc::clone(&wait_drops),
        }),
    )
    .transaction_gate(gate.clone())
    .build()
    .unwrap();
    let mut actor = {
        let mut queued = queued.lock().unwrap();
        assert_eq!(queued.len(), 1);
        queued.pop().unwrap()
    };
    let mut context = Context::from_waker(Waker::noop());
    let mut tasks = Vec::new();
    for value in [1, 2] {
        let scheduled = store.try_send(Action::Schedule(value)).unwrap();
        assert!(actor.as_mut().poll(&mut context).is_pending());
        assert_eq!(
            gate.inspect_pending(|action| matches!(action, Action::Schedule(id) if *id == value)),
            Some(true)
        );
        let boundary = gate.boundary().unwrap();
        let release = gate.release_pending_with_receipt().unwrap();
        assert!(actor.as_mut().poll(&mut context).is_pending());
        drop(release.finish().now_or_never().unwrap().unwrap());
        let task = scheduled.finish().now_or_never().unwrap().unwrap();
        assert!(!task.is_finished());
        assert!(!task.is_cancelled());
        tasks.push(task);
        assert!(gate.acknowledge(boundary));
        assert!(actor.as_mut().poll(&mut context).is_pending());
    }
    let first = &tasks[0];
    let second = &tasks[1];
    assert_eq!(reductions.load(Ordering::SeqCst), 0);
    assert_eq!(payload_drops.load(Ordering::SeqCst), 0);
    assert_eq!(clock.active_sleeps(), 1);

    clock.set_now(MonotonicInstant::from_elapsed(Duration::from_secs(1)));
    assert!(actor.as_mut().poll(&mut context).is_pending());
    assert_eq!(
        gate.inspect_pending(
            |action| matches!(action, Action::Fired(payload) if payload.value == 1)
        ),
        Some(true)
    );
    let first_boundary = gate.boundary().unwrap();
    let release = gate.release_pending_with_receipt().unwrap();
    assert!(actor.as_mut().poll(&mut context).is_pending());
    drop(release.finish().now_or_never().unwrap().unwrap());
    assert_eq!(reductions.load(Ordering::SeqCst), 1);
    assert_eq!(payload_drops.load(Ordering::SeqCst), 1);
    assert!(!first.is_finished());
    assert!(gate.acknowledge(first_boundary));
    assert!(actor.as_mut().poll(&mut context).is_pending());
    assert_eq!(
        gate.inspect_pending(
            |action| matches!(action, Action::Fired(payload) if payload.value == 2)
        ),
        Some(true)
    );
    assert!(!first.is_finished());
    assert!(!second.is_finished());

    // The later timer's gate drains real control before the batch can publish
    // its earlier deferred completion. The two sends have independent roots.
    first.cancel();
    assert!(actor.as_mut().poll(&mut context).is_pending());
    assert!(first.is_cancelled());
    assert!(first.is_finished());
    assert!(!second.is_cancelled());
    assert!(!second.is_finished());
    assert_eq!(reductions.load(Ordering::SeqCst), 1);
    assert_eq!(payload_drops.load(Ordering::SeqCst), 1);
    let second_boundary = gate.boundary().unwrap();
    let release = gate.release_pending_with_receipt().unwrap();
    assert!(actor.as_mut().poll(&mut context).is_pending());
    drop(release.finish().now_or_never().unwrap().unwrap());
    assert!(gate.acknowledge(second_boundary));
    assert!(actor.as_mut().poll(&mut context).is_pending());
    assert!(first.is_cancelled());
    assert!(first.is_finished());
    assert!(!second.is_cancelled());
    assert!(second.is_finished());
    assert_eq!(reductions.load(Ordering::SeqCst), 2);
    assert_eq!(payload_drops.load(Ordering::SeqCst), 2);
    assert_eq!(gate.boundary(), None);
    assert!(!gate.has_pending());

    let mut shutdown = std::pin::pin!(store.shutdown_into_state());
    assert!(shutdown.as_mut().poll(&mut context).is_pending());
    assert!(actor.as_mut().poll(&mut context).is_ready());
    let Poll::Ready(Ok(state)) = shutdown.as_mut().poll(&mut context) else {
        panic!("both gated timers must leave shutdown continuable");
    };
    assert_eq!(state, [1, 2]);
    drop(actor);
    assert!(queued.lock().unwrap().is_empty());
    assert_eq!(clock.active_sleeps(), 0);
    assert_eq!(Arc::strong_count(&payload_drops), 1);
    assert_eq!(Arc::strong_count(&reductions), 1);
}
