//! Nested effect mappings preserve captures through cancellation and panic.

#[expect(dead_code, reason = "mapped-send tests manually poll runtime tasks")]
mod support;

use std::cell::Cell;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::task::{Context, Poll, Waker};

use rca_core::{Effect, EffectSender, ReduceContext, Reducer, RuntimeFuture, Store};

/// Counts action or mapper-capture disposal without running user work.
struct DropCount(Arc<AtomicUsize>);

impl Drop for DropCount {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

struct Child {
    value: Cell<usize>,
    _drop: DropCount,
}

struct Middle(Child);

enum Action {
    Start,
    Value(Middle),
}

/// Exports the real effect sender after its run body starts.
struct Feature {
    export: Option<mpsc::Sender<EffectSender<Child>>>,
    mappings: Arc<Mutex<Vec<u8>>>,
    captures: Arc<AtomicUsize>,
    commits: Arc<AtomicUsize>,
    panic_on_map: bool,
}

impl Reducer for Feature {
    type State = ();
    type Action = Action;

    fn reduce(
        &mut self,
        (): &mut (),
        action: &Action,
        _: &mut ReduceContext<'_>,
    ) -> Effect<Action> {
        match action {
            Action::Start => {
                let export = self.export.take().unwrap();
                let first_capture = DropCount(Arc::clone(&self.captures));
                let second_capture = DropCount(Arc::clone(&self.captures));
                let first_mappings = Arc::clone(&self.mappings);
                let second_mappings = Arc::clone(&self.mappings);
                let panic_on_map = self.panic_on_map;
                Effect::run(move |sender| async move {
                    export.send(sender).unwrap();
                    std::future::pending::<()>().await;
                })
                .map(move |child| {
                    std::hint::black_box(&first_capture);
                    first_mappings.lock().unwrap().push(1);
                    Middle(child)
                })
                .map(move |middle| {
                    std::hint::black_box(&second_capture);
                    second_mappings.lock().unwrap().push(2);
                    assert!(!panic_on_map, "intentional mapping panic");
                    Action::Value(middle)
                })
            }
            Action::Value(middle) => {
                self.commits
                    .fetch_add(middle.0.value.get(), Ordering::SeqCst);
                Effect::none()
            }
        }
    }
}

/// Holds the unpolled actor and pending run body beside the escaped sender.
struct Fixture {
    _runtime: support::ManualRuntime,
    store: Store<(), Action>,
    actor: RuntimeFuture,
    _run: RuntimeFuture,
    sender: EffectSender<Child>,
    mappings: Arc<Mutex<Vec<u8>>>,
    captures: Arc<AtomicUsize>,
    actions: Arc<AtomicUsize>,
    commits: Arc<AtomicUsize>,
}

impl Fixture {
    /// Starts the run body and waits for sender export without polling an action.
    fn new(panic_on_map: bool) -> Self {
        let runtime = support::ManualRuntime::new();
        let (export, receive) = mpsc::channel();
        let mappings = Arc::new(Mutex::new(Vec::new()));
        let captures = Arc::new(AtomicUsize::new(0));
        let commits = Arc::new(AtomicUsize::new(0));
        let store = Store::new(
            (),
            Feature {
                export: Some(export),
                mappings: Arc::clone(&mappings),
                captures: Arc::clone(&captures),
                commits: Arc::clone(&commits),
                panic_on_map,
            },
            runtime.runtime(),
        );
        let mut actor = runtime.take_only_queued_task();
        let receipt = store.try_send(Action::Start).ok().unwrap();
        let mut cx = Context::from_waker(Waker::noop());
        assert!(actor.as_mut().poll(&mut cx).is_pending());
        drop(futures_executor::block_on(receipt.finish()).unwrap());
        let mut run = runtime.take_only_queued_task();
        assert!(run.as_mut().poll(&mut cx).is_pending());
        let sender = receive.try_recv().unwrap();
        Self {
            _runtime: runtime,
            store,
            actor,
            _run: run,
            sender,
            mappings,
            captures,
            actions: Arc::new(AtomicUsize::new(0)),
            commits,
        }
    }

    fn child(&self) -> Child {
        Child {
            value: Cell::new(7),
            _drop: DropCount(Arc::clone(&self.actions)),
        }
    }
}

#[test]
fn nested_mapped_send_preserves_cancellation_acknowledgement_and_capture_lifetimes() {
    let mut fixture = Fixture::new(false);
    drop(fixture.sender.send(fixture.child()));
    assert!(fixture.mappings.lock().unwrap().is_empty());
    assert_eq!(fixture.actions.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.captures.load(Ordering::SeqCst), 0);

    let mut cx = Context::from_waker(Waker::noop());
    let mut cancelled = Box::pin(fixture.sender.send(fixture.child()));
    assert!(cancelled.as_mut().poll(&mut cx).is_pending());
    assert_eq!(*fixture.mappings.lock().unwrap(), [1, 2]);
    assert_eq!(fixture.captures.load(Ordering::SeqCst), 0);
    drop(cancelled);
    assert_eq!(fixture.actions.load(Ordering::SeqCst), 1);
    assert!(fixture.actor.as_mut().poll(&mut cx).is_pending());
    assert_eq!(fixture.commits.load(Ordering::SeqCst), 7);
    assert_eq!(fixture.actions.load(Ordering::SeqCst), 2);

    let mut acknowledged = Box::pin(fixture.sender.send(fixture.child()));
    assert!(acknowledged.as_mut().poll(&mut cx).is_pending());
    assert!(fixture.actor.as_mut().poll(&mut cx).is_pending());
    let Poll::Ready(result) = acknowledged.as_mut().poll(&mut cx) else {
        panic!("mapped send must acknowledge its commit");
    };
    result.unwrap();
    drop(acknowledged);
    assert_eq!(*fixture.mappings.lock().unwrap(), [1, 2, 1, 2]);
    assert_eq!(fixture.commits.load(Ordering::SeqCst), 14);
    assert_eq!(fixture.actions.load(Ordering::SeqCst), 3);
    assert_eq!(fixture.captures.load(Ordering::SeqCst), 0);
    drop(fixture.sender);
    assert_eq!(fixture.captures.load(Ordering::SeqCst), 2);
}

#[test]
fn nested_mapping_panic_drops_action_without_root_admission() {
    let mut fixture = Fixture::new(true);
    let mut send = Box::pin(fixture.sender.send(fixture.child()));
    let panic = catch_unwind(AssertUnwindSafe(|| {
        send.as_mut().poll(&mut Context::from_waker(Waker::noop()))
    }))
    .expect_err("the second mapping must panic before root admission");
    assert_eq!(
        panic.downcast_ref::<&str>(),
        Some(&"intentional mapping panic")
    );
    drop(send);
    assert_eq!(*fixture.mappings.lock().unwrap(), [1, 2]);
    assert_eq!(fixture.actions.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.captures.load(Ordering::SeqCst), 0);
    assert!(
        fixture
            .actor
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_pending()
    );
    assert_eq!(fixture.commits.load(Ordering::SeqCst), 0);
    assert!(!fixture.store.is_closed());
    drop(fixture.sender);
    assert_eq!(fixture.captures.load(Ordering::SeqCst), 2);
}
