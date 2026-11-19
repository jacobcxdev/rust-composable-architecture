//! Qualifies lazy debounce and an explicit reducer-owned fixed-window throttle.

use std::collections::HashMap;
use std::fmt;
use std::pin::Pin;
use std::sync::{Arc, Mutex, Weak};
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use rca_core::ClockDriver;
use rca_core::{
    ActionPath, CaseStatePath, Clock, DependencyKey, Effect, EffectSender, KeyedAction,
    KeyedActionPath, KeyedState, KeyedStatePath, MonotonicInstant, OptionalStatePath,
    ReduceContext, Reducer, ReducerExt, Runtime, RuntimeDriver, RuntimeFuture, Store,
};

/// Fixed throttle window used by the independent timestamp assertions.
const WINDOW: Duration = Duration::from_secs(10);

/// Expresses test deadlines without relying on elapsed wall time.
fn time(seconds: u64) -> MonotonicInstant {
    MonotonicInstant::from_elapsed(Duration::from_secs(seconds))
}

/// A manually advanced clock that wakes waiters after releasing its lock.
#[derive(Clone, Debug, Default)]
struct ControlledClock(Arc<Mutex<ClockState>>);

/// Current time and the latest waker for each live sleep future.
#[derive(Debug)]
struct ClockState {
    now: MonotonicInstant,
    next_wait: u64,
    waits: HashMap<u64, Waker>,
}

impl Default for ClockState {
    fn default() -> Self {
        Self {
            now: MonotonicInstant::ZERO,
            next_wait: 0,
            waits: HashMap::new(),
        }
    }
}

impl ControlledClock {
    /// Moves time forward and lets each sleeping task check its own deadline.
    fn set(&self, seconds: u64) {
        let wakers = {
            let mut state = self.0.lock().unwrap();
            assert!(time(seconds) >= state.now);
            state.now = time(seconds);
            std::mem::take(&mut state.waits)
        };
        for waker in wakers.into_values() {
            waker.wake();
        }
    }
}

/// Removes its own clock registration when completed or cancelled.
struct Wait {
    clock: ControlledClock,
    id: u64,
    deadline: MonotonicInstant,
}

impl Future for Wait {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        let waker = cx.waker().clone();
        let (result, retired) = {
            let mut state = self.clock.0.lock().unwrap();
            if state.now >= self.deadline {
                (Poll::Ready(()), state.waits.remove(&self.id))
            } else {
                (Poll::Pending, state.waits.insert(self.id, waker))
            }
        };
        drop(retired);
        result
    }
}

impl Drop for Wait {
    fn drop(&mut self) {
        let retired = self.clock.0.lock().unwrap().waits.remove(&self.id);
        drop(retired);
    }
}

impl ClockDriver for ControlledClock {
    fn now(&self) -> MonotonicInstant {
        self.0.lock().unwrap().now
    }

    fn sleep_until(&self, deadline: MonotonicInstant) -> RuntimeFuture {
        let id = {
            let mut state = self.0.lock().unwrap();
            state.next_wait += 1;
            state.next_wait
        };
        Box::pin(Wait {
            clock: self.clone(),
            id,
            deadline,
        })
    }
}

#[test]
fn replaced_and_cancelled_clock_wakers_drop_outside_the_clock_lock() {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::task::Wake;

    /// Records clock access during the final release of a registered task waker.
    struct DropProbe {
        clock: Weak<Mutex<ClockState>>,
        drops: Arc<AtomicUsize>,
        unlocked: Arc<AtomicBool>,
    }

    #[expect(
        clippy::manual_noop_waker,
        reason = "the owned waker's destructor observes clock lock release"
    )]
    impl Wake for DropProbe {
        fn wake(self: Arc<Self>) {}
    }

    impl Drop for DropProbe {
        fn drop(&mut self) {
            let clock = self.clock.upgrade().expect("the test still owns its clock");
            self.unlocked
                .store(clock.try_lock().is_ok(), Ordering::Relaxed);
            self.drops.fetch_add(1, Ordering::Relaxed);
        }
    }

    let mut unlocked_on_release = Vec::new();
    for cancel in [false, true] {
        let clock = ControlledClock::default();
        let drops = Arc::new(AtomicUsize::new(0));
        let unlocked = Arc::new(AtomicBool::new(false));
        let mut wait = clock.sleep_until(time(1));
        {
            let waker = Waker::from(Arc::new(DropProbe {
                clock: Arc::downgrade(&clock.0),
                drops: Arc::clone(&drops),
                unlocked: Arc::clone(&unlocked),
            }));
            assert!(
                wait.as_mut()
                    .poll(&mut Context::from_waker(&waker))
                    .is_pending()
            );
        };
        assert_eq!(drops.load(Ordering::Relaxed), 0);

        if !cancel {
            assert!(
                wait.as_mut()
                    .poll(&mut Context::from_waker(Waker::noop()))
                    .is_pending()
            );
        }
        drop(wait);
        assert_eq!(drops.load(Ordering::Relaxed), 1);
        unlocked_on_release.push(unlocked.load(Ordering::Relaxed));
        assert!(clock.0.lock().unwrap().waits.is_empty());
    }
    assert_eq!(
        unlocked_on_release,
        [true, true],
        "replaced and cancelled wakers must drop outside the clock lock"
    );
}

/// Owns queued tasks; drivers do not keep the queue alive after this host drops.
struct Host {
    clock: ControlledClock,
    tasks: Arc<Mutex<Vec<RuntimeFuture>>>,
}

impl fmt::Debug for Host {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Host").finish_non_exhaustive()
    }
}

impl Host {
    /// Starts with an empty queue and a clock at zero.
    fn new() -> Self {
        Self {
            clock: ControlledClock::default(),
            tasks: Arc::default(),
        }
    }

    /// Gives a Store access to this host without transferring task ownership.
    fn driver(&self) -> Driver {
        Driver {
            clock: self.clock.clone(),
            tasks: Arc::downgrade(&self.tasks),
        }
    }

    /// Polls a fixed number of rounds without threads, wall time or ambient IO.
    fn pump(&self) {
        for _ in 0..32 {
            let tasks = std::mem::take(&mut *self.tasks.lock().unwrap());
            let pending = tasks
                .into_iter()
                .filter_map(|mut task| {
                    task.as_mut()
                        .poll(&mut Context::from_waker(Waker::noop()))
                        .is_pending()
                        .then_some(task)
                })
                .collect::<Vec<_>>();
            self.tasks.lock().unwrap().extend(pending);
        }
    }

    /// Completes a receipt or fails within a fixed number of polling rounds.
    fn complete<F: Future>(&self, future: F) -> F::Output {
        let mut future = std::pin::pin!(future);
        for _ in 0..512 {
            if let Poll::Ready(output) = future
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
            {
                self.pump();
                return output;
            }
            self.pump();
        }
        panic!("controlled operation did not complete");
    }

    /// Advances the test clock, then gives queued tasks a chance to observe it.
    fn advance(&self, seconds: u64) {
        self.clock.set(seconds);
        self.pump();
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        let tasks = std::mem::take(&mut *self.tasks.lock().unwrap());
        drop(tasks);
    }
}

/// Borrows the host's queue so a queued actor cannot retain its own executor.
#[derive(Debug)]
struct Driver {
    clock: ControlledClock,
    tasks: Weak<Mutex<Vec<RuntimeFuture>>>,
}

impl RuntimeDriver for Driver {
    fn spawn(&self, task: RuntimeFuture) {
        if let Some(tasks) = self.tasks.upgrade() {
            tasks.lock().unwrap().push(task);
        }
    }

    fn now(&self) -> MonotonicInstant {
        ClockDriver::now(&self.clock)
    }
    fn sleep_until(&self, deadline: MonotonicInstant) -> RuntimeFuture {
        ClockDriver::sleep_until(&self.clock, deadline)
    }
    fn yield_now(&self) -> RuntimeFuture {
        let mut yielded = false;
        Box::pin(futures_util::future::poll_fn(move |cx| {
            if yielded {
                Poll::Ready(())
            } else {
                yielded = true;
                cx.waker().wake_by_ref();
                Poll::Pending
            }
        }))
    }
}

#[test]
fn dropped_host_releases_an_unpolled_store_actor() {
    let state = Arc::new(());
    let retained = Arc::downgrade(&state);
    {
        let host = Host::new();
        let store = Store::new(
            state,
            rca_core::Reduce::new(|_: &mut Arc<()>, (): &(), _| Effect::none()),
            Runtime::new(host.driver()),
        );
        assert_eq!(host.tasks.lock().unwrap().len(), 1);
        drop(store);
    };
    assert!(
        retained.upgrade().is_none(),
        "dropping the host must release its unpolled Store actor"
    );
}

#[test]
fn driver_drops_new_tasks_after_its_host_is_gone() {
    let state = Arc::new(());
    let retained = Arc::downgrade(&state);
    let driver = {
        let host = Host::new();
        host.driver()
    };
    driver.spawn(Box::pin(async move {
        drop(state);
    }));
    assert!(retained.upgrade().is_none());
}

#[test]
fn default_clock_tolerance_rejects_before_constructing_or_registering_waits() {
    let clock = ControlledClock::default();
    for deadline in [MonotonicInstant::ZERO, time(3)] {
        for tolerance in [Duration::ZERO, Duration::MAX] {
            let error = ClockDriver::sleep_until_with_tolerance(&clock, deadline, tolerance)
                .err()
                .expect("default metadata does not promise tolerance");
            assert_eq!(error, rca_core::UnsupportedClockTolerance);
            assert_eq!(
                error.to_string(),
                "clock does not support explicit timer tolerance"
            );
        }
    }
    assert_eq!(clock.0.lock().unwrap().next_wait, 0);
    assert!(clock.0.lock().unwrap().waits.is_empty());
    assert_eq!(clock.now(), MonotonicInstant::ZERO);

    let mut exact = clock.sleep_until(time(3));
    assert_eq!(clock.0.lock().unwrap().next_wait, 1);
    assert_eq!(
        exact.as_mut().poll(&mut Context::from_waker(Waker::noop())),
        Poll::Pending
    );
    assert_eq!(clock.0.lock().unwrap().waits.len(), 1);
    clock.set(3);
    assert_eq!(
        exact.as_mut().poll(&mut Context::from_waker(Waker::noop())),
        Poll::Ready(())
    );
    assert!(clock.0.lock().unwrap().waits.is_empty());
}

#[test]
fn default_runtime_tolerance_rejects_without_spawning_or_constructing_waits() {
    let host = Host::new();
    let driver = host.driver();
    assert_eq!(driver.clock_capabilities().resolution, None);
    assert_eq!(driver.clock_capabilities().minimum_tolerance, None);
    for deadline in [MonotonicInstant::ZERO, time(3)] {
        for tolerance in [Duration::ZERO, Duration::MAX] {
            assert_eq!(
                RuntimeDriver::sleep_until_with_tolerance(&driver, deadline, tolerance)
                    .err()
                    .expect("a runtime must explicitly declare tolerance support"),
                rca_core::UnsupportedClockTolerance
            );
        }
    }
    assert_eq!(host.clock.0.lock().unwrap().next_wait, 0);
    assert!(host.clock.0.lock().unwrap().waits.is_empty());
    assert!(host.tasks.lock().unwrap().is_empty());
    assert_eq!(driver.now(), MonotonicInstant::ZERO);

    let mut exact = RuntimeDriver::sleep_until(&driver, time(3));
    assert_eq!(host.clock.0.lock().unwrap().next_wait, 1);
    assert_eq!(
        exact.as_mut().poll(&mut Context::from_waker(Waker::noop())),
        Poll::Pending
    );
    assert_eq!(host.clock.0.lock().unwrap().waits.len(), 1);
    drop(exact);
    assert!(host.clock.0.lock().unwrap().waits.is_empty());
    assert!(host.tasks.lock().unwrap().is_empty());
}

/// Requires each test to choose the feature clock explicitly.
struct FeatureClock;
impl DependencyKey for FeatureClock {
    type Value = Clock;
    fn live_value() -> Clock {
        panic!("the feature clock must be explicitly injected")
    }
}

/// Chooses debounce, first pending value, or latest pending value.
#[derive(Clone, Copy)]
enum Mode {
    Debounce,
    First,
    Latest,
}

/// Keeps quiet-window work separate from the fixed-window timer.
#[derive(Clone, Copy, Eq, Hash, PartialEq)]
enum CancelId {
    Debounce,
    Window,
}

/// Records input, timer identity and output as separate admitted actions.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Action {
    Input(u8),
    Tick(u64),
    Emitted(u8),
    Cancel,
}

/// Retains literal output timestamps and the current reducer-owned window.
#[derive(Default)]
struct FeatureState {
    output: Vec<(u8, u64)>,
    actions: Vec<Action>,
    /// Distinguishes successive timers even when an old tick remains queued.
    next_nonce: u64,
    window: Option<Window>,
}

/// Holds one fixed deadline and, at most, one deferred output value.
struct Window {
    deadline: MonotonicInstant,
    nonce: u64,
    pending: Option<u8>,
}

/// Implements the policy under test using ordinary effects and an injected clock.
#[derive(Clone)]
struct Feature {
    mode: Mode,
    /// Records client entry separately from output so eager work is detectable.
    starts: Arc<Mutex<Vec<(u8, u64)>>>,
    /// Retains real scoped senders for stale-output admission controls.
    timer_senders: Arc<Mutex<Vec<EffectSender<Action>>>>,
}

impl Feature {
    /// Creates empty shared observations for one selected policy.
    fn new(mode: Mode) -> Self {
        Self {
            mode,
            starts: Arc::default(),
            timer_senders: Arc::default(),
        }
    }

    /// Starts a fresh timer anchored to the time actually observed by the reducer.
    fn open(
        &self,
        state: &mut FeatureState,
        clock: Clock,
        now: MonotonicInstant,
    ) -> Effect<Action> {
        state.next_nonce = state
            .next_nonce
            .checked_add(1)
            .expect("timer nonce exhausted");
        let nonce = state.next_nonce;
        let deadline = now.saturating_add(WINDOW);
        state.window = Some(Window {
            deadline,
            nonce,
            pending: None,
        });
        let senders = Arc::clone(&self.timer_senders);
        Effect::run(move |sender| async move {
            senders.lock().unwrap().push(sender.clone());
            clock.sleep_until(deadline).await;
            let _ = sender.send(Action::Tick(nonce)).await;
        })
        // A timer-fired Tick may replace its originating timer. Transfer its
        // inherited cancellation, but preserve structural and root ownership.
        .without_inherited_cancellation()
        .cancellable_in_flight(CancelId::Window)
    }

    /// Emits a due pending value and opens its successor window, if needed.
    fn flush(
        &self,
        state: &mut FeatureState,
        clock: Clock,
        now: MonotonicInstant,
    ) -> Effect<Action> {
        let window = state.window.take().expect("flush requires a due window");
        if let Some(value) = window.pending {
            state.output.push((value, now.elapsed().as_secs()));
            self.open(state, clock, now)
        } else {
            Effect::cancel(CancelId::Window)
        }
    }
}

impl Reducer for Feature {
    type State = FeatureState;
    type Action = Action;

    fn reduce(
        &mut self,
        state: &mut FeatureState,
        action: &Action,
        context: &mut ReduceContext<'_>,
    ) -> Effect<Action> {
        state.actions.push(*action);
        if matches!(action, Action::Cancel) {
            state.window = None;
            return Effect::merge([
                Effect::cancel(CancelId::Debounce),
                Effect::cancel(CancelId::Window),
            ]);
        }
        let clock = context.dependency::<FeatureClock>().clone();
        match *action {
            Action::Input(value) if matches!(self.mode, Mode::Debounce) => {
                let starts = Arc::clone(&self.starts);
                Effect::run(move |sender| async move {
                    starts
                        .lock()
                        .unwrap()
                        .push((value, clock.now().elapsed().as_secs()));
                    let _ = sender.send(Action::Emitted(value)).await;
                })
                .delay(WINDOW)
                .cancellable_in_flight(CancelId::Debounce)
            }
            Action::Input(value) => {
                let now = clock.now();
                let mut effect = Effect::none();
                if state
                    .window
                    .as_ref()
                    .is_some_and(|window| now >= window.deadline)
                {
                    effect = self.flush(state, clock.clone(), now);
                }
                if let Some(window) = &mut state.window {
                    if window.pending.is_none() || matches!(self.mode, Mode::Latest) {
                        window.pending = Some(value);
                    }
                    effect
                } else {
                    state.output.push((value, now.elapsed().as_secs()));
                    self.open(state, clock, now)
                }
            }
            Action::Tick(nonce) => {
                let now = clock.now();
                if state
                    .window
                    .as_ref()
                    .is_some_and(|window| window.nonce == nonce && now >= window.deadline)
                {
                    self.flush(state, clock, now)
                } else {
                    Effect::none()
                }
            }
            Action::Emitted(value) => {
                state.output.push((value, clock.now().elapsed().as_secs()));
                Effect::none()
            }
            Action::Cancel => unreachable!(),
        }
    }
}

/// Builds a root whose feature clock can intentionally differ from its runtime clock.
fn root(host: &Host, feature: Feature, clock: Clock) -> Store<FeatureState, Action> {
    Store::builder(
        FeatureState::default(),
        feature,
        Runtime::new(host.driver()),
    )
    .with_dependency::<FeatureClock>(clock)
    .build()
    .unwrap()
}

/// Waits for admission while leaving causal work running under clock control.
fn send<S: Send + 'static, A: Send + 'static>(host: &Host, store: &Store<S, A>, action: A) {
    drop(host.complete(store.send(action)).unwrap());
}

/// Takes an ordered snapshot of emitted values and their observed timestamps.
fn outputs(host: &Host, store: &Store<FeatureState, Action>) -> Vec<(u8, u64)> {
    host.complete(store.read(|state| state.output.clone()))
        .unwrap()
}

// Break caught: eager effect factory/client work or an in-flight ID registered
// inside rather than outside the delay would start superseded requests.
#[test]
fn debounce_starts_only_the_latest_client_after_its_quiet_window() {
    let host = Host::new();
    let feature = Feature::new(Mode::Debounce);
    let starts = Arc::clone(&feature.starts);
    let store = root(&host, feature, Clock::new(host.clock.clone()));
    send(&host, &store, Action::Input(1));
    host.advance(3);
    send(&host, &store, Action::Input(2));
    host.advance(8);
    send(&host, &store, Action::Input(3));
    host.advance(17);
    assert!(starts.lock().unwrap().is_empty());
    assert_eq!(outputs(&host, &store), [] as [(u8, u64); 0]);
    host.advance(18);
    assert_eq!(*starts.lock().unwrap(), [(3, 18)]);
    assert_eq!(outputs(&host, &store), [(3, 18)]);
    host.complete(store.shutdown()).unwrap();
}

#[test]
fn debounce_cancel_retires_wait_before_any_client_starts() {
    let host = Host::new();
    let feature = Feature::new(Mode::Debounce);
    let starts = Arc::clone(&feature.starts);
    let store = root(&host, feature, Clock::new(host.clock.clone()));
    send(&host, &store, Action::Input(7));
    send(&host, &store, Action::Cancel);
    host.advance(20);
    assert!(starts.lock().unwrap().is_empty());
    assert_eq!(outputs(&host, &store), [] as [(u8, u64); 0]);
    send(&host, &store, Action::Input(8));
    host.advance(30);
    assert_eq!(*starts.lock().unwrap(), [(8, 30)]);
    host.complete(store.shutdown()).unwrap();
}

// Break caught: first policy overwritten by later inputs, latest retaining the
// first input, or moving the fixed deadline when another input arrives.
#[test]
fn throttle_first_and_latest_keep_a_fixed_window() {
    for (mode, expected) in [(Mode::First, 2), (Mode::Latest, 3)] {
        let host = Host::new();
        let store = root(&host, Feature::new(mode), Clock::new(host.clock.clone()));
        send(&host, &store, Action::Input(1));
        host.advance(2);
        send(&host, &store, Action::Input(2));
        host.advance(7);
        send(&host, &store, Action::Input(3));
        host.advance(9);
        assert_eq!(outputs(&host, &store), [(1, 0)]);
        host.advance(10);
        assert_eq!(outputs(&host, &store), [(1, 0), (expected, 10)]);
        host.advance(20);
        assert_eq!(outputs(&host, &store), [(1, 0), (expected, 10)]);
        host.complete(store.shutdown()).unwrap();
    }
}

// Break caught: input replacing due pending work before flush, old Tick
// duplicating a flush, or anchoring the next interval to the old deadline.
#[test]
fn due_pending_flushes_before_input_for_both_actor_orderings() {
    for mode in [Mode::First, Mode::Latest] {
        for tick_first in [false, true] {
            for (observed, before_next, next, after_flush, after_next) in [
                (
                    10,
                    19,
                    20,
                    vec![(1, 0), (2, 10)],
                    vec![(1, 0), (2, 10), (3, 20)],
                ),
                (
                    12,
                    21,
                    22,
                    vec![(1, 0), (2, 12)],
                    vec![(1, 0), (2, 12), (3, 22)],
                ),
            ] {
                let host = Host::new();
                let store = root(&host, Feature::new(mode), Clock::new(host.clock.clone()));
                send(&host, &store, Action::Input(1));
                host.advance(2);
                send(&host, &store, Action::Input(2));
                let old_nonce = host
                    .complete(store.read(|state| state.window.as_ref().unwrap().nonce))
                    .unwrap();
                // Set observed time without polling to admit input before timer.
                host.clock.set(observed);
                if tick_first {
                    host.pump();
                }
                send(&host, &store, Action::Input(3));
                let actions = host
                    .complete(store.read(|state| state.actions.clone()))
                    .unwrap();
                if tick_first {
                    assert_eq!(
                        actions,
                        [
                            Action::Input(1),
                            Action::Input(2),
                            Action::Tick(1),
                            Action::Input(3)
                        ]
                    );
                } else {
                    assert_eq!(
                        actions,
                        [Action::Input(1), Action::Input(2), Action::Input(3)]
                    );
                }
                send(&host, &store, Action::Tick(old_nonce));
                assert_eq!(outputs(&host, &store), after_flush);
                host.advance(before_next);
                assert_eq!(outputs(&host, &store), after_flush);
                host.advance(next);
                assert_eq!(outputs(&host, &store), after_next);
                host.complete(store.shutdown()).unwrap();
            }
        }
    }
}

#[test]
fn elapsed_window_without_pending_emits_incoming_input_immediately() {
    for tick_first in [false, true] {
        let host = Host::new();
        let store = root(
            &host,
            Feature::new(Mode::Latest),
            Clock::new(host.clock.clone()),
        );
        send(&host, &store, Action::Input(1));
        host.clock.set(12);
        if tick_first {
            host.pump();
        }
        send(&host, &store, Action::Input(2));
        send(&host, &store, Action::Input(3));
        assert_eq!(outputs(&host, &store), [(1, 0), (2, 12)]);
        host.advance(22);
        assert_eq!(outputs(&host, &store), [(1, 0), (2, 12), (3, 22)]);
        host.complete(store.shutdown()).unwrap();
    }
}

#[test]
fn cancel_retires_pending_value_and_nonce_without_duplicate_output() {
    let host = Host::new();
    let store = root(
        &host,
        Feature::new(Mode::First),
        Clock::new(host.clock.clone()),
    );
    send(&host, &store, Action::Input(1));
    send(&host, &store, Action::Input(2));
    send(&host, &store, Action::Cancel);
    send(&host, &store, Action::Input(3));
    send(&host, &store, Action::Input(4));
    host.clock.set(10);
    send(&host, &store, Action::Tick(1));
    // Both stale nonce 1 and current nonce 2 are due. The genuine timer must
    // still commit: output alone cannot distinguish which action flushed 4.
    let actions = host
        .complete(store.read(|state| state.actions.clone()))
        .unwrap();
    assert_eq!(
        actions,
        [
            Action::Input(1),
            Action::Input(2),
            Action::Cancel,
            Action::Input(3),
            Action::Input(4),
            Action::Tick(1),
            Action::Tick(2),
        ]
    );
    assert_eq!(outputs(&host, &store), [(1, 0), (3, 0), (4, 10)]);
    host.advance(20);
    assert_eq!(outputs(&host, &store), [(1, 0), (3, 0), (4, 10)]);
    host.complete(store.shutdown()).unwrap();
}

// Break caught: using the Store/runtime clock in a feature timer instead of
// the injected clock. The deliberately wrong clock is a negative witness.
#[test]
fn independent_wrong_clock_cannot_be_advanced_by_the_runtime_clock() {
    let host = Host::new();
    let independent = ControlledClock::default();
    let store = root(
        &host,
        Feature::new(Mode::Latest),
        Clock::new(independent.clone()),
    );
    send(&host, &store, Action::Input(1));
    send(&host, &store, Action::Input(2));
    host.advance(100);
    assert_eq!(outputs(&host, &store), [(1, 0)]);
    independent.set(10);
    host.pump();
    assert_eq!(outputs(&host, &store), [(1, 0), (2, 10)]);
    host.complete(store.shutdown()).unwrap();
}

/// Applies the same retirement schedule to all three child lifetime forms.
#[derive(Clone, Copy)]
enum Target {
    Optional,
    Case,
    Keyed,
}

/// Lets case replacement retire the same kind of feature as optional removal.
enum Case {
    Empty,
    Active(FeatureState),
}

/// Holds independent children that deliberately share local cancellation identities.
struct ParentState {
    optional: Option<FeatureState>,
    case: Case,
    rows: KeyedState<u8, FeatureState>,
}

/// Routes child actions and parent-owned removal or reinsertion.
enum ParentAction {
    Optional(Action),
    Case(Action),
    Row(KeyedAction<u8, Action>),
    Remove(Target),
    Insert(Target),
}

/// Changes child presence without emitting work of its own.
struct Parent;
impl Reducer for Parent {
    type State = ParentState;
    type Action = ParentAction;
    fn reduce(
        &mut self,
        state: &mut ParentState,
        action: &ParentAction,
        _: &mut ReduceContext<'_>,
    ) -> Effect<ParentAction> {
        match action {
            ParentAction::Remove(Target::Optional) => state.optional = None,
            ParentAction::Remove(Target::Case) => state.case = Case::Empty,
            ParentAction::Remove(Target::Keyed) => {
                state.rows.remove(&7);
            }
            ParentAction::Insert(Target::Optional) => {
                state.optional = Some(FeatureState::default());
            }
            ParentAction::Insert(Target::Case) => {
                state.case = Case::Active(FeatureState::default());
            }
            ParentAction::Insert(Target::Keyed) => {
                state.rows.insert(7, FeatureState::default());
            }
            ParentAction::Optional(_) | ParentAction::Case(_) | ParentAction::Row(_) => {}
        }
        Effect::none()
    }
}

/// Routes a feature action to the selected child, using the reused row key seven.
fn parent_action(target: Target, action: Action) -> ParentAction {
    match target {
        Target::Optional => ParentAction::Optional(action),
        Target::Case => ParentAction::Case(action),
        Target::Keyed => ParentAction::Row(KeyedAction::new(7, action)),
    }
}

/// Composes all child forms through their real state and action paths.
fn parent(host: &Host, feature: Feature) -> Store<ParentState, ParentAction> {
    let reducer = Parent
        .if_some(
            OptionalStatePath::new(
                |state: &ParentState| state.optional.as_ref(),
                |state: &mut ParentState| state.optional.as_mut(),
            ),
            ActionPath::new(
                |action: &ParentAction| match action {
                    ParentAction::Optional(action) => Some(action),
                    _ => None,
                },
                ParentAction::Optional,
            ),
            feature.clone(),
        )
        .if_case(
            CaseStatePath::new(
                |state: &ParentState| match &state.case {
                    Case::Active(child) => Some(child),
                    Case::Empty => None,
                },
                |state: &mut ParentState| match &mut state.case {
                    Case::Active(child) => Some(child),
                    Case::Empty => None,
                },
            ),
            ActionPath::new(
                |action: &ParentAction| match action {
                    ParentAction::Case(action) => Some(action),
                    _ => None,
                },
                ParentAction::Case,
            ),
            feature.clone(),
        )
        .for_each(
            KeyedStatePath::new(
                |state: &ParentState| &state.rows,
                |state: &mut ParentState| &mut state.rows,
            ),
            KeyedActionPath::new(
                |action: &ParentAction| match action {
                    ParentAction::Row(action) => Some(action),
                    _ => None,
                },
                ParentAction::Row,
            ),
            feature,
        );
    let mut rows = KeyedState::new();
    rows.insert(7, FeatureState::default());
    Store::builder(
        ParentState {
            optional: Some(FeatureState::default()),
            case: Case::Active(FeatureState::default()),
            rows,
        },
        reducer,
        Runtime::new(host.driver()),
    )
    .with_dependency::<FeatureClock>(Clock::new(host.clock.clone()))
    .build()
    .unwrap()
}

/// Reads the current child's output, failing if the test did not restore that child.
fn child_output(
    host: &Host,
    store: &Store<ParentState, ParentAction>,
    target: Target,
) -> Vec<(u8, u64)> {
    host.complete(store.read(move |state| match target {
        Target::Optional => state.optional.as_ref().unwrap().output.clone(),
        Target::Case => match &state.case {
            Case::Active(child) => child.output.clone(),
            Case::Empty => panic!("case must be active"),
        },
        Target::Keyed => state.rows.get(&7).unwrap().output.clone(),
    }))
    .unwrap()
}

// Break caught: framework structural generations failing to reject old effect
// senders after removal and reinsertion. New state deliberately reuses nonce 1.
#[test]
fn optional_case_and_same_key_reinsert_cannot_release_old_throttle_pending() {
    for target in [Target::Optional, Target::Case, Target::Keyed] {
        for mode in [Mode::First, Mode::Latest] {
            let host = Host::new();
            let feature = Feature::new(mode);
            let senders = Arc::clone(&feature.timer_senders);
            let store = parent(&host, feature);
            send(&host, &store, parent_action(target, Action::Input(1)));
            send(&host, &store, parent_action(target, Action::Input(2)));
            let old_sender = senders.lock().unwrap()[0].clone();
            send(&host, &store, ParentAction::Remove(target));
            send(&host, &store, ParentAction::Insert(target));
            send(&host, &store, parent_action(target, Action::Input(3)));
            send(&host, &store, parent_action(target, Action::Input(4)));
            host.clock.set(10);
            assert!(host.complete(old_sender.send(Action::Tick(1))).is_err());
            host.pump();
            assert_eq!(child_output(&host, &store, target), [(3, 0), (4, 10)]);
            host.advance(20);
            assert_eq!(child_output(&host, &store, target), [(3, 0), (4, 10)]);
            host.complete(store.shutdown()).unwrap();
        }
    }
}

#[test]
fn debounce_child_removal_prevents_client_start_and_keeps_equal_id_siblings() {
    for target in [Target::Optional, Target::Case, Target::Keyed] {
        let host = Host::new();
        let feature = Feature::new(Mode::Debounce);
        let starts = Arc::clone(&feature.starts);
        let store = parent(&host, feature);
        for sibling in [Target::Optional, Target::Case, Target::Keyed] {
            send(&host, &store, parent_action(sibling, Action::Input(9)));
        }
        send(&host, &store, ParentAction::Remove(target));
        send(&host, &store, ParentAction::Insert(target));
        host.advance(10);
        assert_eq!(starts.lock().unwrap().len(), 2);
        assert_eq!(child_output(&host, &store, target), [] as [(u8, u64); 0]);
        send(&host, &store, parent_action(target, Action::Input(8)));
        host.advance(20);
        assert_eq!(child_output(&host, &store, target), [(8, 20)]);
        assert_eq!(*starts.lock().unwrap(), [(9, 10), (9, 10), (8, 20)]);
        host.complete(store.shutdown()).unwrap();
    }
}

// Break caught: admitting an interval descendant before scope retirement must
// not let it mutate a same-key replacement, or cancel a surviving sibling.
#[cfg(feature = "test-util")]
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one controlled episode exposes interval, admission, retirement and cleanup order"
)]
fn queued_interval_output_is_rejected_after_child_reinsertion() {
    /// Distinguishes an interval tick from output sent by that tick's descendant.
    #[derive(Clone, Copy, Debug)]
    enum TickAction {
        Start,
        Pulse,
        Output,
    }

    /// Exports a scoped sender from each tick while keeping its effect unfinished.
    struct Ticker(Arc<Mutex<Vec<EffectSender<TickAction>>>>);

    impl Reducer for Ticker {
        type State = u32;
        type Action = TickAction;

        fn reduce(
            &mut self,
            state: &mut u32,
            action: &TickAction,
            _context: &mut ReduceContext<'_>,
        ) -> Effect<TickAction> {
            match action {
                TickAction::Start => Effect::interval(WINDOW, || TickAction::Pulse),
                TickAction::Pulse => {
                    *state += 1;
                    let senders = Arc::clone(&self.0);
                    Effect::run(move |sender| async move {
                        senders.lock().unwrap().push(sender);
                        std::future::pending::<()>().await;
                    })
                }
                TickAction::Output => {
                    *state += 100;
                    Effect::none()
                }
            }
        }
    }

    /// Orders removal, same-key reinsertion and an independent capacity probe.
    #[derive(Debug)]
    enum Command {
        Row(KeyedAction<u8, TickAction>),
        Remove,
        Insert,
        Probe,
    }

    /// Changes only row one so row two can prove sibling timer independence.
    struct Rows;

    impl Reducer for Rows {
        type State = KeyedState<u8, u32>;
        type Action = Command;

        fn reduce(
            &mut self,
            state: &mut KeyedState<u8, u32>,
            action: &Command,
            _context: &mut ReduceContext<'_>,
        ) -> Effect<Command> {
            match action {
                Command::Remove => {
                    state.remove(&1);
                }
                Command::Insert => {
                    state.insert(1, 0);
                }
                Command::Row(_) | Command::Probe => {}
            }
            Effect::none()
        }
    }

    let host = Host::new();
    let senders = Arc::new(Mutex::new(Vec::new()));
    let reducer = Rows.for_each(
        KeyedStatePath::new(
            |state: &KeyedState<u8, u32>| state,
            |state: &mut KeyedState<u8, u32>| state,
        ),
        KeyedActionPath::new(
            |action: &Command| match action {
                Command::Row(action) => Some(action),
                _ => None,
            },
            Command::Row,
        ),
        Ticker(Arc::clone(&senders)),
    );
    let mut rows = KeyedState::new();
    rows.insert(1, 0);
    rows.insert(2, 0);
    let store = Store::builder(rows, reducer, Runtime::new(host.driver()))
        .mailbox_capacity(3)
        .build()
        .unwrap();
    let old = host
        .complete(store.send(Command::Row(KeyedAction::new(1, TickAction::Start))))
        .unwrap();
    host.advance(10);
    assert_eq!(senders.lock().unwrap().len(), 1);
    let old_sender = senders.lock().unwrap()[0].clone();
    let sibling = host
        .complete(store.send(Command::Row(KeyedAction::new(2, TickAction::Start))))
        .unwrap();
    assert_eq!(
        host.complete(store.read(|state| (*state.get(&1).unwrap(), *state.get(&2).unwrap())))
            .unwrap(),
        (1, 0)
    );

    // The timer pulse already committed in the actor; it is not a mailbox item.
    // Do not drive the host while admitting removal, reinsertion and the pulse's
    // descendant output in that order. A full three-slot mailbox proves output
    // admission while the old child is still live. The ordinary actor commit
    // path then rejects it after removal and reinsertion, without an assertion gate.
    let removal = store.try_send(Command::Remove).unwrap();
    let insertion = store.try_send(Command::Insert).unwrap();
    let mut output = std::pin::pin!(old_sender.send(TickAction::Output));
    assert!(
        output
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_pending()
    );
    assert!(store.try_send(Command::Probe).unwrap_err().is_full());
    assert!(!old.is_finished());
    let removed = host.complete(removal.finish()).unwrap();
    let _inserted_task = host.complete(insertion.finish()).unwrap();
    let error = host.complete(output).unwrap_err();
    assert!(error.is_cancelled());
    assert!(!error.is_store_stopped());
    host.complete(removed.finish());
    host.complete(old.finish());
    assert!(old.is_finished());
    assert!(!sibling.is_finished());
    assert!(!sibling.is_cancelled());
    assert_eq!(
        host.complete(store.read(|state| (*state.get(&1).unwrap(), *state.get(&2).unwrap())))
            .unwrap(),
        (0, 0)
    );

    let fresh = host
        .complete(store.send(Command::Row(KeyedAction::new(1, TickAction::Start))))
        .unwrap();
    host.advance(20);
    assert_eq!(
        host.complete(store.read(|state| (*state.get(&1).unwrap(), *state.get(&2).unwrap())))
            .unwrap(),
        (1, 1)
    );
    fresh.cancel();
    host.complete(fresh.finish());
    assert!(fresh.is_finished());
    assert!(fresh.is_cancelled());
    assert!(!sibling.is_finished());
    assert!(!sibling.is_cancelled());
    host.advance(30);
    assert_eq!(
        host.complete(store.read(|state| (*state.get(&1).unwrap(), *state.get(&2).unwrap())))
            .unwrap(),
        (1, 2)
    );
    sibling.cancel();
    host.complete(sibling.finish());
    assert!(sibling.is_finished());
    let snapshot = host.complete(store.runtime_snapshot()).unwrap();
    assert_eq!(snapshot.live_task_ids, [] as [u64; 0]);
    assert_eq!(snapshot.timer_deadlines, [] as [MonotonicInstant; 0]);
    assert_eq!(snapshot.cancellation_count, 0);
    assert_eq!(snapshot.observation_count, 0);
    senders.lock().unwrap().clear();
    host.complete(store.shutdown()).unwrap();
    assert!(host.tasks.lock().unwrap().is_empty());
    assert!(host.clock.0.lock().unwrap().waits.is_empty());
}
