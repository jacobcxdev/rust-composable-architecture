//! Admission checks use receipts, ordered reads and an explicit actor gate.
//!
//! No check assumes a task runs within a wall-clock deadline. Concurrent callers
//! may arrive in any order; only successful admission establishes FIFO order.

use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

use rca_core::testing::AssertionGate;
use rca_core::{ActionTask, Effect, ReduceContext, Reducer, Store};

use super::{Completion, RuntimeFactory};

/// Records each reduced input so barriers can inspect admission order and loss.
struct Recorder;

impl Reducer for Recorder {
    type State = Vec<u16>;
    type Action = u16;

    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &Self::Action,
        _context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        state.push(*action);
        Effect::none()
    }
}

/// Checks that three synchronously admitted inputs commit in their admission order.
pub(super) async fn sequential_fifo(factory: impl RuntimeFactory) {
    let store = Store::new(Vec::new(), Recorder, factory.runtime());
    let first = store.try_send(1).expect("first action admission");
    let second = store.try_send(2).expect("second action admission");
    let third = store.try_send(3).expect("third action admission");

    first.await.expect("first commit").finish().await;
    second.await.expect("second commit").finish().await;
    third.await.expect("third commit").finish().await;

    assert_eq!(
        store.read(Clone::clone).await.expect("ordered read"),
        [1, 2, 3]
    );
    store.shutdown().await.expect("clean shutdown");
}

/// Serialises admission and its record together, then compares the actor's order.
///
/// Producers run concurrently, but the lock makes the expected admission order
/// observable without guessing which unsynchronised caller entered first.
pub(super) async fn concurrent_acceptance_order(factory: impl RuntimeFactory) {
    /// Fits in the default mailbox even when no actor work has run yet.
    const PRODUCERS: u16 = 16;

    let store = Store::new(Vec::new(), Recorder, factory.runtime());
    let committed = Arc::new(Mutex::new(Vec::new()));
    let finished = Arc::new(Completion::default());
    for value in 0..PRODUCERS {
        let producer_store = store.clone();
        let producer_committed = Arc::clone(&committed);
        let producer_finished = Arc::clone(&finished);
        factory.spawn_probe(Box::pin(async move {
            let receipt = {
                let mut order = producer_committed.lock().expect("acceptance order lock");
                let receipt = producer_store
                    .try_send(value)
                    .expect("concurrent bounded admission");
                order.push(value);
                receipt
            };
            let task = receipt.await.expect("producer commit");
            task.finish().await;
            producer_finished.complete();
        }));
    }
    finished.wait_for(usize::from(PRODUCERS)).await;

    let state = store.read(Clone::clone).await.expect("ordered read");
    let completed = committed.lock().expect("acceptance order lock").clone();
    assert_eq!(state, completed);
    store.shutdown().await.expect("clean shutdown");
}

/// Marks the two sides of one synchronous reduction.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Phase {
    /// The reducer has entered with this action's identity.
    Enter(u8),
    /// The same reducer call is about to return.
    Leave(u8),
}

/// Gives another OS thread an opportunity to expose overlapping reductions.
struct NonInterleaving;

impl Reducer for NonInterleaving {
    type State = Vec<Phase>;
    type Action = u8;

    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &Self::Action,
        _context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        state.push(Phase::Enter(*action));
        std::thread::yield_now();
        state.push(Phase::Leave(*action));
        Effect::none()
    }
}

/// Checks that each reducer entry is immediately followed by its matching exit.
pub(super) async fn non_interleaving_reducer(factory: impl RuntimeFactory) {
    let store = Store::new(Vec::new(), NonInterleaving, factory.runtime());
    let finished = Arc::new(Completion::default());
    for value in 0..8 {
        let producer_store = store.clone();
        let producer_finished = Arc::clone(&finished);
        factory.spawn_probe(Box::pin(async move {
            producer_store
                .send(value)
                .await
                .expect("concurrent commit")
                .finish()
                .await;
            producer_finished.complete();
        }));
    }
    finished.wait_for(8).await;

    let state = store.read(Clone::clone).await.expect("ordered read");
    assert_eq!(state.len(), 16);
    let (pairs, remainder) = state.as_chunks::<2>();
    assert_eq!(remainder, []);
    for pair in pairs {
        assert!(matches!(pair, [Phase::Enter(left), Phase::Leave(right)] if left == right));
    }
    store.shutdown().await.expect("clean shutdown");
}

/// Checks invalid capacity and eventual delivery through a one-slot mailbox.
pub(super) async fn bounded_backpressure(factory: impl RuntimeFactory) {
    /// Exceeds mailbox capacity without making the scenario a throughput test.
    const ACTIONS: u16 = 64;

    let zero = Store::builder(Vec::new(), Recorder, factory.runtime())
        .mailbox_capacity(0)
        .build()
        .expect_err("zero-capacity mailbox");
    assert!(zero.is_zero_capacity());

    let store = Store::builder(Vec::new(), Recorder, factory.runtime())
        .mailbox_capacity(1)
        .build()
        .expect("bounded Store");
    let finished = Arc::new(Completion::default());
    for value in 0..ACTIONS {
        let producer_store = store.clone();
        let producer_finished = Arc::clone(&finished);
        factory.spawn_probe(Box::pin(async move {
            producer_store
                .send(value)
                .await
                .expect("backpressured commit")
                .finish()
                .await;
            producer_finished.complete();
        }));
    }
    finished.wait_for(usize::from(ACTIONS)).await;

    let mut values = store.read(Clone::clone).await.expect("ordered read");
    values.sort_unstable();
    assert!(values.into_iter().eq(0..ACTIONS));
    store.shutdown().await.expect("clean shutdown");
}

/// Separates the actor-stalling effect from externally admitted test values.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum GatedAction {
    /// Registers the effect that the assertion gate will hold.
    Start,
    /// Occupies the gate without changing the recorded values.
    Effect,
    /// Records a caller's payload after the gate releases the actor.
    Value(u16),
}

/// Starts a gated causal task, then records only explicitly supplied values.
struct GatedRecorder;

impl Reducer for GatedRecorder {
    type State = Vec<u16>;
    type Action = GatedAction;

    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &Self::Action,
        _context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        match action {
            GatedAction::Start => Effect::send(GatedAction::Effect),
            GatedAction::Effect => Effect::none(),
            GatedAction::Value(value) => {
                state.push(*value);
                Effect::none()
            }
        }
    }
}

/// Returns only after the actor has reached a held effect at a known capacity.
///
/// The caller owns both the gate release and the causal task's completion wait.
async fn paused_store(
    factory: &impl RuntimeFactory,
    capacity: usize,
) -> (
    Store<Vec<u16>, GatedAction>,
    AssertionGate<GatedAction>,
    ActionTask,
) {
    let gate = AssertionGate::new();
    let store = Store::builder(Vec::new(), GatedRecorder, factory.runtime())
        .mailbox_capacity(capacity)
        .assertion_gate(gate.clone())
        .build()
        .expect("gated Store");
    let task = store.send(GatedAction::Start).await.expect("gate start");
    gate.wait_for_pending().await;
    (store, gate, task)
}

/// Proves full-queue input recovery and cancellation before async admission.
pub(super) async fn full_mailbox_try_send(factory: impl RuntimeFactory) {
    let (store, gate, gate_task) = paused_store(&factory, 1).await;
    let queued = store
        .try_send(GatedAction::Value(1))
        .expect("first queued action");
    let full = store
        .try_send(GatedAction::Value(2))
        .expect_err("second queued action exceeds capacity");
    assert!(full.is_full());
    assert_eq!(full.into_action(), GatedAction::Value(2));

    let mut backpressured = Box::pin(store.send(GatedAction::Value(3)));
    assert!(poll_once(backpressured.as_mut()).is_pending());
    drop(backpressured);

    assert!(gate.release_pending());
    queued.await.expect("queued commit").finish().await;
    gate_task.finish().await;
    assert_eq!(store.read(Clone::clone).await.expect("ordered read"), [1]);
    store.shutdown().await.expect("clean shutdown");
}

/// Checks that dropping admitted receipts does not cancel their reductions.
pub(super) async fn receipt_cancellation(factory: impl RuntimeFactory) {
    let store = Store::builder(Vec::new(), Recorder, factory.runtime())
        .mailbox_capacity(8)
        .build()
        .expect("bounded Store");

    for value in 0..8 {
        drop(store.try_send(value).expect("bounded admission"));
    }

    let values = store.read(Clone::clone).await.expect("admission barrier");
    assert!(values.into_iter().eq(0..8));
    store.shutdown().await.expect("clean shutdown");
}

/// Distinguishes an unpolled send from an admitted send whose receipt is dropped.
pub(super) async fn cancellation_before_and_after_admission(factory: impl RuntimeFactory) {
    let store = Store::new(Vec::new(), Recorder, factory.runtime());

    drop(store.send(1));
    assert!(store.read(Vec::is_empty).await.expect("pre-admission read"));

    drop(store.try_send(2).expect("admitted action"));
    assert_eq!(
        store.read(Clone::clone).await.expect("admission barrier"),
        [2]
    );
    store.shutdown().await.expect("clean shutdown");
}

/// Distinguishes cancelled unadmitted reads from committed callbacks without waiters.
pub(super) async fn cancelled_reads(factory: impl RuntimeFactory) {
    let store = Store::new(Vec::new(), Recorder, factory.runtime());
    let before = Arc::new(Completion::default());
    let before_query = Arc::clone(&before);
    let query = move |state: &Vec<u16>| {
        before_query.complete();
        state.len()
    };
    drop(store.read(query.clone()));
    store.send(1).await.expect("barrier commit").finish().await;
    assert_eq!(before.count(), 0);
    assert_eq!(store.read(query).await.expect("retained query runs"), 1);
    assert_eq!(before.count(), 1);
    store.shutdown().await.expect("clean shutdown");

    let (store, gate, gate_task) = paused_store(&factory, 8).await;
    let after = Arc::new(Completion::default());
    let after_query = Arc::clone(&after);
    let mut admitted = Box::pin(store.read(move |_| {
        after_query.complete();
    }));
    assert!(poll_once(admitted.as_mut()).is_pending());
    drop(admitted);
    let barrier = store
        .try_send(GatedAction::Value(1))
        .expect("post-read barrier");

    assert!(gate.release_pending());
    barrier.await.expect("barrier commit").finish().await;
    gate_task.finish().await;
    after.wait_for(1).await;
    store.shutdown().await.expect("clean shutdown");
}

/// Uses ordered reads to observe exactly the preceding admitted writes.
pub(super) async fn linearizable_reads(factory: impl RuntimeFactory) {
    let store = Store::new(Vec::new(), Recorder, factory.runtime());

    store.send(1).await.expect("first commit").finish().await;
    let before_second = store.read(Clone::clone).await.expect("first read");
    store.send(2).await.expect("second commit").finish().await;

    assert_eq!(before_second, [1]);
    assert_eq!(store.read(Clone::clone).await.expect("second read"), [1, 2]);
    store.shutdown().await.expect("clean shutdown");
}

/// Type-checks shared handle and send-future bounds using an ordinary constructor.
pub(super) async fn inference_and_public_bounds(factory: impl RuntimeFactory) {
    /// Requires transportability without executing or retaining the input.
    fn assert_send<T: Send>(_value: &T) {}
    /// Requires shared access to the named handle type.
    fn assert_send_sync<T: Send + Sync>() {}

    let store: Store<Vec<u16>, u16> = Store::new(Vec::new(), Recorder, factory.runtime());
    assert_send_sync::<Store<Vec<u16>, u16>>();
    let send = store.send(1);
    assert_send(&send);
    drop(send);
    store.shutdown().await.expect("clean shutdown");
}

/// Samples admission once; the scenario supplies all later progress explicitly.
fn poll_once<F: Future>(future: Pin<&mut F>) -> Poll<F::Output> {
    let mut context = Context::from_waker(Waker::noop());
    future.poll(&mut context)
}
