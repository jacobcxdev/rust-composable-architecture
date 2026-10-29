//! Tests keyed server lifetimes with deterministic `TestStore` scheduling.
//!
//! Exact client IDs and independent reply gates distinguish removed children from
//! live siblings. Virtual-clock advances test cadence, not runtime speed.

use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::Duration,
};

use async_channel::{Receiver, Sender};
use futures_executor::block_on;
use rca_core::KeyedAction;
use rca_example_mcmd_server::{
    backend::{self, BackendClient, BackendError, BackendFuture, BackendKey, Phase, Target},
    polling::{self, QueryClient, QueryError, QueryFuture, QueryKey},
    server,
    servers::{self, Action, State},
};
use rca_test::TestStore;

type BackendResponse = Result<(), BackendError>;
type QueryResponse = Result<u32, QueryError>;

#[derive(Default)]
struct Client {
    calls: Mutex<Vec<(&'static str, String)>>,
    backend: Mutex<VecDeque<Receiver<BackendResponse>>>,
    queries: Mutex<VecDeque<Receiver<QueryResponse>>>,
}

impl Client {
    fn backend_response(&self) -> Sender<BackendResponse> {
        let (tx, rx) = async_channel::bounded(1);
        self.backend.lock().unwrap().push_back(rx);
        tx
    }

    fn query_response(&self) -> Sender<QueryResponse> {
        let (tx, rx) = async_channel::bounded(1);
        self.queries.lock().unwrap().push_back(rx);
        tx
    }

    fn operation(&self, name: &'static str, id: String) -> BackendFuture {
        self.calls.lock().unwrap().push((name, id));
        let rx = self
            .backend
            .lock()
            .unwrap()
            .pop_front()
            .expect("unexpected backend call");
        Box::pin(async move { rx.recv().await.expect("backend response retained") })
    }

    fn assert_calls(&self, expected: &[(&str, &str)]) {
        let calls = self.calls.lock().unwrap();
        assert_eq!(
            calls
                .iter()
                .map(|(op, id)| (*op, id.as_str()))
                .collect::<Vec<_>>(),
            expected
        );
    }
}

impl BackendClient for Client {
    fn start(&self, server_id: String) -> BackendFuture {
        self.operation("start", server_id)
    }

    fn stop(&self, server_id: String) -> BackendFuture {
        self.operation("stop", server_id)
    }
}

impl QueryClient for Client {
    fn poll(&self, server_id: String) -> QueryFuture {
        self.calls.lock().unwrap().push(("query", server_id));
        let rx = self
            .queries
            .lock()
            .unwrap()
            .pop_front()
            .expect("unexpected query call");
        Box::pin(async move { rx.recv().await.expect("query response retained") })
    }
}

fn store(client: &Arc<Client>) -> TestStore<State, Action> {
    TestStore::builder(State::default(), servers::reducer())
        .with_dependency::<BackendKey>(Arc::clone(client) as Arc<dyn BackendClient>)
        .with_dependency::<QueryKey>(Arc::clone(client) as Arc<dyn QueryClient>)
        .build()
}

fn child(id: &str, action: server::Action) -> Action {
    Action::Server(KeyedAction::new(id.into(), action))
}

fn backend(id: &str, action: backend::Action) -> Action {
    child(id, server::Action::Backend(action))
}

fn polling(id: &str, action: polling::Action) -> Action {
    child(id, server::Action::Polling(action))
}

fn config(id: &str) -> rca_example_mcmd_server::proxy::Config {
    rca_example_mcmd_server::proxy::Config {
        server_id: id.into(),
        host: "127.0.0.1".into(),
        java_ports: vec![25565],
        bedrock_ports: vec![19132],
    }
}

async fn add(store: &mut TestStore<State, Action>, id: &'static str) {
    let _ = store
        .send_with(Action::Add(config(id)), move |s| {
            s.servers.insert(
                id.into(),
                server::State {
                    backend: backend::State {
                        server_id: id.into(),
                        target: Target::Stopped,
                        phase: Phase::Stopped,
                        last_error: None,
                    },
                    polling: polling::State {
                        server_id: id.into(),
                        active: false,
                        querying: false,
                        player_count: 0,
                        failures: 0,
                        last_error: None,
                    },
                    proxy: rca_example_mcmd_server::proxy::State {
                        config: config(id),
                        phase: rca_example_mcmd_server::proxy::Phase::Stopped,
                        last_error: None,
                    },
                },
            );
        })
        .await
        .unwrap();
}

async fn forget(store: &mut TestStore<State, Action>, id: &'static str) {
    let _ = store
        .send_with(Action::Forget(id.into()), move |s| {
            s.servers.remove(&id.into());
        })
        .await
        .unwrap();
    store
        .assert_state(move |s| assert!(!s.servers.contains_id(&id.into())))
        .await
        .unwrap();
}
async fn start(
    store: &mut TestStore<State, Action>,
    id: &'static str,
    response: &Sender<BackendResponse>,
) {
    let _ = store
        .send_with(backend(id, backend::Action::RequestStart), move |s| {
            s.servers.get_mut(&id.into()).unwrap().backend.target = Target::Running;
        })
        .await
        .unwrap();
    let _ = store
        .receive_with(
            move |a| *a == backend(id, backend::Action::Reconcile),
            move |s| {
                s.servers.get_mut(&id.into()).unwrap().backend.phase = Phase::Starting;
            },
        )
        .await
        .unwrap();
    store
        .assert_state(move |s| {
            assert_eq!(
                s.servers.get(&id.into()).unwrap().backend.phase,
                Phase::Starting
            );
        })
        .await
        .unwrap();
    response.try_send(Ok(())).unwrap();
    let _ = store
        .receive_with(
            move |a| *a == backend(id, backend::Action::StartFinished(Ok(()))),
            move |s| {
                let child = s.servers.get_mut(&id.into()).unwrap();
                child.backend.phase = Phase::Running;
                child.polling.active = true;
            },
        )
        .await
        .unwrap();
    let _ = store
        .receive_with(
            move |a| *a == polling(id, polling::Action::Tick),
            move |s| {
                s.servers.get_mut(&id.into()).unwrap().polling.querying = true;
            },
        )
        .await
        .unwrap();
    let _ = store
        .receive(move |a| *a == backend(id, backend::Action::Started))
        .await
        .unwrap();
    store
        .assert_state(move |s| assert!(s.servers.get(&id.into()).unwrap().polling.querying))
        .await
        .unwrap();
}

// Duplicate insertion must not replace state, cancel a live query or reorder keys.
#[test]
fn duplicate_add_preserves_live_child() {
    block_on(async {
        let client = Arc::new(Client::default());
        let response = client.backend_response();
        let query = client.query_response();
        let mut store = store(&client);
        add(&mut store, "s1").await;
        add(&mut store, "s2").await;
        start(&mut store, "s1", &response).await;
        let _ = store.send(Action::Add(config("s1"))).await.unwrap();
        let _ = store.send(Action::Forget("absent".into())).await.unwrap();
        store
            .assert_state(|s| {
                assert_eq!(
                    s.servers.ids().map(String::as_str).collect::<Vec<_>>(),
                    ["s1", "s2"]
                );
            })
            .await
            .unwrap();
        assert!(!query.is_closed());
        client.assert_calls(&[("start", "s1"), ("query", "s1")]);
        forget(&mut store, "s1").await;
        assert!(query.is_closed());
        forget(&mut store, "s2").await;
        store.advance(Duration::from_secs(10)).await.unwrap();
        store.finish().await.unwrap();
    });
}

async fn count(store: &mut TestStore<State, Action>, id: &'static str, value: u32) {
    let _ = store
        .receive_with(
            move |a| *a == polling(id, polling::Action::Polled(Ok(value))),
            move |s| {
                let state = &mut s.servers.get_mut(&id.into()).unwrap().polling;
                state.querying = false;
                state.player_count = value;
            },
        )
        .await
        .unwrap();
}

async fn stop(
    store: &mut TestStore<State, Action>,
    id: &'static str,
    response: &Sender<BackendResponse>,
) {
    let _ = store
        .send_with(backend(id, backend::Action::RequestStop), move |s| {
            let state = s.servers.get_mut(&id.into()).unwrap();
            state.backend.target = Target::Stopped;
            state.polling.active = false;
            state.polling.querying = false;
        })
        .await
        .unwrap();
    let _ = store
        .receive_with(
            move |a| *a == backend(id, backend::Action::Reconcile),
            move |s| {
                s.servers.get_mut(&id.into()).unwrap().backend.phase = Phase::Stopping;
            },
        )
        .await
        .unwrap();
    store
        .assert_state(move |s| {
            assert_eq!(
                s.servers.get(&id.into()).unwrap().backend.phase,
                Phase::Stopping
            );
        })
        .await
        .unwrap();
    response.try_send(Ok(())).unwrap();
    let _ = store
        .receive_with(
            move |a| *a == backend(id, backend::Action::StopFinished(Ok(()))),
            move |s| {
                let state = s.servers.get_mut(&id.into()).unwrap();
                state.backend.phase = Phase::Stopped;
                state.polling.player_count = 0;
            },
        )
        .await
        .unwrap();
    let _ = store
        .receive(move |a| *a == backend(id, backend::Action::Stopped))
        .await
        .unwrap();
}

// Equal child cancellation IDs must not share authority across keyed siblings.
#[test]
fn stopping_one_server_preserves_sibling_timer() {
    block_on(async {
        let client = Arc::new(Client::default());
        let first_start = client.backend_response();
        let second_start = client.backend_response();
        let first_stop = client.backend_response();
        let second_stop = client.backend_response();
        let first_query = client.query_response();
        let second_query = client.query_response();
        let timed_query = client.query_response();
        let mut store = store(&client);
        add(&mut store, "s1").await;
        add(&mut store, "s2").await;
        start(&mut store, "s1", &first_start).await;
        start(&mut store, "s2", &second_start).await;
        stop(&mut store, "s1", &first_stop).await;
        assert!(first_query.is_closed());
        assert!(!second_query.is_closed());
        second_query.try_send(Ok(4)).unwrap();
        count(&mut store, "s2", 4).await;
        store.advance(Duration::from_secs(5)).await.unwrap();
        let _ = store
            .receive_with(
                |a| *a == polling("s2", polling::Action::Tick),
                |s| {
                    s.servers.get_mut(&"s2".into()).unwrap().polling.querying = true;
                },
            )
            .await
            .unwrap();
        store
            .assert_state(|s| assert!(s.servers.get(&"s2".into()).unwrap().polling.querying))
            .await
            .unwrap();
        timed_query.try_send(Ok(5)).unwrap();
        count(&mut store, "s2", 5).await;
        stop(&mut store, "s2", &second_stop).await;
        store.finish().await.unwrap();
        client.assert_calls(&[
            ("start", "s1"),
            ("query", "s1"),
            ("start", "s2"),
            ("query", "s2"),
            ("stop", "s1"),
            ("query", "s2"),
            ("stop", "s2"),
        ]);
    });
}

// Equal-key reinsertion must not revive the removed child's pending query.
#[test]
fn forget_and_reinsert_rejects_old_query() {
    block_on(async {
        let client = Arc::new(Client::default());
        let first_start = client.backend_response();
        let next_start = client.backend_response();
        let old_query = client.query_response();
        let new_query = client.query_response();
        let mut store = store(&client);
        add(&mut store, "s1").await;
        start(&mut store, "s1", &first_start).await;
        forget(&mut store, "s1").await;
        assert!(old_query.is_closed());
        add(&mut store, "s1").await;
        start(&mut store, "s1", &next_start).await;
        assert!(old_query.try_send(Ok(99)).is_err());
        new_query.try_send(Ok(9)).unwrap();
        count(&mut store, "s1", 9).await;
        forget(&mut store, "s1").await;
        store.advance(Duration::from_secs(10)).await.unwrap();
        store.finish().await.unwrap();
        // Forget never claims to invoke or await remote shutdown.
        client.assert_calls(&[
            ("start", "s1"),
            ("query", "s1"),
            ("start", "s1"),
            ("query", "s1"),
        ]);
    });
}
