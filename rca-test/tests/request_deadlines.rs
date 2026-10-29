//! Request and deadline races cancel the losing work, including scoped work.

use std::{
    collections::VecDeque,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use async_channel::{Receiver, Sender};
use futures_executor::block_on;
use rca_core::{Effect, ReduceContext, Reducer};
use rca_test::TestStore;

/// Shared virtual timeout with assertions immediately before and at expiry.
const TIMEOUT: Duration = Duration::from_millis(10);

/// Observable request outcome; a terminal action must leave `Running` once.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
enum State {
    #[default]
    Idle,
    Running,
    Succeeded(u8),
    Failed,
    TimedOut,
    Cancelled,
}

/// Drives request, timeout and explicit-cancellation paths independently.
#[derive(Debug, Eq, PartialEq)]
enum Action {
    Start,
    Response(Result<u8, ()>),
    Deadline,
    Cancel,
}

/// Separately cancels the request and its deadline within each child scope.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum Id {
    Operation,
    Deadline,
}

/// Runs manually released requests against an actor-owned timeout.
struct Request {
    /// Each start consumes one receiver; tests retain the matching sender.
    responses: VecDeque<Receiver<Result<u8, ()>>>,
    /// Counts sends after their first poll, proving late output was admitted.
    enqueued: Arc<AtomicUsize>,
}

impl Reducer for Request {
    type State = State;
    type Action = Action;

    fn reduce(
        &mut self,
        state: &mut State,
        action: &Action,
        _: &mut ReduceContext<'_>,
    ) -> Effect<Action> {
        match action {
            Action::Start => {
                *state = State::Running;
                let response = self.responses.pop_front().unwrap();
                let enqueued = Arc::clone(&self.enqueued);
                Effect::merge([
                    Effect::run(move |sender| async move {
                        let result = response.recv().await.unwrap();
                        let mut sending = std::pin::pin!(sender.send(Action::Response(result)));
                        let first_poll = std::future::poll_fn(|cx| {
                            std::task::Poll::Ready(sending.as_mut().poll(cx))
                        })
                        .await;
                        assert!(first_poll.is_pending());
                        enqueued.fetch_add(1, Ordering::SeqCst);
                        let _ = sending.await;
                    })
                    .cancellable_in_flight(Id::Operation),
                    Effect::send_after(TIMEOUT, Action::Deadline)
                        .cancellable_in_flight(Id::Deadline),
                ])
            }
            Action::Response(result) => {
                assert_eq!(*state, State::Running, "obsolete response reached reducer");
                *state = result.map_or(State::Failed, State::Succeeded);
                Effect::cancel(Id::Deadline)
            }
            Action::Deadline => {
                assert_eq!(*state, State::Running, "obsolete deadline reached reducer");
                *state = State::TimedOut;
                Effect::cancel(Id::Operation)
            }
            Action::Cancel => {
                *state = State::Cancelled;
                Effect::merge([Effect::cancel(Id::Operation), Effect::cancel(Id::Deadline)])
            }
        }
    }
}

/// Creates bounded response channels for exactly `count` request starts.
fn request(count: usize) -> (Request, Vec<Sender<Result<u8, ()>>>) {
    let (senders, responses) = (0..count).map(|_| async_channel::bounded(1)).unzip();
    (
        Request {
            responses,
            enqueued: Arc::default(),
        },
        senders,
    )
}

// Omitting loser cancellation retains the receiver even after timeout state commits.
#[test]
fn timeout_releases_operation_and_finishes_root() {
    block_on(async {
        let (feature, responses) = request(1);
        let mut store = TestStore::new(State::Idle, feature);
        let root = store
            .send_with(Action::Start, |s| *s = State::Running)
            .await
            .unwrap();
        store.advance(Duration::from_millis(9)).await.unwrap();
        store
            .assert_state(|s| assert_eq!(*s, State::Running))
            .await
            .unwrap();
        assert!(!responses[0].is_closed());
        assert!(!root.is_finished());
        store.advance(Duration::from_millis(1)).await.unwrap();
        let _ = store
            .receive_with(|a| *a == Action::Deadline, |s| *s = State::TimedOut)
            .await
            .unwrap();
        store
            .assert_state(|s| assert_eq!(*s, State::TimedOut))
            .await
            .unwrap();
        assert!(
            responses[0].is_closed(),
            "timed-out operation receiver remains open"
        );
        root.finish().await.unwrap();
        store.finish().await.unwrap();
    });
}

// Missing timer cancellation leaves the root live after a terminal response.
#[test]
fn response_cancels_deadline_on_success_and_failure() {
    for (response, expected) in [(Ok(42), State::Succeeded(42)), (Err(()), State::Failed)] {
        block_on(async {
            let (feature, responses) = request(1);
            let mut store = TestStore::new(State::Idle, feature);
            let root = store
                .send_with(Action::Start, |s| *s = State::Running)
                .await
                .unwrap();
            store.advance(Duration::from_millis(9)).await.unwrap();
            responses[0].try_send(response).unwrap();
            let _ = store
                .receive_with(
                    move |a| *a == Action::Response(response),
                    move |s| *s = expected,
                )
                .await
                .unwrap();
            root.finish().await.unwrap();
            assert!(responses[0].is_closed());
            store.advance(TIMEOUT).await.unwrap();
            store.finish().await.unwrap();
        });
    }
}

// Explicit and root cancellation must both release work, without synthesising Deadline.
#[test]
fn cancellation_releases_both_branches_without_timeout() {
    for explicit in [false, true] {
        block_on(async {
            let (feature, responses) = request(1);
            let mut store = TestStore::new(State::Idle, feature);
            let root = store
                .send_with(Action::Start, |s| *s = State::Running)
                .await
                .unwrap();
            if explicit {
                let _ = store
                    .send_with(Action::Cancel, |s| *s = State::Cancelled)
                    .await
                    .unwrap();
            } else {
                root.cancel().await.unwrap();
            }
            root.finish().await.unwrap();
            assert!(responses[0].is_closed());
            store.advance(TIMEOUT).await.unwrap();
            store
                .assert_state(move |s| {
                    assert_eq!(
                        *s,
                        if explicit {
                            State::Cancelled
                        } else {
                            State::Running
                        }
                    );
                })
                .await
                .unwrap();
            store.finish().await.unwrap();
        });
    }
}

#[test]
fn shutdown_releases_request_and_deadline() {
    block_on(async {
        let (feature, responses) = request(1);
        let runtime = rca_test::DeterministicRuntime::new();
        let store = rca_core::Store::new(State::Idle, feature, runtime.runtime());
        let root = runtime
            .run(store.send(Action::Start))
            .await
            .unwrap()
            .unwrap();
        runtime.run_until_stalled().unwrap();
        assert!(!responses[0].is_closed());
        runtime.run(store.shutdown()).await.unwrap().unwrap();
        // Shutdown issues aborts; the host must poll runtime-owned wrappers to drop captures.
        runtime.run_until_stalled().unwrap();
        assert!(responses[0].is_closed());
        assert!(root.is_finished());
        assert!(runtime.pending_sleeps().is_empty());
    });
}

// Replacing only one generation would let the old deadline end the new request.
#[test]
fn replacement_resets_operation_and_deadline() {
    block_on(async {
        let (feature, responses) = request(2);
        let mut store = TestStore::new(State::Idle, feature);
        let old = store
            .send_with(Action::Start, |s| *s = State::Running)
            .await
            .unwrap();
        store.advance(Duration::from_millis(9)).await.unwrap();
        let fresh = store.send(Action::Start).await.unwrap();
        old.finish().await.unwrap();
        assert!(responses[0].is_closed());
        assert!(!responses[1].is_closed());
        store.advance(Duration::from_millis(1)).await.unwrap();
        store
            .assert_state(|s| assert_eq!(*s, State::Running))
            .await
            .unwrap();
        assert!(!fresh.is_finished());
        store.advance(Duration::from_millis(9)).await.unwrap();
        let _ = store
            .receive_with(|a| *a == Action::Deadline, |s| *s = State::TimedOut)
            .await
            .unwrap();
        fresh.finish().await.unwrap();
        assert!(responses[1].is_closed());
        store.finish().await.unwrap();
    });
}

// Releasing a gated deadline must invalidate ready losing output before reduction.
#[test]
fn timeout_rejects_ready_late_response() {
    block_on(async {
        let (feature, responses) = request(1);
        let enqueued = Arc::clone(&feature.enqueued);
        let mut store = TestStore::new(State::Idle, feature);
        let root = store
            .send_with(Action::Start, |s| *s = State::Running)
            .await
            .unwrap();
        store.advance(TIMEOUT).await.unwrap();
        responses[0].try_send(Ok(99)).unwrap();
        store
            .assert_state(|s| assert_eq!(*s, State::Running))
            .await
            .unwrap();
        assert_eq!(
            enqueued.load(Ordering::SeqCst),
            1,
            "response was polled into the empty mailbox while deadline was gated"
        );
        let _ = store
            .receive_with(|a| *a == Action::Deadline, |s| *s = State::TimedOut)
            .await
            .unwrap();
        root.finish().await.unwrap();
        assert!(store.finish().await.unwrap_err().is_incomplete_assertion());
        store
            .assert_state(|state| assert_eq!(*state, State::TimedOut))
            .await
            .unwrap();
        let boundary = store
            .pending_boundary()
            .expect("the admitted late response retains its cancelled terminal");
        store
            .receive_terminal(boundary, |error| {
                error.reason() == rca_core::TaskAccessReason::Cancelled
                    && error.progress() == rca_core::TaskProgress::NotStarted
            })
            .await
            .unwrap();
        store.finish().await.unwrap();
    });
}

/// Routes request actions or ends one keyed child's lifetime.
#[derive(Debug, Eq, PartialEq)]
enum JobsAction {
    Request(rca_core::KeyedAction<u8, Action>),
    Remove(u8),
}

/// Removes keyed request state without adding parent-owned work.
struct Jobs;

impl Reducer for Jobs {
    type State = rca_core::KeyedState<u8, State>;
    type Action = JobsAction;
    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &JobsAction,
        _: &mut ReduceContext<'_>,
    ) -> Effect<JobsAction> {
        if let JobsAction::Remove(id) = action {
            state.remove(id);
        }
        Effect::none()
    }
}

/// Addresses one request instance with the same child cancellation IDs.
fn job(id: u8, action: Action) -> JobsAction {
    JobsAction::Request(rca_core::KeyedAction::new(id, action))
}

// Identical typed IDs in two keyed scopes must not cross-cancel.
#[test]
fn keyed_requests_keep_independent_deadlines_and_removal() {
    for remove in [false, true] {
        block_on(async {
            let (feature, responses) = request(2);
            let reducer = rca_core::for_each(
                Jobs,
                rca_core::KeyedStatePath::new(|s| s, |s| s),
                rca_core::KeyedActionPath::new(
                    |a| match a {
                        JobsAction::Request(a) => Some(a),
                        JobsAction::Remove(_) => None,
                    },
                    JobsAction::Request,
                ),
                feature,
            );
            let initial = [(1, State::Idle), (2, State::Idle)].into_iter().collect();
            let mut store = TestStore::new(initial, reducer);
            let first = store
                .send_with(job(1, Action::Start), |s| {
                    *s.get_mut(&1).unwrap() = State::Running;
                })
                .await
                .unwrap();
            let second = store
                .send_with(job(2, Action::Start), |s| {
                    *s.get_mut(&2).unwrap() = State::Running;
                })
                .await
                .unwrap();
            store.advance(Duration::from_millis(9)).await.unwrap();
            if remove {
                let _ = store
                    .send_with(JobsAction::Remove(1), |s| {
                        s.remove(&1);
                    })
                    .await
                    .unwrap();
            } else {
                responses[0].try_send(Ok(7)).unwrap();
                let _ = store
                    .receive_with(
                        |a| *a == job(1, Action::Response(Ok(7))),
                        |s| *s.get_mut(&1).unwrap() = State::Succeeded(7),
                    )
                    .await
                    .unwrap();
            }
            first.finish().await.unwrap();
            assert!(responses[0].is_closed());
            assert!(!responses[1].is_closed());
            assert!(!second.is_finished());
            store.advance(Duration::from_millis(1)).await.unwrap();
            let _ = store
                .receive_with(
                    |a| *a == job(2, Action::Deadline),
                    |s| *s.get_mut(&2).unwrap() = State::TimedOut,
                )
                .await
                .unwrap();
            second.finish().await.unwrap();
            assert!(responses[1].is_closed());
            store.finish().await.unwrap();
        });
    }
}
