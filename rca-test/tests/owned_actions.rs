//! Checks one-time payload transfer, child retirement and projected ownership assertions.

use std::cell::Cell;
use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

use futures_executor::block_on;
use rca_core::{
    Effect, EffectSender, KeyedAction, KeyedActionPath, KeyedState, KeyedStatePath, ReduceContext,
    Reducer, Store, combine, for_each,
};
use rca_test::{DeterministicRuntime, TestStore};

/// Retains a receiver after its action gives up ownership.
#[derive(Default)]
struct WorkerState {
    /// Live receiver whose release becomes visible to its sender.
    input: Option<Receiver<u8>>,
    /// Values read before the receiver moves into state.
    received: Vec<u8>,
}

/// Transfers a receiver directly, after a timer, or through an old sender.
enum WorkerAction {
    /// Exports a sender tied to this child lifetime.
    CaptureSender,
    /// Gives the reducer one opportunity to take the receiver.
    Acquire(Cell<Option<Receiver<u8>>>),
    /// Retains the receiver in a delayed action until delivery or cancellation.
    AcquireLater(Cell<Option<Receiver<u8>>>),
}

/// Consumes receivers and exposes a sender for stale-child checks.
struct Worker {
    /// Returns the child-scoped sender without blocking a runtime task.
    captured: mpsc::Sender<EffectSender<WorkerAction>>,
}

impl Reducer for Worker {
    type State = WorkerState;
    type Action = WorkerAction;

    fn reduce(
        &mut self,
        state: &mut WorkerState,
        action: &WorkerAction,
        _: &mut ReduceContext<'_>,
    ) -> Effect<WorkerAction> {
        match action {
            WorkerAction::CaptureSender => {
                let captured = self.captured.clone();
                Effect::run(move |sender| async move {
                    captured.send(sender).expect("capture child-scoped sender");
                    std::future::pending::<()>().await;
                })
            }
            WorkerAction::Acquire(input) => {
                let input = input.take().expect("designated worker takes input once");
                state.received.push(input.try_recv().expect("ready input"));
                state.input = Some(input);
                Effect::none()
            }
            WorkerAction::AcquireLater(input) => Effect::send_after(
                Duration::from_secs(1),
                WorkerAction::Acquire(Cell::new(input.take())),
            ),
        }
    }
}

/// Routes a child action or changes the lifetime at key seven.
enum WorkersAction {
    /// Routes work to the selected child.
    Worker(KeyedAction<u8, WorkerAction>),
    /// Retires the current child and its effects.
    Remove,
    /// Creates a new child at the reused key.
    Insert,
}

/// Changes child membership without consuming child payloads.
struct Workers;

impl Reducer for Workers {
    type State = KeyedState<u8, WorkerState>;
    type Action = WorkersAction;

    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &WorkersAction,
        _: &mut ReduceContext<'_>,
    ) -> Effect<WorkersAction> {
        match action {
            WorkersAction::Remove => {
                state.remove(&7);
            }
            WorkersAction::Insert => state.insert(7, WorkerState::default()),
            WorkersAction::Worker(_) => {}
        }
        Effect::none()
    }
}

/// Builds one keyed child with an externally observable effect sender.
fn workers(
    host: &DeterministicRuntime,
    captured: mpsc::Sender<EffectSender<WorkerAction>>,
) -> Store<KeyedState<u8, WorkerState>, WorkersAction> {
    let reducer = for_each(
        Workers,
        KeyedStatePath::new(|state| state, |state| state),
        KeyedActionPath::new(
            |action| match action {
                WorkersAction::Worker(action) => Some(action),
                WorkersAction::Remove | WorkersAction::Insert => None,
            },
            WorkersAction::Worker,
        ),
        Worker { captured },
    );
    Store::new(
        [(7, WorkerState::default())].into_iter().collect(),
        reducer,
        host.runtime(),
    )
}

#[test]
fn keyed_receiver_moves_into_state_and_is_released_on_removal() {
    // Catches a borrowed reducer failing to retain the receiver, or removal retaining it.
    let host = DeterministicRuntime::new();
    let (capture, _) = mpsc::channel();
    let store = workers(&host, capture);
    let (sender, input) = mpsc::channel();
    sender.send(23).unwrap();
    block_on(host.run(async move {
        store
            .send(WorkersAction::Worker(KeyedAction::new(
                7,
                WorkerAction::Acquire(Cell::new(Some(input))),
            )))
            .await
            .unwrap()
            .finish()
            .await;
        assert_eq!(
            store
                .read(|state| state[&7].received.clone())
                .await
                .unwrap(),
            [23]
        );
        sender.send(41).expect("state still owns receiver");
        store
            .send(WorkersAction::Remove)
            .await
            .unwrap()
            .finish()
            .await;
        assert_eq!(sender.send(59).unwrap_err().0, 59);
        store.shutdown().await.unwrap();
    }))
    .unwrap();
}

#[test]
fn obsolete_child_acquisitions_drop_receivers_without_attaching_to_reused_key() {
    // Catches cancelled timers or old effect senders delivering into a new child lifetime.
    let host = DeterministicRuntime::new();
    let (capture, captured) = mpsc::channel();
    let store = workers(&host, capture);
    let (delayed_sender, delayed_input) = mpsc::channel();
    delayed_sender.send(11).unwrap();
    let (late_sender, late_input) = mpsc::channel();
    late_sender.send(13).unwrap();
    block_on(host.run(async move {
        let running = store
            .send(WorkersAction::Worker(KeyedAction::new(
                7,
                WorkerAction::CaptureSender,
            )))
            .await
            .unwrap();
        // A read drives the already-scheduled capture without blocking on a real thread.
        store.read(|_| ()).await.unwrap();
        let old_sender = captured
            .try_recv()
            .expect("effect ran before read completed");
        let delayed = store
            .send(WorkersAction::Worker(KeyedAction::new(
                7,
                WorkerAction::AcquireLater(Cell::new(Some(delayed_input))),
            )))
            .await
            .unwrap();
        delayed_sender.send(17).expect("timer owns receiver");
        store
            .send(WorkersAction::Remove)
            .await
            .unwrap()
            .finish()
            .await;
        store
            .send(WorkersAction::Insert)
            .await
            .unwrap()
            .finish()
            .await;
        assert_eq!(delayed_sender.send(19).unwrap_err().0, 19);
        old_sender
            .send(WorkerAction::Acquire(Cell::new(Some(late_input))))
            .await
            .expect_err("obsolete child sender rejects ownership");
        assert_eq!(late_sender.send(29).unwrap_err().0, 29);
        running.finish().await;
        delayed.finish().await;
        assert!(running.is_finished());
        assert!(delayed.is_finished());
        assert_eq!(
            store
                .read(|state| (state[&7].input.is_some(), state[&7].received.clone()))
                .await
                .unwrap(),
            (false, vec![])
        );
        store.shutdown().await.unwrap();
    }))
    .unwrap();
}

/// Retains an owned buffer and records reducer ordering.
#[derive(Default)]
struct JobState {
    /// Original allocation after the designated reducer takes the payload.
    buffer: Option<Box<[Cell<u8>]>>,
    /// Sum after every byte increases once.
    sum: u16,
    /// Metadata and whether each later reducer finds the payload consumed.
    inspected: Vec<(u8, bool)>,
}

/// Carries a buffer that can move through a borrowed action once.
enum JobAction {
    /// Moves the buffer into the effect-produced action.
    Start(Cell<Option<Box<[Cell<u8>]>>>),
    /// Keeps metadata available after the buffer is taken.
    Loaded {
        /// Metadata that does not require access to the owned payload.
        id: u8,
        /// Buffer available to exactly one consumer.
        buffer: Cell<Option<Box<[Cell<u8>]>>>,
    },
}

/// Takes the buffer and stores its updated bytes without copying it.
struct Job;

impl Reducer for Job {
    type State = JobState;
    type Action = JobAction;

    fn reduce(
        &mut self,
        state: &mut JobState,
        action: &JobAction,
        _: &mut ReduceContext<'_>,
    ) -> Effect<JobAction> {
        match action {
            JobAction::Start(buffer) => Effect::send(JobAction::Loaded {
                id: 7,
                buffer: Cell::new(buffer.take()),
            }),
            JobAction::Loaded { buffer, .. } => {
                if let Some(buffer) = buffer.take() {
                    for byte in &buffer {
                        byte.set(byte.get() + 1);
                    }
                    state.sum = buffer.iter().map(|byte| u16::from(byte.get())).sum();
                    state.buffer = Some(buffer);
                }
                Effect::none()
            }
        }
    }
}

/// Records whether an earlier reducer already consumed the buffer.
struct InspectMetadata;

impl Reducer for InspectMetadata {
    type State = JobState;
    type Action = JobAction;

    fn reduce(
        &mut self,
        state: &mut JobState,
        action: &JobAction,
        _: &mut ReduceContext<'_>,
    ) -> Effect<JobAction> {
        if let JobAction::Loaded { id, buffer } = action {
            // A competing take sees None after Job. It steals ownership if placed first.
            state.inspected.push((*id, buffer.take().is_none()));
        }
        Effect::none()
    }
}

/// Copies observable results without imposing traits on the owned domain.
#[derive(Clone, Debug, PartialEq)]
struct JobSnapshot {
    /// Byte values after the designated reducer runs.
    bytes: Option<Vec<u8>>,
    /// Independently asserted sum of those bytes.
    sum: u16,
    /// Recorded metadata and ownership observations.
    inspected: Vec<(u8, bool)>,
}

/// Projects state values without taking the buffer or mutating its bytes.
fn job_snapshot(state: &JobState) -> JobSnapshot {
    JobSnapshot {
        bytes: state
            .buffer
            .as_ref()
            .map(|buffer| buffer.iter().map(Cell::get).collect()),
        sum: state.sum,
        inspected: state.inspected.clone(),
    }
}

#[test]
fn finite_buffer_keeps_allocation_through_borrowed_reduction_and_terminal_extraction() {
    // Catches copying the payload, dropping it with the action, or losing it on extraction.
    let buffer = Box::new([Cell::new(3), Cell::new(5), Cell::new(10)]);
    let pointer = buffer.as_ptr();
    let host = DeterministicRuntime::new();
    let store = Store::new(JobState::default(), Job, host.runtime());
    let state = block_on(host.run(async {
        store
            .send(JobAction::Start(Cell::new(Some(buffer))))
            .await
            .unwrap()
            .finish()
            .await;
        store.shutdown_into_state().await.unwrap()
    }))
    .unwrap();
    assert_eq!(state.buffer.as_ref().unwrap().as_ptr(), pointer);
    assert_eq!(
        job_snapshot(&state),
        JobSnapshot {
            bytes: Some(vec![4, 6, 11]),
            sum: 21,
            inspected: vec![]
        }
    );
}

/// Checks the intended result and exposes competing consumers as state mismatches.
///
/// `steal_in_matcher` deliberately consumes the buffer before the reducer can use it.
async fn projected_job(
    reducer: impl Reducer<State = JobState, Action = JobAction> + Send + 'static,
    steal_in_matcher: bool,
) {
    let mut store = TestStore::projected(JobState::default(), reducer, job_snapshot);
    let _task = store
        .send(JobAction::Start(Cell::new(Some(Box::new([
            Cell::new(3),
            Cell::new(5),
            Cell::new(10),
        ])))))
        .await
        .unwrap();
    let _task = store
        .receive_with(
            move |action| {
                let JobAction::Loaded { id, buffer } = action else {
                    return false;
                };
                // The gate matcher runs BEFORE reduction. Ordinary assertions use only metadata.
                if steal_in_matcher {
                    drop(buffer.take());
                }
                *id == 7
            },
            |state| {
                state.bytes = Some(vec![4, 6, 11]);
                state.sum = 21;
                state.inspected.push((7, true));
            },
        )
        .await
        .unwrap();
    store.finish().await.unwrap();
}

#[test]
fn projected_assertions_cover_transfer_result_and_metadata_after_payload_consumption() {
    // Catches an omitted take or wrong result without needing Clone/Debug/Eq on the domain.
    block_on(projected_job(combine((Job, InspectMetadata)), false));
}

#[test]
#[should_panic(expected = "state mismatch")]
fn projected_assertions_reject_competing_consumer_before_designated_owner() {
    block_on(projected_job(combine((InspectMetadata, Job)), false));
}

#[test]
#[should_panic(expected = "state mismatch")]
fn projected_assertions_reject_matcher_consuming_payload_before_reduction() {
    block_on(projected_job(combine((Job, InspectMetadata)), true));
}
