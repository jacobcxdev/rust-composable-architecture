//! Public attachment and acquisition retain unrelated root and child actions.
use rca_core::{
    Feature, FeatureContext, MountIdentity, OptionalMount, Runtime, RuntimeDriver, RuntimeFuture,
    Store,
};
use std::cell::Cell;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

#[path = "mount_acquisition/authority.rs"]
mod authority;
#[path = "mount_acquisition/children_controls.rs"]
mod children_controls;
#[cfg(feature = "test-util")]
#[path = "mount_acquisition/frontier.rs"]
mod frontier;
#[path = "mount_acquisition/initial_order.rs"]
mod initial_order;
#[path = "mount_acquisition/lifetime.rs"]
mod lifetime;
#[cfg(feature = "test-util")]
#[path = "mount_acquisition/startup.rs"]
mod startup;

/// Polls spawned work only when a test explicitly advances a turn.
///
/// Time stays at zero, so only elapsed sleeps complete. A yield requests one
/// wake before completing, but wakes do not poll queued work automatically.
#[derive(Clone, Default)]
struct Driver(Arc<Mutex<Vec<RuntimeFuture>>>);
impl std::fmt::Debug for Driver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("MountDriver")
    }
}
impl RuntimeDriver for Driver {
    fn spawn(&self, task: RuntimeFuture) {
        self.0.lock().unwrap().push(task);
    }
    fn now(&self) -> rca_core::MonotonicInstant {
        rca_core::MonotonicInstant::from_elapsed(std::time::Duration::ZERO)
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
impl Driver {
    /// Polls one snapshot of tasks without holding the queue mutex.
    ///
    /// New tasks wait for the next turn. Pending tasks return to the queue.
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
}

#[test]
fn driver_contract_sleep_completes_only_elapsed_deadlines() {
    use rca_core::MonotonicInstant;
    use std::time::Duration;

    let driver = Driver::default();
    let mut context = Context::from_waker(Waker::noop());
    let mut elapsed = driver.sleep_until(MonotonicInstant::ZERO);
    assert!(elapsed.as_mut().poll(&mut context).is_ready());

    for deadline in [
        MonotonicInstant::from_elapsed(Duration::from_nanos(1)),
        MonotonicInstant::MAX,
    ] {
        let mut future = driver.sleep_until(deadline);
        assert!(future.as_mut().poll(&mut context).is_pending());
        assert!(future.as_mut().poll(&mut context).is_pending());
    }
}

#[test]
fn driver_contract_yield_wakes_once_before_completion() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::Wake;

    /// Counts scheduling requests without running another task.
    #[derive(Default)]
    struct WakeCount(AtomicUsize);

    impl Wake for WakeCount {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    let notifications = Arc::new(WakeCount::default());
    let waker = Waker::from(Arc::clone(&notifications));
    let mut context = Context::from_waker(&waker);
    let mut future = Driver::default().yield_now();

    assert_eq!(notifications.0.load(Ordering::SeqCst), 0);
    assert!(future.as_mut().poll(&mut context).is_pending());
    assert_eq!(notifications.0.load(Ordering::SeqCst), 1);
    assert!(future.as_mut().poll(&mut context).is_ready());
    assert_eq!(notifications.0.load(Ordering::SeqCst), 1);
}

struct Root {
    child: Option<Cell<usize>>,
}
enum RootAction {
    Noop,
}
enum ChildAction {
    Add(usize),
}
struct RootFeature;
impl Feature for RootFeature {
    type State = Root;
    type Action = RootAction;
    fn reduce(
        &mut self,
        _: &mut Root,
        _: &RootAction,
        _: &mut FeatureContext<'_, Root, RootAction>,
    ) {
    }
}
/// Keeps a private reducer count apart from the mounted State cell.
///
/// The count changes on reduction while State records the supplied value.
struct ChildFeature(Cell<usize>);
impl Feature for ChildFeature {
    type State = Cell<usize>;
    type Action = ChildAction;
    fn reduce(
        &mut self,
        state: &mut Cell<usize>,
        action: &ChildAction,
        _: &mut FeatureContext<'_, Cell<usize>, ChildAction>,
    ) {
        let ChildAction::Add(value) = action;
        self.0.set(self.0.get() + 1);
        state.set(state.get() + value);
    }
}
/// Polls one future without scheduling a later wake.
fn poll<F: Future>(future: std::pin::Pin<&mut F>) -> Poll<F::Output> {
    future.poll(&mut Context::from_waker(Waker::noop()))
}

#[test]
fn ordinary_acquisition_does_not_add_root_send_or_child_sync_bounds() {
    #[expect(
        clippy::type_complexity,
        reason = "The public compiler witness spells the owned Send future and independent root and child types explicitly"
    )]
    fn witness(
        store: &Store<std::rc::Rc<()>, std::rc::Rc<()>>,
        site: &rca_core::MountSite<std::rc::Rc<()>, Cell<usize>, Cell<usize>, std::rc::Rc<()>>,
    ) -> impl Future<
        Output = Result<
            Option<rca_core::TaskContext<Cell<usize>, Cell<usize>>>,
            rca_core::TaskAccessError,
        >,
    > + Send
    + 'static {
        store.acquire(site)
    }
    let _ = witness;
}

#[test]
fn builder_site_acquires_independent_send_not_sync_child() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let driver = Driver::default();
    let counter = Cell::new(0); // Send, not Sync constructor capture.
    let selections = Arc::new(AtomicUsize::new(0));
    let selected = Arc::clone(&selections);
    let mount = OptionalMount::new(
        |root: &Root| root.child.as_ref(),
        |root: &mut Root| root.child.as_mut(),
        move |root: &Root| {
            selected.fetch_add(1, Ordering::SeqCst);
            root.child.as_ref().map(|_| MountIdentity {
                key: (),
                replacement: 0,
                configuration: 0,
            })
        },
        move |_| {
            counter.set(counter.get() + 1);
            ChildFeature(Cell::new(0))
        },
    );
    assert_eq!(format!("{mount:?}"), "OptionalMount { .. }");
    assert_eq!(selections.load(Ordering::SeqCst), 0);
    assert_eq!(
        format!("{:?}", rca_core::RootMount::new(RootFeature)),
        "RootMount { .. }"
    );
    assert_eq!(
        format!(
            "{:?}",
            rca_core::EntryMount::new(ChildFeature(Cell::new(0)))
        ),
        "EntryMount { .. }"
    );
    assert_eq!(selections.load(Ordering::SeqCst), 0);
    let (builder, site) = Store::feature_builder(
        Root {
            child: Some(Cell::new(1)),
        },
        RootFeature,
        Runtime::new(driver.clone()),
    )
    .mount_with_site(mount);
    let site_debug = format!("{site:?}");
    assert_eq!(site_debug, "MountSite { .. }");
    let store = builder.build().unwrap();
    assert_eq!(format!("{store:?}"), "Store { closed: false, .. }");
    assert_eq!(selections.load(Ordering::SeqCst), 0);
    let mut ready = Box::pin(store.ready());
    assert!(poll(ready.as_mut()).is_pending());
    driver.turn();
    assert!(matches!(poll(ready.as_mut()), Poll::Ready(Ok(()))));
    let mut acquisition = Box::pin(store.acquire(&site));
    assert!(poll(acquisition.as_mut()).is_pending());
    driver.turn();
    let Poll::Ready(Ok(Some(child))) = poll(acquisition.as_mut()) else {
        panic!("installed child context");
    };
    let mut send = Box::pin(child.send(ChildAction::Add(2)));
    assert!(poll(send.as_mut()).is_pending());
    driver.turn();
    assert!(matches!(poll(send.as_mut()), Poll::Ready(Ok(_))));
    let mut update = Box::pin(child.update(|state| {
        state.set(state.get() + 4);
        state.get()
    }));
    assert!(poll(update.as_mut()).is_pending());
    driver.turn();
    let Poll::Ready(Ok(committed)) = poll(update.as_mut()) else {
        panic!("committed child update");
    };
    assert_eq!(*committed.value(), 7);
    let mut read = Box::pin(child.read(Cell::get));
    assert!(poll(read.as_mut()).is_pending());
    driver.turn();
    assert!(matches!(poll(read.as_mut()), Poll::Ready(Ok(7))));
    let mut root = Box::pin(store.send(RootAction::Noop));
    assert!(poll(root.as_mut()).is_pending());
    driver.turn();
    assert!(matches!(poll(root.as_mut()), Poll::Ready(Ok(_))));
    let mut shutdown = Box::pin(store.shutdown());
    assert!(poll(shutdown.as_mut()).is_pending());
    driver.turn();
    assert!(matches!(poll(shutdown.as_mut()), Poll::Ready(Ok(()))));
    assert_eq!(format!("{store:?}"), "Store { closed: true, .. }");
    assert_eq!(format!("{site:?}"), site_debug);
    let mut ended = Box::pin(store.acquire(&site));
    let Poll::Ready(Err(error)) = poll(ended.as_mut()) else {
        panic!("a shutdown mount cannot be acquired again");
    };
    assert_eq!(error.reason(), rca_core::TaskAccessReason::StoreClosed);
    assert_eq!(error.progress(), rca_core::TaskProgress::NotStarted);
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "the mounted child and ordered effect schedule form one observable case"
)]
fn child_concatenation_commits_before_starting_next_routed_effect() {
    enum Action {
        Start,
        First,
        Second,
    }
    struct SequencedChild(Arc<Mutex<Vec<&'static str>>>);
    impl Feature for SequencedChild {
        type State = Cell<usize>;
        type Action = Action;
        fn reduce(
            &mut self,
            state: &mut Cell<usize>,
            action: &Action,
            context: &mut FeatureContext<'_, Cell<usize>, Action>,
        ) {
            match action {
                Action::Start => {
                    self.0.lock().unwrap().push("start");
                    let trace = Arc::clone(&self.0);
                    context.effect(rca_core::Effect::concatenate([
                        rca_core::Effect::send(Action::First),
                        rca_core::Effect::run(move |sender| async move {
                            trace.lock().unwrap().push("second started");
                            sender.send(Action::Second).await.unwrap();
                        }),
                    ]));
                }
                Action::First => {
                    assert_eq!(state.get(), 0);
                    state.set(1);
                    self.0.lock().unwrap().push("first committed");
                }
                Action::Second => {
                    assert_eq!(state.get(), 1);
                    state.set(2);
                    self.0.lock().unwrap().push("second committed");
                }
            }
        }
    }
    let driver = Driver::default();
    let trace = Arc::new(Mutex::new(Vec::new()));
    let child_trace = Arc::clone(&trace);
    let mount = OptionalMount::new(
        |root: &Root| root.child.as_ref(),
        |root: &mut Root| root.child.as_mut(),
        |root: &Root| {
            root.child.as_ref().map(|_| MountIdentity {
                key: (),
                replacement: 0,
                configuration: 0,
            })
        },
        move |_| SequencedChild(Arc::clone(&child_trace)),
    );
    let (builder, site) = Store::feature_builder(
        Root {
            child: Some(Cell::new(0)),
        },
        RootFeature,
        Runtime::new(driver.clone()),
    )
    .mount_with_site(mount);
    let store = builder.build().unwrap();
    let mut ready = Box::pin(store.ready());
    assert!(poll(ready.as_mut()).is_pending());
    driver.turn();
    assert!(matches!(poll(ready.as_mut()), Poll::Ready(Ok(()))));
    let mut acquisition = Box::pin(store.acquire(&site));
    assert!(poll(acquisition.as_mut()).is_pending());
    driver.turn();
    let Poll::Ready(Ok(Some(child))) = poll(acquisition.as_mut()) else {
        panic!("installed child context");
    };
    let mut send = Box::pin(child.send(Action::Start));
    assert!(poll(send.as_mut()).is_pending());
    driver.turn();
    let Poll::Ready(Ok(sequence)) = poll(send.as_mut()) else {
        panic!("child sequence admitted");
    };
    assert!(!sequence.is_finished());
    assert_eq!(*trace.lock().unwrap(), ["start"]);
    // Each leaf gets one send poll and one acknowledged completion poll.
    for _ in 0..4 {
        driver.turn();
    }
    assert_eq!(
        *trace.lock().unwrap(),
        [
            "start",
            "first committed",
            "second started",
            "second committed"
        ]
    );
    assert!(sequence.is_finished());
    assert!(!sequence.is_cancelled());
    let mut finished = Box::pin(sequence.finish());
    assert!(poll(finished.as_mut()).is_ready());
    let mut shutdown = Box::pin(store.shutdown_into_state());
    assert!(poll(shutdown.as_mut()).is_pending());
    driver.turn();
    let Poll::Ready(Ok(state)) = poll(shutdown.as_mut()) else {
        panic!("actor returns committed child state");
    };
    assert_eq!(state.child.unwrap().get(), 2);
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "the held first leaf exposes pruning before the routed sequence can advance"
)]
fn mounted_sequence_prunes_retired_inner_effect_before_running_live_tail() {
    use rca_core::{
        ActionPath, Effect, OptionalStatePath, Reduce, Reducer, ReducerExt, ReducerFeature,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct State {
        child: Option<MountedState>,
    }
    struct MountedState {
        inner: Option<()>,
        completed: usize,
    }
    enum Action {
        Start,
        RemoveInner,
        Live,
    }
    struct Root;
    impl Feature for Root {
        type State = State;
        type Action = ();
        fn reduce(&mut self, _: &mut State, (): &(), _: &mut FeatureContext<'_, State, ()>) {}
    }
    struct Capture(Arc<AtomicUsize>);
    impl Drop for Capture {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    let driver = Driver::default();
    let old_starts = Arc::new(AtomicUsize::new(0));
    let old_drops = Arc::new(AtomicUsize::new(0));
    let live_starts = Arc::new(AtomicUsize::new(0));
    let scoped_starts = Arc::clone(&old_starts);
    let scoped_drops = Arc::clone(&old_drops);
    let live_calls = Arc::clone(&live_starts);
    let mount = OptionalMount::new(
        |state: &State| state.child.as_ref(),
        |state: &mut State| state.child.as_mut(),
        |state: &State| {
            state.child.as_ref().map(|_| MountIdentity {
                key: (),
                replacement: 0,
                configuration: 0,
            })
        },
        move |_| {
            let mut capture = Some(Capture(Arc::clone(&scoped_drops)));
            let starts = Arc::clone(&scoped_starts);
            let child = Reduce::new(move |(): &mut (), _: &Action, _| {
                let capture = capture.take().expect("one scoped effect is authored");
                let starts = Arc::clone(&starts);
                Effect::run(move |_| async move {
                    starts.fetch_add(1, Ordering::SeqCst);
                    drop(capture);
                })
            });
            let parent = Reduce::new(|state: &mut MountedState, action: &Action, _| {
                match action {
                    Action::RemoveInner => state.inner = None,
                    Action::Live => state.completed += 1,
                    Action::Start => {}
                }
                Effect::none()
            });
            let mut optional = parent.if_some(
                OptionalStatePath::new(
                    |state: &MountedState| state.inner.as_ref(),
                    |state: &mut MountedState| state.inner.as_mut(),
                ),
                ActionPath::new(
                    |action: &Action| matches!(action, Action::Start).then_some(action),
                    std::convert::identity,
                ),
                child,
            );
            let live_calls = Arc::clone(&live_calls);
            ReducerFeature::new(Reduce::new(
                move |state: &mut MountedState, action: &Action, context| {
                    let scoped = optional.reduce(state, action, context);
                    if !matches!(action, Action::Start) {
                        return scoped;
                    }
                    let live_calls = Arc::clone(&live_calls);
                    Effect::concatenate([
                        Effect::send(Action::RemoveInner),
                        scoped,
                        Effect::run(move |sender| async move {
                            live_calls.fetch_add(1, Ordering::SeqCst);
                            sender.send(Action::Live).await.unwrap();
                        }),
                    ])
                },
            ))
        },
    );
    let (builder, site) = Store::feature_builder(
        State {
            child: Some(MountedState {
                inner: Some(()),
                completed: 0,
            }),
        },
        Root,
        Runtime::new(driver.clone()),
    )
    .mount_with_site(mount);
    let store = builder.build().unwrap();
    let mut ready = Box::pin(store.ready());
    assert!(poll(ready.as_mut()).is_pending());
    driver.turn();
    assert!(matches!(poll(ready.as_mut()), Poll::Ready(Ok(()))));
    let mut acquire = Box::pin(store.acquire(&site));
    assert!(poll(acquire.as_mut()).is_pending());
    driver.turn();
    let Poll::Ready(Ok(Some(child))) = poll(acquire.as_mut()) else {
        panic!("mounted child remains installed");
    };
    let mut start = Box::pin(child.send(Action::Start));
    assert!(poll(start.as_mut()).is_pending());
    driver.turn();
    let Poll::Ready(Ok(task)) = poll(start.as_mut()) else {
        panic!("mounted sequence starts");
    };
    let mut spawned = std::mem::take(&mut *driver.0.lock().unwrap()).into_iter();
    let mut first = spawned.next().expect("first effect is scheduled");
    let mut actor = spawned.next().expect("actor remains scheduled");
    assert!(spawned.next().is_none());
    assert!(
        first
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_pending()
    );
    assert_eq!(old_drops.load(Ordering::SeqCst), 0);
    assert!(
        actor
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_pending()
    );
    // The RemoveInner commit retires only the nested composition scope. The
    // first send is still awaiting its acknowledgement, so the sequence has
    // not advanced to either deferred leaf yet.
    assert_eq!(old_drops.load(Ordering::SeqCst), 1);
    assert_eq!(old_starts.load(Ordering::SeqCst), 0);
    assert_eq!(live_starts.load(Ordering::SeqCst), 0);
    assert!(!task.is_finished());
    assert!(
        first
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_ready()
    );
    drop(first);
    driver.0.lock().unwrap().push(actor);
    for _ in 0..8 {
        if task.is_finished() {
            break;
        }
        driver.turn();
    }
    assert!(task.is_finished());
    assert!(!task.is_cancelled());
    assert_eq!(old_starts.load(Ordering::SeqCst), 0);
    assert_eq!(old_drops.load(Ordering::SeqCst), 1);
    assert_eq!(live_starts.load(Ordering::SeqCst), 1);
    assert!(poll(Box::pin(task.finish()).as_mut()).is_ready());
    let mut shutdown = Box::pin(store.shutdown_into_state());
    assert!(poll(shutdown.as_mut()).is_pending());
    driver.turn();
    let Poll::Ready(Ok(state)) = poll(shutdown.as_mut()) else {
        panic!("shutdown returns the still-installed mounted child");
    };
    let child_state = state.child.expect("outer mounted route stays installed");
    assert!(child_state.inner.is_none());
    assert_eq!(child_state.completed, 1);
    assert!(driver.0.lock().unwrap().is_empty());
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one mounted sequence verifies owned future and fallible deliveries through completion"
)]
fn mounted_future_and_fallible_effects_preserve_owned_payloads_and_sequence() {
    use rca_core::{Effect, EffectError};
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Owned {
        value: Box<usize>,
        drops: Arc<AtomicUsize>,
    }
    impl Drop for Owned {
        fn drop(&mut self) {
            self.drops.fetch_add(1, Ordering::SeqCst);
        }
    }
    enum Action {
        Start,
        Value(Owned),
    }
    struct EffectsChild {
        effect: Option<Effect<Action>>,
        identities: Arc<Mutex<Vec<usize>>>,
    }
    impl Feature for EffectsChild {
        type State = Cell<usize>;
        type Action = Action;
        fn reduce(
            &mut self,
            state: &mut Cell<usize>,
            action: &Action,
            context: &mut FeatureContext<'_, Cell<usize>, Action>,
        ) {
            match action {
                Action::Start => context.effect(self.effect.take().unwrap()),
                Action::Value(value) => {
                    self.identities
                        .lock()
                        .unwrap()
                        .push(std::ptr::from_ref(&*value.value).addr());
                    state.set(state.get() * 10 + *value.value);
                }
            }
        }
    }
    let driver = Driver::default();
    let drops = Arc::new(AtomicUsize::new(0));
    let owned = |value| Owned {
        value: Box::new(value),
        drops: Arc::clone(&drops),
    };
    let first = owned(1);
    let second = owned(2);
    let failure = owned(3);
    let expected =
        [&first, &second, &failure].map(|value| std::ptr::from_ref(&*value.value).addr());
    let capture = Arc::new(());
    let weak_capture = Arc::downgrade(&capture);
    let empty_runs = Arc::new(AtomicUsize::new(0));
    let empty_run = Arc::clone(&empty_runs);
    let mut effect = Some(Effect::concatenate([
        Effect::future(async move { Some(Action::Value(first)) }),
        Effect::future(async move {
            empty_run.fetch_add(1, Ordering::SeqCst);
            drop(capture);
            None
        }),
        Effect::try_run(
            move |sender| async move {
                sender.send(Action::Value(second)).await.unwrap();
                Err(EffectError::Failed(failure))
            },
            |failure, sender| async move {
                sender.send(Action::Value(failure)).await.unwrap();
            },
        ),
    ]));
    let identities = Arc::new(Mutex::new(Vec::new()));
    let observed = Arc::clone(&identities);
    let mount = OptionalMount::new(
        |root: &Root| root.child.as_ref(),
        |root: &mut Root| root.child.as_mut(),
        |root: &Root| {
            root.child.as_ref().map(|_| MountIdentity {
                key: (),
                replacement: 0,
                configuration: 0,
            })
        },
        move |_| EffectsChild {
            effect: effect.take(),
            identities: Arc::clone(&observed),
        },
    );
    let (builder, site) = Store::feature_builder(
        Root {
            child: Some(Cell::new(0)),
        },
        RootFeature,
        Runtime::new(driver.clone()),
    )
    .mount_with_site(mount);
    let store = builder.build().unwrap();
    driver.turn();
    assert!(matches!(
        poll(Box::pin(store.ready()).as_mut()),
        Poll::Ready(Ok(()))
    ));
    let mut acquisition = Box::pin(store.acquire(&site));
    assert!(poll(acquisition.as_mut()).is_pending());
    driver.turn();
    let Poll::Ready(Ok(Some(child))) = poll(acquisition.as_mut()) else {
        panic!("installed effect child");
    };
    let mut start = Box::pin(child.send(Action::Start));
    assert!(poll(start.as_mut()).is_pending());
    driver.turn();
    let Poll::Ready(Ok(task)) = poll(start.as_mut()) else {
        panic!("mounted effect sequence committed");
    };
    assert!(!task.is_finished());
    assert!(identities.lock().unwrap().is_empty());
    assert!(weak_capture.upgrade().is_some());
    assert_eq!(empty_runs.load(Ordering::SeqCst), 0);
    for _ in 0..32 {
        driver.turn();
    }
    assert!(task.is_finished());
    assert!(!task.is_cancelled());
    assert!(poll(Box::pin(task.finish()).as_mut()).is_ready());
    assert_eq!(*identities.lock().unwrap(), expected);
    assert_eq!(drops.load(Ordering::SeqCst), 3);
    assert!(weak_capture.upgrade().is_none());
    assert_eq!(empty_runs.load(Ordering::SeqCst), 1);
    let mut shutdown = Box::pin(store.shutdown_into_state());
    assert!(poll(shutdown.as_mut()).is_pending());
    driver.turn();
    let Poll::Ready(Ok(state)) = poll(shutdown.as_mut()) else {
        panic!("mounted sequence returned its committed state");
    };
    assert_eq!(state.child.unwrap().get(), 123);
}

// Catches eager task polling, omitted Initial work, or repeating an installed hook.
#[test]
fn initial_child_factory_captures_owned_value_before_ready_and_defers_task_body() {
    use rca_core::ReactionChange;
    use rca_core::{DependencyKey, Task};
    struct Amount;
    impl DependencyKey for Amount {
        type Value = usize;
        fn live_value() -> usize {
            7
        }
    }
    let driver = Driver::default();
    let trace = Arc::new(Mutex::new(Vec::new()));
    let factory_trace = std::sync::Arc::clone(&trace);
    let mount = OptionalMount::new(
        |root: &Root| root.child.as_ref(),
        |root: &mut Root| root.child.as_mut(),
        |root: &Root| {
            root.child.as_ref().map(|_| MountIdentity {
                key: (),
                replacement: 0,
                configuration: 0,
            })
        },
        |_| ChildFeature(Cell::new(0)),
    )
    .on_initial(Cell::get, move |change, mut capture| {
        let ReactionChange::Initial { current } = change else {
            panic!("initial-only hook");
        };
        let amount = *capture.dependency::<Amount>();
        factory_trace.lock().unwrap().push("factory");
        let trace = std::sync::Arc::clone(&factory_trace);
        Task::run(move |context| async move {
            trace.lock().unwrap().push("body");
            let _ = context
                .send(ChildAction::Add(current + amount))
                .await
                .unwrap();
        })
    });
    let (builder, site) = Store::feature_builder(
        Root {
            child: Some(Cell::new(1)),
        },
        RootFeature,
        Runtime::new(driver.clone()),
    )
    .mount_with_site(mount);
    let store = builder.build().unwrap();
    assert!(trace.lock().unwrap().is_empty());
    driver.turn();
    assert_eq!(*trace.lock().unwrap(), ["factory"]);
    assert!(matches!(
        poll(Box::pin(store.ready()).as_mut()),
        Poll::Ready(Ok(()))
    ));
    driver.turn(); // First task poll; its send is admitted only after readiness.
    assert_eq!(*trace.lock().unwrap(), ["factory", "body"]);
    driver.turn();
    let mut acquisition = Box::pin(store.acquire(&site));
    assert!(poll(acquisition.as_mut()).is_pending());
    driver.turn();
    let Poll::Ready(Ok(Some(child))) = poll(acquisition.as_mut()) else {
        panic!("installed child");
    };
    let mut read = Box::pin(child.read(Cell::get));
    assert!(poll(read.as_mut()).is_pending());
    driver.turn();
    assert!(matches!(poll(read.as_mut()), Poll::Ready(Ok(9))));
    let mut action = Box::pin(store.send(RootAction::Noop));
    assert!(poll(action.as_mut()).is_pending());
    driver.turn();
    assert!(matches!(poll(action.as_mut()), Poll::Ready(Ok(_))));
    assert_eq!(*trace.lock().unwrap(), ["factory", "body"]);
    let mut shutdown = Box::pin(store.shutdown());
    assert!(poll(shutdown.as_mut()).is_pending());
    driver.turn();
    assert!(matches!(poll(shutdown.as_mut()), Poll::Ready(Ok(()))));
}

#[cfg(feature = "test-util")]
#[test]
fn reducer_builder_mount_defers_installation_and_retires_child() {
    use rca_core::{Effect, Reduce, Task};
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct MountedChild(Arc<AtomicUsize>);
    impl Drop for MountedChild {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
    impl Feature for MountedChild {
        type State = Cell<usize>;
        type Action = ChildAction;
        fn reduce(
            &mut self,
            _: &mut Self::State,
            _: &Self::Action,
            _: &mut FeatureContext<'_, Self::State, Self::Action>,
        ) {
        }
    }

    let driver = Driver::default();
    let constructed = Arc::new(AtomicUsize::new(0));
    let installed = Arc::new(AtomicUsize::new(0));
    let dropped = Arc::new(AtomicUsize::new(0));
    let make_count = Arc::clone(&constructed);
    let drop_count = Arc::clone(&dropped);
    let initial_count = Arc::clone(&installed);
    let mount = OptionalMount::new(
        |root: &Root| root.child.as_ref(),
        |root: &mut Root| root.child.as_mut(),
        |root: &Root| {
            root.child.as_ref().map(|_| MountIdentity {
                key: (),
                replacement: 0,
                configuration: 0,
            })
        },
        move |_| {
            make_count.fetch_add(1, Ordering::SeqCst);
            MountedChild(Arc::clone(&drop_count))
        },
    )
    .on_initial(Cell::get, move |_, _| {
        initial_count.fetch_add(1, Ordering::SeqCst);
        Task::run(|_| async {})
    });
    let store = Store::builder(
        Root {
            child: Some(Cell::new(1)),
        },
        Reduce::new(|root: &mut Root, (): &(), _| {
            root.child = None;
            Effect::none()
        }),
        Runtime::new(driver.clone()),
    )
    .mount(mount)
    .build()
    .unwrap();

    assert_eq!(constructed.load(Ordering::SeqCst), 0);
    assert_eq!(installed.load(Ordering::SeqCst), 0);
    assert_eq!(dropped.load(Ordering::SeqCst), 0);
    let mut ready = Box::pin(store.ready());
    assert!(poll(ready.as_mut()).is_pending());
    driver.turn();
    assert!(matches!(poll(ready.as_mut()), Poll::Ready(Ok(()))));
    assert_eq!(constructed.load(Ordering::SeqCst), 1);
    assert_eq!(installed.load(Ordering::SeqCst), 1);
    assert_eq!(dropped.load(Ordering::SeqCst), 0);
    driver.turn();

    let mut retire = Box::pin(store.send(()));
    assert!(poll(retire.as_mut()).is_pending());
    driver.turn();
    let Poll::Ready(Ok(task)) = poll(retire.as_mut()) else {
        panic!("root retirement action must commit");
    };
    assert!(task.is_finished());
    drop(retire);
    assert_eq!(dropped.load(Ordering::SeqCst), 1);
    let mut shutdown = Box::pin(store.shutdown_into_state());
    assert!(poll(shutdown.as_mut()).is_pending());
    driver.turn();
    let Poll::Ready(Ok(root)) = poll(shutdown.as_mut()) else {
        panic!("shutdown returns retired root state");
    };
    assert!(root.child.is_none());
    assert_eq!(constructed.load(Ordering::SeqCst), 1);
    assert_eq!(dropped.load(Ordering::SeqCst), 1);
}

// Catches root installation being omitted or ordered before the restored child.
#[test]
fn root_mount_uses_same_builder_and_initial_hooks_run_child_before_root() {
    use rca_core::{RootMount, Task};
    let driver = Driver::default();
    let trace = Arc::new(Mutex::new(Vec::new()));
    let root_trace = std::sync::Arc::clone(&trace);
    let child_trace = std::sync::Arc::clone(&trace);
    let root = RootMount::new(RootFeature).on_initial(
        |root: &Root| root.child.as_ref().map(Cell::get),
        move |_, _| {
            root_trace.lock().unwrap().push("root factory");
            let trace = std::sync::Arc::clone(&root_trace);
            Task::run(move |_| async move {
                trace.lock().unwrap().push("root body");
            })
        },
    );
    let child = OptionalMount::new(
        |root: &Root| root.child.as_ref(),
        |root: &mut Root| root.child.as_mut(),
        |root: &Root| {
            root.child.as_ref().map(|_| MountIdentity {
                key: (),
                replacement: 0,
                configuration: 0,
            })
        },
        |_| ChildFeature(Cell::new(0)),
    )
    .on_initial(Cell::get, move |_, _| {
        child_trace.lock().unwrap().push("child factory");
        let trace = std::sync::Arc::clone(&child_trace);
        Task::run(move |_| async move {
            trace.lock().unwrap().push("child body");
        })
    });
    let store = root
        .builder(
            Root {
                child: Some(Cell::new(1)),
            },
            Runtime::new(driver.clone()),
        )
        .mount(child)
        .build()
        .unwrap();
    assert!(trace.lock().unwrap().is_empty());
    driver.turn();
    assert_eq!(*trace.lock().unwrap(), ["child factory", "root factory"]);
    assert!(matches!(
        poll(Box::pin(store.ready()).as_mut()),
        Poll::Ready(Ok(()))
    ));
    driver.turn();
    assert_eq!(
        *trace.lock().unwrap(),
        ["child factory", "root factory", "child body", "root body"]
    );
    let mut shutdown = Box::pin(store.shutdown());
    assert!(poll(shutdown.as_mut()).is_pending());
    driver.turn();
    assert!(matches!(poll(shutdown.as_mut()), Poll::Ready(Ok(()))));
}
