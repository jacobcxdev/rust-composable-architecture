//! Exercises actual named admission against concrete body completion.

#[expect(
    dead_code,
    reason = "this target uses only captured-task host controls"
)]
#[path = "support/mod.rs"]
mod support;

use std::hash::{Hash, Hasher};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::task::{Context, Poll, Wake, Waker};
use std::time::Duration;

use rca_core::{Effect, EffectSender, OperationPolicy, Reduce, RuntimeFuture, Store};

/// Caps thread coordination so a failed ordering check cannot wait forever.
const BOUND: Duration = Duration::from_secs(10);

/// Polls once; each test chooses the next actor or body to run explicitly.
fn poll<T>(future: Pin<&mut (impl Future<Output = T> + ?Sized)>) -> Poll<T> {
    future.poll(&mut Context::from_waker(Waker::noop()))
}

/// Requires one actor turn between submitting a request and receiving its reply.
fn ordered<T>(actor: &mut RuntimeFuture, request: impl Future<Output = T>) -> T {
    let mut request = Box::pin(request);
    assert!(poll(request.as_mut()).is_pending());
    assert!(poll(actor.as_mut()).is_pending());
    match poll(request.as_mut()) {
        Poll::Ready(value) => value,
        Poll::Pending => panic!("actor did not answer ordered request"),
    }
}

/// Checks admission ownership while the rejected action's payload is destroyed.
struct Payload {
    /// Runs before publishing that destruction has finished.
    on_drop: Option<Box<dyn FnOnce() + Send>>,
    /// Lets the sender check that the drop callback has returned.
    destroyed: Arc<AtomicBool>,
}

impl Drop for Payload {
    fn drop(&mut self) {
        self.on_drop.take().unwrap()();
        self.destroyed.store(true, Ordering::Release);
    }
}

/// Starts nested work or attempts delivery through an escaped sender.
enum Action {
    /// Starts the outer named body.
    Start,
    /// Starts a nested body with a separate operation name.
    Inner,
    /// Carries a real drop-observed payload that must never reach the reducer.
    Rejected(Payload),
}

/// Matches by value while exposing the registry's ownership through a weak pointer.
struct OperationKey {
    /// The only part used for equality and hashing.
    id: u8,
    /// Present only in the key supplied by the original effect.
    _lifetime: Option<Arc<()>>,
}

impl PartialEq for OperationKey {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

impl Eq for OperationKey {}

impl Hash for OperationKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.id.hash(state);
    }
}

/// Counts wakes after the actor has become idle, without polling it again.
#[derive(Default)]
struct ActorWake(AtomicUsize);

impl Wake for ActorWake {
    fn wake(self: Arc<Self>) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

/// Holds the producing thread inside the inner operation's completion wake.
struct CompletionBarrier {
    /// Reports that the inner operation reached zero active owners.
    entered: mpsc::Sender<()>,
    /// Holds the terminal transition until the test releases the producer.
    release: Mutex<mpsc::Receiver<()>>,
}

impl Wake for CompletionBarrier {
    fn wake(self: Arc<Self>) {
        self.entered.send(()).unwrap();
        self.release.lock().unwrap().recv_timeout(BOUND).unwrap();
    }
}

/// A rejected payload keeps partial outer ownership until its drop callback ends.
///
/// The inner completion waker pauses before its producing task becomes terminal.
/// Its escaped sender still has authority, but the inner operation rejects new work.
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "One partial-admission sequence preserves payload release before idle-actor wake"
)]
fn partial_admission_releases_after_payload_and_wakes_the_idle_actor() {
    let host = support::ManualRuntime::new();
    let (outer_release, outer_ready) = async_channel::bounded(1);
    let (inner_release, inner_ready) = async_channel::bounded(1);
    let (escaped, escaped_receiver) = mpsc::channel::<EffectSender<Action>>();
    let entered_reducer = Arc::new(AtomicUsize::new(0));
    let reductions = Arc::clone(&entered_reducer);
    let lifetime = Arc::new(());
    let key_lifetime = Arc::downgrade(&lifetime);
    let mut outer_key = Some(OperationKey {
        id: 1,
        _lifetime: Some(lifetime),
    });
    let reducer = Reduce::<(), Action, _>::new(move |(), action, _| match action {
        Action::Start => {
            let ready = outer_ready.clone();
            Effect::run(move |sender| async move {
                sender.send(Action::Inner).await.unwrap();
                ready.recv().await.unwrap();
            })
            .named(outer_key.take().unwrap(), OperationPolicy::Coexist)
        }
        Action::Inner => {
            let escaped = escaped.clone();
            let ready = inner_ready.clone();
            Effect::run(move |sender| async move {
                escaped.send(sender).unwrap();
                ready.recv().await.unwrap();
            })
            .named("inner", OperationPolicy::Coexist)
        }
        Action::Rejected(payload) => {
            // Borrowing the real payload here prevents treating this variant as
            // a synthetic empty command. Neither rejection may enter reduction.
            let _ = &payload.destroyed;
            reductions.fetch_add(1, Ordering::SeqCst);
            Effect::none()
        }
    });
    let store = Store::new((), reducer, host.runtime());
    let outer = store.operation(OperationKey {
        id: 1,
        _lifetime: None,
    });
    let inner = store.operation("inner");
    let mut actor = host.take_only_queued_task();
    let receipt = store.try_send(Action::Start).ok().unwrap();
    assert!(poll(actor.as_mut()).is_pending());
    let mut outer_body = host.take_only_queued_task();
    assert!(poll(outer_body.as_mut()).is_pending());
    assert!(poll(actor.as_mut()).is_pending());
    let mut inner_body = host.take_only_queued_task();
    assert!(poll(outer_body.as_mut()).is_pending());
    assert!(poll(inner_body.as_mut()).is_pending());
    let sender = escaped_receiver.recv_timeout(BOUND).unwrap();

    let outer_wait = Arc::new(Mutex::new(Box::pin(
        ordered(&mut actor, outer.capture()).unwrap(),
    )));
    assert!(poll(outer_wait.lock().unwrap().as_mut()).is_pending());
    let mut inner_wait = Box::pin(ordered(&mut actor, inner.capture()).unwrap());
    let (at_zero, zero_receiver) = mpsc::channel();
    let (unblock, unblocked) = mpsc::channel();
    let barrier = Waker::from(Arc::new(CompletionBarrier {
        entered: at_zero,
        release: Mutex::new(unblocked),
    }));
    assert!(
        inner_wait
            .as_mut()
            .poll(&mut Context::from_waker(&barrier))
            .is_pending()
    );

    let (outer_done, outer_completed) = mpsc::channel();
    let outer_thread = std::thread::spawn(move || {
        futures_executor::block_on(outer_body.as_mut());
        outer_done.send(()).unwrap();
        outer_body
    });
    inner_release.try_send(()).unwrap();
    let inner_thread = std::thread::spawn(move || {
        futures_executor::block_on(inner_body.as_mut());
        inner_body
    });
    zero_receiver.recv_timeout(BOUND).unwrap();

    let actor = Arc::new(Mutex::new(actor));
    let actor_wake = Arc::new(ActorWake::default());
    let destroyed = Arc::new(AtomicBool::new(false));
    let payload = Payload {
        destroyed: Arc::clone(&destroyed),
        on_drop: Some(Box::new({
            let actor = Arc::clone(&actor);
            let wake = Arc::clone(&actor_wake);
            let wait = Arc::clone(&outer_wait);
            move || {
                outer_release.try_send(()).unwrap();
                outer_completed.recv_timeout(BOUND).unwrap();
                // Original outer body is now really gone. Only the admission's
                // partial acquisition can keep this wait pending in payload Drop.
                assert!(poll(wait.lock().unwrap().as_mut()).is_pending());
                let waker = Waker::from(Arc::clone(&wake));
                assert!(
                    actor
                        .lock()
                        .unwrap()
                        .as_mut()
                        .poll(&mut Context::from_waker(&waker))
                        .is_pending()
                );
                wake.0.store(0, Ordering::SeqCst);
            }
        })),
    };
    let rejected = futures_executor::block_on(sender.send(Action::Rejected(payload)));
    assert!(rejected.is_err());
    assert!(destroyed.load(Ordering::Acquire));
    assert!(
        actor_wake.0.load(Ordering::SeqCst) > 0,
        "final partial release must wake the already-idle actor without mailbox traffic"
    );
    assert_eq!(
        poll(outer_wait.lock().unwrap().as_mut()),
        Poll::Ready(Ok(()))
    );
    unblock.send(()).unwrap();
    let inner_shell = inner_thread.join().unwrap();
    let outer_shell = outer_thread.join().unwrap();
    assert_eq!(poll(inner_wait.as_mut()), Poll::Ready(Ok(())));
    // Escaped provenance owns the original generation key too. Drop it before
    // checking that the idle actor alone releases the registry's final owner.
    drop(sender);
    assert!(key_lifetime.upgrade().is_some());
    assert!(poll(actor.lock().unwrap().as_mut()).is_pending());
    assert!(
        key_lifetime.upgrade().is_none(),
        "control processing must reclaim the key before status/capture/new traffic"
    );
    assert_eq!(entered_reducer.load(Ordering::SeqCst), 0);
    assert_eq!(
        ordered(&mut actor.lock().unwrap(), outer.status())
            .unwrap()
            .active_count(),
        0
    );
    assert_eq!(
        ordered(&mut actor.lock().unwrap(), inner.status())
            .unwrap()
            .active_count(),
        0
    );

    drop((store, receipt, inner_shell, outer_shell));
    assert!(poll(actor.lock().unwrap().as_mut()).is_ready());
}

/// An escaped sender cannot restart its operation after the producing body finishes.
#[test]
fn escaped_sender_rejects_actual_post_body_late_admission_without_resurrection() {
    let host = support::ManualRuntime::new();
    let (escaped, receiver) = mpsc::channel();
    let entered = Arc::new(AtomicUsize::new(0));
    let reductions = Arc::clone(&entered);
    let reducer = Reduce::<(), Action, _>::new(move |(), action, _| match action {
        Action::Start => {
            let escaped = escaped.clone();
            Effect::run(move |sender| async move {
                escaped.send(sender).unwrap();
            })
            .named("late", OperationPolicy::Coexist)
        }
        Action::Rejected(payload) => {
            let _ = &payload.destroyed;
            reductions.fetch_add(1, Ordering::SeqCst);
            Effect::none()
        }
        Action::Inner => Effect::none(),
    });
    let store = Store::new((), reducer, host.runtime());
    let operation = store.operation("late");
    let mut actor = host.take_only_queued_task();
    let receipt = store.try_send(Action::Start).ok().unwrap();
    assert!(poll(actor.as_mut()).is_pending());
    let mut body = host.take_only_queued_task();
    let mut captured = Box::pin(ordered(&mut actor, operation.capture()).unwrap());
    assert!(poll(captured.as_mut()).is_pending());
    assert!(poll(body.as_mut()).is_ready());
    let sender = receiver.recv_timeout(BOUND).unwrap();
    assert_eq!(poll(captured.as_mut()), Poll::Ready(Ok(())));
    let destroyed = Arc::new(AtomicBool::new(false));
    let payload = Payload {
        on_drop: Some(Box::new(|| {})),
        destroyed: Arc::clone(&destroyed),
    };
    assert!(futures_executor::block_on(sender.send(Action::Rejected(payload))).is_err());
    assert!(destroyed.load(Ordering::Acquire));
    assert_eq!(entered.load(Ordering::SeqCst), 0);
    assert!(poll(actor.as_mut()).is_pending());
    assert_eq!(
        ordered(&mut actor, operation.status())
            .unwrap()
            .active_count(),
        0
    );
    drop((sender, store, receipt, body));
    assert!(poll(actor.as_mut()).is_ready());
}
