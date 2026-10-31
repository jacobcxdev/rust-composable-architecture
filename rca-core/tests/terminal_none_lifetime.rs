//! Checks terminal action publication, acknowledgement and retained cancellation identity.

#[expect(
    dead_code,
    reason = "terminal-response tests manually poll runtime tasks"
)]
mod support;

use std::cell::Cell;
use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, mpsc};
use std::task::{Context, Poll, Waker};

use rca_core::{
    ActionTask, Effect, EffectSender, Observation, ReduceContext, Reducer, RuntimeFuture, Store,
};

/// A move-only, non-`Sync` action payload counts its own destruction.
struct Payload {
    value: Cell<u64>,
    drops: Arc<AtomicUsize>,
}

impl Drop for Payload {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}

/// Counts key destruction while equality and hashing depend only on its value.
struct Key {
    value: u8,
    drops: Arc<AtomicUsize>,
}

impl PartialEq for Key {
    fn eq(&self, other: &Self) -> bool {
        self.value == other.value
    }
}

impl Eq for Key {}

impl Hash for Key {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.value.hash(state);
    }
}

impl Drop for Key {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}

/// Separates pending-effect launch, generation cancellation and terminal output.
enum Action {
    Start,
    Cancel,
    Value(Payload),
}

/// Exports a cancellable sender and records ordered terminal commits.
struct Feature {
    export: Option<mpsc::Sender<EffectSender<Action>>>,
    commits: Arc<AtomicUsize>,
    key_drops: Arc<AtomicUsize>,
    cancel_key_drops: Arc<AtomicUsize>,
}

impl Reducer for Feature {
    type State = u64;
    type Action = Action;

    fn reduce(
        &mut self,
        state: &mut u64,
        action: &Action,
        _: &mut ReduceContext<'_>,
    ) -> Effect<Action> {
        match action {
            Action::Start => {
                let export = self.export.take().unwrap();
                Effect::run(move |sender| async move {
                    export.send(sender).unwrap();
                    std::future::pending::<()>().await;
                })
                .cancellable(Key {
                    value: 7,
                    drops: Arc::clone(&self.key_drops),
                })
            }
            Action::Cancel => Effect::cancel(Key {
                value: 7,
                drops: Arc::clone(&self.cancel_key_drops),
            }),
            Action::Value(payload) => {
                *state = *state * 10 + payload.value.get();
                self.commits.fetch_add(1, Ordering::SeqCst);
                Effect::none()
            }
        }
    }
}

/// Owns the manually driven actor, effect, sender and independent lifetime witnesses.
struct Fixture {
    runtime: support::ManualRuntime,
    store: Store<u64, Action>,
    actor: RuntimeFuture,
    run: RuntimeFuture,
    task: ActionTask,
    sender: EffectSender<Action>,
    commits: Arc<AtomicUsize>,
    payload_drops: Arc<AtomicUsize>,
    key_drops: Arc<AtomicUsize>,
    cancel_key_drops: Arc<AtomicUsize>,
}

impl Fixture {
    /// Commits launch and exports the sender from its still-pending effect.
    fn new() -> Self {
        let runtime = support::ManualRuntime::new();
        let (export, receive) = mpsc::channel();
        let commits = Arc::new(AtomicUsize::new(0));
        let key_drops = Arc::new(AtomicUsize::new(0));
        let cancel_key_drops = Arc::new(AtomicUsize::new(0));
        let store = Store::new(
            0,
            Feature {
                export: Some(export),
                commits: Arc::clone(&commits),
                key_drops: Arc::clone(&key_drops),
                cancel_key_drops: Arc::clone(&cancel_key_drops),
            },
            runtime.runtime(),
        );
        let mut actor = runtime.take_only_queued_task();
        let receipt = store.try_send(Action::Start).ok().unwrap();
        let mut cx = Context::from_waker(Waker::noop());
        assert!(actor.as_mut().poll(&mut cx).is_pending());
        let task = futures_executor::block_on(receipt.finish()).unwrap();
        let mut run = runtime.take_only_queued_task();
        assert!(run.as_mut().poll(&mut cx).is_pending());
        Self {
            runtime,
            store,
            actor,
            run,
            task,
            sender: receive.try_recv().unwrap(),
            commits,
            payload_drops: Arc::new(AtomicUsize::new(0)),
            key_drops,
            cancel_key_drops,
        }
    }

    /// Creates one counted payload for an effect-produced action.
    fn value(&self, value: u64) -> Action {
        Action::Value(Payload {
            value: Cell::new(value),
            drops: Arc::clone(&self.payload_drops),
        })
    }

    /// Registers a projection with an optional panic at the second published state.
    fn observe(&mut self, panic_at_twelve: bool) -> Observation<u64> {
        let mut registration = Box::pin(self.store.observe(move |state| {
            assert!(
                !panic_at_twelve || *state != 12,
                "intentional publication panic"
            );
            *state
        }));
        let mut cx = Context::from_waker(Waker::noop());
        assert!(registration.as_mut().poll(&mut cx).is_pending());
        assert!(self.actor.as_mut().poll(&mut cx).is_pending());
        futures_executor::block_on(registration).unwrap()
    }

    /// Cancels pending work, releases sender identity and extracts the final state.
    fn finish(mut self) -> u64 {
        let mut cx = Context::from_waker(Waker::noop());
        self.task.cancel();
        assert!(self.actor.as_mut().poll(&mut cx).is_pending());
        assert!(self.run.as_mut().poll(&mut cx).is_ready());
        assert!(self.actor.as_mut().poll(&mut cx).is_pending());
        assert!(self.task.is_finished());
        assert_eq!(
            self.key_drops.load(Ordering::SeqCst),
            0,
            "sender still owns key"
        );
        drop(self.sender);
        let mut shutdown = Box::pin(self.store.shutdown_into_state());
        assert!(shutdown.as_mut().poll(&mut cx).is_pending());
        assert!(self.actor.as_mut().poll(&mut cx).is_ready());
        let state = futures_executor::block_on(shutdown).unwrap();
        drop(self.actor);
        drop(self.run);
        drop(self.task);
        assert_eq!(self.key_drops.load(Ordering::SeqCst), 1);
        state
    }
}

/// Takes a published value immediately or after one actor turn.
fn take(actor: &mut RuntimeFuture, observation: &mut Observation<u64>) -> Option<u64> {
    let mut next = Box::pin(observation.next());
    let mut cx = Context::from_waker(Waker::noop());
    if let Poll::Ready(value) = next.as_mut().poll(&mut cx) {
        return value;
    }
    assert!(actor.as_mut().poll(&mut cx).is_pending());
    let Poll::Ready(value) = next.as_mut().poll(&mut cx) else {
        panic!("actor must answer the published-value take");
    };
    value
}

#[test]
fn terminal_response_publishes_before_ack_without_finishing_root() {
    let mut fixture = Fixture::new();
    let mut observation = fixture.observe(false);
    assert_eq!(take(&mut fixture.actor, &mut observation), Some(0));
    let mut cx = Context::from_waker(Waker::noop());
    for (value, expected, payload_drops) in [(1, 1, 1), (2, 12, 2)] {
        let mut send = Box::pin(fixture.sender.send(fixture.value(value)));
        assert!(send.as_mut().poll(&mut cx).is_pending());
        assert!(send.as_mut().poll(&mut cx).is_pending());
        assert!(!fixture.task.is_finished());
        assert_eq!(fixture.key_drops.load(Ordering::SeqCst), 0);
        assert!(fixture.actor.as_mut().poll(&mut cx).is_pending());
        assert_eq!(take(&mut fixture.actor, &mut observation), Some(expected));
        assert!(matches!(send.as_mut().poll(&mut cx), Poll::Ready(Ok(()))));
        assert!(!fixture.task.is_finished());
        assert_eq!(fixture.runtime.spawn_count(), 2);
        assert_eq!(fixture.key_drops.load(Ordering::SeqCst), 0);
        assert_eq!(fixture.payload_drops.load(Ordering::SeqCst), payload_drops);
    }
    assert_eq!(fixture.commits.load(Ordering::SeqCst), 2);
    assert_eq!(fixture.finish(), 12);
    assert_eq!(futures_executor::block_on(observation.next()), None);
}

#[test]
fn publication_panic_rejects_ack_and_drops_accepted_payload_once() {
    let mut fixture = Fixture::new();
    let mut observation = fixture.observe(true);
    assert_eq!(take(&mut fixture.actor, &mut observation), Some(0));
    let mut cx = Context::from_waker(Waker::noop());
    let mut warm = Box::pin(fixture.sender.send(fixture.value(1)));
    assert!(warm.as_mut().poll(&mut cx).is_pending());
    assert!(fixture.actor.as_mut().poll(&mut cx).is_pending());
    assert!(matches!(warm.as_mut().poll(&mut cx), Poll::Ready(Ok(()))));
    drop(warm);
    assert_eq!(take(&mut fixture.actor, &mut observation), Some(1));
    let mut send = Box::pin(fixture.sender.send(fixture.value(2)));
    assert!(send.as_mut().poll(&mut cx).is_pending());
    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        fixture.actor.as_mut().poll(&mut cx)
    }));
    let panic = panic.expect_err("publication callback must panic");
    assert_eq!(
        panic.downcast_ref::<&str>(),
        Some(&"intentional publication panic")
    );
    assert_eq!(
        fixture.commits.load(Ordering::SeqCst),
        2,
        "reduction preceded panic"
    );
    assert_eq!(fixture.payload_drops.load(Ordering::SeqCst), 2);
    assert_eq!(fixture.key_drops.load(Ordering::SeqCst), 0);
    // An unwound actor is torn down, never repolled or treated as rolled back.
    drop(fixture.actor);
    let Poll::Ready(Err(error)) = send.as_mut().poll(&mut cx) else {
        panic!("failed publication must not acknowledge success");
    };
    assert!(error.is_store_stopped());
    drop(send);
    assert!(fixture.run.as_mut().poll(&mut cx).is_ready());
    drop(fixture.run);
    drop(fixture.sender);
    drop(fixture.task);
    drop(fixture.store);
    assert_eq!(fixture.payload_drops.load(Ordering::SeqCst), 2);
    assert_eq!(fixture.key_drops.load(Ordering::SeqCst), 1);
    assert_eq!(futures_executor::block_on(observation.next()), None);
}

#[test]
fn cancellation_rejects_admitted_output_and_releases_identity_once() {
    let mut fixture = Fixture::new();
    let mut cx = Context::from_waker(Waker::noop());
    let cancel = fixture.store.try_send(Action::Cancel).ok().unwrap();
    let mut send = Box::pin(fixture.sender.send(fixture.value(2)));
    assert!(send.as_mut().poll(&mut cx).is_pending());
    assert!(send.as_mut().poll(&mut cx).is_pending());
    assert_eq!(fixture.payload_drops.load(Ordering::SeqCst), 0);
    assert!(fixture.actor.as_mut().poll(&mut cx).is_pending());
    let cancelled = futures_executor::block_on(cancel.finish()).unwrap();
    assert!(cancelled.is_finished());
    let Poll::Ready(Err(error)) = send.as_mut().poll(&mut cx) else {
        panic!("obsolete admitted output must be rejected");
    };
    assert!(error.is_cancelled());
    assert_eq!(fixture.commits.load(Ordering::SeqCst), 0);
    assert_eq!(fixture.payload_drops.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.key_drops.load(Ordering::SeqCst), 0);
    assert_eq!(fixture.cancel_key_drops.load(Ordering::SeqCst), 1);
    drop(send);
    assert_eq!(fixture.finish(), 0);
}
