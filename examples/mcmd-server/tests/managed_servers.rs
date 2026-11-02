//! Checks keyed owner routing, child cancellation and confirmed removal.
//!
//! `TestStore` drives RCA and virtual time deterministically. Tests poll external owners separately.
//! Tokio supplies a cooperative deadlock guard, not the polling schedule.
//! Typed call gates expose the target identity and each external operation's completion.

use std::{pin::Pin, sync::Arc, time::Duration};

use async_channel::{Receiver, Sender};
use rca_core::KeyedAction;
use rca_example_mcmd_server::{
    backend::{self, BackendClient, BackendError, BackendFuture, BackendKey},
    managed_proxy::{self, Error, Events, Phase, Recovery, StartFuture, StopFuture},
    managed_servers::{self, Action, ServerAction, ServerState, State},
    polling::{self, QueryClient, QueryFuture, QueryKey},
    proxy::{self, ProxyEvent},
};
use rca_test::TestStore;

/// Rejects any external operation during composition.
struct Unused;

impl managed_proxy::Client for Unused {
    fn start(&self, _: proxy::Config) -> managed_proxy::StartFuture {
        panic!("composition must not start external work")
    }

    fn stop(&self, _: String) -> managed_proxy::StopFuture {
        panic!("composition must not stop external work")
    }
}

fn config(id: &'static str) -> proxy::Config {
    proxy::Config {
        server_id: id.into(),
        host: "127.0.0.1".into(),
        java_ports: vec![25565],
        bedrock_ports: vec![19132],
    }
}

#[test]
fn duplicate_ids_cannot_create_multiple_external_owners() {
    let Err(error) = managed_servers::compose([config("alpha"), config("alpha")], Arc::new(Unused))
    else {
        panic!("duplicate identities must not return multiple owners")
    };
    assert_eq!(error, managed_servers::DuplicateId("alpha".into()));
}

/// Records client entry and transfers the operation's completion gate.
enum Call {
    Start(proxy::Config, Sender<Result<Events, Error>>),
    Stop(String, Sender<Result<(), Error>>),
    Backend(&'static str, String, Sender<Result<(), BackendError>>),
    Query(String, Sender<Result<u32, polling::QueryError>>),
}

/// Keeps proxy, backend and query calls observable through one bounded queue.
struct Client(Sender<Call>);

impl managed_proxy::Client for Client {
    fn start(&self, config: proxy::Config) -> StartFuture {
        let calls = self.0.clone();
        Box::pin(async move {
            let (reply, receive) = async_channel::bounded(1);
            calls.send(Call::Start(config, reply)).await.unwrap();
            receive.recv().await.unwrap()
        })
    }

    fn stop(&self, server_id: String) -> StopFuture {
        let calls = self.0.clone();
        Box::pin(async move {
            let (reply, receive) = async_channel::bounded(1);
            calls.send(Call::Stop(server_id, reply)).await.unwrap();
            receive.recv().await.unwrap()
        })
    }
}

impl Client {
    fn backend(&self, operation: &'static str, id: String) -> BackendFuture {
        let calls = self.0.clone();
        Box::pin(async move {
            let (reply, receive) = async_channel::bounded(1);
            calls
                .send(Call::Backend(operation, id, reply))
                .await
                .unwrap();
            receive.recv().await.unwrap()
        })
    }
}

impl BackendClient for Client {
    fn start(&self, server_id: String) -> BackendFuture {
        self.backend("start", server_id)
    }

    fn stop(&self, server_id: String) -> BackendFuture {
        self.backend("stop", server_id)
    }
}

impl QueryClient for Client {
    fn poll(&self, server_id: String) -> QueryFuture {
        let calls = self.0.clone();
        Box::pin(async move {
            let (reply, receive) = async_channel::bounded(1);
            calls.send(Call::Query(server_id, reply)).await.unwrap();
            receive.recv().await.unwrap()
        })
    }
}

/// Holds an external owner polled separately from `TestStore`'s deterministic runtime.
type Host = Pin<Box<dyn Future<Output = Recovery>>>;

/// Bounds cooperative waits without advancing `TestStore`'s virtual time.
fn check(test: impl Future<Output = ()>) {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            tokio::time::timeout(Duration::from_secs(15), test)
                .await
                .unwrap();
        });
}

/// Wires two independent owners and injects the observed backend and query clients.
fn setup() -> (TestStore<State, Action>, Receiver<Call>, Vec<Host>) {
    let (send, calls) = async_channel::bounded(4);
    let client = Arc::new(Client(send));
    let (state, reducer, owners) = managed_servers::compose(
        [config("alpha"), config("beta")],
        Arc::clone(&client) as Arc<dyn managed_proxy::Client>,
    )
    .unwrap();
    assert_eq!(owners.len(), 2);
    let store = TestStore::builder(state, reducer)
        .with_dependency::<BackendKey>(Arc::clone(&client) as Arc<dyn BackendClient>)
        .with_dependency::<QueryKey>(client as Arc<dyn QueryClient>)
        .build();
    (
        store,
        calls,
        owners
            .into_iter()
            .map(|owner| Box::pin(owner.run()) as Host)
            .collect(),
    )
}

fn child<'a>(state: &'a mut State, id: &'static str) -> &'a mut ServerState {
    state.servers.get_mut(&id.to_owned()).unwrap()
}

fn action(id: &'static str, action: ServerAction) -> Action {
    Action::Server(KeyedAction::new(id.to_owned(), action))
}

fn proxy_action(id: &'static str, action: managed_proxy::Action) -> Action {
    self::action(id, ServerAction::Proxy(action))
}

async fn step(host: &mut Host) {
    assert!(futures_util::poll!(host.as_mut()).is_pending());
}

async fn start(
    store: &mut TestStore<State, Action>,
    calls: &Receiver<Call>,
    host: &mut Host,
    id: &'static str,
) -> Sender<ProxyEvent> {
    let _ = store
        .send_with(proxy_action(id, managed_proxy::Action::Start), move |s| {
            child(s, id).proxy.phase = Phase::Starting;
        })
        .await
        .unwrap();
    step(host).await;
    let Call::Start(actual, reply) = calls.recv().await.unwrap() else {
        panic!("expected proxy start")
    };
    assert_eq!(actual, config(id));
    let (events, receive) = async_channel::bounded(2);
    reply.send(Ok(Box::pin(receive))).await.unwrap();
    step(host).await;
    let _ = store
        .receive_with(
            move |a| *a == proxy_action(id, managed_proxy::Action::Ready),
            move |s| {
                child(s, id).proxy.phase = Phase::Listening;
            },
        )
        .await
        .unwrap();
    events
}

async fn stop_request(
    store: &mut TestStore<State, Action>,
    calls: &Receiver<Call>,
    host: &mut Host,
    id: &'static str,
) -> (rca_test::TestActionTask, Sender<Result<(), Error>>) {
    let task = store
        .send_with(proxy_action(id, managed_proxy::Action::Stop), move |s| {
            child(s, id).proxy.phase = Phase::Stopping;
            child(s, id).proxy.last_error = None;
        })
        .await
        .unwrap();
    step(host).await;
    let Call::Stop(actual, reply) = calls.recv().await.unwrap() else {
        panic!("expected proxy stop")
    };
    assert_eq!(actual, id);
    (task, reply)
}

async fn stopped(
    store: &mut TestStore<State, Action>,
    host: &mut Host,
    id: &'static str,
    reply: Sender<Result<(), Error>>,
) {
    reply.send(Ok(())).await.unwrap();
    step(host).await;
    let _ = store
        .receive_with(
            move |a| *a == proxy_action(id, managed_proxy::Action::Stopped),
            move |s| {
                child(s, id).proxy.phase = Phase::Stopped;
            },
        )
        .await
        .unwrap();
}

async fn query_result(
    store: &mut TestStore<State, Action>,
    calls: &Receiver<Call>,
    id: &'static str,
    count: u32,
) {
    let Call::Query(actual, reply) = calls.recv().await.unwrap() else {
        panic!("expected query")
    };
    assert_eq!(actual, id);
    reply.send(Ok(count)).await.unwrap();
    let _ = store
        .receive_with(
            move |a| {
                *a == action(
                    id,
                    ServerAction::Polling(polling::Action::Polled(Ok(count))),
                )
            },
            move |s| {
                child(s, id).polling.querying = false;
                child(s, id).polling.player_count = count;
            },
        )
        .await
        .unwrap();
}

async fn wake_backend(
    store: &mut TestStore<State, Action>,
    calls: &Receiver<Call>,
    id: &'static str,
    events: &Sender<ProxyEvent>,
) {
    let reply = begin_backend(store, calls, id, events).await;
    complete_backend(store, calls, id, reply).await;
}

async fn begin_backend(
    store: &mut TestStore<State, Action>,
    calls: &Receiver<Call>,
    id: &'static str,
    events: &Sender<ProxyEvent>,
) -> Sender<Result<(), BackendError>> {
    events.send(ProxyEvent::JoinObserved).await.unwrap();
    let _ = store
        .receive_with(
            move |a| *a == proxy_action(id, managed_proxy::Action::Event(ProxyEvent::JoinObserved)),
            move |s| {
                child(s, id).backend.target = backend::Target::Running;
            },
        )
        .await
        .unwrap();
    let _ = store
        .receive_with(
            move |a| *a == action(id, ServerAction::Backend(backend::Action::Reconcile)),
            move |s| {
                child(s, id).backend.phase = backend::Phase::Starting;
            },
        )
        .await
        .unwrap();
    let Call::Backend(operation, actual, reply) = calls.recv().await.unwrap() else {
        panic!("expected backend start")
    };
    assert_eq!((operation, actual.as_str()), ("start", id));
    reply
}

async fn complete_backend(
    store: &mut TestStore<State, Action>,
    calls: &Receiver<Call>,
    id: &'static str,
    reply: Sender<Result<(), BackendError>>,
) {
    reply.send(Ok(())).await.unwrap();
    let _ = store
        .receive_with(
            move |a| {
                *a == action(
                    id,
                    ServerAction::Backend(backend::Action::StartFinished(Ok(()))),
                )
            },
            move |s| {
                child(s, id).backend.phase = backend::Phase::Running;
                child(s, id).polling.active = true;
            },
        )
        .await
        .unwrap();
    let _ = store
        .receive_with(
            move |a| *a == action(id, ServerAction::Polling(polling::Action::Tick)),
            move |s| {
                child(s, id).polling.querying = true;
            },
        )
        .await
        .unwrap();
    let _ = store
        .receive(move |a| *a == action(id, ServerAction::Backend(backend::Action::Started)))
        .await
        .unwrap();
    query_result(store, calls, id, 7).await;
}

async fn stop_backend(
    store: &mut TestStore<State, Action>,
    calls: &Receiver<Call>,
    id: &'static str,
) {
    let _ = store
        .send_with(
            action(id, ServerAction::Backend(backend::Action::RequestStop)),
            move |s| {
                child(s, id).backend.target = backend::Target::Stopped;
                child(s, id).polling.active = false;
                child(s, id).polling.querying = false;
            },
        )
        .await
        .unwrap();
    let _ = store
        .receive_with(
            move |a| *a == action(id, ServerAction::Backend(backend::Action::Reconcile)),
            move |s| {
                child(s, id).backend.phase = backend::Phase::Stopping;
            },
        )
        .await
        .unwrap();
    let Call::Backend(operation, actual, reply) = calls.recv().await.unwrap() else {
        panic!("expected backend stop")
    };
    assert_eq!((operation, actual.as_str()), ("stop", id));
    reply.send(Ok(())).await.unwrap();
    let _ = store
        .receive_with(
            move |a| {
                *a == action(
                    id,
                    ServerAction::Backend(backend::Action::StopFinished(Ok(()))),
                )
            },
            move |s| {
                child(s, id).backend.phase = backend::Phase::Stopped;
                child(s, id).polling.player_count = 0;
            },
        )
        .await
        .unwrap();
    let _ = store
        .receive(move |a| *a == action(id, ServerAction::Backend(backend::Action::Stopped)))
        .await
        .unwrap();
}

// A fixed reducer control would send beta's operations to alpha and fail this trace.
#[test]
fn two_owners_route_events_to_their_composed_backend_and_polling() {
    check(async {
        let (mut store, calls, mut hosts) = setup();
        let alpha = start(&mut store, &calls, &mut hosts[0], "alpha").await;
        let beta = start(&mut store, &calls, &mut hosts[1], "beta").await;
        wake_backend(&mut store, &calls, "alpha", &alpha).await;
        wake_backend(&mut store, &calls, "beta", &beta).await;
        let _ = store.send(Action::Remove("alpha".into())).await.unwrap();
        let (_, reply) = stop_request(&mut store, &calls, &mut hosts[0], "alpha").await;
        stopped(&mut store, &mut hosts[0], "alpha", reply).await;
        assert!(alpha.is_closed());
        assert!(!beta.is_closed());
        // Proxy cleanup is insufficient while this server's backend remains live.
        let _ = store.send(Action::Remove("alpha".into())).await.unwrap();
        stop_backend(&mut store, &calls, "alpha").await;
        store.advance(Duration::from_secs(5)).await.unwrap();
        let _ = store
            .receive_with(
                move |a| *a == action("beta", ServerAction::Polling(polling::Action::Tick)),
                move |s| {
                    child(s, "beta").polling.querying = true;
                },
            )
            .await
            .unwrap();
        query_result(&mut store, &calls, "beta", 11).await;
        stop_backend(&mut store, &calls, "beta").await;
        let (_, reply) = stop_request(&mut store, &calls, &mut hosts[1], "beta").await;
        stopped(&mut store, &mut hosts[1], "beta", reply).await;
        store.finish().await.unwrap();
        drop(store);
        for host in hosts {
            assert!(!host.await.is_pending());
        }
        assert!(calls.is_empty());
    });
}

// Missing cancellation hand-off would cancel backend acquisition with proxy events.
#[test]
fn proxy_stop_preserves_pending_backend_start_and_live_sibling() {
    check(async {
        let (mut store, calls, mut hosts) = setup();
        let alpha = start(&mut store, &calls, &mut hosts[0], "alpha").await;
        let beta = start(&mut store, &calls, &mut hosts[1], "beta").await;
        let backend_reply = begin_backend(&mut store, &calls, "alpha", &alpha).await;
        let (_, reply) = stop_request(&mut store, &calls, &mut hosts[0], "alpha").await;
        stopped(&mut store, &mut hosts[0], "alpha", reply).await;
        assert!(alpha.is_closed());
        assert!(!beta.is_closed());
        assert!(!backend_reply.is_closed());
        complete_backend(&mut store, &calls, "alpha", backend_reply).await;
        stop_backend(&mut store, &calls, "alpha").await;
        let (_, reply) = stop_request(&mut store, &calls, &mut hosts[1], "beta").await;
        stopped(&mut store, &mut hosts[1], "beta", reply).await;
        store.finish().await.unwrap();
        drop(store);
        for host in hosts {
            assert!(!host.await.is_pending());
        }
        assert!(calls.is_empty());
    });
}

// A process-global ID lookup would redirect equal IDs to the other composition.
#[test]
fn equal_ids_in_separate_compositions_retain_distinct_owners() {
    check(async {
        let (mut first, first_calls, mut first_hosts) = setup();
        let (mut second, second_calls, mut second_hosts) = setup();
        let first_events = start(&mut first, &first_calls, &mut first_hosts[0], "alpha").await;
        assert!(second_calls.is_empty());
        let second_events = start(&mut second, &second_calls, &mut second_hosts[0], "alpha").await;
        assert!(first_calls.is_empty());
        let (_, reply) = stop_request(&mut first, &first_calls, &mut first_hosts[0], "alpha").await;
        stopped(&mut first, &mut first_hosts[0], "alpha", reply).await;
        assert!(first_events.is_closed());
        assert!(!second_events.is_closed());
        first.finish().await.unwrap();
        drop(first);
        for host in first_hosts {
            assert!(!host.await.is_pending());
        }
        let (_, reply) =
            stop_request(&mut second, &second_calls, &mut second_hosts[0], "alpha").await;
        stopped(&mut second, &mut second_hosts[0], "alpha", reply).await;
        assert!(second_events.is_closed());
        second.finish().await.unwrap();
        drop(second);
        for host in second_hosts {
            assert!(!host.await.is_pending());
        }
        assert!(first_calls.is_empty());
        assert!(second_calls.is_empty());
    });
}

// Premature removal or treating failed cleanup as Stopped loses the retained owner.
#[test]
fn failed_stop_blocks_removal_until_retry_then_restore_reuses_owner() {
    check(async {
        let (mut store, calls, mut hosts) = setup();
        let old_events = start(&mut store, &calls, &mut hosts[0], "alpha").await;
        let (_, reply) = stop_request(&mut store, &calls, &mut hosts[0], "alpha").await;
        let _ = store.send(Action::Remove("alpha".into())).await.unwrap();
        let _ = store.send(Action::Restore("alpha".into())).await.unwrap();
        let error = Error::StopFailed("still bound".into());
        reply.send(Err(error.clone())).await.unwrap();
        step(&mut hosts[0]).await;
        let _ = store
            .receive_with(
                move |a| *a == proxy_action("alpha", managed_proxy::Action::Failed(error.clone())),
                move |s| {
                    child(s, "alpha").proxy.phase = Phase::Failed;
                    child(s, "alpha").proxy.last_error = Some("StopFailed(\"still bound\")".into());
                },
            )
            .await
            .unwrap();
        let _ = store.send(Action::Remove("alpha".into())).await.unwrap();
        let _ = store
            .send(proxy_action("alpha", managed_proxy::Action::Start))
            .await
            .unwrap();
        let (_, reply) = stop_request(&mut store, &calls, &mut hosts[0], "alpha").await;
        stopped(&mut store, &mut hosts[0], "alpha", reply).await;
        let expected = ServerState {
            backend: backend::State {
                server_id: "alpha".into(),
                target: backend::Target::Stopped,
                phase: backend::Phase::Stopped,
                last_error: None,
            },
            polling: polling::State {
                server_id: "alpha".into(),
                active: false,
                querying: false,
                player_count: 0,
                failures: 0,
                last_error: None,
            },
            proxy: managed_proxy::State {
                phase: Phase::Stopped,
                last_error: None,
            },
        };
        let _ = store
            .send_with(Action::Remove("alpha".into()), move |s| {
                s.servers.remove(&"alpha".to_owned());
            })
            .await
            .unwrap();
        let _ = store.send(Action::Restore("unknown".into())).await.unwrap();
        let _ = store
            .send_with(Action::Restore("alpha".into()), move |s| {
                s.servers.insert("alpha".into(), expected);
            })
            .await
            .unwrap();
        assert!(old_events.try_send(ProxyEvent::JoinObserved).is_err());
        let new_events = start(&mut store, &calls, &mut hosts[0], "alpha").await;
        let (_, reply) = stop_request(&mut store, &calls, &mut hosts[0], "alpha").await;
        stopped(&mut store, &mut hosts[0], "alpha", reply).await;
        assert!(new_events.is_closed());
        store.finish().await.unwrap();
        drop(store);
        for host in hosts {
            assert!(!host.await.is_pending());
        }
        assert!(calls.is_empty());
    });
}

#[test]
fn closed_store_does_not_discard_external_cleanup_failure() {
    check(async {
        let (mut store, calls, mut hosts) = setup();
        let events = start(&mut store, &calls, &mut hosts[0], "alpha").await;
        let (task, reply) = stop_request(&mut store, &calls, &mut hosts[0], "alpha").await;
        task.cancel().await.unwrap();
        store.finish().await.unwrap();
        drop(store);
        assert!(events.is_closed());
        reply
            .send(Err(Error::StopFailed("retained".into())))
            .await
            .unwrap();
        let mut recovery = hosts.remove(0).await;
        assert!(recovery.is_pending());
        assert_eq!(
            recovery.last_error(),
            Some(&Error::StopFailed("retained".into()))
        );
        assert!(!hosts.remove(0).await.is_pending());
        let mut retry = Box::pin(recovery.retry());
        assert!(futures_util::poll!(retry.as_mut()).is_pending());
        let Call::Stop(id, reply) = calls.recv().await.unwrap() else {
            panic!("expected recovery stop")
        };
        assert_eq!(id, "alpha");
        reply.send(Ok(())).await.unwrap();
        retry.await.unwrap();
        assert!(!recovery.is_pending());
        assert!(calls.is_empty());
    });
}
