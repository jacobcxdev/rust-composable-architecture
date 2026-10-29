//! Exercises a finite keyed job queue with real client limits, retry delays and cancellation.
//!
//! Job IDs are unique and internal actions come only from this fixture. The
//! client owns the resource bound; reducer reservations alone do not prove it.

use std::{
    collections::{BTreeMap, VecDeque},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU8, Ordering},
    },
    time::Duration,
};

use async_channel::{Receiver, Sender};
use futures_executor::block_on;
use rca_core::{
    DependencyKey, Effect, KeyedAction, KeyedActionPath, KeyedState, KeyedStatePath, ReduceContext,
    Reducer, for_each,
};
use rca_test::TestStore;

// Removing the reservation bound must fail before any child Begin is released.
#[test]
fn admission_is_bounded_and_results_keep_identity() {
    block_on(async {
        let (client, controls) = client(5);
        let mut store = store(5, client.clone());
        let root = store
            .send_with(BatchAction::Start, |s| {
                s.started = true;
                admit(s, &[0, 1]);
            })
            .await
            .unwrap();
        let mut began = [begin(&mut store).await, begin(&mut store).await];
        began.sort_unstable();
        assert_eq!(began, [0, 1]);
        assert_active(&client, 2);
        for (id, next) in [
            (1, Some(2)),
            (0, Some(3)),
            (3, Some(4)),
            (2, None),
            (4, None),
        ] {
            let value = 100 + u16::from(id);
            controls[&(id, 1)].try_send(Ok(value)).unwrap();
            let _ = store
                .receive_with(
                    move |a| {
                        matches!(a, BatchAction::Job(k)
                if *k.id() == id && *k.action() == JobAction::Response(Ok(value)))
                    },
                    move |s| {
                        terminal(s, id, Outcome::Succeeded(value), next);
                    },
                )
                .await
                .unwrap();
            if let Some(next) = next {
                assert_eq!(begin(&mut store).await, next);
            }
        }
        root.finish().await.unwrap();
        store
            .assert_state(|s| {
                assert!(s.pending.is_empty());
                assert!(s.jobs.is_empty());
                assert_eq!(
                    s.outcomes,
                    [
                        (0, Outcome::Succeeded(100)),
                        (1, Outcome::Succeeded(101)),
                        (2, Outcome::Succeeded(102)),
                        (3, Outcome::Succeeded(103)),
                        (4, Outcome::Succeeded(104))
                    ]
                    .into_iter()
                    .collect()
                );
            })
            .await
            .unwrap();
        assert_eq!(client.inspect(|activity| activity.peak), 2);
        assert_active(&client, 0);
        store.finish().await.unwrap();
    });
}

/// Limits both reducer reservations and independently held client resources.
const CAPACITY: usize = 2;

/// Bounds transient retries, including the initial request.
const ATTEMPTS: u8 = 3;

// Removing the attempt limit or bypassing Store-clock backoff breaks this trace.
#[test]
fn retry_backoff_is_finite_and_uses_store_time() {
    for success in [false, true] {
        block_on(async move {
            let (client, controls) = client(1);
            let mut store = store(1, client.clone());
            let root = store
                .send_with(BatchAction::Start, |s| {
                    s.started = true;
                    admit(s, &[0]);
                })
                .await
                .unwrap();
            assert_eq!(begin(&mut store).await, 0);
            for (attempt, wait) in [(1, 10), (2, 20)] {
                reply(
                    &mut store,
                    &controls,
                    0,
                    attempt,
                    Err(Failure::Transient),
                    |s| {
                        s.jobs.get_mut(&0).unwrap().phase = Phase::Backoff;
                    },
                )
                .await;
                assert_active(&client, 0);
                store
                    .advance(Duration::from_millis(wait - 1))
                    .await
                    .unwrap();
                assert_eq!(
                    client.inspect(|activity| activity.starts.len()),
                    usize::from(attempt)
                );
                assert!(!root.is_finished());
                store.advance(Duration::from_millis(1)).await.unwrap();
                assert_eq!(begin(&mut store).await, 0);
            }
            let result = if success {
                Ok(9)
            } else {
                Err(Failure::Transient)
            };
            reply(&mut store, &controls, 0, 3, result, move |s| {
                terminal(
                    s,
                    0,
                    if success {
                        Outcome::Succeeded(9)
                    } else {
                        Outcome::Failed(Failure::Transient)
                    },
                    None,
                );
            })
            .await;
            store.advance(Duration::from_secs(1)).await.unwrap();
            assert_eq!(
                client.inspect(|activity| activity.starts.clone()),
                [(0, 1), (0, 2), (0, 3)]
            );
            root.finish().await.unwrap();
            store.finish().await.unwrap();
        });
    }
}

// Permanent errors and channel closure must not enter the transient retry branch.
#[test]
fn permanent_and_disconnected_failures_do_not_retry() {
    for disconnected in [false, true] {
        block_on(async move {
            let (client, mut controls) = client(1);
            let mut store = store(1, client.clone());
            let root = store
                .send_with(BatchAction::Start, |s| {
                    s.started = true;
                    admit(s, &[0]);
                })
                .await
                .unwrap();
            assert_eq!(begin(&mut store).await, 0);
            let response = controls.remove(&(0, 1)).unwrap();
            if !disconnected {
                response.try_send(Err(Failure::Permanent)).unwrap();
            }
            drop(response);
            let _ = store
                .receive_with(
                    |a| {
                        matches!(a, BatchAction::Job(k)
                if *k.id() == 0 && *k.action() == JobAction::Response(Err(Failure::Permanent)))
                    },
                    |s| {
                        terminal(s, 0, Outcome::Failed(Failure::Permanent), None);
                    },
                )
                .await
                .unwrap();
            store.advance(Duration::from_secs(1)).await.unwrap();
            assert_eq!(client.inspect(|activity| activity.starts.clone()), [(0, 1)]);
            root.finish().await.unwrap();
            store.finish().await.unwrap();
        });
    }
}

// Repeated Start must not duplicate reservations or start a completed batch again.
#[test]
fn empty_and_repeated_start_do_not_duplicate_work() {
    block_on(async {
        let (client, controls) = client(1);
        let mut empty = store(0, client.clone());
        empty
            .send_with(BatchAction::Start, |s| s.started = true)
            .await
            .unwrap()
            .finish()
            .await
            .unwrap();
        empty
            .send(BatchAction::Start)
            .await
            .unwrap()
            .finish()
            .await
            .unwrap();
        empty.finish().await.unwrap();
        assert!(client.inspect(|activity| activity.starts.is_empty()));

        let mut store = store(1, client.clone());
        let root = store
            .send_with(BatchAction::Start, |s| {
                s.started = true;
                admit(s, &[0]);
            })
            .await
            .unwrap();
        assert_eq!(begin(&mut store).await, 0);
        store
            .send(BatchAction::Start)
            .await
            .unwrap()
            .finish()
            .await
            .unwrap();
        reply(&mut store, &controls, 0, 1, Ok(8), |s| {
            terminal(s, 0, Outcome::Succeeded(8), None);
        })
        .await;
        store
            .send(BatchAction::Start)
            .await
            .unwrap()
            .finish()
            .await
            .unwrap();
        root.finish().await.unwrap();
        assert_eq!(client.inspect(|activity| activity.starts.clone()), [(0, 1)]);
        store.finish().await.unwrap();
    });
}

// Losing scoped work must release its slot and captures, not cancel the sibling.
#[test]
fn cancellation_of_running_backoff_and_queued_jobs_preserves_siblings() {
    for scenario in 0..3 {
        block_on(async move {
            let (client, controls) = client(3);
            let mut store = store(3, client.clone());
            let root = store
                .send_with(BatchAction::Start, |s| {
                    s.started = true;
                    admit(s, &[0, 1]);
                })
                .await
                .unwrap();
            let mut began = [begin(&mut store).await, begin(&mut store).await];
            began.sort_unstable();
            assert_eq!(began, [0, 1]);
            if scenario == 1 {
                reply(&mut store, &controls, 0, 1, Err(Failure::Transient), |s| {
                    s.jobs.get_mut(&0).unwrap().phase = Phase::Backoff;
                })
                .await;
            }
            let cancelled = if scenario == 2 { 2 } else { 0 };
            let cancel = store
                .send_with(BatchAction::CancelJob(cancelled), move |s| {
                    if cancelled == 2 {
                        assert_eq!(s.pending.pop_front(), Some(2));
                        s.outcomes.insert(2, Outcome::Cancelled);
                    } else {
                        terminal(s, 0, Outcome::Cancelled, Some(2));
                    }
                })
                .await
                .unwrap();
            if cancelled == 0 {
                assert!(controls[&(0, 1)].is_closed());
                assert_eq!(begin(&mut store).await, 2);
            }
            assert!(!controls[&(1, 1)].is_closed());
            store.advance(Duration::from_secs(1)).await.unwrap();
            assert!(!client.inspect(|activity| activity.starts.contains(&(0, 2))));
            for id in if cancelled == 2 { [0, 1] } else { [2, 1] } {
                reply(&mut store, &controls, id, 1, Ok(u16::from(id)), move |s| {
                    terminal(s, id, Outcome::Succeeded(u16::from(id)), None);
                })
                .await;
            }
            cancel.finish().await.unwrap();
            root.finish().await.unwrap();
            assert_active(&client, 0);
            assert_eq!(client.inspect(|activity| activity.peak), 2);
            store.finish().await.unwrap();
        });
    }
}

// Stopping a batch must suppress both later admission and retry timers.
#[test]
fn batch_cancellation_preserves_completed_results() {
    block_on(async {
        let (client, controls) = client(4);
        let mut store = store(4, client.clone());
        let root = store
            .send_with(BatchAction::Start, |s| {
                s.started = true;
                admit(s, &[0, 1]);
            })
            .await
            .unwrap();
        let _ = begin(&mut store).await;
        let _ = begin(&mut store).await;
        reply(&mut store, &controls, 0, 1, Ok(7), |s| {
            terminal(s, 0, Outcome::Succeeded(7), Some(2));
        })
        .await;
        assert_eq!(begin(&mut store).await, 2);
        reply(&mut store, &controls, 1, 1, Err(Failure::Transient), |s| {
            s.jobs.get_mut(&1).unwrap().phase = Phase::Backoff;
        })
        .await;
        store
            .send_with(BatchAction::Cancel, |s| {
                s.stopped = true;
                s.pending.clear();
                s.jobs = KeyedState::new();
                s.outcomes.extend([
                    (1, Outcome::Cancelled),
                    (2, Outcome::Cancelled),
                    (3, Outcome::Cancelled),
                ]);
            })
            .await
            .unwrap()
            .finish()
            .await
            .unwrap();
        root.finish().await.unwrap();
        assert!(controls[&(2, 1)].is_closed());
        store
            .send(BatchAction::Start)
            .await
            .unwrap()
            .finish()
            .await
            .unwrap();
        store.advance(Duration::from_secs(1)).await.unwrap();
        assert_eq!(client.inspect(|activity| activity.starts.len()), 3);
        assert_active(&client, 0);
        store.finish().await.unwrap();
    });
}

// Root cancellation must reach work launched by admitted child actions.
#[test]
fn root_cancellation_releases_calls_without_domain_outcomes() {
    block_on(async {
        let (client, controls) = client(3);
        let mut store = store(3, client.clone());
        let root = store
            .send_with(BatchAction::Start, |s| {
                s.started = true;
                admit(s, &[0, 1]);
            })
            .await
            .unwrap();
        let _ = begin(&mut store).await;
        let _ = begin(&mut store).await;
        root.cancel().await.unwrap();
        root.finish().await.unwrap();
        assert!(controls[&(0, 1)].is_closed());
        assert!(controls[&(1, 1)].is_closed());
        assert_active(&client, 0);
        store.advance(Duration::from_secs(1)).await.unwrap();
        store
            .assert_state(|s| {
                assert!(s.outcomes.is_empty());
                assert_eq!(s.pending, [2]);
            })
            .await
            .unwrap();
        store.finish().await.unwrap();
    });
}

// An admitted child may be removed before its emitted Begin action is committed.
#[test]
fn cancellation_before_child_begin_rejects_queued_launch() {
    for whole_batch in [false, true] {
        block_on(async move {
            let (client, _) = client(1);
            let runtime = rca_test::DeterministicRuntime::new();
            let store = rca_core::Store::builder(initial(1), reducer(), runtime.runtime())
                .with_dependency::<ClientKey>(client.clone())
                .build()
                .unwrap();
            let start = store.try_send(BatchAction::Start).unwrap();
            let cancel = store
                .try_send(if whole_batch {
                    BatchAction::Cancel
                } else {
                    BatchAction::CancelJob(0)
                })
                .unwrap();
            runtime.run_until_stalled().unwrap();
            let root = runtime.run(start.finish()).await.unwrap().unwrap();
            let _ = runtime.run(cancel.finish()).await.unwrap().unwrap();
            let outcome = runtime
                .run(store.read(|s| s.outcomes.get(&0).copied()))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(outcome, Some(Outcome::Cancelled));
            assert!(client.inspect(|activity| activity.starts.is_empty()));
            runtime.run(root.finish()).await.unwrap();
            runtime.run(store.shutdown()).await.unwrap().unwrap();
        });
    }
}

// Shutdown must release admitted client calls and prevent queued jobs from starting.
#[test]
fn shutdown_releases_active_batch_calls() {
    block_on(async {
        let (client, controls) = client(3);
        let runtime = rca_test::DeterministicRuntime::new();
        let store = rca_core::Store::builder(initial(3), reducer(), runtime.runtime())
            .with_dependency::<ClientKey>(client.clone())
            .build()
            .unwrap();
        let root = runtime
            .run(store.send(BatchAction::Start))
            .await
            .unwrap()
            .unwrap();
        runtime.run_until_stalled().unwrap();
        assert_active(&client, 2);
        runtime.run(store.shutdown()).await.unwrap().unwrap();
        runtime.run_until_stalled().unwrap();
        assert!(root.is_finished());
        assert!(controls[&(0, 1)].is_closed());
        assert!(controls[&(1, 1)].is_closed());
        assert_active(&client, 0);
        assert_eq!(client.inspect(|activity| activity.starts.len()), 2);
        assert!(runtime.pending_sleeps().is_empty());
    });
}

// Abort acknowledgement is not resource destruction: deliberately poll replacement first.
#[test]
fn cancelled_call_keeps_capacity_until_its_capture_drops() {
    for cancel_waiting in [false, true] {
        block_on(async move {
            let (client, controls) = client(3);
            let host = rca_test::DeterministicRuntime::new();
            let holds = Arc::new(Mutex::new(Vec::new()));
            let driver = PausableDriver {
                host: host.clone(),
                holds: Arc::clone(&holds),
            };
            let store =
                rca_core::Store::builder(initial(3), reducer(), rca_core::Runtime::new(driver))
                    .with_dependency::<ClientKey>(client.clone())
                    .build()
                    .unwrap();
            let root = host
                .run(store.send(BatchAction::Start))
                .await
                .unwrap()
                .unwrap();
            host.run_until_stalled().unwrap();
            assert_active(&client, 2);
            let workers = {
                let holds = holds.lock().unwrap();
                assert!(holds.len() >= 3, "actor and worker wrappers were spawned");
                holds[1..].to_vec()
            };
            for hold in &workers {
                hold.paused.store(true, Ordering::SeqCst);
            }
            let _ = host
                .run(store.send(BatchAction::CancelJob(0)))
                .await
                .unwrap()
                .unwrap();
            host.run_until_stalled().unwrap();
            assert!(
                !controls[&(0, 1)].is_closed(),
                "cancelled wrapper deliberately unpolled"
            );
            let active = client.inspect(|activity| activity.active);
            assert_eq!(
                active, 2,
                "replacement exceeded actual capacity before aborted capture dropped"
            );
            assert!(!client.inspect(|activity| activity.starts.contains(&(2, 1))));
            if cancel_waiting {
                let _ = host
                    .run(store.send(BatchAction::Cancel))
                    .await
                    .unwrap()
                    .unwrap();
                host.run_until_stalled().unwrap();
            }
            for hold in &workers {
                hold.paused.store(false, Ordering::SeqCst);
                if let Some(waker) = hold.waker.lock().unwrap().take() {
                    waker.wake();
                }
            }
            host.run_until_stalled().unwrap();
            assert!(controls[&(0, 1)].is_closed());
            assert_eq!(controls[&(1, 1)].is_closed(), cancel_waiting);
            assert_eq!(
                client.inspect(|activity| activity.starts.contains(&(2, 1))),
                !cancel_waiting
            );
            assert_eq!(client.inspect(|activity| activity.peak), 2);
            host.run(store.shutdown()).await.unwrap().unwrap();
            host.run_until_stalled().unwrap();
            assert!(root.is_finished());
            assert_active(&client, 0);
            assert_eq!(
                client.permits.len(),
                2,
                "cancellation must not leak a permit"
            );
        });
    }
}

/// A failed count assertion must leave the real call's cleanup usable.
#[test]
fn failed_activity_assertion_does_not_poison_call_cleanup() {
    let (client, controls) = client(1);
    client.permits.try_recv().unwrap();
    let response = client.responses.lock().unwrap().remove(&(0, 1)).unwrap();
    client.activity.lock().unwrap().active = 1;
    let call = ActiveCall {
        activity: Arc::clone(&client.activity),
        response: Some(response),
        returned: client.returned.clone(),
    };

    let mismatch = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        assert_active(&client, 0);
    }));
    assert!(mismatch.is_err(), "the deliberately wrong count must fail");

    // Catch the separate drop so a regression reports a failure, not a double panic.
    let cleanup = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(call)));
    assert!(
        cleanup.is_ok(),
        "the original mismatch must not break cleanup"
    );
    assert!(controls[&(0, 1)].is_closed());
    assert_active(&client, 0);
    assert_eq!(client.permits.len(), CAPACITY);
}

/// Checks the actual client count, independently of reducer reservations.
fn assert_active(client: &Client, expected: usize) {
    assert_eq!(client.inspect(|activity| activity.active), expected);
}

/// Defers one worker poll while retaining the wake needed for explicit release.
#[derive(Default)]
struct PollHold {
    paused: AtomicBool,
    waker: Mutex<Option<std::task::Waker>>,
}

/// Pauses workers after the first spawned actor on this single-threaded host.
///
/// This fixture controls polling order, not arbitrary concurrent wake races.
struct PausableDriver {
    host: rca_test::DeterministicRuntime,
    holds: Arc<Mutex<Vec<Arc<PollHold>>>>,
}

impl std::fmt::Debug for PausableDriver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PausableDriver").finish_non_exhaustive()
    }
}

impl rca_core::RuntimeDriver for PausableDriver {
    fn spawn(&self, mut task: rca_core::RuntimeFuture) {
        let hold = Arc::new(PollHold::default());
        self.holds.lock().unwrap().push(Arc::clone(&hold));
        self.host
            .spawn_task(Box::pin(std::future::poll_fn(move |context| {
                if hold.paused.load(Ordering::SeqCst) {
                    *hold.waker.lock().unwrap() = Some(context.waker().clone());
                    std::task::Poll::Pending
                } else {
                    task.as_mut().poll(context)
                }
            })));
    }

    fn now(&self) -> rca_core::MonotonicInstant {
        self.host.now()
    }

    fn sleep_until(&self, deadline: rca_core::MonotonicInstant) -> rca_core::RuntimeFuture {
        self.host.sleep_until(deadline)
    }

    fn yield_now(&self) -> rca_core::RuntimeFuture {
        let mut yielded = false;
        Box::pin(std::future::poll_fn(move |context| {
            if yielded {
                std::task::Poll::Ready(())
            } else {
                yielded = true;
                context.waker().wake_by_ref();
                std::task::Poll::Pending
            }
        }))
    }
}

/// Supplies a real endpoint result and checks its routed identity and state commit.
async fn reply(
    store: &mut TestStore<BatchState, BatchAction>,
    controls: &Controls,
    id: u8,
    attempt: u8,
    result: Result<u16, Failure>,
    update: impl FnOnce(&mut BatchState) + Send + 'static,
) {
    controls[&(id, attempt)].try_send(result).unwrap();
    let _ = store
        .receive_with(
            move |a| {
                matches!(a, BatchAction::Job(k)
        if *k.id() == id && *k.action() == JobAction::Response(result))
            },
            update,
        )
        .await
        .unwrap();
}

/// Distinguishes retryable failure from a terminal response or disconnection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Failure {
    Transient,
    Permanent,
}

/// Retains a terminal result after the corresponding child is removed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Outcome {
    Succeeded(u16),
    Failed(Failure),
    Cancelled,
}

/// Keeps a reservation while running or waiting for the next retry deadline.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Phase {
    Admitted,
    Running,
    Backoff,
    Done(Outcome),
}

/// Records one stable job ID, its attempt count and current reservation phase.
#[derive(Clone, Debug, Eq, PartialEq)]
struct JobState {
    id: u8,
    attempts: u8,
    phase: Phase,
}

/// Separates queued IDs, reserved children and completed results.
#[derive(Clone, Debug, PartialEq)]
struct BatchState {
    pending: VecDeque<u8>,
    jobs: KeyedState<u8, JobState>,
    outcomes: BTreeMap<u8, Outcome>,
    started: bool,
    stopped: bool,
}

/// Starts an admitted attempt or delivers its controlled result.
#[derive(Debug, Eq, PartialEq)]
enum JobAction {
    Begin,
    Response(Result<u16, Failure>),
}

/// Starts or cancels the finite batch and routes child responses by ID.
#[derive(Debug, Eq, PartialEq)]
enum BatchAction {
    Start,
    Job(KeyedAction<u8, JobAction>),
    CancelJob(u8),
    Cancel,
}

/// Owns the one-use response receiver for each job and attempt.
type Responses = BTreeMap<(u8, u8), Receiver<Result<u16, Failure>>>;

/// Lets each scenario supply endpoint results without injecting reducer actions.
type Controls = BTreeMap<(u8, u8), Sender<Result<u16, Failure>>>;

/// Observes physical client ownership separately from reducer state.
#[derive(Default)]
struct Activity {
    active: usize,
    peak: usize,
    starts: Vec<(u8, u8)>,
}

/// Shares endpoint plans and a finite permit pool across child effects.
#[derive(Clone)]
struct Client {
    responses: Arc<Mutex<Responses>>,
    activity: Arc<Mutex<Activity>>,
    permits: Receiver<()>,
    returned: Sender<()>,
}

impl Client {
    /// Copies an observation before caller assertions, leaving cleanup's mutex unheld.
    fn inspect<Value>(&self, read: impl FnOnce(&Activity) -> Value) -> Value {
        read(&self.activity.lock().unwrap())
    }
}

/// Returns one permit only after its actual response receiver has been destroyed.
struct ActiveCall {
    activity: Arc<Mutex<Activity>>,
    response: Option<Receiver<Result<u16, Failure>>>,
    returned: Sender<()>,
}
impl Drop for ActiveCall {
    fn drop(&mut self) {
        // Release the actual resource before another task can obtain its permit.
        drop(self.response.take());
        self.activity.lock().unwrap().active -= 1;
        self.returned
            .try_send(())
            .expect("owned permit returns exactly once");
    }
}

/// Requires every test to inject its own finite client.
struct ClientKey;
impl DependencyKey for ClientKey {
    type Value = Client;
    fn live_value() -> Client {
        panic!("fixture requires an explicit client")
    }
}

/// Prepares distinct response channels for every bounded job/attempt pair.
fn client(count: u8) -> (Client, Controls) {
    let (returned, permits) = async_channel::bounded(CAPACITY);
    for _ in 0..CAPACITY {
        returned.try_send(()).unwrap();
    }
    let mut responses = BTreeMap::new();
    let mut controls = BTreeMap::new();
    for id in 0..count {
        for attempt in 1..=ATTEMPTS {
            let (tx, rx) = async_channel::bounded(1);
            controls.insert((id, attempt), tx);
            responses.insert((id, attempt), rx);
        }
    }
    (
        Client {
            responses: Arc::new(Mutex::new(responses)),
            activity: Arc::default(),
            permits,
            returned,
        },
        controls,
    )
}

/// Runs finite attempts and schedules retries on the Store clock.
struct Job;
impl Reducer for Job {
    type State = JobState;
    type Action = JobAction;
    fn reduce(
        &mut self,
        state: &mut JobState,
        action: &JobAction,
        context: &mut ReduceContext<'_>,
    ) -> Effect<JobAction> {
        match action {
            JobAction::Begin => {
                assert!(matches!(state.phase, Phase::Admitted | Phase::Backoff));
                state.attempts += 1;
                state.phase = Phase::Running;
                let id = state.id;
                let attempt = state.attempts;
                let client = context.dependency::<ClientKey>().clone();
                Effect::run(move |sender| async move {
                    client.permits.recv().await.unwrap();
                    let response = client
                        .responses
                        .lock()
                        .unwrap()
                        .remove(&(id, attempt))
                        .unwrap();
                    let active = {
                        let mut activity = client.activity.lock().unwrap();
                        activity.active += 1;
                        activity.peak = activity.peak.max(activity.active);
                        activity.starts.push((id, attempt));
                        ActiveCall {
                            activity: Arc::clone(&client.activity),
                            response: Some(response),
                            returned: client.returned.clone(),
                        }
                    };
                    let result = active
                        .response
                        .as_ref()
                        .unwrap()
                        .recv()
                        .await
                        .unwrap_or(Err(Failure::Permanent));
                    drop(active);
                    let _ = sender.send(JobAction::Response(result)).await;
                })
            }
            JobAction::Response(result) => {
                assert_eq!(state.phase, Phase::Running, "obsolete result reached child");
                match result {
                    Err(Failure::Transient) if state.attempts < ATTEMPTS => {
                        state.phase = Phase::Backoff;
                        Effect::send_after(
                            Duration::from_millis(10 * u64::from(state.attempts)),
                            JobAction::Begin,
                        )
                    }
                    result => {
                        state.phase = Phase::Done(match result {
                            Ok(value) => Outcome::Succeeded(*value),
                            Err(error) => Outcome::Failed(*error),
                        });
                        Effect::none()
                    }
                }
            }
        }
    }
}

/// Refills reservations after terminal children leave the keyed collection.
struct Batch;

/// Cancels a queued initial action before its child has begun execution.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct Launch(u8);

impl Reducer for Batch {
    type State = BatchState;
    type Action = BatchAction;
    fn reduce(
        &mut self,
        state: &mut BatchState,
        action: &BatchAction,
        _: &mut ReduceContext<'_>,
    ) -> Effect<BatchAction> {
        let mut effects = Vec::new();
        match action {
            BatchAction::Start => {
                state.started = true;
            }
            BatchAction::Job(keyed) => {
                if let Phase::Done(outcome) = state.jobs.get(keyed.id()).unwrap().phase {
                    state.outcomes.insert(*keyed.id(), outcome);
                    state.jobs.remove(keyed.id());
                }
            }
            BatchAction::CancelJob(id) => {
                if state.jobs.remove(id).is_some() || state.pending.contains(id) {
                    effects.push(Effect::cancel(Launch(*id)));
                    state.pending.retain(|pending| pending != id);
                    state.outcomes.insert(*id, Outcome::Cancelled);
                }
            }
            BatchAction::Cancel => {
                state.stopped = true;
                for id in state
                    .pending
                    .drain(..)
                    .chain(state.jobs.iter().map(|(id, _)| *id))
                {
                    state.outcomes.insert(id, Outcome::Cancelled);
                    effects.push(Effect::cancel(Launch(id)));
                }
                state.jobs = KeyedState::new();
            }
        }
        if state.started && !state.stopped {
            while state.jobs.len() < CAPACITY && !state.pending.is_empty() {
                let id = state.pending.pop_front().unwrap();
                state.jobs.insert(
                    id,
                    JobState {
                        id,
                        attempts: 0,
                        phase: Phase::Admitted,
                    },
                );
                effects.push(
                    Effect::send(BatchAction::Job(KeyedAction::new(id, JobAction::Begin)))
                        .cancellable(Launch(id)),
                );
            }
        }
        Effect::merge(effects)
    }
}

/// Composes child reduction before parent result collection and removal.
fn reducer() -> impl Reducer<State = BatchState, Action = BatchAction> {
    for_each(
        Batch,
        KeyedStatePath::new(|s: &BatchState| &s.jobs, |s: &mut BatchState| &mut s.jobs),
        KeyedActionPath::new(
            |a| match a {
                BatchAction::Job(k) => Some(k),
                _ => None,
            },
            BatchAction::Job,
        ),
        Job,
    )
}

/// Creates a finite queue with unique IDs and no admitted work.
fn initial(count: u8) -> BatchState {
    BatchState {
        pending: (0..count).collect(),
        jobs: KeyedState::new(),
        outcomes: BTreeMap::new(),
        started: false,
        stopped: false,
    }
}

/// Installs the scenario's client in an exhaustive `TestStore`.
fn store(count: u8, client: Client) -> TestStore<BatchState, BatchAction> {
    TestStore::builder(initial(count), reducer())
        .with_dependency::<ClientKey>(client)
        .build()
}

/// Applies literal expected admissions, never a result calculated by the reducer.
fn admit(state: &mut BatchState, ids: &[u8]) {
    for &id in ids {
        assert_eq!(state.pending.pop_front(), Some(id));
        state.jobs.insert(
            id,
            JobState {
                id,
                attempts: 0,
                phase: Phase::Admitted,
            },
        );
    }
}

/// Applies an expected outcome and an explicitly chosen replacement ID.
fn terminal(state: &mut BatchState, id: u8, outcome: Outcome, next: Option<u8>) {
    state.jobs.remove(&id);
    state.outcomes.insert(id, outcome);
    if let Some(next) = next {
        admit(state, &[next]);
    }
}

/// Checks an admitted child's begin commit and returns its ID for a literal comparison.
async fn begin(store: &mut TestStore<BatchState, BatchAction>) -> u8 {
    let id = Arc::new(AtomicU8::new(u8::MAX));
    let matched = Arc::clone(&id);
    let expected = Arc::clone(&id);
    let _ = store
        .receive_with(
            move |a| {
                if let BatchAction::Job(k) = a
                    && *k.action() == JobAction::Begin
                {
                    matched.store(*k.id(), Ordering::SeqCst);
                    true
                } else {
                    false
                }
            },
            move |s| {
                let job = s.jobs.get_mut(&expected.load(Ordering::SeqCst)).unwrap();
                job.attempts += 1;
                job.phase = Phase::Running;
            },
        )
        .await
        .unwrap();
    id.load(Ordering::SeqCst)
}
