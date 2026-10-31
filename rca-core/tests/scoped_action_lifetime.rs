//! Scoped sends keep payload and embedder ownership separate until final disposal.

#[path = "../src/allocation_test/child.rs"]
mod allocation_test;

#[expect(dead_code, reason = "lifetime tests manually poll the actor")]
mod support;

use std::cell::Cell;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

use rca_core::{
    ActionPath, Effect, OptionalStatePath, ReduceContext, Reducer, RuntimeFuture, ScopedStore,
    StatePath, Store,
};

/// Runs at most one re-entrant action when a payload or embedder is dropped.
type DropHook = Arc<Mutex<Option<Box<dyn FnOnce() + Send>>>>;

/// Records payload-versus-capture disposal order before running its hook.
struct DropCount {
    count: Arc<AtomicUsize>,
    events: Arc<Mutex<Vec<&'static str>>>,
    label: &'static str,
    hook: DropHook,
}

impl Drop for DropCount {
    fn drop(&mut self) {
        self.count.fetch_add(1, Ordering::SeqCst);
        self.events.lock().unwrap().push(self.label);
        let hook = self.hook.lock().unwrap().take();
        if let Some(hook) = hook {
            hook();
        }
    }
}

struct Action {
    value: Box<Cell<usize>>,
    remove_optional: bool,
    drop_probe: DropCount,
}

struct State {
    required: usize,
    optional: Option<usize>,
}

struct Counter;

impl Reducer for Counter {
    type State = State;
    type Action = Action;

    fn reduce(
        &mut self,
        state: &mut State,
        action: &Action,
        _: &mut ReduceContext<'_>,
    ) -> Effect<Action> {
        if action.remove_optional {
            state.optional = None;
        } else {
            state.required += action.value.get();
        }
        Effect::none()
    }
}

/// Holds either an actor-registered optional child or a direct required path.
struct Fixture {
    _runtime: support::ManualRuntime,
    store: Store<State, Action>,
    child: ScopedStore<usize, Action>,
    actor: RuntimeFuture,
    calls: Arc<AtomicUsize>,
    captures: Arc<AtomicUsize>,
    actions: Arc<AtomicUsize>,
    events: Arc<Mutex<Vec<&'static str>>>,
    capture_hook: DropHook,
}

impl Fixture {
    /// Builds both path modes with the usual single-slot mailbox.
    fn new(registered: bool, panic_on_embed: bool) -> Self {
        Self::with_capacity(registered, panic_on_embed, 1)
    }

    /// Leaves the actor under manual control after optional registration.
    fn with_capacity(registered: bool, panic_on_embed: bool, capacity: usize) -> Self {
        let runtime = support::ManualRuntime::new();
        let store = Store::builder(
            State {
                required: 0,
                optional: Some(0),
            },
            Counter,
            runtime.runtime(),
        )
        .mailbox_capacity(capacity)
        .build()
        .unwrap();
        let mut actor = runtime.take_only_queued_task();
        let calls = Arc::new(AtomicUsize::new(0));
        let captures = Arc::new(AtomicUsize::new(0));
        let events = Arc::new(Mutex::new(Vec::new()));
        let capture_hook = Arc::new(Mutex::new(None));
        let capture = DropCount {
            count: Arc::clone(&captures),
            events: Arc::clone(&events),
            label: "capture",
            hook: Arc::clone(&capture_hook),
        };
        let embed_calls = Arc::clone(&calls);
        let embed_events = Arc::clone(&events);
        let path = ActionPath::new(
            |action: &Action| Some(action),
            move |action| {
                std::hint::black_box(&capture);
                embed_calls.fetch_add(1, Ordering::SeqCst);
                embed_events.lock().unwrap().push("embed");
                assert!(!panic_on_embed, "intentional embedding panic");
                action
            },
        );
        let child = if registered {
            let registration = store.scope_some(
                OptionalStatePath::new(
                    |state: &State| state.optional.as_ref(),
                    |state: &mut State| state.optional.as_mut(),
                ),
                path,
            );
            let mut registration = std::pin::pin!(registration);
            let mut cx = Context::from_waker(Waker::noop());
            assert!(registration.as_mut().poll(&mut cx).is_pending());
            assert!(actor.as_mut().poll(&mut cx).is_pending());
            let Poll::Ready(child) = registration.as_mut().poll(&mut cx) else {
                panic!("registration must complete");
            };
            child.unwrap()
        } else {
            store.scope(
                StatePath::new(
                    |state: &State| &state.required,
                    |state: &mut State| &mut state.required,
                ),
                path,
            )
        };
        Self {
            _runtime: runtime,
            store,
            child,
            actor,
            calls,
            captures,
            actions: Arc::new(AtomicUsize::new(0)),
            events,
            capture_hook,
        }
    }

    /// Allocates a distinct payload whose destruction enters the event log.
    fn action(&self) -> Action {
        Action {
            value: Box::new(Cell::new(7)),
            remove_optional: false,
            drop_probe: DropCount {
                count: Arc::clone(&self.actions),
                events: Arc::clone(&self.events),
                label: "child",
                hook: Arc::new(Mutex::new(None)),
            },
        }
    }
}

#[test]
fn send_only_children_preserve_embedding_timing_and_drop_once() {
    fn require_send<T: Send>(value: T) -> T {
        value
    }
    fn require_sync<T: Sync>(_: &T) {}
    for registered in [false, true] {
        let mut fixture = Fixture::new(registered, false);
        require_sync(&fixture.child);
        let task = {
            let send = require_send(fixture.child.send(fixture.action()));
            let mut send = std::pin::pin!(send);
            assert_eq!(fixture.calls.load(Ordering::SeqCst), 0);
            let mut cx = Context::from_waker(Waker::noop());
            assert!(send.as_mut().poll(&mut cx).is_pending());
            assert_eq!(
                fixture.calls.load(Ordering::SeqCst),
                usize::from(!registered)
            );
            assert!(fixture.actor.as_mut().poll(&mut cx).is_pending());
            let Poll::Ready(task) = send.as_mut().poll(&mut cx) else {
                panic!("send must complete");
            };
            task.unwrap()
        };
        futures_executor::block_on(task.finish());
        assert_eq!(fixture.calls.load(Ordering::SeqCst), 1);
        assert_eq!(fixture.actions.load(Ordering::SeqCst), 1);
        drop(fixture.child);
        assert_eq!(fixture.captures.load(Ordering::SeqCst), 1);
    }
}

#[test]
fn rejected_and_abandoned_actions_release_without_embedding() {
    for registered in [false, true] {
        let fixture = Fixture::new(registered, false);
        let first = fixture.child.try_send(fixture.action()).ok().unwrap();
        let rejected = fixture.action();
        let allocation = std::ptr::from_ref(rejected.value.as_ref());
        let full = fixture.child.try_send(rejected).err().unwrap();
        assert!(full.is_full());
        assert_eq!(std::ptr::from_ref(full.action().value.as_ref()), allocation);
        let recovered = full.into_action();
        assert_eq!(recovered.value.get(), 7);
        assert_eq!(std::ptr::from_ref(recovered.value.as_ref()), allocation);
        assert_eq!(fixture.actions.load(Ordering::SeqCst), 0);
        drop(recovered);
        let closed_action = fixture.action();
        let allocation = std::ptr::from_ref(closed_action.value.as_ref());
        drop(fixture.actor);
        futures_executor::block_on(first.finish()).unwrap_err();
        assert_eq!(fixture.actions.load(Ordering::SeqCst), 2);
        let closed = fixture.child.try_send(closed_action).err().unwrap();
        assert!(closed.is_store_stopped());
        assert!(!closed.is_scope_ended());
        assert_eq!(
            std::ptr::from_ref(closed.action().value.as_ref()),
            allocation
        );
        let recovered = closed.into_action();
        assert_eq!(std::ptr::from_ref(recovered.value.as_ref()), allocation);
        assert_eq!(fixture.actions.load(Ordering::SeqCst), 2);
        drop(recovered);
        assert_eq!(fixture.actions.load(Ordering::SeqCst), 3);
        assert_eq!(fixture.calls.load(Ordering::SeqCst), 0);
        drop(fixture.child);
        drop(fixture.store);
        assert_eq!(fixture.captures.load(Ordering::SeqCst), 1);
    }
}

#[test]
fn queued_command_retains_embedding_after_scoped_handle_drop() {
    for registered in [false, true] {
        let fixture = Fixture::new(registered, false);
        let receipt = fixture.child.try_send(fixture.action()).ok().unwrap();
        drop(fixture.child);
        drop(fixture.store);
        assert_eq!(fixture.captures.load(Ordering::SeqCst), 0);
        assert_eq!(fixture.actions.load(Ordering::SeqCst), 0);
        drop(fixture.actor);
        futures_executor::block_on(receipt.finish()).unwrap_err();
        assert_eq!(fixture.calls.load(Ordering::SeqCst), 0);
        assert_eq!(fixture.actions.load(Ordering::SeqCst), 1);
        assert_eq!(fixture.captures.load(Ordering::SeqCst), 1);
        assert_eq!(*fixture.events.lock().unwrap(), ["child", "capture"]);
    }
}

#[test]
fn embedding_panic_releases_action_and_closes_receipt() {
    for registered in [false, true] {
        let mut fixture = Fixture::new(registered, true);
        let receipt = fixture.child.try_send(fixture.action()).ok().unwrap();
        drop(fixture.child);
        let result = catch_unwind(AssertUnwindSafe(|| {
            fixture
                .actor
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
        }));
        let panic = result.unwrap_err();
        assert_eq!(
            panic.downcast_ref::<&str>(),
            Some(&"intentional embedding panic")
        );
        assert_eq!(
            *fixture.events.lock().unwrap(),
            ["embed", "child", "capture"]
        );
        assert_eq!(fixture.captures.load(Ordering::SeqCst), 1);
        drop(fixture.actor);
        futures_executor::block_on(receipt.finish()).unwrap_err();
        assert!(fixture.store.is_closed());
        assert_eq!(fixture.calls.load(Ordering::SeqCst), 1);
        assert_eq!(fixture.actions.load(Ordering::SeqCst), 1);
        drop(fixture.store);
        assert_eq!(fixture.captures.load(Ordering::SeqCst), 1);
    }
}

#[test]
fn shutdown_discards_scoped_payload_before_final_embedder_capture() {
    for registered in [false, true] {
        let mut fixture = Fixture::with_capacity(registered, false, 2);
        let mut shutdown = Box::pin(fixture.store.clone().shutdown_into_state());
        let mut cx = Context::from_waker(Waker::noop());
        assert!(shutdown.as_mut().poll(&mut cx).is_pending());
        let receipt = fixture.child.try_send(fixture.action()).ok().unwrap();
        drop(fixture.child);
        assert!(fixture.actor.as_mut().poll(&mut cx).is_ready());
        let Poll::Ready(result) = shutdown.as_mut().poll(&mut cx) else {
            panic!("shutdown must complete after actor cleanup");
        };
        let state = result.unwrap();
        assert_eq!(state.required, 0);
        assert_eq!(state.optional, Some(0));
        futures_executor::block_on(receipt.finish()).unwrap_err();
        assert!(fixture.store.is_closed());
        assert_eq!(fixture.calls.load(Ordering::SeqCst), 0);
        assert_eq!(fixture.actions.load(Ordering::SeqCst), 1);
        assert_eq!(fixture.captures.load(Ordering::SeqCst), 1);
        assert_eq!(*fixture.events.lock().unwrap(), ["child", "capture"]);
    }
}

#[test]
fn stale_scoped_payload_drops_before_final_embedder_capture() {
    let mut fixture = Fixture::with_capacity(true, false, 2);
    let mut removal = fixture.action();
    removal.remove_optional = true;
    removal.drop_probe.label = "remove";
    let removal = fixture.store.try_send(removal).ok().unwrap();
    let receipt = fixture.child.try_send(fixture.action()).ok().unwrap();
    drop(fixture.child);
    let mut cx = Context::from_waker(Waker::noop());
    assert!(fixture.actor.as_mut().poll(&mut cx).is_pending());
    assert!(
        futures_executor::block_on(removal.finish())
            .unwrap()
            .is_finished()
    );
    futures_executor::block_on(receipt.finish()).unwrap_err();
    assert!(!fixture.store.is_closed());
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 0);
    assert_eq!(fixture.actions.load(Ordering::SeqCst), 2);
    assert_eq!(fixture.captures.load(Ordering::SeqCst), 1);
    assert_eq!(
        *fixture.events.lock().unwrap(),
        ["remove", "child", "capture"]
    );
    let mut read = Box::pin(fixture.store.read(|state| (state.required, state.optional)));
    assert!(read.as_mut().poll(&mut cx).is_pending());
    assert!(fixture.actor.as_mut().poll(&mut cx).is_pending());
    assert!(matches!(
        read.as_mut().poll(&mut cx),
        Poll::Ready(Ok((0, None)))
    ));
}

#[test]
#[cfg_attr(
    miri,
    ignore = "deadlock verification launches a subprocess, which Miri isolation forbids"
)]
fn scoped_discard_destructors_can_reenter_root() {
    if allocation_test::run_in_child("scoped_discard_destructors_can_reenter_root") {
        return;
    }

    for registered in [false, true] {
        let fixture = Fixture::new(registered, false);
        let action = fixture.action();
        let outcomes = Arc::new(Mutex::new(Vec::new()));
        for hook in [&action.drop_probe.hook, &fixture.capture_hook] {
            let store = fixture.store.clone();
            let mut nested = fixture.action();
            nested.drop_probe.label = "nested";
            let outcomes = Arc::clone(&outcomes);
            *hook.lock().unwrap() = Some(Box::new(move || {
                // The outcome is asserted afterwards, never inside a destructor.
                let stopped = store
                    .try_send(nested)
                    .is_err_and(|error| error.is_store_stopped());
                outcomes.lock().unwrap().push(stopped);
            }));
        }
        let receipt = fixture.child.try_send(action).ok().unwrap();
        drop(fixture.child);
        drop(fixture.actor);
        futures_executor::block_on(receipt.finish()).unwrap_err();
        assert!(fixture.store.is_closed());
        assert_eq!(*outcomes.lock().unwrap(), [true, true]);
        assert_eq!(
            *fixture.events.lock().unwrap(),
            ["child", "nested", "capture", "nested"]
        );
        assert_eq!(fixture.calls.load(Ordering::SeqCst), 0);
        assert_eq!(fixture.actions.load(Ordering::SeqCst), 3);
        assert_eq!(fixture.captures.load(Ordering::SeqCst), 1);
    }
}
