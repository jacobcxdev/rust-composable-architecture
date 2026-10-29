//! Checks child lifetimes against an independent sequence model and fixed mailbox schedules.
//!
//! Generated operations compare state, commit versions and task ownership after each step.
//! The final mailbox test checks the reference policy alone, not concurrent actor execution.

#![cfg(feature = "test-util")]

#[path = "support/thread_runtime.rs"]
mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

use proptest::prelude::*;
use rca_core::{
    ActionPath, ActionTask, Effect, OptionalStatePath, ReduceContext, Reducer, Store,
    if_some_identified, testing::AssertionGate,
};

use support::ThreadRuntime;

/// Indicates that the effect did not complete a send attempt.
const SEND_PENDING: u8 = 0;
/// Indicates that the released output committed before cancellation.
const SEND_ACCEPTED: u8 = 1;
/// Detects an attempted send after its task or child lifetime ended.
const SEND_CANCELLED: u8 = 2;
/// Detects an attempted send after the root actor stopped.
const SEND_STOPPED: u8 = 3;
/// Places all outputs in one cancellation group within each child lifetime.
const CANCELLATION_ID: u8 = 7;

/// Describes generated caller operations without reading the real actor's state.
#[derive(Clone, Copy, Debug)]
enum Operation {
    /// Commits an ordinary child action only when the model has a child.
    External(u8),
    /// Starts an output held behind a caller-owned gate.
    StartOutput,
    /// Releases a previous output by position, wrapping within the pending list.
    ReleaseOutput(u8),
    /// Inserts a child only when none exists.
    Enter(u8),
    /// Removes the child and retires its effects.
    Leave,
    /// Changes an existing child's identity without inserting a missing child.
    Replace(u8),
    /// Cancels the current child's output group.
    Cancel,
    /// Ends the actor before later operations probe its closed admission.
    Shutdown,
}

/// Distinguishes ordinary commits from effect outputs in the observable history.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Trace {
    /// Records the caller's ordinary action value.
    External(u8),
    /// Records the position of the output that committed.
    Output(u16),
}

/// Compares the actor and model without exposing actor-owned task metadata.
#[derive(Clone, Debug, Eq, PartialEq)]
struct ObservableState {
    /// Identifies the currently installed child, if any.
    child_id: Option<u8>,
    /// Retains commits from both current and retired children.
    trace: Vec<Trace>,
}

/// Stores one child's identity and its commits before parent archival.
struct ChildState {
    /// Defines the child lifetime used by identified composition.
    id: u8,
    /// Accumulates this lifetime's ordinary actions and effect outputs.
    trace: Vec<Trace>,
}

/// Keeps current child state separate from the history of retired children.
struct State {
    /// Holds the optional child selected by the composition path.
    child: Option<ChildState>,
    /// Preserves history when a child leaves or changes identity.
    committed_trace: Vec<Trace>,
}

/// Drives the real child without implementing the model's lifetime rules.
enum ChildAction {
    /// Appends an ordinary caller value to the child's trace.
    External(u8),
    /// Starts a controlled output and publishes the eventual send result.
    StartOutput {
        /// Identifies this output independently of RCA task identifiers.
        id: u16,
        /// Waits for the caller's single release signal.
        release: async_channel::Receiver<()>,
        /// Lets the model distinguish no send attempt from a completed send.
        send_status: Arc<AtomicU8>,
    },
    /// Appends a released effect output to the child's trace.
    Output(u16),
    /// Cancels the child's shared output group.
    Cancel,
}

/// Routes child work and parent lifecycle changes through the public Store API.
enum Action {
    /// Selects the optional child reducer.
    Child(ChildAction),
    /// Inserts a missing child.
    Enter(u8),
    /// Archives and removes a child.
    Leave,
    /// Replaces an existing child with a different identity.
    Replace(u8),
    /// Checks stopped admission without changing live state.
    Probe,
}

/// Records commits and delegates cancellation to RCA.
struct Child;

impl Reducer for Child {
    type State = ChildState;
    type Action = ChildAction;

    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &Self::Action,
        _context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        match action {
            ChildAction::External(value) => {
                state.trace.push(Trace::External(*value));
                Effect::none()
            }
            ChildAction::StartOutput {
                id,
                release,
                send_status,
            } => output_effect(*id, release.clone(), Arc::clone(send_status)),
            ChildAction::Output(id) => {
                state.trace.push(Trace::Output(*id));
                Effect::none()
            }
            ChildAction::Cancel => Effect::cancel(CANCELLATION_ID),
        }
    }
}

/// Holds output until release and records whether its send completed.
fn output_effect(
    id: u16,
    release: async_channel::Receiver<()>,
    send_status: Arc<AtomicU8>,
) -> Effect<ChildAction> {
    Effect::run(move |sender| async move {
        if release.recv().await.is_ok() {
            let status = match sender.send(ChildAction::Output(id)).await {
                Ok(()) => SEND_ACCEPTED,
                Err(error) if error.is_cancelled() => SEND_CANCELLED,
                Err(error) if error.is_store_stopped() => SEND_STOPPED,
                Err(error) => panic!("fixture output has an unexpected delivery failure: {error}"),
            };
            send_status.store(status, Ordering::Release);
        }
    })
    .cancellable(CANCELLATION_ID)
}

/// Applies only state changes, leaving effect retirement to identified composition.
struct Parent;

impl Reducer for Parent {
    type State = State;
    type Action = Action;

    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &Self::Action,
        _context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        match action {
            Action::Enter(id) if state.child.is_none() => {
                state.child = Some(ChildState {
                    id: *id,
                    trace: Vec::new(),
                });
            }
            Action::Leave => archive_child(state),
            Action::Replace(id) if state.child.as_ref().is_some_and(|child| child.id != *id) => {
                archive_child(state);
                state.child = Some(ChildState {
                    id: *id,
                    trace: Vec::new(),
                });
            }
            Action::Child(_) | Action::Enter(_) | Action::Replace(_) | Action::Probe => {}
        }
        Effect::none()
    }
}

/// Preserves a retiring child's commits before removing its state.
fn archive_child(state: &mut State) {
    if let Some(child) = state.child.take() {
        state.committed_trace.extend(child.trace);
    }
}

/// Combines retired and current child history for comparison with the model.
fn observable(state: &State) -> ObservableState {
    let mut trace = state.committed_trace.clone();
    if let Some(child) = &state.child {
        trace.extend_from_slice(&child.trace);
    }
    ObservableState {
        child_id: state.child.as_ref().map(|child| child.id),
        trace,
    }
}

/// Composes parent state changes with identity-bound optional child effects.
fn reducer() -> impl Reducer<State = State, Action = Action> {
    if_some_identified(
        Parent,
        OptionalStatePath::new(
            |state: &State| state.child.as_ref(),
            |state: &mut State| state.child.as_mut(),
        ),
        ActionPath::new(
            |action: &Action| match action {
                Action::Child(action) => Some(action),
                Action::Enter(_) | Action::Leave | Action::Replace(_) | Action::Probe => None,
            },
            Action::Child,
        ),
        |child: &ChildState| &child.id,
        Child,
    )
}

/// Keeps independent model authority beside the real output's control handles.
struct PendingOutput {
    /// Records the model lifetime in which this output started.
    generation: u64,
    /// Comes from model operations, not the real task's completion state.
    active: bool,
    /// Permits one release, even when cancellation already closed the receiver.
    release: Option<async_channel::Sender<()>>,
    /// Observes the effect's actual send result after task completion.
    send_status: Arc<AtomicU8>,
    /// Waits for real work without determining the model's expected authority.
    task: ActionTask,
}

/// Predicts child authority and committed history from caller operations alone.
struct ReferenceModel {
    /// Identifies the expected current child.
    child_id: Option<u8>,
    /// Distinguishes removal and reinsertion even when the public identity repeats.
    generation: u64,
    /// Records only operations that the model permits to commit.
    trace: Vec<Trace>,
    /// Counts actor commits, including parent actions that leave state unchanged.
    version: u64,
    /// Marks shutdown as terminal for all later generated operations.
    stopped: bool,
}

impl ReferenceModel {
    /// Starts with the same explicit child identity as the fixture.
    fn new() -> Self {
        Self {
            child_id: Some(0),
            generation: 0,
            trace: Vec::new(),
            version: 0,
            stopped: false,
        }
    }

    /// Exposes expected state independently of the real actor's observations.
    fn observable(&self) -> ObservableState {
        ObservableState {
            child_id: self.child_id,
            trace: self.trace.clone(),
        }
    }

    /// Retires outputs from earlier model lifetimes after a child identity transition.
    fn invalidate(&self, pending: &mut [PendingOutput]) {
        for output in pending
            .iter_mut()
            .filter(|output| output.active && output.generation != self.generation)
        {
            output.active = false;
        }
    }

    /// Ends all model authority without consulting task completion.
    fn stop(&mut self, pending: &mut [PendingOutput]) {
        self.stopped = true;
        for output in pending {
            output.active = false;
        }
    }
}

/// Owns the Store, assertion gate and host in their required destruction order.
struct Fixture {
    /// Drops before the host joins tasks if an assertion unwinds.
    store: Store<State, Action>,
    /// Controls effect admission without deciding expected state.
    gate: AssertionGate<Action>,
    /// Joins actor and effect threads after Store ownership ends.
    runtime: ThreadRuntime,
}

impl Fixture {
    /// Starts the real composed actor with child identity zero.
    fn new() -> Self {
        let runtime = ThreadRuntime::new();
        let gate = AssertionGate::new();
        let store = Store::builder(
            State {
                child: Some(ChildState {
                    id: 0,
                    trace: Vec::new(),
                }),
                committed_trace: Vec::new(),
            },
            reducer(),
            runtime.runtime(),
        )
        .assertion_gate(gate.clone())
        .build()
        .expect("property Store configuration is valid");
        Self {
            store,
            gate,
            runtime,
        }
    }
}

/// Generates lifecycle transitions and release slots for every possible output position.
fn operation_strategy() -> impl Strategy<Value = Operation> {
    prop_oneof![
        (0_u8..8).prop_map(Operation::External),
        Just(Operation::StartOutput),
        (0_u8..24).prop_map(Operation::ReleaseOutput),
        (0_u8..4).prop_map(Operation::Enter),
        Just(Operation::Leave),
        (0_u8..4).prop_map(Operation::Replace),
        Just(Operation::Cancel),
        Just(Operation::Shutdown),
    ]
}

/// Preserves the requested case budget, except for four cases under Miri.
fn property_config() -> ProptestConfig {
    let defaults = ProptestConfig::default();
    ProptestConfig {
        cases: if cfg!(miri) { 4 } else { defaults.cases },
        failure_persistence: None,
        ..defaults
    }
}

#[test]
fn lifecycle_runner_honours_requested_case_count() {
    let expected = if cfg!(miri) {
        4
    } else {
        ProptestConfig::default().cases
    };
    let executed = std::cell::Cell::new(0_u32);
    proptest::test_runner::TestRunner::new(property_config())
        .run(&Just(()), |()| {
            executed.set(executed.get() + 1);
            Ok(())
        })
        .expect("infallible counting property");
    assert_eq!(executed.get(), expected);
}

/// Checks every step, then shuts down and joins all runtime-owned work.
fn run_sequence(operations: &[Operation]) {
    let fixture = Fixture::new();
    let mut model = ReferenceModel::new();
    let mut pending = Vec::new();

    futures_executor::block_on(async {
        for operation in operations {
            apply_operation(*operation, &fixture, &mut model, &mut pending).await;
            settle_inactive(&pending).await;
            assert_step(&fixture, &model, &pending).await;
        }
        if !model.stopped {
            fixture.store.shutdown().await.expect("property shutdown");
            model.stop(&mut pending);
            settle_inactive(&pending).await;
            assert_step(&fixture, &model, &pending).await;
        }
    });
    fixture.runtime.join_all();
}

/// Applies one caller operation to RCA and updates the independent model.
async fn apply_operation(
    operation: Operation,
    fixture: &Fixture,
    model: &mut ReferenceModel,
    pending: &mut Vec<PendingOutput>,
) {
    if model.stopped {
        fixture
            .store
            .send(Action::Probe)
            .await
            .expect_err("stopped Store rejects model action");
        return;
    }
    match operation {
        Operation::External(value) => external(value, fixture, model).await,
        Operation::StartOutput => start_output(fixture, model, pending).await,
        Operation::ReleaseOutput(slot) => release_output(slot, fixture, model, pending).await,
        Operation::Enter(id) => enter(id, fixture, model, pending).await,
        Operation::Leave => leave(fixture, model, pending).await,
        Operation::Replace(id) => replace(id, fixture, model, pending).await,
        Operation::Cancel => cancel(fixture, model, pending).await,
        Operation::Shutdown => {
            fixture.store.shutdown().await.expect("modelled shutdown");
            model.stop(pending);
        }
    }
}

/// Commits an ordinary action only while the model has a child.
async fn external(value: u8, fixture: &Fixture, model: &mut ReferenceModel) {
    if model.child_id.is_some() {
        commit(&fixture.store, Action::Child(ChildAction::External(value))).await;
        model.trace.push(Trace::External(value));
        model.version += 1;
    }
}

/// Starts a gated output and records its model lifetime before any release.
async fn start_output(
    fixture: &Fixture,
    model: &mut ReferenceModel,
    pending: &mut Vec<PendingOutput>,
) {
    if model.child_id.is_none() {
        return;
    }
    let id = u16::try_from(pending.len()).expect("bounded property output count fits u16");
    let (release, receiver) = async_channel::bounded(1);
    let send_status = Arc::new(AtomicU8::new(SEND_PENDING));
    let task = fixture
        .store
        .send(Action::Child(ChildAction::StartOutput {
            id,
            release: receiver,
            send_status: Arc::clone(&send_status),
        }))
        .await
        .expect("start output commits");
    pending.push(PendingOutput {
        generation: model.generation,
        active: true,
        release: Some(release),
        send_status,
        task,
    });
    model.version += 1;
}

/// Releases one output and checks whether its original lifetime still permits admission.
async fn release_output(
    slot: u8,
    fixture: &Fixture,
    model: &mut ReferenceModel,
    pending: &mut [PendingOutput],
) {
    if pending.is_empty() {
        return;
    }
    let index = usize::from(slot) % pending.len();
    let output = &mut pending[index];
    let Some(release) = output.release.take() else {
        return;
    };
    let was_current =
        output.active && model.child_id.is_some() && output.generation == model.generation;
    // Retired tasks can already have dropped their gate receiver.
    let _ = release.send(()).await;
    if was_current {
        fixture.gate.wait_for_pending().await;
        assert_eq!(
            fixture.gate.inspect_pending(|action| match action {
                Action::Child(ChildAction::Output(id)) => Some(*id),
                _ => None,
            }),
            Some(Some(u16::try_from(index).expect("bounded index fits u16")))
        );
        let receipt = fixture
            .gate
            .release_pending_with_receipt()
            .expect("current output is pending");
        receipt
            .finish()
            .await
            .expect("current output commits")
            .finish()
            .await;
        output.task.finish().await;
        output.active = false;
        model.trace.push(Trace::Output(
            u16::try_from(index).expect("bounded index fits u16"),
        ));
        model.version += 1;
        assert_eq!(output.send_status.load(Ordering::Acquire), SEND_ACCEPTED);
    } else {
        output.task.finish().await;
        output.active = false;
        assert_eq!(output.send_status.load(Ordering::Acquire), SEND_PENDING);
    }
}

/// Inserts a missing child and starts a new model lifetime.
async fn enter(
    id: u8,
    fixture: &Fixture,
    model: &mut ReferenceModel,
    pending: &mut [PendingOutput],
) {
    commit(&fixture.store, Action::Enter(id)).await;
    if model.child_id.is_none() {
        model.generation += 1;
        model.child_id = Some(id);
        model.invalidate(pending);
    }
    model.version += 1;
}

/// Archives a present child and invalidates its outstanding model outputs.
async fn leave(fixture: &Fixture, model: &mut ReferenceModel, pending: &mut [PendingOutput]) {
    commit(&fixture.store, Action::Leave).await;
    if model.child_id.take().is_some() {
        model.generation += 1;
        model.invalidate(pending);
    }
    model.version += 1;
}

/// Retires a child only when the existing identity actually changes.
async fn replace(
    id: u8,
    fixture: &Fixture,
    model: &mut ReferenceModel,
    pending: &mut [PendingOutput],
) {
    commit(&fixture.store, Action::Replace(id)).await;
    if model.child_id.is_some_and(|current| current != id) {
        model.generation += 1;
        model.child_id = Some(id);
        model.invalidate(pending);
    }
    model.version += 1;
}

/// Cancels the current child's outputs without ending its model lifetime.
async fn cancel(fixture: &Fixture, model: &mut ReferenceModel, pending: &mut [PendingOutput]) {
    if model.child_id.is_none() {
        return;
    }
    commit(&fixture.store, Action::Child(ChildAction::Cancel)).await;
    for output in pending
        .iter_mut()
        .filter(|output| output.active && output.generation == model.generation)
    {
        output.active = false;
    }
    model.version += 1;
}

/// Waits for an ordinary action and all work caused by its commit.
async fn commit(store: &Store<State, Action>, action: Action) {
    store
        .send(action)
        .await
        .expect("external model action commits")
        .finish()
        .await;
}

/// Waits for model-retired tasks before comparing the actor's live-task counts.
async fn settle_inactive(pending: &[PendingOutput]) {
    for output in pending.iter().filter(|output| !output.active) {
        output.task.finish().await;
        assert!(output.task.is_finished());
    }
}

/// Compares real state, authority and commit counts against independent model expectations.
async fn assert_step(fixture: &Fixture, model: &ReferenceModel, pending: &[PendingOutput]) {
    let live_count = pending.iter().filter(|output| output.active).count();
    for output in pending {
        assert_eq!(!output.task.is_finished(), output.active);
        if output.active {
            assert!(output.release.is_some());
        }
    }
    assert!(!fixture.gate.has_pending());
    if model.stopped {
        assert!(fixture.store.is_closed());
        assert_eq!(live_count, 0);
        return;
    }
    assert_eq!(
        fixture
            .store
            .read(observable)
            .await
            .expect("observable state"),
        model.observable()
    );
    let snapshot = fixture
        .store
        .runtime_snapshot()
        .await
        .expect("actor remains live");
    assert_eq!(snapshot.commit_version, model.version);
    assert_eq!(snapshot.live_task_ids.len(), live_count);
    assert_eq!(snapshot.cancellation_count, usize::from(live_count != 0));
    assert!(snapshot.timer_deadlines.is_empty());
    assert_eq!(snapshot.observation_count, 0);
}

proptest! {
    #![proptest_config(property_config())]

    #[test]
    fn lifecycle_sequences_match_actor_owned_reference_model(
        operations in prop::collection::vec(operation_strategy(), 1..25)
    ) {
        run_sequence(&operations);
    }
}

#[test]
fn lifecycle_actor_orders_accept_only_current_generation_output() {
    run_sequence(&[
        Operation::StartOutput,
        Operation::ReleaseOutput(0),
        Operation::Leave,
        Operation::External(1),
    ]);
    run_sequence(&[
        Operation::StartOutput,
        Operation::Leave,
        Operation::ReleaseOutput(0),
        Operation::Enter(1),
        Operation::External(2),
    ]);
}

#[test]
fn lifecycle_reference_enumerates_both_mailbox_enqueue_orders() {
    /// Orders effect release and the two possible mailbox submissions.
    #[derive(Clone, Copy)]
    enum Event {
        /// Makes output submission possible.
        Release,
        /// Appends output to the model mailbox.
        EnqueueOutput,
        /// Appends child removal to the model mailbox.
        EnqueueRemoval,
    }

    /// Models actor processing order without using the real runtime.
    #[derive(Clone, Copy)]
    enum Command {
        /// Attempts a commit using the current child authority.
        Output,
        /// Ends the child's authority before later commands.
        Removal,
    }

    let schedules = [
        (
            [Event::Release, Event::EnqueueOutput, Event::EnqueueRemoval],
            (true, false),
        ),
        (
            [Event::Release, Event::EnqueueRemoval, Event::EnqueueOutput],
            (false, true),
        ),
        (
            [Event::EnqueueRemoval, Event::Release, Event::EnqueueOutput],
            (false, true),
        ),
    ];
    for (schedule, expected) in schedules {
        let mut released = false;
        let mut queue = Vec::new();
        for event in schedule {
            match event {
                Event::Release => released = true,
                Event::EnqueueOutput => {
                    assert!(released, "output enqueue follows effect release");
                    queue.push(Command::Output);
                }
                Event::EnqueueRemoval => queue.push(Command::Removal),
            }
        }

        let mut current = true;
        let mut output_committed = false;
        let mut receipt_cancelled = false;
        let mut active_waiters = 1;
        for command in queue {
            match command {
                Command::Output => {
                    if current {
                        output_committed = true;
                    } else {
                        receipt_cancelled = true;
                    }
                    active_waiters = 0;
                }
                Command::Removal => current = false,
            }
        }
        assert_eq!((output_committed, receipt_cancelled), expected);
        assert!(!current);
        assert_eq!(active_waiters, 0);
    }
}
