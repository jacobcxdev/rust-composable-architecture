//! Tests backend and polling coordination through the composed reducer.
//!
//! `TestStore` checks complete expected snapshots and advances virtual time. Separate
//! client gates distinguish cancelling a query from confirming backend shutdown.

use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::Duration,
};

use async_channel::{Receiver, Sender};
use futures_executor::block_on;
use rca_example_mcmd_server::{
    backend::{self, BackendClient, BackendError, BackendFuture, BackendKey, Phase, Target},
    polling::{self, QueryClient, QueryError, QueryFuture, QueryKey},
    server::{self, Action, State},
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
        let actual = self.calls.lock().unwrap();
        assert_eq!(
            actual
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
    TestStore::builder(
        State::new(rca_example_mcmd_server::proxy::Config {
            server_id: "s1".into(),
            host: "127.0.0.1".into(),
            java_ports: vec![25565],
            bedrock_ports: vec![19132],
        }),
        server::reducer(),
    )
    .with_dependency::<BackendKey>(Arc::clone(client) as Arc<dyn BackendClient>)
    .with_dependency::<QueryKey>(Arc::clone(client) as Arc<dyn QueryClient>)
    .build()
}

async fn request_start(store: &mut TestStore<State, Action>) {
    let _ = store
        .send_with(Action::Backend(backend::Action::RequestStart), |s| {
            s.backend.target = Target::Running;
        })
        .await
        .unwrap();
    let _ = store
        .receive_with(
            |a| *a == Action::Backend(backend::Action::Reconcile),
            |s| s.backend.phase = Phase::Starting,
        )
        .await
        .unwrap();
    store
        .assert_state(|s| assert_eq!(s.backend.phase, Phase::Starting))
        .await
        .unwrap();
}

async fn started(store: &mut TestStore<State, Action>) {
    let _ = store
        .receive_with(
            |a| *a == Action::Backend(backend::Action::StartFinished(Ok(()))),
            |s| {
                s.backend.phase = Phase::Running;
                s.polling.active = true;
            },
        )
        .await
        .unwrap();
    let _ = store
        .receive_with(
            |a| *a == Action::Polling(polling::Action::Tick),
            |s| s.polling.querying = true,
        )
        .await
        .unwrap();
    let _ = store
        .receive(|a| *a == Action::Backend(backend::Action::Started))
        .await
        .unwrap();
    store
        .assert_state(|s| assert!(s.polling.querying))
        .await
        .unwrap();
}

async fn count(store: &mut TestStore<State, Action>, value: u32) {
    let _ = store
        .receive_with(
            move |a| *a == Action::Polling(polling::Action::Polled(Ok(value))),
            move |s| {
                s.polling.querying = false;
                s.polling.player_count = value;
                s.polling.failures = 0;
                s.polling.last_error = None;
            },
        )
        .await
        .unwrap();
}

async fn request_stop(store: &mut TestStore<State, Action>) {
    let _ = store
        .send_with(Action::Backend(backend::Action::RequestStop), |s| {
            s.backend.target = Target::Stopped;
            s.polling.active = false;
            s.polling.querying = false;
        })
        .await
        .unwrap();
    let _ = store
        .receive_with(
            |a| *a == Action::Backend(backend::Action::Reconcile),
            |s| s.backend.phase = Phase::Stopping,
        )
        .await
        .unwrap();
    store
        .assert_state(|s| assert_eq!(s.backend.phase, Phase::Stopping))
        .await
        .unwrap();
}

async fn stopped(store: &mut TestStore<State, Action>) {
    let _ = store
        .receive_with(
            |a| *a == Action::Backend(backend::Action::StopFinished(Ok(()))),
            |s| {
                s.backend.phase = Phase::Stopped;
                s.polling.player_count = 0;
                s.polling.failures = 0;
                s.polling.last_error = None;
            },
        )
        .await
        .unwrap();
    let _ = store
        .receive(|a| *a == Action::Backend(backend::Action::Stopped))
        .await
        .unwrap();
}

// Omitting scoped coordination or reset must fail full-state observations.
#[test]
fn wanted_start_polls_and_confirmed_stop_resets() {
    block_on(async {
        let client = Arc::new(Client::default());
        let start = client.backend_response();
        let stop = client.backend_response();
        let query = client.query_response();
        let failed_query = client.query_response();
        let mut store = store(&client);
        request_start(&mut store).await;
        start.try_send(Ok(())).unwrap();
        started(&mut store).await;
        query.try_send(Ok(7)).unwrap();
        count(&mut store, 7).await;
        store.advance(Duration::from_secs(5)).await.unwrap();
        let _ = store
            .receive_with(
                |a| *a == Action::Polling(polling::Action::Tick),
                |s| s.polling.querying = true,
            )
            .await
            .unwrap();
        store
            .assert_state(|s| assert!(s.polling.querying))
            .await
            .unwrap();
        failed_query
            .try_send(Err(QueryError::Other("query failed".into())))
            .unwrap();
        let _ = store
            .receive_with(
                |a| {
                    *a == Action::Polling(polling::Action::Polled(Err(QueryError::Other(
                        "query failed".into(),
                    ))))
                },
                |s| {
                    s.polling.querying = false;
                    s.polling.failures = 1;
                    s.polling.last_error = Some("Other(\"query failed\")".into());
                },
            )
            .await
            .unwrap();
        request_stop(&mut store).await;
        stop.try_send(Ok(())).unwrap();
        stopped(&mut store).await;
        store.finish().await.unwrap();
        client.assert_calls(&[
            ("start", "s1"),
            ("query", "s1"),
            ("query", "s1"),
            ("stop", "s1"),
        ]);
    });
}

// Erasing a query category/payload or failing to recover must fail full-state checks.
#[test]
fn protocol_error_survives_composition_and_success_recovers() {
    block_on(async {
        let client = Arc::new(Client::default());
        let start = client.backend_response();
        let stop = client.backend_response();
        let query = client.query_response();
        let failed = client.query_response();
        let recovered = client.query_response();
        let mut store = store(&client);
        request_start(&mut store).await;
        start.try_send(Ok(())).unwrap();
        started(&mut store).await;
        query.try_send(Ok(9)).unwrap();
        count(&mut store, 9).await;
        store.advance(Duration::from_secs(5)).await.unwrap();
        let _ = store
            .receive_with(
                |a| *a == Action::Polling(polling::Action::Tick),
                |s| s.polling.querying = true,
            )
            .await
            .unwrap();
        let error = QueryError::Protocol("bad \"packet\"\n".into());
        failed.try_send(Err(error.clone())).unwrap();
        let _ = store
            .receive_with(
                move |a| *a == Action::Polling(polling::Action::Polled(Err(error.clone()))),
                |s| {
                    s.polling.querying = false;
                    s.polling.failures = 1;
                    s.polling.last_error = Some("Protocol(\"bad \\\"packet\\\"\\n\")".into());
                },
            )
            .await
            .unwrap();
        store.advance(Duration::from_secs(5)).await.unwrap();
        let _ = store
            .receive_with(
                |a| *a == Action::Polling(polling::Action::Tick),
                |s| s.polling.querying = true,
            )
            .await
            .unwrap();
        recovered.try_send(Ok(4)).unwrap();
        count(&mut store, 4).await;
        request_stop(&mut store).await;
        stop.try_send(Ok(())).unwrap();
        stopped(&mut store).await;
        store.finish().await.unwrap();
        client.assert_calls(&[
            ("start", "s1"),
            ("query", "s1"),
            ("query", "s1"),
            ("query", "s1"),
            ("stop", "s1"),
        ]);
    });
}

// Removing cancellation must leave a receiver live or produce unexpected ticks.
#[test]
fn stop_cancels_pending_query_before_backend_finishes() {
    block_on(async {
        let client = Arc::new(Client::default());
        let start = client.backend_response();
        let stop = client.backend_response();
        let query = client.query_response();
        let mut store = store(&client);
        request_start(&mut store).await;
        start.try_send(Ok(())).unwrap();
        started(&mut store).await;
        request_stop(&mut store).await;
        assert!(query.is_closed(), "stop must release the pending query");
        assert!(query.try_send(Ok(99)).is_err());
        store.advance(Duration::from_secs(10)).await.unwrap();
        store
            .assert_state(|s| assert_eq!(s.backend.phase, Phase::Stopping))
            .await
            .unwrap();
        client.assert_calls(&[("start", "s1"), ("query", "s1"), ("stop", "s1")]);
        stop.try_send(Ok(())).unwrap();
        stopped(&mut store).await;
        store.finish().await.unwrap();
    });
}

// Restart must wait for backend stop and must not revive the old query lifetime.
#[test]
fn restart_waits_for_stop_and_uses_fresh_polling() {
    block_on(async {
        let client = Arc::new(Client::default());
        let start = client.backend_response();
        let stop = client.backend_response();
        let restart = client.backend_response();
        let final_stop = client.backend_response();
        let old_query = client.query_response();
        let next_query = client.query_response();
        let mut store = store(&client);
        request_start(&mut store).await;
        start.try_send(Ok(())).unwrap();
        started(&mut store).await;
        request_stop(&mut store).await;
        assert!(old_query.is_closed());
        let _ = store
            .send_with(Action::Backend(backend::Action::RequestStart), |s| {
                s.backend.target = Target::Running;
            })
            .await
            .unwrap();
        let _ = store
            .receive(|a| *a == Action::Backend(backend::Action::Reconcile))
            .await
            .unwrap();
        client.assert_calls(&[("start", "s1"), ("query", "s1"), ("stop", "s1")]);
        stop.try_send(Ok(())).unwrap();
        stopped(&mut store).await;
        let _ = store
            .receive_with(
                |a| *a == Action::Backend(backend::Action::Reconcile),
                |s| s.backend.phase = Phase::Starting,
            )
            .await
            .unwrap();
        store
            .assert_state(|s| assert!(!s.polling.active))
            .await
            .unwrap();
        restart.try_send(Ok(())).unwrap();
        started(&mut store).await;
        assert!(old_query.try_send(Ok(99)).is_err());
        next_query.try_send(Ok(9)).unwrap();
        count(&mut store, 9).await;
        request_stop(&mut store).await;
        final_stop.try_send(Ok(())).unwrap();
        stopped(&mut store).await;
        store.finish().await.unwrap();
        client.assert_calls(&[
            ("start", "s1"),
            ("query", "s1"),
            ("stop", "s1"),
            ("start", "s1"),
            ("query", "s1"),
            ("stop", "s1"),
        ]);
    });
}

// Activating polling for any Running phase, ignoring its target, must fail.
#[test]
fn unwanted_start_never_polls() {
    block_on(async {
        let client = Arc::new(Client::default());
        let start = client.backend_response();
        let stop = client.backend_response();
        let mut store = store(&client);
        request_start(&mut store).await;
        let _ = store
            .send_with(Action::Backend(backend::Action::RequestStop), |s| {
                s.backend.target = Target::Stopped;
            })
            .await
            .unwrap();
        let _ = store
            .receive(|a| *a == Action::Backend(backend::Action::Reconcile))
            .await
            .unwrap();
        start.try_send(Ok(())).unwrap();
        let _ = store
            .receive_with(
                |a| *a == Action::Backend(backend::Action::StartFinished(Ok(()))),
                |s| s.backend.phase = Phase::Running,
            )
            .await
            .unwrap();
        let _ = store
            .receive_with(
                |a| *a == Action::Backend(backend::Action::Reconcile),
                |s| s.backend.phase = Phase::Stopping,
            )
            .await
            .unwrap();
        store
            .assert_state(|s| assert!(!s.polling.active))
            .await
            .unwrap();
        stop.try_send(Ok(())).unwrap();
        stopped(&mut store).await;
        store.finish().await.unwrap();
        client.assert_calls(&[("start", "s1"), ("stop", "s1")]);
    });
}

// Failure must not start polling or erase observations before confirmed stop.
#[test]
fn failures_suspend_polling_and_preserve_observations() {
    block_on(async {
        let client = Arc::new(Client::default());
        let start = client.backend_response();
        let mut first = store(&client);
        request_start(&mut first).await;
        start
            .try_send(Err(BackendError("start failed".into())))
            .unwrap();
        let _ = first
            .receive_with(
                |a| {
                    *a == Action::Backend(backend::Action::StartFinished(Err(BackendError(
                        "start failed".into(),
                    ))))
                },
                |s| {
                    s.backend.phase = Phase::Error;
                    s.backend.last_error = Some("start failed".into());
                },
            )
            .await
            .unwrap();
        first.finish().await.unwrap();
        client.assert_calls(&[("start", "s1")]);

        let client = Arc::new(Client::default());
        let start = client.backend_response();
        let stop = client.backend_response();
        let query = client.query_response();
        let mut store = store(&client);
        request_start(&mut store).await;
        start.try_send(Ok(())).unwrap();
        started(&mut store).await;
        query.try_send(Ok(7)).unwrap();
        count(&mut store, 7).await;
        request_stop(&mut store).await;
        stop.try_send(Err(BackendError("stop failed".into())))
            .unwrap();
        let _ = store
            .receive_with(
                |a| {
                    *a == Action::Backend(backend::Action::StopFinished(Err(BackendError(
                        "stop failed".into(),
                    ))))
                },
                |s| {
                    s.backend.phase = Phase::Error;
                    s.backend.last_error = Some("stop failed".into());
                },
            )
            .await
            .unwrap();
        store.advance(Duration::from_secs(10)).await.unwrap();
        store.finish().await.unwrap();
        client.assert_calls(&[("start", "s1"), ("query", "s1"), ("stop", "s1")]);
    });
}

// Re-running coordination on delegates must not create another timer or query.
#[test]
fn duplicate_delegates_do_not_duplicate_polling() {
    block_on(async {
        let client = Arc::new(Client::default());
        let start = client.backend_response();
        let stop = client.backend_response();
        let first_query = client.query_response();
        let second_query = client.query_response();
        let mut store = store(&client);
        request_start(&mut store).await;
        start.try_send(Ok(())).unwrap();
        started(&mut store).await;
        let _ = store
            .send(Action::Backend(backend::Action::Started))
            .await
            .unwrap();
        let _ = store
            .send(Action::Backend(backend::Action::Started))
            .await
            .unwrap();
        first_query.try_send(Ok(1)).unwrap();
        count(&mut store, 1).await;
        store.advance(Duration::from_secs(5)).await.unwrap();
        let _ = store
            .receive_with(
                |a| *a == Action::Polling(polling::Action::Tick),
                |s| s.polling.querying = true,
            )
            .await
            .unwrap();
        store
            .assert_state(|s| assert!(s.polling.querying))
            .await
            .unwrap();
        second_query.try_send(Ok(2)).unwrap();
        count(&mut store, 2).await;
        request_stop(&mut store).await;
        stop.try_send(Ok(())).unwrap();
        stopped(&mut store).await;
        store.finish().await.unwrap();
        client.assert_calls(&[
            ("start", "s1"),
            ("query", "s1"),
            ("query", "s1"),
            ("stop", "s1"),
        ]);
    });
}
