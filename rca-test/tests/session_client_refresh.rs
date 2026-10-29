//! Separates captured clients, current services and optimistic request identity.

use std::collections::VecDeque;
use std::future::poll_fn;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use futures_executor::block_on;
use futures_util::FutureExt as _;
use rca_core::{DependencyKey, Feature, FeatureContext, Task, TaskAccessError, TaskContext};
use rca_test::{Completion, ControlledFuture, TestStore, controlled_future};

// Catches implicit provider refresh, captured policy and manually supplied replies.
#[test]
fn captured_client_and_explicit_current_service_have_distinct_epochs() {
    for immediate in [true, false] {
        block_on(async {
            let (a, _) = client("A", []);
            let (b, mut b_replies) = client("B", [Request::Fetch, Request::Captured]);
            let (c, mut c_replies) = client("C", [Request::Fresh]);
            let mut store = store(&a, &b);
            let first = b_replies.pop_front().unwrap();
            let captured = b_replies.pop_front().unwrap();
            let fresh = c_replies.pop_front().unwrap();
            if immediate {
                first.complete(Ok(3)).unwrap();
                captured.complete(Ok(7)).unwrap();
                fresh.complete(Ok(11)).unwrap();
                let _handover = store
                    .send_with(Action::Current(c.clone(), 4), |s| {
                        s.current = "C";
                        s.policy = 4;
                    })
                    .await
                    .unwrap();
                let work = store.send(Action::Refresh).await.unwrap();
                assert!(store.finish().await.is_err());
                assert_eq!(b.inspect(|backend| backend.pending), 0);
                assert_eq!(c.inspect(|backend| backend.pending), 0);
                expect_refresh(&mut store).await;
                work.finish().await.unwrap();
            } else {
                let work = store.send(Action::Refresh).await.unwrap();
                assert!(store.finish().await.is_err());
                assert_eq!(
                    b.inspect(|backend| backend.started.clone()),
                    [Request::Fetch]
                );
                assert!(b.inspect(|backend| backend.pending) > 0);
                let _handover = store
                    .send_with(Action::Current(c.clone(), 4), |s| {
                        s.current = "C";
                        s.policy = 4;
                    })
                    .await
                    .unwrap();
                first.complete(Ok(3)).unwrap();
                assert!(store.finish().await.is_err());
                assert_eq!(
                    b.inspect(|backend| backend.started.clone()),
                    [Request::Fetch, Request::Captured]
                );
                assert!(c.inspect(|backend| backend.started.is_empty()));
                captured.complete(Ok(7)).unwrap();
                assert!(store.finish().await.is_err());
                assert_eq!(
                    c.inspect(|backend| backend.started.clone()),
                    [Request::Fresh]
                );
                assert!(c.inspect(|backend| backend.pending) > 0);
                fresh.complete(Ok(11)).unwrap();
                expect_refresh(&mut store).await;
                work.finish().await.unwrap();
            }
            store.finish().await.unwrap();
            assert!(a.inspect(|backend| backend.started.is_empty()));
            assert_eq!(
                b.inspect(|backend| backend.started.clone()),
                [Request::Fetch, Request::Captured]
            );
            assert_eq!(
                c.inspect(|backend| backend.started.clone()),
                [Request::Fresh]
            );
            assert_eq!(b.inspect(|backend| backend.released), 2);
            assert_eq!(c.inspect(|backend| backend.released), 1);
        });
    }
}

/// Requires an actual task-update commit with the three independently named clients.
async fn expect_refresh(store: &mut TestStore<State, Action, Snapshot>) {
    assert!(
        store.finish().await.is_err(),
        "response update is still owed"
    );
    store
        .receive(|_| panic!("a task update is not an input"))
        .await
        .expect_err("a task update must not be received as an input");
    let _response = store
        .receive_update(|s| {
            s.refreshed = Some(Refreshed {
                constructed: "A",
                captured: "B",
                current: "C",
                policy: 4,
                responses: [Ok(3), Ok(7), Ok(11)],
            });
        })
        .await
        .unwrap();
}

// Catches stale success/error overwrites and lost equal-state commit debt.
#[test]
fn optimistic_replies_do_not_roll_back_a_newer_request() {
    for older in [Err(Failure::Denied), Ok(8)] {
        block_on(async {
            let (client, mut replies) = client(
                "session",
                [Request::Set(1, 8), Request::Set(2, 9), Request::Set(3, 10)],
            );
            let mut store = store(&client, &client);
            let _invalid = store
                .send_with(Action::Set(0), |s| s.failure = Some(Failure::InvalidValue))
                .await
                .unwrap();
            assert!(client.inspect(|backend| backend.started.is_empty()));
            let first = store
                .send_with(Action::Set(8), |s| {
                    s.visible = 8;
                    s.request = 1;
                    s.pending = true;
                    s.failure = None;
                })
                .await
                .unwrap();
            let second = store
                .send_with(Action::Set(9), |s| {
                    s.visible = 9;
                    s.request = 2;
                })
                .await
                .unwrap();
            assert!(store.finish().await.is_err());
            assert_eq!(
                client.inspect(|backend| backend.started.clone()),
                [Request::Set(1, 8), Request::Set(2, 9)]
            );
            assert_eq!(client.inspect(|backend| backend.released), 0);
            let old_reply = replies.pop_front().unwrap();
            replies.pop_front().unwrap().complete(Ok(9)).unwrap();
            let _newer = store
                .receive_update(|s| {
                    s.confirmed = 9;
                    s.pending = false;
                })
                .await
                .unwrap();
            second.finish().await.unwrap();
            assert!(!first.is_finished());
            old_reply.complete(older).unwrap();
            assert!(
                store.finish().await.is_err(),
                "stale no-op is still a commit"
            );
            let _stale = store.receive_update(|_| {}).await.unwrap();
            first.finish().await.unwrap();
            let third = store
                .send_with(Action::Set(10), |s| {
                    s.visible = 10;
                    s.request = 3;
                    s.pending = true;
                })
                .await
                .unwrap();
            replies
                .pop_front()
                .unwrap()
                .complete(Err(Failure::Denied))
                .unwrap();
            let _failure = store
                .receive_update(|s| {
                    s.visible = 9;
                    s.pending = false;
                    s.failure = Some(Failure::Denied);
                })
                .await
                .unwrap();
            third.finish().await.unwrap();
            store.finish().await.unwrap();
            assert_eq!(client.inspect(|backend| backend.released), 3);
        });
    }
}

// Catches a fresh fixture accidentally reusing an existing backend or responses.
#[test]
fn fresh_backends_are_independent_while_cloned_handles_share() {
    block_on(async {
        let (shared, mut replies) = client("shared", [Request::Set(1, 8), Request::Set(1, 9)]);
        let alias = shared.clone();
        let (independent, mut separate) = client("independent", [Request::Set(1, 8)]);
        assert!(Arc::ptr_eq(&shared.backend, &alias.backend));
        assert!(!Arc::ptr_eq(&shared.backend, &independent.backend));
        let mut first = store(&shared, &shared);
        let mut cloned = store(&alias, &alias);
        let mut fresh = store(&independent, &independent);
        let first_work = first
            .send_with(Action::Set(8), |s| {
                s.visible = 8;
                s.request = 1;
                s.pending = true;
            })
            .await
            .unwrap();
        let cloned_work = cloned
            .send_with(Action::Set(9), |s| {
                s.visible = 9;
                s.request = 1;
                s.pending = true;
            })
            .await
            .unwrap();
        let fresh_work = fresh
            .send_with(Action::Set(8), |s| {
                s.visible = 8;
                s.request = 1;
                s.pending = true;
            })
            .await
            .unwrap();
        assert!(first.finish().await.is_err());
        assert!(cloned.finish().await.is_err());
        assert!(fresh.finish().await.is_err());
        assert_eq!(
            shared.inspect(|backend| backend.started.clone()),
            [Request::Set(1, 8), Request::Set(1, 9)]
        );
        assert_eq!(
            independent.inspect(|backend| backend.started.clone()),
            [Request::Set(1, 8)]
        );
        replies.pop_front().unwrap().complete(Ok(8)).unwrap();
        let _first_reply = first
            .receive_update(|s| {
                s.confirmed = 8;
                s.pending = false;
            })
            .await
            .unwrap();
        first_work.finish().await.unwrap();
        first.finish().await.unwrap();
        assert!(cloned.finish().await.is_err());
        assert!(fresh.finish().await.is_err());
        replies.pop_front().unwrap().complete(Ok(9)).unwrap();
        let _cloned_reply = cloned
            .receive_update(|s| {
                s.confirmed = 9;
                s.pending = false;
            })
            .await
            .unwrap();
        cloned_work.finish().await.unwrap();
        cloned.finish().await.unwrap();
        assert!(fresh.finish().await.is_err());
        fresh
            .assert_state(|s| {
                assert_eq!(s.visible, 8);
                assert_eq!(s.confirmed, 7);
                assert!(s.pending);
            })
            .await
            .unwrap();
        assert_eq!(independent.inspect(|backend| backend.released), 0);
        separate
            .pop_front()
            .unwrap()
            .complete(Err(Failure::Denied))
            .unwrap();
        let _fresh_reply = fresh
            .receive_update(|s| {
                s.visible = 7;
                s.pending = false;
                s.failure = Some(Failure::Denied);
            })
            .await
            .unwrap();
        fresh_work.finish().await.unwrap();
        fresh.finish().await.unwrap();
        assert_eq!(shared.inspect(|backend| backend.released), 2);
        assert_eq!(independent.inspect(|backend| backend.released), 1);
    });
}

/// A wrong or unplanned request must not prevent a pending call from cleaning up.
#[test]
fn wrong_request_does_not_poison_pending_call_cleanup() {
    for planned in [true, false] {
        let requests = std::iter::once(Request::Fetch).chain(planned.then_some(Request::Fresh));
        let (client, _replies) = client("cleanup", requests);
        let mut pending = Box::pin(client.call(Request::Fetch));
        assert!(pending.as_mut().now_or_never().is_none());

        let mismatch = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _unexpected_reply = block_on(client.call(Request::Captured));
        }));
        assert!(
            mismatch.is_err(),
            "the deliberately wrong request must fail"
        );

        // Keep cleanup outside the first unwind so a regression cannot abort the process.
        let cleanup = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(pending)));
        assert!(
            cleanup.is_ok(),
            "the original mismatch must not break cleanup"
        );
        assert_eq!(client.inspect(|backend| backend.released), 1);
    }
}

/// Separates input validation, server denial and an abandoned controlled reply.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Failure {
    InvalidValue,
    Denied,
    Abandoned,
}

/// Records construction, task capture and explicit current-state lookup separately.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Refreshed {
    constructed: &'static str,
    captured: &'static str,
    current: &'static str,
    policy: u8,
    responses: [Result<u8, Failure>; 3],
}

/// Holds the current service and optimistic value alongside the last confirmed value.
struct State {
    current: Client,
    policy: u8,
    visible: u8,
    confirmed: u8,
    request: u64,
    pending: bool,
    failure: Option<Failure>,
    refreshed: Option<Refreshed>,
}

impl State {
    /// Starts at a known confirmed value without any pending request.
    fn new(current: Client) -> Self {
        Self {
            current,
            policy: 2,
            visible: 7,
            confirmed: 7,
            request: 0,
            pending: false,
            failure: None,
            refreshed: None,
        }
    }
}

/// Replaces the current service, refreshes it or begins a numbered optimistic update.
enum Action {
    Current(Client, u8),
    Refresh,
    Set(u8),
}

/// Retains the service identity seen during feature construction.
struct Session {
    constructed: &'static str,
}

impl Session {
    /// Captures construction identity without retaining the mutable backend.
    fn new(state: &State) -> Self {
        Self {
            constructed: state.current.name,
        }
    }
}

impl Feature for Session {
    type State = State;
    type Action = Action;

    fn reduce(
        &mut self,
        state: &mut State,
        action: &Action,
        context: &mut FeatureContext<'_, State, Action>,
    ) {
        match action {
            Action::Current(client, policy) => {
                state.current = client.clone();
                state.policy = *policy;
            }
            Action::Refresh => {
                let client = context.reducer_context().dependency::<ClientKey>().clone();
                let constructed = self.constructed;
                context.task(Task::run(move |context| async move {
                    // A stopped owner terminates this operation; never retry via root input.
                    let _stopped = refresh(&context, constructed, client).await;
                }));
            }
            Action::Set(0) => state.failure = Some(Failure::InvalidValue),
            Action::Set(value) => {
                state.request += 1;
                state.visible = *value;
                state.pending = true;
                state.failure = None;
                let request = state.request;
                let value = *value;
                let client = context.reducer_context().dependency::<ClientKey>().clone();
                context.task(Task::run(
                    move |context: TaskContext<State, Action>| async move {
                        let response = client.call(Request::Set(request, value)).await;
                        let _stopped = context
                            .update(move |state| {
                                if state.request != request {
                                    return;
                                }
                                state.pending = false;
                                match response {
                                    Ok(value) => {
                                        state.visible = value;
                                        state.confirmed = value;
                                    }
                                    Err(failure) => {
                                        state.visible = state.confirmed;
                                        state.failure = Some(failure);
                                    }
                                }
                            })
                            .await;
                    },
                ));
            }
        }
    }
}

/// Reads an owned current service without replacing the already captured service.
async fn refresh(
    context: &TaskContext<State, Action>,
    constructed: &'static str,
    captured: Client,
) -> Result<(), TaskAccessError> {
    let first = captured.call(Request::Fetch).await;
    let (current, policy) = context
        .read(|state| (state.current.clone(), state.policy))
        .await?;
    let captured_response = captured.call(Request::Captured).await;
    let fresh_response = current.call(Request::Fresh).await;
    context
        .update(move |state| {
            state.refreshed = Some(Refreshed {
                constructed,
                captured: captured.name,
                current: current.name,
                policy,
                responses: [first, captured_response, fresh_response],
            });
        })
        .await?;
    Ok(())
}

/// Stores owned domain history without aliasing mutable client observations.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Snapshot {
    current: &'static str,
    policy: u8,
    visible: u8,
    confirmed: u8,
    request: u64,
    pending: bool,
    failure: Option<Failure>,
    refreshed: Option<Refreshed>,
}

/// Copies domain values while deliberately excluding mutable endpoint state.
fn snapshot(state: &State) -> Snapshot {
    Snapshot {
        current: state.current.name,
        policy: state.policy,
        visible: state.visible,
        confirmed: state.confirmed,
        request: state.request,
        pending: state.pending,
        failure: state.failure,
        refreshed: state.refreshed.clone(),
    }
}

/// Separates feature construction from the client supplied at work admission.
fn store(construction: &Client, admission: &Client) -> TestStore<State, Action, Snapshot> {
    let mut state = State::new(construction.clone());
    let session = Session::new(&state);
    // This explicit State handover precedes admission, not State construction.
    state.current = admission.clone();
    TestStore::projected_feature_builder(state, session, snapshot)
        .with_dependency::<ClientKey>(admission.clone())
        .build()
}

/// Supplies the captured client independently of the service stored in state.
struct ClientKey;
impl DependencyKey for ClientKey {
    type Value = Client;
}

/// Names finite endpoint operations; their controls supply replies, never RCA commits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Request {
    Fetch,
    Captured,
    Fresh,
    Set(u64, u8),
}

/// Returns a domain value or a typed domain failure from a controlled endpoint.
type Reply = Result<u8, Failure>;

/// Holds a finite call plan and independently observed execution counts.
struct Backend {
    /// Removes each expected request and response future exactly once.
    planned: VecDeque<(Request, ControlledFuture<Reply>)>,
    /// Records actual client requests, not reducer actions or expected history.
    started: Vec<Request>,
    /// Counts actual pending polls rather than the current number of requests.
    pending: usize,
    /// Counts destroyed started-call guards, including cancelled waits.
    released: usize,
}

/// Clones share one backend; separate factory calls create independent backends.
#[derive(Clone)]
struct Client {
    name: &'static str,
    backend: Arc<Mutex<Backend>>,
}

/// Records release when a started request finishes or its future is dropped.
struct Call(Arc<Mutex<Backend>>);
impl Drop for Call {
    fn drop(&mut self) {
        self.0.lock().unwrap().released += 1;
    }
}

impl Client {
    /// Copies an observation before caller assertions, leaving cleanup's mutex unheld.
    fn inspect<Value>(&self, read: impl FnOnce(&Backend) -> Value) -> Value {
        read(&self.backend.lock().unwrap())
    }

    /// Checks one planned request and waits on its externally controlled reply.
    async fn call(&self, request: Request) -> Reply {
        let planned = self.backend.lock().unwrap().planned.pop_front();
        let (expected, mut response) = planned.expect("finite client call");
        assert_eq!(request, expected);
        self.backend.lock().unwrap().started.push(request);
        let _call = Call(Arc::clone(&self.backend));
        poll_fn(|cx| {
            let result = Pin::new(&mut response).poll(cx);
            if result.is_pending() {
                self.backend.lock().unwrap().pending += 1;
            }
            result
        })
        .await
        .unwrap_or(Err(Failure::Abandoned))
    }
}

/// Builds a fresh finite endpoint plan and its ordered completion controls.
fn client(
    name: &'static str,
    requests: impl IntoIterator<Item = Request>,
) -> (Client, VecDeque<Completion<Reply>>) {
    let mut planned = VecDeque::new();
    let mut controls = VecDeque::new();
    for request in requests {
        let (control, response) = controlled_future();
        planned.push_back((request, response));
        controls.push_back(control);
    }
    (
        Client {
            name,
            backend: Arc::new(Mutex::new(Backend {
                planned,
                started: Vec::new(),
                pending: 0,
                released: 0,
            })),
        },
        controls,
    )
}
