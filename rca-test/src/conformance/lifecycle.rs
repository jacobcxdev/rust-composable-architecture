//! Checks Store teardown, owned state extraction and optional-child lifetimes.

use std::future::pending;
use std::sync::Arc;
use std::time::Duration;

use rca_core::{
    ActionPath, Effect, MonotonicInstant, OptionalStatePath, ReduceContext, Reducer, Store, if_some,
};

use super::{Completion, RuntimeFactory, poll_once};

/// Positive delay used to prove finite timer delivery through an optional scope.
const SCOPED_TIMER_DELAY: Duration = Duration::from_millis(4);

/// Records committed updates and reports disposal independently of Store status.
struct State {
    /// Value changed only by accepted input actions.
    value: usize,
    /// Counts actual state destruction, including final-owner teardown.
    dropped: Arc<Completion>,
}

impl Drop for State {
    fn drop(&mut self) {
        self.dropped.complete();
    }
}

#[derive(Clone, Copy)]
/// Selects a finite state update or work that needs external cancellation.
enum Action {
    /// Adds a known value before shutdown or extraction.
    Add(usize),
    /// Starts an effect that cannot finish on its own.
    StartPending,
}

/// Supplies finite updates and indefinitely pending work for teardown checks.
struct Lifecycle;

impl Reducer for Lifecycle {
    type State = State;
    type Action = Action;

    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &Self::Action,
        _context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        match action {
            Action::Add(value) => {
                state.value += value;
                Effect::none()
            }
            Action::StartPending => Effect::run(|_sender| pending()),
        }
    }
}

/// Returns an isolated Store and its independently owned state-drop signal.
fn make_store(factory: &impl RuntimeFactory) -> (Store<State, Action>, Arc<Completion>) {
    let dropped = Arc::new(Completion::default());
    let store = Store::new(
        State {
            value: 0,
            dropped: Arc::clone(&dropped),
        },
        Lifecycle,
        factory.runtime(),
    );
    (store, dropped)
}

/// Checks ordered state disposal, rejected later input and idempotent shutdown.
pub(super) async fn graceful_shutdown(factory: impl RuntimeFactory) {
    let (store, dropped) = make_store(&factory);

    store
        .send(Action::Add(3))
        .await
        .expect("prior commit")
        .finish()
        .await;
    store.shutdown().await.expect("ordered shutdown");

    assert_eq!(dropped.count(), 1);
    assert!(store.is_closed());
    store
        .send(Action::Add(1))
        .await
        .expect_err("closed Store rejects sends");
    store.shutdown().await.expect("idempotent shutdown");
}

/// Requires shutdown to cancel pending work before releasing its state.
pub(super) async fn pending_effect_shutdown(factory: impl RuntimeFactory) {
    let (store, dropped) = make_store(&factory);
    let task = store
        .send(Action::StartPending)
        .await
        .expect("pending task");

    store.shutdown().await.expect("pending-effect shutdown");
    task.finish().await;
    dropped.wait_for(1).await;

    assert!(task.is_cancelled());
    assert!(task.is_finished());
    assert_eq!(dropped.count(), 1);
}

/// Carries a stable allocation through the consuming shutdown handoff.
struct OwnedState {
    /// Nonempty allocation whose address must survive state extraction.
    product: Box<[usize; 1]>,
    /// Distinguishes extraction from premature state destruction.
    dropped: Arc<Completion>,
}

impl Drop for OwnedState {
    fn drop(&mut self) {
        self.dropped.complete();
    }
}

/// Changes the owned payload without replacing its allocation.
struct OwnedReducer;

impl Reducer for OwnedReducer {
    type State = OwnedState;
    type Action = Action;

    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &Self::Action,
        _context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        match action {
            Action::Add(value) => {
                state.product[0] += value;
                Effect::none()
            }
            Action::StartPending => Effect::run(|_sender| pending()),
        }
    }
}

/// Checks state identity, pending-task cancellation and closure of retained handles.
pub(super) async fn owned_state_extraction(factory: impl RuntimeFactory) {
    let dropped = Arc::new(Completion::default());
    let store = Store::new(
        OwnedState {
            product: Box::new([0]),
            dropped: Arc::clone(&dropped),
        },
        OwnedReducer,
        factory.runtime(),
    );
    let survivor = store.clone();
    let closed = survivor.closed();
    let mut observation = survivor
        .observe(|state| state.product[0])
        .await
        .expect("initial observation");
    assert_eq!(observation.next().await, Some(0));
    let identity = survivor
        .read(|state| state.product.as_ptr() as usize)
        .await
        .expect("initial product allocation");

    store
        .send(Action::Add(3))
        .await
        .expect("prior commit")
        .finish()
        .await;
    let pending_task = store
        .send(Action::StartPending)
        .await
        .expect("pending effect");
    let state = store.shutdown_into_state().await.expect("owned handoff");

    assert_eq!(state.product[0], 3);
    assert_eq!(state.product.as_ptr() as usize, identity);
    assert_eq!(dropped.count(), 0);
    closed.await;
    pending_task.finish().await;
    assert!(pending_task.is_cancelled());
    assert!(pending_task.is_finished());
    assert!(survivor.is_closed());
    survivor
        .send(Action::Add(1))
        .await
        .expect_err("extracted Store rejects sends");
    assert_eq!(observation.next().await, None);
    drop(state);
    assert_eq!(dropped.count(), 1);
}

/// Checks that only the last owning Store handle triggers teardown.
pub(super) async fn final_owner_drop(factory: impl RuntimeFactory) {
    let (store, dropped) = make_store(&factory);
    let closed = store.closed();
    let clone = store.clone();

    drop(store);
    assert!(!clone.is_closed());
    drop(clone);
    closed.await;
    dropped.wait_for(1).await;

    assert_eq!(dropped.count(), 1);
}

/// Checks independent state and shutdown for Stores on the same runtime.
pub(super) async fn store_isolation(factory: impl RuntimeFactory) {
    let (left, _) = make_store(&factory);
    let (right, _) = make_store(&factory);

    left.send(Action::Add(2))
        .await
        .expect("left commit")
        .finish()
        .await;
    right
        .send(Action::Add(7))
        .await
        .expect("right commit")
        .finish()
        .await;

    assert_eq!(left.read(|state| state.value).await.expect("left read"), 2);
    assert_eq!(
        right.read(|state| state.value).await.expect("right read"),
        7
    );
    left.shutdown().await.expect("left shutdown");
    assert!(!right.is_closed());
    right.shutdown().await.expect("right shutdown");
}

/// Checks that observations, task handles and closure futures cannot retain a Store.
pub(super) async fn non_owning_resources_close(factory: impl RuntimeFactory) {
    let (store, dropped) = make_store(&factory);
    let mut observation = store
        .observe(|state| state.value)
        .await
        .expect("observation");
    assert_eq!(observation.next().await, Some(0));
    let task = store
        .send(Action::StartPending)
        .await
        .expect("pending task");
    let closed = store.closed();

    drop(store);
    closed.await;
    task.finish().await;
    dropped.wait_for(1).await;

    assert!(task.is_cancelled());
    assert_eq!(observation.next().await, None);
    assert_eq!(dropped.count(), 1);
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
/// Summarises observable optional-child commits without using runtime bookkeeping.
struct ScopedTrace {
    /// Counts every accepted root action, including child output actions.
    parent_calls: usize,
    /// Counts outputs only while the child still exists.
    child_outputs: usize,
    /// Distinguishes an absent child from a present child with no outputs.
    child_present: bool,
}

/// Counts outputs committed during one optional-child lifetime.
struct ScopedChildState {
    /// Number of accepted effect and timer actions.
    outputs: usize,
}

/// Tracks child presence separately from the root's accepted action count.
struct ScopedState {
    /// Removing this value ends its owned effect and timer lifetimes.
    child: Option<ScopedChildState>,
    /// Counts root reduction calls, including incorrectly admitted stale actions.
    parent_calls: usize,
}

/// Exercises handshaken asynchronous output and a scheduled child timer.
enum ScopedChildAction {
    /// Starts gated output and a parked timer for later child removal.
    Start,
    /// Starts a finite timer without a competing removal action.
    StartTimer,
    /// Output released by the scenario's explicit completion gate.
    Output,
    /// Output released when the child timer becomes due.
    TimerOutput,
}

/// Routes child work and independent parent lifetime transitions.
enum ScopedAction {
    /// Work associated with the optional child's current lifetime.
    Child(ScopedChildAction),
    /// Removes the child and invalidates its work.
    Remove,
    /// Proves that the parent remains responsive after child removal.
    Probe,
}

/// Uses independent handshakes to order effect output against child removal.
struct ScopedChild {
    /// Signals that the asynchronous effect reached its release gate.
    ready: Arc<Completion>,
    /// Allows that effect to attempt its output.
    release: Arc<Completion>,
    /// Distinguishes cancellation before the send from rejection after an attempt.
    send_attempted: Arc<Completion>,
    /// Signals completion of the effect's send attempt.
    send_finished: Arc<Completion>,
}

impl Reducer for ScopedChild {
    type State = ScopedChildState;
    type Action = ScopedChildAction;

    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &Self::Action,
        _context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        match action {
            ScopedChildAction::Start => {
                let ready = Arc::clone(&self.ready);
                let release = Arc::clone(&self.release);
                let send_attempted = Arc::clone(&self.send_attempted);
                let send_finished = Arc::clone(&self.send_finished);
                Effect::merge([
                    Effect::run(move |sender| async move {
                        ready.complete();
                        release.wait_for(1).await;
                        send_attempted.complete();
                        let _ = sender.send(ScopedChildAction::Output).await;
                        send_finished.complete();
                    }),
                    Effect::send_at(MonotonicInstant::MAX, ScopedChildAction::TimerOutput),
                ])
            }
            ScopedChildAction::StartTimer => {
                Effect::send_after(SCOPED_TIMER_DELAY, ScopedChildAction::TimerOutput)
            }
            ScopedChildAction::Output | ScopedChildAction::TimerOutput => {
                state.outputs += 1;
                Effect::none()
            }
        }
    }
}

/// Counts accepted actions and removes the optional child on request.
struct ScopedParent;

impl Reducer for ScopedParent {
    type State = ScopedState;
    type Action = ScopedAction;

    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &Self::Action,
        _context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        state.parent_calls += 1;
        if let ScopedAction::Remove = action {
            state.child = None;
        }
        Effect::none()
    }
}

/// Projects user state rather than deriving expectations from actor internals.
fn scoped_trace(state: &ScopedState) -> ScopedTrace {
    ScopedTrace {
        parent_calls: state.parent_calls,
        child_outputs: state.child.as_ref().map_or(0, |child| child.outputs),
        child_present: state.child.is_some(),
    }
}

/// Retains the Store and handshakes needed to order child lifecycle scenarios.
struct ScopedFixture {
    /// Owns the parent actor and optional child lifetime.
    store: Store<ScopedState, ScopedAction>,
    /// Confirms that the child effect reached its wait point.
    ready: Arc<Completion>,
    /// Permits output only after the scenario chooses its order.
    release: Arc<Completion>,
    /// Records whether removal allowed the effect to attempt a send.
    send_attempted: Arc<Completion>,
    /// Allows output-first tests to wait until the send attempt completes.
    send_finished: Arc<Completion>,
}

/// Builds one present optional child with independent effect handshakes.
fn scoped_store(factory: &impl RuntimeFactory) -> ScopedFixture {
    let ready = Arc::new(Completion::default());
    let release = Arc::new(Completion::default());
    let send_attempted = Arc::new(Completion::default());
    let send_finished = Arc::new(Completion::default());
    let reducer = if_some(
        ScopedParent,
        OptionalStatePath::new(
            |state: &ScopedState| state.child.as_ref(),
            |state: &mut ScopedState| state.child.as_mut(),
        ),
        ActionPath::new(
            |action: &ScopedAction| match action {
                ScopedAction::Child(action) => Some(action),
                ScopedAction::Remove | ScopedAction::Probe => None,
            },
            ScopedAction::Child,
        ),
        ScopedChild {
            ready: Arc::clone(&ready),
            release: Arc::clone(&release),
            send_attempted: Arc::clone(&send_attempted),
            send_finished: Arc::clone(&send_finished),
        },
    );
    ScopedFixture {
        store: Store::new(
            ScopedState {
                child: Some(ScopedChildState { outputs: 0 }),
                parent_calls: 0,
            },
            reducer,
            factory.runtime(),
        ),
        ready,
        release,
        send_attempted,
        send_finished,
    }
}

/// Checks both causal orders of child output and parent removal.
pub(super) async fn optional_scope_total_orders(factory: impl RuntimeFactory) {
    live_optional_timer(factory.clone()).await;
    output_before_removal(factory.clone()).await;
    removal_before_output_release(factory).await;
}

/// Proves that optional-scope timer output is accepted when its child stays alive.
async fn live_optional_timer(factory: impl RuntimeFactory) {
    let ScopedFixture { store, .. } = scoped_store(&factory);
    let task = store
        .send(ScopedAction::Child(ScopedChildAction::StartTimer))
        .await
        .expect("finite optional timer starts");
    factory
        .advance(SCOPED_TIMER_DELAY)
        .await
        .expect("finite optional timer advances");
    task.finish().await;
    assert_eq!(
        store.read(scoped_trace).await.expect("live timer state"),
        ScopedTrace {
            parent_calls: 2,
            child_outputs: 1,
            child_present: true,
        }
    );
    store
        .shutdown()
        .await
        .expect("finite optional timer shutdown");
}

/// Requires accepted child output to commit before the subsequent removal.
async fn output_before_removal(factory: impl RuntimeFactory) {
    let ScopedFixture {
        store,
        ready,
        release,
        send_attempted,
        send_finished,
    } = scoped_store(&factory);
    let mut observation = store
        .observe(scoped_trace)
        .await
        .expect("scope observation");
    assert_eq!(
        observation.next().await,
        Some(ScopedTrace {
            parent_calls: 0,
            child_outputs: 0,
            child_present: true,
        })
    );
    let task = store
        .send(ScopedAction::Child(ScopedChildAction::Start))
        .await
        .expect("scoped start commits");
    assert_eq!(
        observation.next().await,
        Some(ScopedTrace {
            parent_calls: 1,
            child_outputs: 0,
            child_present: true,
        })
    );
    ready.wait_for(1).await;
    assert_eq!(
        store
            .runtime_snapshot()
            .await
            .expect("installed child timer")
            .timer_deadlines,
        [MonotonicInstant::MAX],
    );

    release.complete();
    send_finished.wait_for(1).await;
    assert_eq!(send_attempted.count(), 1);
    assert_eq!(
        observation.next().await,
        Some(ScopedTrace {
            parent_calls: 2,
            child_outputs: 1,
            child_present: true,
        })
    );
    store
        .send(ScopedAction::Remove)
        .await
        .expect("removal commits after output")
        .finish()
        .await;
    assert_eq!(
        observation.next().await,
        Some(ScopedTrace {
            parent_calls: 3,
            child_outputs: 0,
            child_present: false,
        })
    );
    factory
        .advance(SCOPED_TIMER_DELAY)
        .await
        .expect("departed timer advances");
    task.finish().await;
    assert!(task.is_finished());
    let snapshot = store
        .runtime_snapshot()
        .await
        .expect("output-first snapshot");
    assert_eq!(snapshot.commit_version, 3);
    assert_eq!(snapshot.live_task_ids, [] as [u64; 0]);
    assert_eq!(snapshot.cancellation_count, 0);
    assert_eq!(
        snapshot.timer_deadlines,
        [] as [rca_core::MonotonicInstant; 0]
    );
    assert_eq!(snapshot.observation_count, 1);
    store.shutdown().await.expect("output-first shutdown");
}

/// Requires removal to cancel unreleased output without another observable commit.
async fn removal_before_output_release(factory: impl RuntimeFactory) {
    let ScopedFixture {
        store,
        ready,
        release,
        send_attempted,
        ..
    } = scoped_store(&factory);
    let mut observation = store
        .observe(scoped_trace)
        .await
        .expect("scope observation");
    assert_eq!(
        observation
            .next()
            .await
            .expect("initial trace")
            .parent_calls,
        0
    );
    let task = store
        .send(ScopedAction::Child(ScopedChildAction::Start))
        .await
        .expect("scoped start commits");
    assert_eq!(
        observation.next().await.expect("start trace").parent_calls,
        1
    );
    ready.wait_for(1).await;
    assert_eq!(
        store
            .runtime_snapshot()
            .await
            .expect("installed child timer")
            .timer_deadlines,
        [MonotonicInstant::MAX],
    );

    store
        .send(ScopedAction::Remove)
        .await
        .expect("removal commits before output release")
        .finish()
        .await;
    assert_eq!(
        observation.next().await,
        Some(ScopedTrace {
            parent_calls: 2,
            child_outputs: 0,
            child_present: false,
        })
    );
    let removed_version = store
        .runtime_snapshot()
        .await
        .expect("removal snapshot")
        .commit_version;
    assert_eq!(removed_version, 2);

    release.complete();
    factory
        .advance(SCOPED_TIMER_DELAY)
        .await
        .expect("cancelled output advances");
    task.finish().await;
    assert!(task.is_finished());
    assert_eq!(send_attempted.count(), 0);
    let snapshot = store
        .runtime_snapshot()
        .await
        .expect("rejected output snapshot");
    assert_eq!(snapshot.commit_version, removed_version);
    assert_eq!(snapshot.live_task_ids, [] as [u64; 0]);
    assert_eq!(snapshot.cancellation_count, 0);
    assert_eq!(
        snapshot.timer_deadlines,
        [] as [rca_core::MonotonicInstant; 0]
    );
    assert_eq!(snapshot.observation_count, 1);
    let mut next = Box::pin(observation.next());
    assert!(poll_once(next.as_mut()).is_pending());
    drop(next);

    store
        .send(ScopedAction::Probe)
        .await
        .expect("Store remains live")
        .finish()
        .await;
    assert_eq!(
        observation.next().await,
        Some(ScopedTrace {
            parent_calls: 3,
            child_outputs: 0,
            child_present: false,
        })
    );
    let snapshot = store.runtime_snapshot().await.expect("live Store snapshot");
    assert_eq!(snapshot.commit_version, 3);
    assert_eq!(snapshot.live_task_ids, [] as [u64; 0]);
    assert_eq!(snapshot.cancellation_count, 0);
    assert_eq!(
        snapshot.timer_deadlines,
        [] as [rca_core::MonotonicInstant; 0]
    );
    assert_eq!(snapshot.observation_count, 1);
    store.shutdown().await.expect("removal-first shutdown");
}

#[cfg(test)]
mod tests {
    use super::{SCOPED_TIMER_DELAY, output_before_removal, poll_once};
    use crate::conformance::DeterministicFactory;

    #[test]
    fn optional_scope_ordering_allows_a_delayed_caller() {
        let factory = DeterministicFactory::new();
        let scheduler = factory.scheduler();
        let mut scenario = Box::pin(output_before_removal(factory));

        // Register the initial observation, then submit the child start action.
        assert!(poll_once(scenario.as_mut()).is_pending());
        scheduler
            .run_until_stalled()
            .expect("initial observation registers");
        assert!(poll_once(scenario.as_mut()).is_pending());
        scheduler
            .run_until_stalled()
            .expect("child starts before the caller resumes");

        // A real-clock runtime can progress while its caller is descheduled.
        scheduler
            .advance(SCOPED_TIMER_DELAY)
            .expect("timer expires during the caller's scheduling delay");
        futures_executor::block_on(scheduler.run(scenario))
            .expect("a conforming runtime still passes the ordering scenario");
        futures_executor::block_on(scheduler.drain_deadlines(0))
            .expect("the scenario releases its actor and timers");
    }
}
