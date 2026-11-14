//! Checks proxy, backend and polling composition through deterministic `TestStore`.
//!
//! Leases record local resource release. Separate reply gates control external
//! backend and query completion; dropping a proxy stream must not complete them.

use std::{
    collections::VecDeque,
    panic::{AssertUnwindSafe, catch_unwind},
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll},
};

use async_channel::{Receiver, Sender};
use futures_core::Stream;
use futures_executor::block_on;
use rca_core::KeyedAction;
use rca_example_mcmd_server::{
    backend::{self, BackendClient, BackendError, BackendFuture, BackendKey},
    polling::{self, QueryClient, QueryFuture, QueryKey},
    proxy::{self, AcquireFuture, ProxyClient, ProxyError, ProxyEvent, ProxyKey, ProxySession},
    server::{self, Action, State},
    servers,
};
use rca_test::TestStore;

type BackendResult = Result<(), BackendError>;

/// Records local release without owning or completing backend work.
struct Lease {
    id: usize,
    released: Arc<Mutex<Vec<usize>>>,
}

impl Drop for Lease {
    fn drop(&mut self) {
        self.released.lock().unwrap().push(self.id);
    }
}

struct Session {
    events: Pin<Box<Receiver<ProxyEvent>>>,
    _lease: Lease,
}

impl Stream for Session {
    type Item = ProxyEvent;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.events.as_mut().poll_next(cx)
    }
}

struct Pending {
    result: Receiver<Result<(), ProxyError>>,
    events: Receiver<ProxyEvent>,
}

#[derive(Default)]
struct Client {
    configs: Mutex<Vec<proxy::Config>>,
    proxy: Mutex<VecDeque<Pending>>,
    released: Arc<Mutex<Vec<usize>>>,
    calls: Mutex<Vec<(&'static str, String)>>,
    backend: Mutex<VecDeque<Receiver<BackendResult>>>,
    queries: Mutex<VecDeque<Receiver<Result<u32, polling::QueryError>>>>,
}

impl Client {
    fn proxy(&self) -> (Sender<Result<(), ProxyError>>, Sender<ProxyEvent>) {
        let (tx, result) = async_channel::bounded(1);
        let (events_tx, events) = async_channel::bounded(4);
        self.proxy
            .lock()
            .unwrap()
            .push_back(Pending { result, events });
        (tx, events_tx)
    }

    fn backend(&self) -> Sender<BackendResult> {
        let (tx, rx) = async_channel::bounded(1);
        self.backend.lock().unwrap().push_back(rx);
        tx
    }

    fn query(&self) -> Sender<Result<u32, polling::QueryError>> {
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
        assert_eq!(
            self.calls
                .lock()
                .unwrap()
                .iter()
                .map(|(op, id)| (*op, id.as_str()))
                .collect::<Vec<_>>(),
            expected
        );
    }

    fn assert_released(&self, expected: &[usize]) {
        let actual = self.released.lock().unwrap().clone();
        assert_eq!(actual, expected);
    }
}

impl ProxyClient for Client {
    fn acquire(&self, config: proxy::Config) -> AcquireFuture {
        let mut configs = self.configs.lock().unwrap();
        configs.push(config);
        let id = configs.len();
        drop(configs);
        let pending = self
            .proxy
            .lock()
            .unwrap()
            .pop_front()
            .expect("unexpected proxy call");
        let lease = Lease {
            id,
            released: Arc::clone(&self.released),
        };
        Box::pin(async move {
            pending
                .result
                .recv()
                .await
                .expect("proxy response retained")?;
            Ok(Box::pin(Session {
                events: Box::pin(pending.events),
                _lease: lease,
            }) as ProxySession)
        })
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

fn config(id: &str) -> proxy::Config {
    proxy::Config {
        server_id: id.into(),
        host: "127.0.0.1".into(),
        java_ports: vec![25565],
        bedrock_ports: vec![19132],
    }
}

fn store(client: &Arc<Client>) -> TestStore<State, Action> {
    TestStore::builder(State::new(config("s1")), server::reducer())
        .with_dependency::<ProxyKey>(Arc::clone(client) as Arc<dyn ProxyClient>)
        .with_dependency::<BackendKey>(Arc::clone(client) as Arc<dyn BackendClient>)
        .with_dependency::<QueryKey>(Arc::clone(client) as Arc<dyn QueryClient>)
        .build()
}

async fn proxy_start(store: &mut TestStore<State, Action>) {
    let _ = store
        .send_with(Action::Proxy(proxy::Action::Start), |s| {
            s.proxy.phase = proxy::Phase::Starting;
            s.proxy.last_error = None;
        })
        .await
        .unwrap();
    store
        .assert_state(|s| assert_eq!(s.proxy.phase, proxy::Phase::Starting))
        .await
        .unwrap();
}

async fn ready(store: &mut TestStore<State, Action>) {
    let _ = store
        .receive_with(
            |a| *a == Action::Proxy(proxy::Action::Ready),
            |s| s.proxy.phase = proxy::Phase::Listening,
        )
        .await
        .unwrap();
}

async fn join(store: &mut TestStore<State, Action>, events: &Sender<ProxyEvent>, wake: bool) {
    events.try_send(ProxyEvent::JoinObserved).unwrap();
    let _ = store
        .receive_with(
            |a| *a == Action::Proxy(proxy::Action::Event(ProxyEvent::JoinObserved)),
            move |s| {
                if wake {
                    s.backend.target = backend::Target::Running;
                }
            },
        )
        .await
        .unwrap();
}

async fn starting(store: &mut TestStore<State, Action>) {
    let _ = store
        .receive_with(
            |a| *a == Action::Backend(backend::Action::Reconcile),
            |s| s.backend.phase = backend::Phase::Starting,
        )
        .await
        .unwrap();
    store
        .assert_state(|s| assert_eq!(s.backend.phase, backend::Phase::Starting))
        .await
        .unwrap();
}

async fn running(store: &mut TestStore<State, Action>) {
    let _ = store
        .receive_with(
            |a| *a == Action::Backend(backend::Action::StartFinished(Ok(()))),
            |s| {
                s.backend.phase = backend::Phase::Running;
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
}

async fn proxy_stop(store: &mut TestStore<State, Action>) {
    let _ = store
        .send_with(Action::Proxy(proxy::Action::Stop), |s| {
            s.proxy.phase = proxy::Phase::Stopped;
            s.proxy.last_error = None;
        })
        .await
        .unwrap();
    store
        .assert_state(|s| assert_eq!(s.proxy.phase, proxy::Phase::Stopped))
        .await
        .unwrap();
}

async fn stopping(store: &mut TestStore<State, Action>) {
    let _ = store
        .send_with(Action::Backend(backend::Action::RequestStop), |s| {
            s.backend.target = backend::Target::Stopped;
            s.polling.active = false;
            s.polling.querying = false;
        })
        .await
        .unwrap();
    let _ = store
        .receive_with(
            |a| *a == Action::Backend(backend::Action::Reconcile),
            |s| s.backend.phase = backend::Phase::Stopping,
        )
        .await
        .unwrap();
    store
        .assert_state(|s| assert_eq!(s.backend.phase, backend::Phase::Stopping))
        .await
        .unwrap();
}

#[test]
fn failed_release_assertion_preserves_live_session_cleanup() {
    block_on(async {
        let client = Arc::new(Client::default());
        let (acquired, events) = client.proxy();
        let mut store = store(&client);
        proxy_start(&mut store).await;
        acquired.try_send(Ok(())).unwrap();
        ready(&mut store).await;
        assert!(
            !events.is_closed(),
            "the assertion must run with a live lease"
        );

        let mismatch = catch_unwind(AssertUnwindSafe(|| client.assert_released(&[1])));
        let poisoned = client.released.is_poisoned();
        // Permit the red control to release its live lease without a second panic.
        // The recorded poison state still determines the regression's result.
        client.released.clear_poison();

        proxy_stop(&mut store).await;
        assert!(events.is_closed());
        client.assert_released(&[1]);
        client.assert_calls(&[]);
        store.finish().await.unwrap();
        assert!(
            mismatch.is_err(),
            "the deliberately wrong release log must fail"
        );
        assert!(!poisoned, "an oracle panic must not poison lease cleanup");
    });
}

// A real event is gated before reduction; causal cancellation must discard it.
// This does not claim Stop can overtake an already-gated action: TestStore
// forbids that ordering. Stop/Start follow explicit causal cancellation here.
#[test]
fn causal_cancellation_rejects_gated_old_join_before_proxy_restart() {
    block_on(async {
        let client = Arc::new(Client::default());
        let (old_acquired, old_events) = client.proxy();
        let mut store = store(&client);
        let old_task = store
            .send_with(Action::Proxy(proxy::Action::Start), |s| {
                s.proxy.phase = proxy::Phase::Starting;
            })
            .await
            .unwrap();
        old_acquired.try_send(Ok(())).unwrap();
        ready(&mut store).await;
        old_events.try_send(ProxyEvent::JoinObserved).unwrap();
        let pending = store
            .receive(|action| {
                assert_eq!(
                    *action,
                    Action::Proxy(proxy::Action::Event(ProxyEvent::JoinObserved))
                );
                false
            })
            .await
            .expect_err("deliberate mismatch retains the real event at the gate");
        assert!(pending.is_action_mismatch());
        let boundary = store.pending_boundary().unwrap();
        store
            .assert_state(|s| {
                assert_eq!(s.proxy.phase, proxy::Phase::Listening);
                assert_eq!(s.backend.phase, backend::Phase::Stopped);
                assert_eq!(s.backend.target, backend::Target::Stopped);
            })
            .await
            .unwrap();
        client.assert_calls(&[]);

        old_task.cancel().await.unwrap();
        assert!(old_task.is_cancelled());
        old_task.finish().await.unwrap();
        let missing = store
            .receive(|_| true)
            .await
            .expect_err("cancelled event is absent");
        assert!(missing.is_no_received_action());
        assert!(old_events.is_closed());
        client.assert_released(&[1]);
        client.assert_calls(&[]);
        store
            .receive_terminal(boundary, |error| {
                error.reason() == rca_core::TaskAccessReason::Cancelled
                    && error.progress() == rca_core::TaskProgress::NotStarted
            })
            .await
            .unwrap();
        proxy_stop(&mut store).await;

        let (new_acquired, new_events) = client.proxy();
        let started = client.backend();
        proxy_start(&mut store).await;
        new_acquired.try_send(Ok(())).unwrap();
        ready(&mut store).await;
        client.assert_calls(&[]);
        join(&mut store, &new_events, true).await;
        starting(&mut store).await;
        client.assert_calls(&[("start", "s1")]);
        started
            .try_send(Err(BackendError("controlled start failure".into())))
            .unwrap();
        let _ = store
            .receive_with(
                |action| {
                    *action
                        == Action::Backend(backend::Action::StartFinished(Err(BackendError(
                            "controlled start failure".into(),
                        ))))
                },
                |s| {
                    s.backend.phase = backend::Phase::Error;
                    s.backend.last_error = Some("controlled start failure".into());
                },
            )
            .await
            .unwrap();
        proxy_stop(&mut store).await;
        assert!(new_events.is_closed());
        client.assert_released(&[1, 2]);
        client.assert_calls(&[("start", "s1")]);
        store.finish().await.unwrap();
    });
}

// Missing proxy routing, wake policy or independent cancellation fails full state.
#[test]
fn proxy_stop_does_not_cancel_pending_backend_start() {
    block_on(async {
        let client = Arc::new(Client::default());
        let (acquired, events) = client.proxy();
        let started = client.backend();
        let query = client.query();
        let mut store = store(&client);
        proxy_start(&mut store).await;
        acquired.try_send(Ok(())).unwrap();
        ready(&mut store).await;
        join(&mut store, &events, true).await;
        starting(&mut store).await;
        proxy_stop(&mut store).await;
        assert!(events.is_closed());
        assert!(!started.is_closed());
        client.assert_released(&[1]);
        started.try_send(Ok(())).unwrap();
        running(&mut store).await;
        let stopped = client.backend();
        stopping(&mut store).await;
        assert!(query.is_closed());
        stopped.try_send(Ok(())).unwrap();
        let _ = store
            .receive_with(
                |a| *a == Action::Backend(backend::Action::StopFinished(Ok(()))),
                |s| s.backend.phase = backend::Phase::Stopped,
            )
            .await
            .unwrap();
        let _ = store
            .receive(|a| *a == Action::Backend(backend::Action::Stopped))
            .await
            .unwrap();
        client.assert_calls(&[("start", "s1"), ("query", "s1"), ("stop", "s1")]);
        store.finish().await.unwrap();
    });
}

#[test]
fn join_does_not_reverse_stop_or_retry_error() {
    block_on(async {
        let client = Arc::new(Client::default());
        let (acquired, events) = client.proxy();
        let started = client.backend();
        let query = client.query();
        let mut store = store(&client);
        proxy_start(&mut store).await;
        acquired.try_send(Ok(())).unwrap();
        ready(&mut store).await;
        join(&mut store, &events, true).await;
        starting(&mut store).await;
        started.try_send(Ok(())).unwrap();
        running(&mut store).await;
        let stopped = client.backend();
        stopping(&mut store).await;
        assert!(query.is_closed());
        assert!(!events.is_closed());
        client.assert_released(&[]);
        join(&mut store, &events, false).await;
        assert!(!stopped.is_closed());
        stopped
            .try_send(Err(BackendError("stop failed".into())))
            .unwrap();
        let _ = store
            .receive_with(
                |a| {
                    *a == Action::Backend(backend::Action::StopFinished(Err(BackendError(
                        "stop failed".into(),
                    ))))
                },
                |s| {
                    s.backend.phase = backend::Phase::Error;
                    s.backend.last_error = Some("stop failed".into());
                },
            )
            .await
            .unwrap();
        join(&mut store, &events, false).await;
        client.assert_calls(&[("start", "s1"), ("query", "s1"), ("stop", "s1")]);
        assert!(!events.is_closed());
        proxy_stop(&mut store).await;
        client.assert_released(&[1]);
        store.finish().await.unwrap();
    });
}

#[test]
fn proxy_failure_and_eof_preserve_backend() {
    for (error, diagnostic) in [
        (
            ProxyError::StartFailed("bind \"failed\"\nretry".into()),
            "StartFailed(\"bind \\\"failed\\\"\\nretry\")",
        ),
        (
            ProxyError::Other("bind \"failed\"\nretry".into()),
            "Other(\"bind \\\"failed\\\"\\nretry\")",
        ),
    ] {
        block_on(async {
            let client = Arc::new(Client::default());
            let (failed, old_events) = client.proxy();
            let mut store = store(&client);
            proxy_start(&mut store).await;
            failed.try_send(Err(error.clone())).unwrap();
            let _ = store
                .receive_with(
                    move |a| *a == Action::Proxy(proxy::Action::Failed(error.clone())),
                    move |s| {
                        s.proxy.phase = proxy::Phase::Failed;
                        s.proxy.last_error = Some(diagnostic.into());
                    },
                )
                .await
                .unwrap();
            client.assert_released(&[1]);
            assert!(old_events.is_closed());
            let (acquired, events) = client.proxy();
            proxy_start(&mut store).await;
            acquired.try_send(Ok(())).unwrap();
            ready(&mut store).await;
            drop(events);
            let _ = store
                .receive_with(
                    |a| *a == Action::Proxy(proxy::Action::Ended),
                    |s| s.proxy.phase = proxy::Phase::Stopped,
                )
                .await
                .unwrap();
            client.assert_released(&[1, 2]);
            client.assert_calls(&[]);
            assert_eq!(
                *client.configs.lock().unwrap(),
                [config("s1"), config("s1")]
            );
            store.finish().await.unwrap();
        });
    }
}

fn keyed(id: &str, action: Action) -> servers::Action {
    servers::Action::Server(KeyedAction::new(id.into(), action))
}

async fn keyed_proxy_start(
    store: &mut TestStore<servers::State, servers::Action>,
    id: &'static str,
    acquired: &Sender<Result<(), ProxyError>>,
) {
    let _ = store
        .send_with(keyed(id, Action::Proxy(proxy::Action::Start)), move |s| {
            s.servers.get_mut(&id.into()).unwrap().proxy.phase = proxy::Phase::Starting;
        })
        .await
        .unwrap();
    acquired.try_send(Ok(())).unwrap();
    let _ = store
        .receive_with(
            move |a| *a == keyed(id, Action::Proxy(proxy::Action::Ready)),
            move |s| s.servers.get_mut(&id.into()).unwrap().proxy.phase = proxy::Phase::Listening,
        )
        .await
        .unwrap();
}

async fn keyed_forget(store: &mut TestStore<servers::State, servers::Action>, id: &'static str) {
    let _ = store
        .send_with(servers::Action::Forget(id.into()), move |s| {
            s.servers.remove(&id.into());
        })
        .await
        .unwrap();
    store
        .assert_state(move |s| assert!(!s.servers.contains_id(&id.into())))
        .await
        .unwrap();
}

#[test]
fn keyed_proxy_stop_preserves_live_sibling_events() {
    block_on(async {
        let client = Arc::new(Client::default());
        let (first, stopped_events) = client.proxy();
        let (second, sibling_events) = client.proxy();
        let mut store = TestStore::builder(servers::State::default(), servers::reducer())
            .with_dependency::<ProxyKey>(Arc::clone(&client) as Arc<dyn ProxyClient>)
            .with_dependency::<BackendKey>(Arc::clone(&client) as Arc<dyn BackendClient>)
            .build();
        for id in ["s1", "s2"] {
            let _ = store
                .send_with(servers::Action::Add(config(id)), move |s| {
                    s.servers.insert(id.into(), State::new(config(id)));
                })
                .await
                .unwrap();
        }
        keyed_proxy_start(&mut store, "s1", &first).await;
        keyed_proxy_start(&mut store, "s2", &second).await;
        let _ = store
            .send_with(keyed("s1", Action::Proxy(proxy::Action::Stop)), |s| {
                s.servers.get_mut(&"s1".into()).unwrap().proxy.phase = proxy::Phase::Stopped;
            })
            .await
            .unwrap();
        assert!(stopped_events.is_closed());
        assert!(!sibling_events.is_closed());
        client.assert_released(&[1]);

        let backend = client.backend();
        sibling_events.try_send(ProxyEvent::JoinObserved).unwrap();
        let _ = store
            .receive_with(
                |a| {
                    *a == keyed(
                        "s2",
                        Action::Proxy(proxy::Action::Event(ProxyEvent::JoinObserved)),
                    )
                },
                |s| {
                    s.servers.get_mut(&"s2".into()).unwrap().backend.target =
                        backend::Target::Running;
                },
            )
            .await
            .unwrap();
        let _ = store
            .receive_with(
                |a| *a == keyed("s2", Action::Backend(backend::Action::Reconcile)),
                |s| {
                    s.servers.get_mut(&"s2".into()).unwrap().backend.phase =
                        backend::Phase::Starting;
                },
            )
            .await
            .unwrap();
        client.assert_calls(&[("start", "s2")]);
        keyed_forget(&mut store, "s1").await;
        assert!(!backend.is_closed());
        assert!(!sibling_events.is_closed());
        keyed_forget(&mut store, "s2").await;
        assert!(backend.is_closed());
        assert!(sibling_events.is_closed());
        client.assert_released(&[1, 2]);
        store.finish().await.unwrap();
    });
}

#[test]
fn keyed_forget_preserves_sibling_and_reinsertion_config() {
    block_on(async {
        let client = Arc::new(Client::default());
        let (first, old_events) = client.proxy();
        let (second, sibling_events) = client.proxy();
        let mut store = TestStore::builder(servers::State::default(), servers::reducer())
            .with_dependency::<ProxyKey>(Arc::clone(&client) as Arc<dyn ProxyClient>)
            .with_dependency::<BackendKey>(Arc::clone(&client) as Arc<dyn BackendClient>)
            .with_dependency::<QueryKey>(Arc::clone(&client) as Arc<dyn QueryClient>)
            .build();
        for id in ["s1", "s2"] {
            let _ = store
                .send_with(servers::Action::Add(config(id)), move |s| {
                    s.servers.insert(id.into(), State::new(config(id)));
                })
                .await
                .unwrap();
        }
        keyed_proxy_start(&mut store, "s1", &first).await;
        keyed_proxy_start(&mut store, "s2", &second).await;
        let mut replacement = config("s1");
        replacement.host = "::1".into();
        replacement.java_ports = vec![25566];
        replacement.bedrock_ports = vec![19133];
        let _ = store
            .send(servers::Action::Add(replacement.clone()))
            .await
            .unwrap();
        client.assert_released(&[]);
        keyed_forget(&mut store, "s1").await;
        assert!(old_events.is_closed());
        assert!(!sibling_events.is_closed());
        client.assert_released(&[1]);

        // The surviving sibling still routes real stream events into backend policy.
        let backend = client.backend();
        sibling_events.try_send(ProxyEvent::JoinObserved).unwrap();
        let _ = store
            .receive_with(
                |a| {
                    *a == keyed(
                        "s2",
                        Action::Proxy(proxy::Action::Event(ProxyEvent::JoinObserved)),
                    )
                },
                |s| {
                    s.servers.get_mut(&"s2".into()).unwrap().backend.target =
                        backend::Target::Running;
                },
            )
            .await
            .unwrap();
        let _ = store
            .receive_with(
                |a| *a == keyed("s2", Action::Backend(backend::Action::Reconcile)),
                |s| {
                    s.servers.get_mut(&"s2".into()).unwrap().backend.phase =
                        backend::Phase::Starting;
                },
            )
            .await
            .unwrap();

        let expected = replacement.clone();
        let _ = store
            .send_with(servers::Action::Add(replacement.clone()), move |s| {
                s.servers.insert("s1".into(), State::new(expected));
            })
            .await
            .unwrap();
        let (third, new_events) = client.proxy();
        keyed_proxy_start(&mut store, "s1", &third).await;
        assert!(old_events.try_send(ProxyEvent::JoinObserved).is_err());
        assert!(!new_events.is_closed());
        assert!(!backend.is_closed());
        assert_eq!(
            *client.configs.lock().unwrap(),
            [config("s1"), config("s2"), replacement]
        );
        keyed_forget(&mut store, "s1").await;
        client.assert_released(&[1, 3]);
        assert!(new_events.is_closed());
        assert!(!sibling_events.is_closed());
        assert!(!backend.is_closed());
        keyed_forget(&mut store, "s2").await;
        assert!(sibling_events.is_closed());
        assert!(backend.is_closed());
        client.assert_released(&[1, 3, 2]);
        client.assert_calls(&[("start", "s2")]);
        store.finish().await.unwrap();
    });
}

#[test]
fn join_wakes_backend_and_stop_domains_stay_independent() {
    block_on(async {
        let client = Arc::new(Client::default());
        let (acquired, events) = client.proxy();
        let started = client.backend();
        let query = client.query();
        let mut store = store(&client);
        proxy_start(&mut store).await;
        acquired.try_send(Ok(())).unwrap();
        ready(&mut store).await;
        join(&mut store, &events, true).await;
        starting(&mut store).await;
        join(&mut store, &events, false).await;
        started.try_send(Ok(())).unwrap();
        running(&mut store).await;
        join(&mut store, &events, false).await;
        proxy_stop(&mut store).await;
        assert!(events.is_closed());
        assert!(!query.is_closed());
        client.assert_released(&[1]);
        client.assert_calls(&[("start", "s1"), ("query", "s1")]);
        let stopped = client.backend();
        stopping(&mut store).await;
        assert!(query.is_closed());
        stopped.try_send(Ok(())).unwrap();
        let _ = store
            .receive_with(
                |a| *a == Action::Backend(backend::Action::StopFinished(Ok(()))),
                |s| s.backend.phase = backend::Phase::Stopped,
            )
            .await
            .unwrap();
        let _ = store
            .receive(|a| *a == Action::Backend(backend::Action::Stopped))
            .await
            .unwrap();
        client.assert_calls(&[("start", "s1"), ("query", "s1"), ("stop", "s1")]);
        assert_eq!(*client.configs.lock().unwrap(), [config("s1")]);
        store.finish().await.unwrap();
    });
}
