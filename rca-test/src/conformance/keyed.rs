//! Exercises keyed output, cancellation, replacement and nested scope lifetimes.
//!
//! Explicit state traces distinguish accepted actions from stale output. Completion
//! signals coordinate effect bodies without assuming a fixed number of scheduler turns.
//! A separate thread controls mailbox order while the actor is inside a reducer.

use std::future::pending;
use std::sync::{Arc, Barrier, mpsc};
use std::time::Duration;

use rca_core::{
    ActionPath, Effect, EffectSender, KeyedAction, KeyedActionPath, KeyedState, KeyedStatePath,
    MonotonicInstant, OptionalStatePath, ReduceContext, Reducer, StatePath, Store, combine,
    for_each, if_some, scope,
};

use super::{Completion, RuntimeFactory, poll_once};

/// Proves finite timer delivery independently of the removal ordering scenarios.
const TIMER_DELAY: Duration = Duration::from_millis(4);
/// Deliberately matches across keyed children and an unrelated sibling scope.
const LOCAL_CANCELLATION_ID: u8 = 7;
/// Holds the removal and subsequent stale output while the actor is blocked.
const ORDERING_MAILBOX_CAPACITY: usize = 2;

/// Separates parent reduction, child output and collection membership in observations.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Trace {
    /// Counts accepted actions, including those that leave the child unchanged.
    parent_calls: usize,
    /// Counts output actions that reach a live child.
    outputs: usize,
    /// Distinguishes removal from an unchanged child with no output.
    rows: usize,
}

/// Stores the visible result of accepted child output.
struct Row {
    /// Increments for both immediate output and timer output.
    outputs: usize,
}

/// Keeps child membership separate from the parent action count.
struct State {
    /// Owns the keyed child lifetimes under examination.
    rows: KeyedState<u8, Row>,
    /// Exposes stale actions that incorrectly reach the parent reducer.
    parent_calls: usize,
}

/// Selects effect setup, direct output or same-turn child removal.
enum RowAction {
    /// Starts a gated output task and a timer in the same child scope.
    Start,
    /// Starts gated output and a parked timer before separate-action removal.
    StartForRemoval,
    /// Retains a sender outside its effect body for ordered stale delivery.
    CaptureSender,
    /// Records a gated or externally submitted output action.
    Output,
    /// Records delivery from the child timer.
    TimerOutput,
    /// Returns an effect before the parent removes this child.
    StartAndDelete,
}

/// Combines child actions with explicit parent control actions.
enum Action {
    /// Routes the enclosed action to its keyed child.
    Row(KeyedAction<u8, RowAction>),
    /// Stops the actor inside reduction while another thread fills the mailbox.
    Block,
    /// Ends the selected child lifetime without a child action.
    Remove(u8),
    /// Checks that the parent still accepts actions after cancellation or rejection.
    Probe,
}

/// Emits observable output and exposes effect progress to each scenario.
struct Child {
    /// Signals that the effect reached its gate or exported its sender.
    ready: Arc<Completion>,
    /// Allows the gated effect to attempt output.
    release: Arc<Completion>,
    /// Distinguishes cancellation before output from rejection after an attempted send.
    send_attempted: Arc<Completion>,
    /// Detects execution of an effect returned by a deleting action.
    launched_during_delete: Arc<Completion>,
    /// Records child reduction before the parent removes that child.
    reduced_during_delete: Arc<Completion>,
    /// Transfers a sender without blocking the runtime thread.
    captured_sender: mpsc::SyncSender<EffectSender<RowAction>>,
    /// Signals output reduction independently from effect completion.
    output_reduced: Arc<Completion>,
}

impl Reducer for Child {
    type State = Row;
    type Action = RowAction;

    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &Self::Action,
        _context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        match action {
            RowAction::Start | RowAction::StartForRemoval => {
                let ready = Arc::clone(&self.ready);
                let release = Arc::clone(&self.release);
                let send_attempted = Arc::clone(&self.send_attempted);
                Effect::merge([
                    Effect::run(move |sender| async move {
                        ready.complete();
                        release.wait_for(1).await;
                        send_attempted.complete();
                        let _ = sender.send(RowAction::Output).await;
                    }),
                    if matches!(action, RowAction::StartForRemoval) {
                        Effect::send_at(MonotonicInstant::MAX, RowAction::TimerOutput)
                    } else {
                        Effect::send_after(TIMER_DELAY, RowAction::TimerOutput)
                    },
                ])
            }
            RowAction::CaptureSender => {
                let ready = Arc::clone(&self.ready);
                let captured_sender = self.captured_sender.clone();
                Effect::run(move |sender| async move {
                    let _ = captured_sender.try_send(sender);
                    ready.complete();
                    pending::<()>().await;
                })
            }
            RowAction::StartAndDelete => {
                self.reduced_during_delete.complete();
                let launched = Arc::clone(&self.launched_during_delete);
                Effect::run(move |_sender| async move {
                    launched.complete();
                    pending::<()>().await;
                })
            }
            RowAction::Output => {
                self.output_reduced.complete();
                state.outputs += 1;
                Effect::none()
            }
            RowAction::TimerOutput => {
                state.outputs += 1;
                Effect::none()
            }
        }
    }
}

/// Coordinates one reducer invocation with a separate ordering thread.
///
/// The ordering thread needs no runtime worker. It releases the actor before
/// asserting its results, so a failed assertion does not strand the reducer.
struct ActorBlock {
    /// Pairs the blocked actor with the thread that will submit ordered commands.
    entered: Barrier,
    /// Holds the actor until the ordering thread finishes its mailbox operations.
    release: Barrier,
}

impl ActorBlock {
    /// Creates a two-party rendezvous for actor entry and release.
    fn new() -> Self {
        Self {
            entered: Barrier::new(2),
            release: Barrier::new(2),
        }
    }

    /// Signals reducer entry and waits for the ordering thread to release it.
    fn block(&self) {
        self.entered.wait();
        self.release.wait();
    }

    /// Prevents mailbox submissions until the actor reaches the blocking reducer.
    fn wait_until_blocked(&self) {
        self.entered.wait();
    }

    /// Resumes the actor after the ordering thread attempts its mailbox submissions.
    fn release(&self) {
        self.release.wait();
    }
}

/// Counts every accepted action and controls child removal and actor blocking.
struct Parent {
    /// Shares the actor rendezvous with the ordering thread.
    block: Arc<ActorBlock>,
}

impl Reducer for Parent {
    type State = State;
    type Action = Action;

    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &Self::Action,
        _context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        state.parent_calls += 1;
        match action {
            Action::Row(action) if matches!(action.action(), RowAction::StartAndDelete) => {
                state.rows.remove(action.id());
            }
            Action::Remove(id) => {
                state.rows.remove(id);
            }
            Action::Block => self.block.block(),
            Action::Row(_) | Action::Probe => {}
        }
        Effect::none()
    }
}

/// Projects observable facts without reproducing scope routing or cancellation logic.
fn trace(state: &State) -> Trace {
    Trace {
        parent_calls: state.parent_calls,
        outputs: state.rows.values().map(|row| row.outputs).sum(),
        rows: state.rows.len(),
    }
}

/// Retains the Store and external controls for output and ordering scenarios.
struct Fixture {
    /// Owns one keyed child and the parent action counter.
    store: Store<State, Action>,
    /// Waits until the child effect reaches the intended suspension point.
    ready: Arc<Completion>,
    /// Releases the gated output after the scenario establishes its required order.
    release: Arc<Completion>,
    /// Records whether cancellation prevented the output attempt itself.
    send_attempted: Arc<Completion>,
    /// Detects effects that incorrectly survive same-turn child removal.
    launched_during_delete: Arc<Completion>,
    /// Distinguishes discarded child effects from skipped child reduction.
    reduced_during_delete: Arc<Completion>,
    /// Receives the sender used to submit output from an old child lifetime.
    captured_sender: mpsc::Receiver<EffectSender<RowAction>>,
    /// Allows the live case to observe output before it advances adapter time.
    output_reduced: Arc<Completion>,
    /// Coordinates the actor and the external mailbox-ordering thread.
    block: Arc<ActorBlock>,
}

/// Creates one keyed child and a mailbox with two ordering slots.
fn fixture(factory: &impl RuntimeFactory) -> Fixture {
    let ready = Arc::new(Completion::default());
    let release = Arc::new(Completion::default());
    let send_attempted = Arc::new(Completion::default());
    let launched_during_delete = Arc::new(Completion::default());
    let reduced_during_delete = Arc::new(Completion::default());
    let output_reduced = Arc::new(Completion::default());
    let block = Arc::new(ActorBlock::new());
    let (capture_tx, capture_rx) = mpsc::sync_channel(1);
    let reducer = for_each(
        Parent {
            block: Arc::clone(&block),
        },
        KeyedStatePath::new(
            |state: &State| &state.rows,
            |state: &mut State| &mut state.rows,
        ),
        KeyedActionPath::new(
            |action: &Action| match action {
                Action::Row(action) => Some(action),
                Action::Block | Action::Remove(_) | Action::Probe => None,
            },
            Action::Row,
        ),
        Child {
            ready: Arc::clone(&ready),
            release: Arc::clone(&release),
            send_attempted: Arc::clone(&send_attempted),
            launched_during_delete: Arc::clone(&launched_during_delete),
            reduced_during_delete: Arc::clone(&reduced_during_delete),
            captured_sender: capture_tx,
            output_reduced: Arc::clone(&output_reduced),
        },
    );
    Fixture {
        store: Store::builder(
            State {
                rows: [(1, Row { outputs: 0 })].into_iter().collect(),
                parent_calls: 0,
            },
            reducer,
            factory.runtime(),
        )
        .mailbox_capacity(ORDERING_MAILBOX_CAPACITY)
        .build()
        .expect("keyed Store"),
        ready,
        release,
        send_attempted,
        launched_during_delete,
        reduced_during_delete,
        captured_sender: capture_rx,
        output_reduced,
        block,
    }
}

/// Establishes successful gated output and timer delivery before cancellation checks.
async fn live_outputs_and_timers(factory: impl RuntimeFactory) {
    let Fixture {
        store,
        ready,
        release,
        send_attempted,
        output_reduced,
        ..
    } = fixture(&factory);
    let task = store
        .send(Action::Row(KeyedAction::new(1, RowAction::Start)))
        .await
        .expect("live keyed start commits");
    ready.wait_for(1).await;
    release.complete();
    output_reduced.wait_for(1).await;
    factory
        .advance(TIMER_DELAY)
        .await
        .expect("live keyed timer advances");
    task.finish().await;

    assert_eq!(send_attempted.count(), 1);
    assert_eq!(
        store.read(trace).await.expect("live keyed state"),
        Trace {
            parent_calls: 3,
            outputs: 2,
            rows: 1,
        }
    );
    store.shutdown().await.expect("live keyed Store shuts down");
}

/// Compares live observations with state and task metadata after child removal.
async fn stale_outputs_timers_and_observations(factory: impl RuntimeFactory) {
    let Fixture {
        store,
        ready,
        release,
        send_attempted,
        ..
    } = fixture(&factory);
    let mut observation = store.observe(trace).await.expect("keyed observation");
    assert_eq!(
        observation.next().await,
        Some(Trace {
            parent_calls: 0,
            outputs: 0,
            rows: 1,
        })
    );
    let task = store
        .send(Action::Row(KeyedAction::new(1, RowAction::StartForRemoval)))
        .await
        .expect("keyed start commits");
    assert_eq!(
        observation.next().await,
        Some(Trace {
            parent_calls: 1,
            outputs: 0,
            rows: 1,
        })
    );
    ready.wait_for(1).await;
    assert_eq!(
        store
            .runtime_snapshot()
            .await
            .expect("installed keyed timer")
            .timer_deadlines,
        [MonotonicInstant::MAX],
    );

    store
        .send(Action::Remove(1))
        .await
        .expect("keyed removal commits")
        .finish()
        .await;
    assert_eq!(
        observation.next().await,
        Some(Trace {
            parent_calls: 2,
            outputs: 0,
            rows: 0,
        })
    );
    let removed_version = store
        .runtime_snapshot()
        .await
        .expect("keyed removal snapshot")
        .commit_version;

    release.complete();
    factory
        .advance(TIMER_DELAY)
        .await
        .expect("cancelled keyed timer advances");
    task.finish().await;
    assert!(task.is_finished());
    assert_eq!(send_attempted.count(), 0);
    let snapshot = store
        .runtime_snapshot()
        .await
        .expect("keyed cancellation snapshot");
    assert_eq!(snapshot.commit_version, removed_version);
    assert!(snapshot.live_task_ids.is_empty());
    assert!(snapshot.timer_deadlines.is_empty());
    assert_eq!(snapshot.observation_count, 1);
    let mut next = Box::pin(observation.next());
    assert!(poll_once(next.as_mut()).is_pending());
    drop(next);

    store
        .send(Action::Probe)
        .await
        .expect("keyed Store remains live")
        .finish()
        .await;
    assert_eq!(
        observation.next().await,
        Some(Trace {
            parent_calls: 3,
            outputs: 0,
            rows: 0,
        })
    );
    store.shutdown().await.expect("keyed Store shuts down");
}

/// Orders removal before stale output and detects any subsequent parent reduction.
async fn queued_old_generation_output_is_rejected_before_reduction(factory: impl RuntimeFactory) {
    let Fixture {
        store,
        ready,
        captured_sender,
        output_reduced,
        block,
        ..
    } = fixture(&factory);
    let task = store
        .send(Action::Row(KeyedAction::new(1, RowAction::CaptureSender)))
        .await
        .expect("keyed sender capture commits");
    ready.wait_for(1).await;
    let sender = captured_sender
        .try_recv()
        .expect("old keyed sender is captured");

    let ordering_store = store.clone();
    let ordering_block = Arc::clone(&block);
    let ordering = std::thread::spawn(move || {
        ordering_block.wait_until_blocked();
        let removal = ordering_store.try_send(Action::Remove(1));
        let mut stale_output = Box::pin(sender.send(RowAction::Output));
        let stale_output_is_pending = poll_once(stale_output.as_mut()).is_pending();
        drop(stale_output);
        let full_mailbox = ordering_store.try_send(Action::Probe);
        ordering_block.release();
        assert!(
            full_mailbox
                .expect_err("removal and stale output occupy both slots")
                .is_full(),
            "the stale output was admitted before its future was dropped"
        );
        assert!(
            stale_output_is_pending,
            "old keyed output enters the remaining mailbox slot"
        );
        removal.expect("keyed removal enters the mailbox first")
    });

    store
        .send(Action::Block)
        .await
        .expect("actor ordering block commits")
        .finish()
        .await;
    let removal = ordering.join().expect("ordering thread completes");
    removal
        .finish()
        .await
        .expect("mailbox-ordered keyed removal commits")
        .finish()
        .await;
    task.finish().await;

    assert_eq!(output_reduced.count(), 0);
    assert_eq!(
        store
            .read(|state| (state.parent_calls, state.rows.len()))
            .await
            .expect("state after rejected old keyed output"),
        (3, 0)
    );
    let rejected_snapshot = store
        .runtime_snapshot()
        .await
        .expect("rejected old keyed output snapshot");
    assert_eq!(rejected_snapshot.commit_version, 3);
    assert!(rejected_snapshot.live_task_ids.is_empty());

    store
        .send(Action::Probe)
        .await
        .expect("keyed Store remains live after stale rejection")
        .finish()
        .await;
    assert_eq!(
        store
            .read(|state| (state.parent_calls, state.rows.len()))
            .await
            .expect("live keyed Store state"),
        (4, 0)
    );
    store
        .shutdown()
        .await
        .expect("queued stale output Store shuts down");
}

/// Contrasts a live standalone effect with the same effect after child removal.
async fn deleting_action_discards_its_returned_effect(factory: impl RuntimeFactory) {
    // Without parent removal, the same returned effect must launch and stay live.
    let launched = Arc::new(Completion::default());
    let reduced = Arc::new(Completion::default());
    let (captured_sender, _captured) = mpsc::sync_channel(1);
    let control = Store::new(
        Row { outputs: 0 },
        Child {
            ready: Arc::new(Completion::default()),
            release: Arc::new(Completion::default()),
            send_attempted: Arc::new(Completion::default()),
            launched_during_delete: Arc::clone(&launched),
            reduced_during_delete: Arc::clone(&reduced),
            captured_sender,
            output_reduced: Arc::new(Completion::default()),
        },
        factory.runtime(),
    );
    let task = control
        .send(RowAction::StartAndDelete)
        .await
        .expect("standalone child action commits");
    launched.wait_for(1).await;
    let mut finish = Box::pin(task.finish());
    assert!(poll_once(finish.as_mut()).is_pending());
    drop(finish);
    task.cancel();
    task.finish().await;
    assert!(task.is_finished());
    assert_eq!(launched.count(), 1);
    assert_eq!(reduced.count(), 1);
    assert_eq!(control.read(|row| row.outputs).await.unwrap(), 0);
    let snapshot = control
        .runtime_snapshot()
        .await
        .expect("cancelled standalone effect snapshot");
    assert_eq!(snapshot.commit_version, 1);
    assert!(snapshot.live_task_ids.is_empty());
    control
        .shutdown()
        .await
        .expect("standalone child Store shuts down");

    let Fixture {
        store,
        launched_during_delete,
        reduced_during_delete,
        ..
    } = fixture(&factory);
    store
        .send(Action::Row(KeyedAction::new(1, RowAction::StartAndDelete)))
        .await
        .expect("deleting child action commits")
        .finish()
        .await;
    factory
        .advance(Duration::ZERO)
        .await
        .expect("adapter yields after deleted effect");

    assert_eq!(launched_during_delete.count(), 0);
    assert_eq!(reduced_during_delete.count(), 1);
    let snapshot = store
        .runtime_snapshot()
        .await
        .expect("deleted effect snapshot");
    assert_eq!(snapshot.commit_version, 1);
    assert!(snapshot.live_task_ids.is_empty());
    store
        .shutdown()
        .await
        .expect("deleted effect Store shuts down");
}

/// Makes cancellation observable through task lifetimes rather than state changes.
struct CancellationRow;

/// Places two keyed children beside an independently scoped sibling.
struct CancellationState {
    /// Gives equal cancellation IDs distinct keyed lifetimes.
    rows: KeyedState<u8, CancellationRow>,
    /// Provides a non-keyed scope that must survive keyed cancellation.
    sibling: (),
}

/// Starts or cancels work under the shared local cancellation ID.
enum CancellationRowAction {
    /// Creates work that remains pending until cancellation or shutdown.
    Start,
    /// Cancels matching work only within the selected child scope.
    Cancel,
}

/// Separates keyed cancellation from sibling work and collection mutations.
enum CancellationAction {
    /// Selects the keyed lifetime for a start or cancellation request.
    Row(KeyedAction<u8, CancellationRowAction>),
    /// Starts equal-ID work outside the keyed collection.
    Sibling(CancellationSiblingAction),
    /// Changes row order without changing either row lifetime.
    Move,
    /// Ends one keyed lifetime through parent state removal.
    Remove(u8),
}

/// Starts the unrelated work used as a cancellation isolation control.
enum CancellationSiblingAction {
    /// Keeps sibling work live through the keyed cancellation assertions.
    Start,
}

/// Uses one local cancellation ID for each independently scoped keyed child.
struct CancellationChild;

impl Reducer for CancellationChild {
    type State = CancellationRow;
    type Action = CancellationRowAction;

    fn reduce(
        &mut self,
        _state: &mut Self::State,
        action: &Self::Action,
        _context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        match action {
            CancellationRowAction::Start => {
                Effect::run(|_| pending()).cancellable(LOCAL_CANCELLATION_ID)
            }
            CancellationRowAction::Cancel => Effect::cancel(LOCAL_CANCELLATION_ID),
        }
    }
}

/// Changes collection order or membership without returning its own effects.
struct CancellationParent;

impl Reducer for CancellationParent {
    type State = CancellationState;
    type Action = CancellationAction;

    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &Self::Action,
        _context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        match action {
            CancellationAction::Move => state.rows.move_index(0, 1),
            CancellationAction::Remove(id) => {
                state.rows.remove(id);
            }
            CancellationAction::Row(_) | CancellationAction::Sibling(_) => {}
        }
        Effect::none()
    }
}

/// Reuses the child cancellation ID in an unrelated scope.
struct CancellationSibling;

impl Reducer for CancellationSibling {
    type State = ();
    type Action = CancellationSiblingAction;

    fn reduce(
        &mut self,
        _state: &mut Self::State,
        action: &Self::Action,
        _context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        match action {
            CancellationSiblingAction::Start => {
                Effect::run(|_| pending()).cancellable(LOCAL_CANCELLATION_ID)
            }
        }
    }
}

/// Composes two keyed scopes and a sibling scope with equal cancellation IDs.
fn cancellation_store(
    factory: &impl RuntimeFactory,
) -> Store<CancellationState, CancellationAction> {
    let parent = combine((
        scope(
            StatePath::new(
                |state: &CancellationState| &state.sibling,
                |state: &mut CancellationState| &mut state.sibling,
            ),
            ActionPath::new(
                |action: &CancellationAction| match action {
                    CancellationAction::Sibling(action) => Some(action),
                    CancellationAction::Row(_)
                    | CancellationAction::Move
                    | CancellationAction::Remove(_) => None,
                },
                CancellationAction::Sibling,
            ),
            CancellationSibling,
        ),
        CancellationParent,
    ));
    let reducer = for_each(
        parent,
        KeyedStatePath::new(
            |state: &CancellationState| &state.rows,
            |state: &mut CancellationState| &mut state.rows,
        ),
        KeyedActionPath::new(
            |action: &CancellationAction| match action {
                CancellationAction::Row(action) => Some(action),
                CancellationAction::Sibling(_)
                | CancellationAction::Move
                | CancellationAction::Remove(_) => None,
            },
            CancellationAction::Row,
        ),
        CancellationChild,
    );
    Store::new(
        CancellationState {
            rows: [(1, CancellationRow), (2, CancellationRow)]
                .into_iter()
                .collect(),
            sibling: (),
        },
        reducer,
        factory.runtime(),
    )
}

/// Separates reorder, removal, local cancellation and shutdown by task completion.
async fn move_and_equal_cancellation_ids_remain_isolated(factory: impl RuntimeFactory) {
    let store = cancellation_store(&factory);
    let first = store
        .send(CancellationAction::Row(KeyedAction::new(
            1,
            CancellationRowAction::Start,
        )))
        .await
        .expect("first keyed task starts");
    let second = store
        .send(CancellationAction::Row(KeyedAction::new(
            2,
            CancellationRowAction::Start,
        )))
        .await
        .expect("second keyed task starts");
    let sibling = store
        .send(CancellationAction::Sibling(
            CancellationSiblingAction::Start,
        ))
        .await
        .expect("equal-ID sibling task starts");
    store
        .send(CancellationAction::Move)
        .await
        .expect("keyed move commits")
        .finish()
        .await;
    assert_eq!(
        store
            .read(|state| state.rows.ids().copied().collect::<Vec<_>>())
            .await
            .expect("moved keyed IDs read"),
        [2, 1]
    );
    let mut first_finish = Box::pin(first.finish());
    let mut second_finish = Box::pin(second.finish());
    assert!(poll_once(first_finish.as_mut()).is_pending());
    assert!(poll_once(second_finish.as_mut()).is_pending());
    drop(first_finish);
    drop(second_finish);

    store
        .send(CancellationAction::Remove(1))
        .await
        .expect("first keyed removal commits")
        .finish()
        .await;
    first.finish().await;
    assert!(first.is_finished());
    let mut second_finish = Box::pin(second.finish());
    assert!(poll_once(second_finish.as_mut()).is_pending());
    drop(second_finish);

    store
        .send(CancellationAction::Row(KeyedAction::new(
            2,
            CancellationRowAction::Cancel,
        )))
        .await
        .expect("second local cancellation commits")
        .finish()
        .await;
    second.finish().await;
    assert!(second.is_finished());
    let mut sibling_finish = Box::pin(sibling.finish());
    assert!(poll_once(sibling_finish.as_mut()).is_pending());
    drop(sibling_finish);
    assert_eq!(
        store
            .read(|state| state.rows.ids().copied().collect::<Vec<_>>())
            .await
            .expect("remaining keyed IDs read"),
        [2]
    );
    store
        .shutdown()
        .await
        .expect("isolated keyed Store shuts down");
    sibling.finish().await;
    assert!(sibling.is_finished());
}

/// Selects collection mutations that end the first child's original lifetime.
#[derive(Clone, Copy)]
enum Replacement {
    /// Reuses the same key for a new lifetime within one parent reduction.
    RemoveReinsert,
    /// Ends every entry lifetime through collection clearing.
    Clear,
    /// Removes the first key through the retention callback.
    Retain,
    /// Replaces the collection, even though the visible keys remain identical.
    Assign,
}

/// Keeps replacement identity independent from payload equality.
struct ReplacementState {
    /// Uses unit payloads so only collection operations distinguish lifetimes.
    rows: KeyedState<u8, ()>,
}

/// Starts work that cannot finish naturally before a collection mutation.
enum ReplacementRowAction {
    /// Keeps the original entry lifetime observable through its action task.
    Start,
}

/// Routes child work separately from parent replacement operations.
enum ReplacementAction {
    /// Starts work under one keyed entry lifetime.
    Row(KeyedAction<u8, ReplacementRowAction>),
    /// Applies the selected lifetime-changing collection operation.
    Replace(Replacement),
}

/// Returns pending work so replacement must account for its completion.
struct ReplacementChild;

impl Reducer for ReplacementChild {
    type State = ();
    type Action = ReplacementRowAction;

    fn reduce(
        &mut self,
        _state: &mut Self::State,
        action: &Self::Action,
        _context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        match action {
            ReplacementRowAction::Start => Effect::run(|_| pending()),
        }
    }
}

/// Applies each collection mutation directly, without explicit effect cancellation.
struct ReplacementParent;

impl Reducer for ReplacementParent {
    type State = ReplacementState;
    type Action = ReplacementAction;

    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &Self::Action,
        _context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        let ReplacementAction::Replace(replacement) = action else {
            return Effect::none();
        };
        match replacement {
            Replacement::RemoveReinsert => {
                state.rows.remove(&1);
                state.rows.insert(1, ());
            }
            Replacement::Clear => state.rows.clear(),
            Replacement::Retain => state.rows.retain(|id, ()| *id != 1),
            Replacement::Assign => {
                state.rows = [(1, ()), (2, ())].into_iter().collect();
            }
        }
        Effect::none()
    }
}

/// Requires old work to finish after each supported lifetime-changing mutation.
async fn replacements_cancel_old_entry_lifetimes(factory: impl RuntimeFactory) {
    for replacement in [
        Replacement::RemoveReinsert,
        Replacement::Clear,
        Replacement::Retain,
        Replacement::Assign,
    ] {
        let reducer = for_each(
            ReplacementParent,
            KeyedStatePath::new(
                |state: &ReplacementState| &state.rows,
                |state: &mut ReplacementState| &mut state.rows,
            ),
            KeyedActionPath::new(
                |action: &ReplacementAction| match action {
                    ReplacementAction::Row(action) => Some(action),
                    ReplacementAction::Replace(_) => None,
                },
                ReplacementAction::Row,
            ),
            ReplacementChild,
        );
        let store = Store::new(
            ReplacementState {
                rows: [(1, ()), (2, ())].into_iter().collect(),
            },
            reducer,
            factory.runtime(),
        );
        let task = store
            .send(ReplacementAction::Row(KeyedAction::new(
                1,
                ReplacementRowAction::Start,
            )))
            .await
            .expect("replacement task starts");
        let second = store
            .send(ReplacementAction::Row(KeyedAction::new(
                2,
                ReplacementRowAction::Start,
            )))
            .await
            .expect("second entry task starts");
        let snapshot = store
            .runtime_snapshot()
            .await
            .expect("live replacement task snapshot");
        assert!(!snapshot.live_task_ids.is_empty());
        assert!(!task.is_finished());
        assert!(!second.is_finished());
        store
            .send(ReplacementAction::Replace(replacement))
            .await
            .expect("replacement commits")
            .finish()
            .await;
        task.finish().await;

        assert!(task.is_finished());
        let (expected_ids, second_survives): (&[u8], bool) = match replacement {
            Replacement::RemoveReinsert => (&[2, 1], true),
            Replacement::Clear => (&[], false),
            Replacement::Retain => (&[2], true),
            Replacement::Assign => (&[1, 2], false),
        };
        assert_eq!(
            store
                .read(|state| state.rows.ids().copied().collect::<Vec<_>>())
                .await
                .expect("replacement membership"),
            expected_ids,
        );
        if second_survives {
            let mut finish = Box::pin(second.finish());
            assert!(poll_once(finish.as_mut()).is_pending());
        } else {
            second.finish().await;
            assert!(second.is_finished());
        }
        let snapshot = store
            .runtime_snapshot()
            .await
            .expect("replacement snapshot");
        assert_eq!(snapshot.live_task_ids.is_empty(), !second_survives);
        store
            .shutdown()
            .await
            .expect("replacement Store shuts down");
        second.finish().await;
        assert!(second.is_finished());
    }
}

/// Places an optional child beneath one keyed entry.
struct NestedRow {
    /// Adds a descendant scope beneath the keyed row scope.
    child: Option<()>,
}

/// Owns the collection whose removal must cancel nested optional work.
struct NestedState {
    /// Gives the nested child an enclosing keyed lifetime.
    rows: KeyedState<u8, NestedRow>,
}

/// Starts the pending work at the deepest scope.
enum NestedChildAction {
    /// Keeps nested work live until its enclosing row disappears.
    Start,
}

/// Routes the row action into its optional child scope.
enum NestedRowAction {
    /// Preserves the child action while the optional scope adds its authority.
    Child(NestedChildAction),
}

/// Separates nested child routing from removal at the collection root.
enum NestedAction {
    /// Selects the row before routing to its optional child.
    Row(KeyedAction<u8, NestedRowAction>),
    /// Ends the enclosing keyed lifetime from the parent reducer.
    Remove,
}

/// Creates pending work whose lifetime must descend from both enclosing scopes.
struct NestedChild;

impl Reducer for NestedChild {
    type State = ();
    type Action = NestedChildAction;

    fn reduce(
        &mut self,
        _state: &mut Self::State,
        action: &Self::Action,
        _context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        match action {
            NestedChildAction::Start => Effect::run(|_| pending()),
        }
    }
}

/// Leaves row state unchanged during nested child actions.
struct NestedRowParent;

impl Reducer for NestedRowParent {
    type State = NestedRow;
    type Action = NestedRowAction;

    fn reduce(
        &mut self,
        _state: &mut Self::State,
        _action: &Self::Action,
        _context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        Effect::none()
    }
}

/// Removes the row without directly addressing its optional child.
struct NestedParent;

impl Reducer for NestedParent {
    type State = NestedState;
    type Action = NestedAction;

    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &Self::Action,
        _context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        if matches!(action, NestedAction::Remove) {
            state.rows.remove(&1);
        }
        Effect::none()
    }
}

/// Requires outer keyed removal to finish a live optional descendant task.
async fn nested_optional_work_is_descendant_of_key(factory: impl RuntimeFactory) {
    let row_reducer = if_some(
        NestedRowParent,
        OptionalStatePath::new(
            |state: &NestedRow| state.child.as_ref(),
            |state: &mut NestedRow| state.child.as_mut(),
        ),
        ActionPath::new(
            |action: &NestedRowAction| match action {
                NestedRowAction::Child(action) => Some(action),
            },
            NestedRowAction::Child,
        ),
        NestedChild,
    );
    let reducer = for_each(
        NestedParent,
        KeyedStatePath::new(
            |state: &NestedState| &state.rows,
            |state: &mut NestedState| &mut state.rows,
        ),
        KeyedActionPath::new(
            |action: &NestedAction| match action {
                NestedAction::Row(action) => Some(action),
                NestedAction::Remove => None,
            },
            NestedAction::Row,
        ),
        row_reducer,
    );
    let store = Store::new(
        NestedState {
            rows: [(1, NestedRow { child: Some(()) })].into_iter().collect(),
        },
        reducer,
        factory.runtime(),
    );
    let task = store
        .send(NestedAction::Row(KeyedAction::new(
            1,
            NestedRowAction::Child(NestedChildAction::Start),
        )))
        .await
        .expect("nested keyed task starts");
    let snapshot = store
        .runtime_snapshot()
        .await
        .expect("live nested keyed task snapshot");
    assert!(!snapshot.live_task_ids.is_empty());
    assert!(!task.is_finished());
    store
        .send(NestedAction::Remove)
        .await
        .expect("nested keyed row removal commits")
        .finish()
        .await;
    task.finish().await;

    assert!(task.is_finished());
    let snapshot = store
        .runtime_snapshot()
        .await
        .expect("nested keyed snapshot");
    assert!(snapshot.live_task_ids.is_empty());
    store
        .shutdown()
        .await
        .expect("nested keyed Store shuts down");
}

/// Runs live controls before the keyed rejection and cancellation scenarios.
pub(super) async fn run(factory: impl RuntimeFactory) {
    live_outputs_and_timers(factory.clone()).await;
    stale_outputs_timers_and_observations(factory.clone()).await;
    queued_old_generation_output_is_rejected_before_reduction(factory.clone()).await;
    deleting_action_discards_its_returned_effect(factory.clone()).await;
    move_and_equal_cancellation_ids_remain_isolated(factory.clone()).await;
    replacements_cancel_old_entry_lifetimes(factory.clone()).await;
    nested_optional_work_is_descendant_of_key(factory).await;
}
