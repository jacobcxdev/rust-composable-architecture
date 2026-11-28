//! Checks observation delivery and ownership with explicit actor barriers.

use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll, Waker};

use rca_core::testing::AssertionGate;
use rca_core::{Effect, ReduceContext, Reducer, Store};

use super::{Completion, RuntimeFactory};

/// Adds each action so committed versions have independently expected values.
struct Counter;

impl Reducer for Counter {
    type State = usize;
    type Action = usize;

    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &Self::Action,
        _context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        *state += *action;
        Effect::none()
    }
}

/// Checks initial state, successive committed values and shutdown completion.
pub(super) async fn initial_and_ordered(factory: impl RuntimeFactory) {
    let store = Store::new(0, Counter, factory.runtime());
    let mut values = store.observe(|state| *state).await.expect("observation");

    assert_eq!(values.next().await, Some(0));
    store.send(1).await.expect("first commit").finish().await;
    assert_eq!(values.next().await, Some(1));
    store.send(2).await.expect("second commit").finish().await;
    assert_eq!(values.next().await, Some(3));

    store.shutdown().await.expect("clean shutdown");
    assert_eq!(values.next().await, None);
}

/// Checks that an unread observation retains the newest committed value.
pub(super) async fn slow_consumer_coalescing(factory: impl RuntimeFactory) {
    let store = Store::new(0, Counter, factory.runtime());
    let mut values = store.observe(|state| *state).await.expect("observation");
    assert_eq!(values.next().await, Some(0));

    for _ in 0..32 {
        store.send(1).await.expect("counter commit").finish().await;
    }

    assert_eq!(values.next().await, Some(32));
    store.shutdown().await.expect("clean shutdown");
}

/// Cancels an owned pending take before receiving the next committed value.
pub(super) async fn cancelled_take_resumes(factory: impl RuntimeFactory) {
    let store = Store::new(0, Counter, factory.runtime());
    let mut values = store.observe(|state| *state).await.expect("observation");
    assert_eq!(values.next().await, Some(0));

    let mut pending = Box::pin(values.next());
    assert!(poll_once(pending.as_mut()).is_pending());
    drop(pending);
    store.send(1).await.expect("counter commit").finish().await;

    assert_eq!(values.next().await, Some(1));
    store.shutdown().await.expect("clean shutdown");
}

/// Checks selector calls and exactly-once capture release after observation drop.
pub(super) async fn dropped_observation_closes(factory: impl RuntimeFactory) {
    let store = Store::new(0, Counter, factory.runtime());
    let dropped = Arc::new(Completion::default());
    let invocations = Arc::new(Completion::default());
    let mut values = store
        .observe(tracked_selector(
            Arc::clone(&invocations),
            Arc::clone(&dropped),
        ))
        .await
        .expect("observation");
    assert_eq!(values.next().await, Some(0));
    assert_eq!(invocations.count(), 1);

    store.send(3).await.expect("counter commit").finish().await;
    assert_eq!(values.next().await, Some(3));
    assert_eq!(invocations.count(), 2);

    drop(values);
    dropped.wait_for(1).await;
    assert_eq!(dropped.count(), 1);

    store.shutdown().await.expect("clean shutdown");
}

/// Move-only state prevents the test from relying on a state-cloning fallback.
struct NonCloneState {
    /// Sum of committed actions.
    value: usize,
}

/// Move-only projection checks transfer through the observation's latest-value slot.
struct NonCloneProjection {
    /// Captured committed sum.
    value: usize,
}

/// Updates move-only state without replacing or cloning it.
struct NonCloneCounter;

impl Reducer for NonCloneCounter {
    type State = NonCloneState;
    type Action = usize;

    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &Self::Action,
        _context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        state.value += *action;
        Effect::none()
    }
}

/// Checks move-only values through coalescing and pending-take wakeups.
pub(super) async fn clone_free_notification_take_race(factory: impl RuntimeFactory) {
    let store = Store::new(
        NonCloneState { value: 0 },
        NonCloneCounter,
        factory.runtime(),
    );
    let mut values = store
        .observe(|state| NonCloneProjection { value: state.value })
        .await
        .expect("clone-free observation");
    assert_eq!(values.next().await.expect("initial projection").value, 0);

    store
        .send(1)
        .await
        .expect("first replacement")
        .finish()
        .await;
    store
        .send(1)
        .await
        .expect("second replacement")
        .finish()
        .await;
    assert_eq!(values.next().await.expect("latest replacement").value, 2);

    for expected in 3..=18 {
        let mut next = Box::pin(values.next());
        assert!(poll_once(next.as_mut()).is_pending());
        store.send(1).await.expect("counter commit").finish().await;
        assert_eq!(next.await.expect("committed projection").value, expected);
    }

    drop(values);
    store.shutdown().await.expect("clean shutdown");
}

/// Separates assertion-gate admission from the later observable actor barrier.
#[derive(Clone, Copy)]
enum GatedAction {
    /// Sends the effect action that pauses the actor at its assertion gate.
    Start,
    /// Occupies the gate without changing state.
    Effect,
    /// Commits after the cancelled observation registration leaves the queue.
    Barrier,
}

/// Pauses effect admission so observation construction can be cancelled deterministically.
struct GatedCounter;

impl Reducer for GatedCounter {
    type State = usize;
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
            GatedAction::Barrier => {
                *state += 1;
                Effect::none()
            }
        }
    }
}

/// Checks selector release before polling and after registration enters the mailbox.
pub(super) async fn cancelled_observation_construction(factory: impl RuntimeFactory) {
    let store = Store::new(0, Counter, factory.runtime());
    let before = Arc::new(Completion::default());
    let before_invocations = Arc::new(Completion::default());
    drop(store.observe(tracked_selector(
        Arc::clone(&before_invocations),
        Arc::clone(&before),
    )));
    before.wait_for(1).await;
    assert_eq!(before.count(), 1);
    assert_eq!(
        store
            .runtime_snapshot()
            .await
            .expect("pre-admission snapshot")
            .observation_count,
        0
    );
    assert_eq!(
        before_invocations.count(),
        0,
        "unpolled observation must not evaluate its selector"
    );
    store.shutdown().await.expect("clean shutdown");

    let gate = AssertionGate::new();
    let store = Store::builder(0, GatedCounter, factory.runtime())
        .mailbox_capacity(8)
        .assertion_gate(gate.clone())
        .build()
        .expect("gated Store");
    let gate_task = store.send(GatedAction::Start).await.expect("gate start");
    gate.wait_for_pending().await;

    let after = Arc::new(Completion::default());
    let after_invocations = Arc::new(Completion::default());
    let mut admitted = Box::pin(store.observe(tracked_selector(
        Arc::clone(&after_invocations),
        Arc::clone(&after),
    )));
    assert!(poll_once(admitted.as_mut()).is_pending());
    drop(admitted);
    let barrier = store
        .try_send(GatedAction::Barrier)
        .expect("observation barrier");

    assert!(gate.release_pending());
    barrier.await.expect("barrier commit").finish().await;
    gate_task.finish().await;
    after.wait_for(1).await;
    assert_eq!(after.count(), 1);
    assert_eq!(
        store
            .runtime_snapshot()
            .await
            .expect("post-admission snapshot")
            .observation_count,
        0
    );
    assert_eq!(
        after_invocations.count(),
        0,
        "cancelled registration must not evaluate its selector"
    );
    store.shutdown().await.expect("clean shutdown");
}

/// Inspects an owned future before cancelling it or replacing its noop waker.
fn poll_once<F: std::future::Future>(future: Pin<&mut F>) -> Poll<F::Output> {
    let mut context = Context::from_waker(Waker::noop());
    future.poll(&mut context)
}

/// Counts selector invocations separately from release of its captured resource.
fn tracked_selector(
    invocations: Arc<Completion>,
    dropped: Arc<Completion>,
) -> impl Fn(&usize) -> usize + Send + Sync {
    let capture = DropSignal(dropped);
    move |state| {
        let _keep_capture = &capture;
        invocations.complete();
        *state
    }
}

/// Reports release of a selector capture independently of selector invocation.
struct DropSignal(
    /// Shared event count retained until the selector drops its capture.
    Arc<Completion>,
);

impl Drop for DropSignal {
    fn drop(&mut self) {
        self.0.complete();
    }
}
