#![cfg(feature = "local")]

use super::poll;
use rca_core::MonotonicInstant;
use rca_core::diagnostics::{CancellationCause, LifecycleEvent, LifecycleKind};
use rca_core::local::{
    Effect, Feature, FeatureContext, Reduce, Runtime, RuntimeDriver, RuntimeFuture, Store,
};
use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll, Wake, Waker};

type Queue = Rc<RefCell<Vec<RuntimeFuture>>>;
/// Spawns only while the owner-thread queue remains alive.
///
/// Tests retain the strong queue separately. Time stays at zero, so future waits stay pending.
struct Driver(Weak<RefCell<Vec<RuntimeFuture>>>);
impl std::fmt::Debug for Driver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("LifecycleLocalHost")
    }
}
impl RuntimeDriver for Driver {
    fn spawn(&self, task: RuntimeFuture) {
        self.0.upgrade().unwrap().borrow_mut().push(task);
    }
    fn now(&self) -> MonotonicInstant {
        MonotonicInstant::ZERO
    }
    fn sleep_until(&self, deadline: MonotonicInstant) -> RuntimeFuture {
        if deadline <= MonotonicInstant::ZERO {
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
/// Returns the strong queue beside a Runtime that holds only its weak driver.
fn runtime() -> (Queue, Runtime) {
    let queue = Rc::new(RefCell::new(Vec::new()));
    let runtime = Runtime::new(Driver(Rc::downgrade(&queue)));
    (queue, runtime)
}

#[test]
fn local_driver_elapsed_deadline_is_ready_on_first_poll() {
    let (queue, runtime) = runtime();
    let mut wait =
        RuntimeDriver::sleep_until(&Driver(Rc::downgrade(&queue)), MonotonicInstant::ZERO);
    assert!(poll(wait.as_mut()).is_ready());
    drop(runtime);
}

#[test]
fn local_driver_yield_wakes_once_before_completion() {
    struct Notification(AtomicUsize);
    impl Wake for Notification {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::Release);
        }
    }

    let (queue, runtime) = runtime();
    let mut yielding = RuntimeDriver::yield_now(&Driver(Rc::downgrade(&queue)));
    let notification = Arc::new(Notification(AtomicUsize::new(0)));
    let waker = Waker::from(Arc::clone(&notification));
    assert!(
        yielding
            .as_mut()
            .poll(&mut Context::from_waker(&waker))
            .is_pending()
    );
    assert_eq!(notification.0.load(Ordering::Acquire), 1);
    assert!(poll(yielding.as_mut()).is_ready());
    drop(runtime);
}

/// Records owner-thread events and disposal order without requiring `Send`.
#[derive(Default)]
struct Probe {
    events: RefCell<[Option<LifecycleEvent>; 16]>,
    count: Cell<usize>,
    state_drops: Cell<usize>,
    sink_drops: Cell<usize>,
    /// Captures State disposal count at the instant the sink itself drops.
    state_drops_at_sink_drop: Cell<usize>,
}
struct State {
    value: Rc<Cell<u8>>,
    probe: Rc<Probe>,
}
impl Drop for State {
    fn drop(&mut self) {
        self.probe.state_drops.set(self.probe.state_drops.get() + 1);
    }
}
/// Checks callback and destructor execution on the originating thread.
struct Sink {
    probe: Rc<Probe>,
    thread: std::thread::ThreadId,
}
impl Sink {
    fn record(&self, event: &LifecycleEvent) {
        assert_eq!(std::thread::current().id(), self.thread);
        self.probe.events.borrow_mut()[self.probe.count.get()] = Some(*event);
        self.probe.count.set(self.probe.count.get() + 1);
    }
}
impl Drop for Sink {
    fn drop(&mut self) {
        assert_eq!(std::thread::current().id(), self.thread);
        self.probe.sink_drops.set(self.probe.sink_drops.get() + 1);
        self.probe
            .state_drops_at_sink_drop
            .set(self.probe.state_drops.get());
    }
}
struct Counter;
impl Feature for Counter {
    type State = State;
    type Action = Rc<u8>;
    fn reduce(
        &mut self,
        state: &mut State,
        action: &Rc<u8>,
        _: &mut FeatureContext<'_, State, Rc<u8>>,
    ) {
        state.value.set(state.value.get() + **action);
    }
}

/// Builds either Local surface with the same original Rc-backed State allocation.
fn build(
    feature: bool,
    runtime: Runtime,
    probe: &Rc<Probe>,
) -> (Store<State, Rc<u8>>, *const Cell<u8>) {
    let state = State {
        value: Rc::new(Cell::new(2)),
        probe: Rc::clone(probe),
    };
    let address = Rc::as_ptr(&state.value);
    let sink = Sink {
        probe: Rc::clone(probe),
        thread: std::thread::current().id(),
    };
    let store = if feature {
        Store::feature_builder(state, Counter, runtime)
            .with_lifecycle_diagnostics(move |event| sink.record(event))
            .build()
            .unwrap()
    } else {
        Store::builder(
            state,
            Reduce::new(
                |state: &mut State, action: &Rc<u8>, _: &mut rca_core::ReduceContext<'_>| {
                    state.value.set(state.value.get() + **action);
                    Effect::none()
                },
            ),
            runtime,
        )
        .with_lifecycle_diagnostics(move |event| sink.record(event))
        .build()
        .unwrap()
    };
    (store, address)
}

#[test]
fn both_local_builders_keep_rc_values_and_sink_on_the_owner_through_extraction() {
    for feature in [false, true] {
        let (queue, runtime) = runtime();
        let probe = Rc::new(Probe::default());
        let (store, original) = build(feature, runtime, &probe);
        let mut actor = queue.borrow_mut().pop().unwrap();
        assert!(poll(actor.as_mut()).is_pending());
        assert!(matches!(
            probe.events.borrow()[0].unwrap().kind(),
            LifecycleKind::Installed {
                lifetime: 0,
                parent: None,
                configuration: Some(0),
            }
        ));
        let mut send = Box::pin(store.send(Rc::new(3)));
        assert!(poll(send.as_mut()).is_pending());
        assert!(poll(actor.as_mut()).is_pending());
        assert!(matches!(poll(send.as_mut()), Poll::Ready(Ok(_))));
        drop(send);
        let mut extract = Box::pin(store.shutdown_into_state());
        assert!(poll(extract.as_mut()).is_pending());
        assert!(poll(actor.as_mut()).is_ready());
        let Poll::Ready(Ok(state)) = poll(extract.as_mut()) else {
            panic!("local extraction reply");
        };
        assert_eq!(Rc::as_ptr(&state.value), original);
        assert_eq!(state.value.get(), 5);
        assert_eq!(probe.sink_drops.get(), 1);
        assert_eq!(probe.state_drops_at_sink_drop.get(), 0);
        assert_eq!(probe.state_drops.get(), 0);
        assert_eq!(probe.count.get(), 4);
        for (index, kind) in [
            LifecycleKind::IngressClosed,
            LifecycleKind::CancellationRequested {
                cause: CancellationCause::ActorStop,
                scope: None,
            },
            LifecycleKind::ActorStopped,
        ]
        .into_iter()
        .enumerate()
        {
            let event = probe.events.borrow()[index + 1].unwrap();
            assert_eq!(event.kind(), kind);
            assert_eq!(event.last_committed_version(), 1);
        }
        drop(state);
        assert_eq!(probe.state_drops.get(), 1);
    }
}

#[test]
fn local_shutdown_destroys_state_before_owner_local_sink_capture() {
    for feature in [false, true] {
        let (queue, runtime) = runtime();
        let probe = Rc::new(Probe::default());
        let (store, _) = build(feature, runtime, &probe);
        let mut actor = queue.borrow_mut().pop().unwrap();
        assert!(poll(actor.as_mut()).is_pending());
        let mut shutdown = Box::pin(store.shutdown());
        assert!(poll(shutdown.as_mut()).is_pending());
        assert!(poll(actor.as_mut()).is_ready());
        assert!(matches!(poll(shutdown.as_mut()), Poll::Ready(Ok(()))));
        assert_eq!(probe.state_drops_at_sink_drop.get(), 1);
        assert_eq!(probe.state_drops.get(), 1);
        assert_eq!(probe.sink_drops.get(), 1);
        assert_eq!(probe.count.get(), 4);
    }
}

struct NativeState {
    child: Option<Rc<Cell<u8>>>,
    replacement: u64,
}
struct NativeRoot;
impl Feature for NativeRoot {
    type State = NativeState;
    type Action = Rc<()>;
    fn reduce(
        &mut self,
        state: &mut NativeState,
        _: &Rc<()>,
        _: &mut FeatureContext<'_, NativeState, Rc<()>>,
    ) {
        state.replacement += 1;
    }
}
struct NativeChild;
impl Feature for NativeChild {
    type State = Rc<Cell<u8>>;
    type Action = Rc<()>;
    fn reduce(
        &mut self,
        _: &mut Rc<Cell<u8>>,
        _: &Rc<()>,
        _: &mut FeatureContext<'_, Rc<Cell<u8>>, Rc<()>>,
    ) {
    }
}

#[test]
fn native_local_equal_value_replacement_reports_real_authority_retirement() {
    let (queue, runtime) = runtime();
    let probe = Rc::new(Probe::default());
    let sink = Sink {
        probe: Rc::clone(&probe),
        thread: std::thread::current().id(),
    };
    let descriptor = rca_core::local::OptionalMount::new(
        |state: &NativeState| state.child.as_ref(),
        |state: &mut NativeState| state.child.as_mut(),
        |state: &NativeState| {
            state.child.as_ref().map(|_| rca_core::MountIdentity {
                key: (),
                replacement: state.replacement,
                configuration: 0,
            })
        },
        |_| NativeChild,
    );
    let (builder, site) = Store::feature_builder(
        NativeState {
            child: Some(Rc::new(Cell::new(3))),
            replacement: 0,
        },
        NativeRoot,
        runtime,
    )
    .mount_with_site(descriptor);
    let store = builder
        .with_lifecycle_diagnostics(move |event| sink.record(event))
        .build()
        .unwrap();
    let mut actor = queue.borrow_mut().pop().unwrap();
    assert!(poll(actor.as_mut()).is_pending());
    assert_eq!(probe.count.get(), 2);
    let LifecycleKind::Installed {
        lifetime: old,
        parent: Some(0),
        configuration: Some(0),
    } = probe.events.borrow()[1].unwrap().kind()
    else {
        panic!("actual local child installation");
    };
    let mut acquire = Box::pin(store.acquire(&site));
    assert!(poll(acquire.as_mut()).is_pending());
    assert!(poll(actor.as_mut()).is_pending());
    let Poll::Ready(Ok(Some(child))) = poll(acquire.as_mut()) else {
        panic!("local child context")
    };
    let mut replace = Box::pin(store.send(Rc::new(())));
    assert!(poll(replace.as_mut()).is_pending());
    assert!(poll(actor.as_mut()).is_pending());
    assert!(matches!(poll(replace.as_mut()), Poll::Ready(Ok(_))));
    assert_eq!(probe.count.get(), 5);
    let events = probe.events.borrow();
    assert_eq!(
        events[2].unwrap().kind(),
        LifecycleKind::Retired { lifetime: old }
    );
    assert!(
        matches!(events[3].unwrap().kind(), LifecycleKind::Installed { lifetime, parent: Some(0), configuration: Some(0) } if lifetime != old)
    );
    assert_eq!(
        events[4].unwrap().kind(),
        LifecycleKind::CancellationRequested {
            cause: CancellationCause::ScopeRetirement,
            scope: Some(old)
        }
    );
    assert!(
        events[2..5]
            .iter()
            .all(|event| event.unwrap().last_committed_version() == 1)
    );
    drop(events);
    let selected = Rc::new(Cell::new(false));
    let flag = Rc::clone(&selected);
    let mut read = Box::pin(child.read(move |_| {
        flag.set(true);
        Rc::new(3)
    }));
    assert!(poll(read.as_mut()).is_pending());
    assert!(poll(actor.as_mut()).is_pending());
    assert!(
        matches!(poll(read.as_mut()), Poll::Ready(Err(error)) if error.reason() == rca_core::TaskAccessReason::ScopeEnded)
    );
    assert!(!selected.get());
    drop(replace);
    drop(store);
    assert!(poll(actor.as_mut()).is_ready());
    assert_eq!(probe.count.get(), 8);
    assert_eq!(probe.sink_drops.get(), 1);
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one ordered refresh and removal schedule ties lifetime, revision, and ownership assertions together"
)]
fn local_configuration_refresh_preserves_lifetime_and_reports_precommit_revisions() {
    struct RootState {
        child: Option<Rc<Cell<u8>>>,
        marker: u64,
    }
    enum Action {
        Configure(u64),
        Remove,
    }
    struct Root;
    impl Feature for Root {
        type State = RootState;
        type Action = Action;
        fn reduce(
            &mut self,
            state: &mut RootState,
            action: &Action,
            _: &mut FeatureContext<'_, RootState, Action>,
        ) {
            match action {
                Action::Configure(marker) => state.marker = *marker,
                Action::Remove => state.child = None,
            }
        }
    }
    struct Child(Rc<Cell<usize>>);
    impl Feature for Child {
        type State = Rc<Cell<u8>>;
        type Action = ();
        fn reduce(
            &mut self,
            _: &mut Rc<Cell<u8>>,
            (): &(),
            _: &mut FeatureContext<'_, Rc<Cell<u8>>, ()>,
        ) {
            panic!("configuration refresh must not reduce the child");
        }
    }
    impl Drop for Child {
        fn drop(&mut self) {
            self.0.set(self.0.get() + 1);
        }
    }
    fn ordered<T>(actor: &mut RuntimeFuture, request: impl Future<Output = T>) -> T {
        let mut request = Box::pin(request);
        assert!(poll(request.as_mut()).is_pending());
        assert!(poll(actor.as_mut()).is_pending());
        let Poll::Ready(value) = poll(request.as_mut()) else {
            panic!("local actor-ordered request must resolve");
        };
        value
    }

    let (queue, runtime) = runtime();
    let probe = Rc::new(Probe::default());
    let sink = Sink {
        probe: Rc::clone(&probe),
        thread: std::thread::current().id(),
    };
    let constructions = Rc::new(Cell::new(0));
    let drops = Rc::new(Cell::new(0));
    let made = Rc::clone(&constructions);
    let dropped = Rc::clone(&drops);
    let value = Rc::new(Cell::new(7));
    let address = Rc::as_ptr(&value);
    let weak_value = Rc::downgrade(&value);
    let mount = rca_core::local::OptionalMount::new(
        |state: &RootState| state.child.as_ref(),
        |state: &mut RootState| state.child.as_mut(),
        |state: &RootState| {
            state.child.as_ref().map(|_| rca_core::MountIdentity {
                key: (),
                replacement: 0,
                configuration: state.marker,
            })
        },
        move |_| {
            made.set(made.get() + 1);
            Child(Rc::clone(&dropped))
        },
    );
    let (builder, site) = Store::feature_builder(
        RootState {
            child: Some(value),
            marker: 41,
        },
        Root,
        runtime,
    )
    .mount_with_site(mount);
    let store = builder
        .with_lifecycle_diagnostics(move |event| sink.record(event))
        .build()
        .unwrap();
    let mut actor = queue.borrow_mut().pop().unwrap();
    assert!(poll(actor.as_mut()).is_pending());
    assert_eq!(probe.count.get(), 2);
    let LifecycleKind::Installed {
        lifetime,
        parent: Some(0),
        configuration: Some(0),
    } = probe.events.borrow()[1].unwrap().kind()
    else {
        panic!("actual local child installation");
    };
    let original = ordered(&mut actor, store.acquire(&site)).unwrap().unwrap();

    for (marker, previous, current) in [(9001, 0, 1), (41, 1, 2)] {
        let before = ordered(&mut actor, store.acquire(&site)).unwrap().unwrap();
        let prior = ordered(&mut actor, before.update(|value| value.get()))
            .unwrap()
            .revision();
        let event_index = probe.count.get();
        drop(ordered(&mut actor, store.send(Action::Configure(marker))).unwrap());
        assert_eq!(probe.count.get(), event_index + 1);
        let event = probe.events.borrow()[event_index].unwrap();
        assert_eq!(
            event.kind(),
            LifecycleKind::ConfigurationChanged {
                lifetime,
                previous,
                current,
            }
        );
        assert_eq!(event.last_committed_version(), prior);
        let fresh = ordered(&mut actor, store.acquire(&site)).unwrap().unwrap();
        assert_eq!(
            ordered(
                &mut actor,
                fresh.read(|value| (Rc::as_ptr(value), value.get()))
            )
            .unwrap(),
            (address, 7)
        );
        assert_eq!(constructions.get(), 1);
        assert_eq!(drops.get(), 0);
    }
    let error = ordered(
        &mut actor,
        original.read::<(), _>(|_| panic!("old configuration must reject before selection")),
    )
    .unwrap_err();
    assert_eq!(
        (error.reason(), error.progress()),
        (
            rca_core::TaskAccessReason::ScopeEnded,
            rca_core::TaskProgress::NotStarted
        )
    );
    let unchanged_count = probe.count.get();
    drop(ordered(&mut actor, store.send(Action::Configure(41))).unwrap());
    assert_eq!(probe.count.get(), unchanged_count);
    let current = ordered(&mut actor, store.acquire(&site)).unwrap().unwrap();
    let prior = ordered(&mut actor, current.update(|value| value.get()))
        .unwrap()
        .revision();
    drop(ordered(&mut actor, store.send(Action::Remove)).unwrap());
    assert_eq!(probe.count.get(), unchanged_count + 2);
    let retired = probe.events.borrow()[unchanged_count].unwrap();
    assert_eq!(retired.kind(), LifecycleKind::Retired { lifetime });
    assert_eq!(retired.last_committed_version(), prior);
    assert_eq!(constructions.get(), 1);
    assert_eq!(drops.get(), 1);
    assert!(weak_value.upgrade().is_none());
    assert!(ordered(&mut actor, store.acquire(&site)).unwrap().is_none());
    drop(store);
    assert!(poll(actor.as_mut()).is_ready());
    assert_eq!(drops.get(), 1);
    assert_eq!(probe.sink_drops.get(), 1);
}
