//! Controlled local TCP cleanup through the managed owner, not event-stream Drop.
//!
//! `TestStore` drives RCA deterministically. Tests drive the owner separately.
//! Native I/O has separate deadlines because Tokio cannot interrupt blocking calls.
//! Connections are pre-acquired, so these tests do not cover asynchronous connection setup.

use std::{
    io::{ErrorKind, Read, Write},
    net::{Ipv4Addr, TcpListener, TcpStream},
    pin::Pin,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use async_channel::{Receiver, Sender};
use rca_core::KeyedAction;
use rca_example_mcmd_server::{
    managed_proxy::{
        Action, Client, Error, Events, ManagedProxy, Phase, Recovery, StartFuture, State,
        StopFuture, channel,
    },
    managed_servers,
    proxy::{Config, ProxyEvent},
};
use rca_test::TestStore;

/// Bounds stalled loopback operations, not the expected cleanup duration.
const IO_LIMIT: Duration = Duration::from_secs(2);

type Reply = Sender<Result<(), Error>>;

/// Holds an external owner polled separately from `TestStore`'s deterministic runtime.
type Host = Pin<Box<dyn Future<Output = Recovery>>>;

/// Keeps the native socket independent of the returned event stream.
struct SocketClient {
    id: &'static str,
    probe: [u8; 2],
    socket: Arc<Mutex<Option<TcpStream>>>,
    events: Mutex<Option<Receiver<ProxyEvent>>>,
    stops: Sender<Reply>,
}

impl Client for SocketClient {
    fn start(&self, config: Config) -> StartFuture {
        assert_eq!(config, config_for(self.id));
        let events = self.events.lock().unwrap().take().unwrap();
        Box::pin(async move { Ok(Box::pin(events) as Events) })
    }

    fn stop(&self, server_id: String) -> StopFuture {
        assert_eq!(server_id, self.id);
        let stops = self.stops.clone();
        let socket = Arc::clone(&self.socket);
        Box::pin(async move {
            let (reply, receive) = async_channel::bounded(1);
            stops.send(reply).await.unwrap();
            receive.recv().await.unwrap()?;
            // Success confirms native release, independently of event delivery.
            drop(socket.lock().unwrap().take());
            Ok(())
        })
    }
}

impl SocketClient {
    /// Proves both directions remain usable before confirmed cleanup.
    fn round_trip(&self, peer: &mut TcpStream) {
        let mut guard = self.socket.lock().unwrap();
        let socket = guard.as_mut().unwrap();
        peer.write_all(&[self.probe[0]]).unwrap();
        let mut byte = [0];
        socket.read_exact(&mut byte).unwrap();
        assert_eq!(byte, [self.probe[0]]);
        socket.write_all(&[self.probe[1]]).unwrap();
        peer.read_exact(&mut byte).unwrap();
        assert_eq!(byte, [self.probe[1]]);
    }
}

fn config() -> Config {
    config_for("alpha")
}

fn config_for(id: &'static str) -> Config {
    Config {
        server_id: id.into(),
        host: "127.0.0.1".into(),
        java_ports: vec![],
        bedrock_ports: vec![],
    }
}

fn fixture() -> (
    Arc<SocketClient>,
    TcpStream,
    Receiver<Reply>,
    Sender<ProxyEvent>,
) {
    fixture_for("alpha", [17, 29])
}

fn fixture_for(
    id: &'static str,
    probe: [u8; 2],
) -> (
    Arc<SocketClient>,
    TcpStream,
    Receiver<Reply>,
    Sender<ProxyEvent>,
) {
    // Pre-acquired transport isolates cleanup; this does not test async connect.
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let peer = TcpStream::connect_timeout(&listener.local_addr().unwrap(), IO_LIMIT).unwrap();
    listener.set_nonblocking(true).unwrap();
    let deadline = Instant::now() + IO_LIMIT;
    let socket = loop {
        match listener.accept() {
            Ok((socket, _)) => break socket,
            Err(error) if error.kind() == ErrorKind::WouldBlock => {
                assert!(Instant::now() < deadline, "loopback accept deadline");
                std::thread::sleep(Duration::from_millis(1));
            }
            Err(error) => panic!("loopback accept: {error}"),
        }
    };
    for stream in [&peer, &socket] {
        stream.set_nonblocking(false).unwrap();
        stream.set_read_timeout(Some(IO_LIMIT)).unwrap();
        stream.set_write_timeout(Some(IO_LIMIT)).unwrap();
    }
    let (stops, receive) = async_channel::bounded(1);
    let (events, stream) = async_channel::bounded(1);
    (
        Arc::new(SocketClient {
            id,
            probe,
            socket: Arc::new(Mutex::new(Some(socket))),
            events: Mutex::new(Some(stream)),
            stops,
        }),
        peer,
        receive,
        events,
    )
}

fn eof(peer: &mut TcpStream) {
    // A nonempty read must return EOF, not an acknowledgement or a timeout.
    assert_eq!(peer.read(&mut [0_u8; 1]).expect("native peer EOF"), 0);
}

/// Bounds cooperative waits while native I/O uses its own deadlines.
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

async fn step(host: &mut Host) {
    assert!(futures_util::poll!(host.as_mut()).is_pending());
}

async fn start(store: &mut TestStore<State, Action>, host: &mut Host) {
    let _ = store
        .send_with(Action::Start, |s| s.phase = Phase::Starting)
        .await
        .unwrap();
    step(host).await;
    let _ = store
        .receive_with(|a| *a == Action::Ready, |s| s.phase = Phase::Listening)
        .await
        .unwrap();
}

#[test]
fn pending_and_failed_stop_retain_socket_until_explicit_retry() {
    check(async {
        let (client, mut peer, stops, events) = fixture();
        let (control, owner) = channel(config(), Arc::<SocketClient>::clone(&client));
        let mut host: Host = Box::pin(owner.run());
        let mut store = TestStore::new(State::default(), ManagedProxy::new(control));
        start(&mut store, &mut host).await;
        let _ = store
            .send_with(Action::Stop, |s| s.phase = Phase::Stopping)
            .await
            .unwrap();
        step(&mut host).await;
        let reply = stops.recv().await.unwrap();
        assert!(events.is_closed());
        store
            .assert_state(|s| assert_eq!(s.phase, Phase::Stopping))
            .await
            .unwrap();
        client.round_trip(&mut peer);
        let error = Error::StopFailed("socket busy".into());
        reply.send(Err(error.clone())).await.unwrap();
        step(&mut host).await;
        let expected = error.clone();
        let _ = store
            .receive_with(
                move |a| *a == Action::Failed(expected.clone()),
                move |s| {
                    s.phase = Phase::Failed;
                    s.last_error = Some(error.to_string());
                },
            )
            .await
            .unwrap();
        client.round_trip(&mut peer);
        let _ = store
            .send_with(Action::Stop, |s| {
                s.phase = Phase::Stopping;
                s.last_error = None;
            })
            .await
            .unwrap();
        step(&mut host).await;
        stops.recv().await.unwrap().send(Ok(())).await.unwrap();
        step(&mut host).await;
        eof(&mut peer);
        let _ = store
            .receive_with(|a| *a == Action::Stopped, |s| s.phase = Phase::Stopped)
            .await
            .unwrap();
        store.finish().await.unwrap();
        drop(store);
        let recovery = host.await;
        assert!(!recovery.is_pending());
        assert_eq!(recovery.last_error(), None);
        assert!(stops.is_empty());
        drop(client);
    });
}

/// Routes each identity to its own socket and stop gate.
struct SocketClients([Arc<SocketClient>; 2]);

impl SocketClients {
    fn select(&self, id: &str) -> &SocketClient {
        self.0.iter().find(|client| client.id == id).unwrap()
    }
}

impl Client for SocketClients {
    fn start(&self, config: Config) -> StartFuture {
        self.select(&config.server_id).start(config)
    }

    fn stop(&self, server_id: String) -> StopFuture {
        self.select(&server_id).stop(server_id)
    }
}

fn keyed(id: &'static str, action: Action) -> managed_servers::Action {
    managed_servers::Action::Server(KeyedAction::new(
        id.into(),
        managed_servers::ServerAction::Proxy(action),
    ))
}

fn proxy_state<'a>(state: &'a mut managed_servers::State, id: &'static str) -> &'a mut State {
    &mut state.servers.get_mut(&id.to_owned()).unwrap().proxy
}

async fn keyed_start(
    store: &mut TestStore<managed_servers::State, managed_servers::Action>,
    host: &mut Host,
    id: &'static str,
) {
    let _ = store
        .send_with(keyed(id, Action::Start), move |s| {
            proxy_state(s, id).phase = Phase::Starting;
        })
        .await
        .unwrap();
    step(host).await;
    let _ = store
        .receive_with(
            move |a| *a == keyed(id, Action::Ready),
            move |s| {
                proxy_state(s, id).phase = Phase::Listening;
            },
        )
        .await
        .unwrap();
}

async fn keyed_stop(
    store: &mut TestStore<managed_servers::State, managed_servers::Action>,
    host: &mut Host,
    stops: &Receiver<Reply>,
    id: &'static str,
) -> Reply {
    let _ = store
        .send_with(keyed(id, Action::Stop), move |s| {
            proxy_state(s, id).phase = Phase::Stopping;
            proxy_state(s, id).last_error = None;
        })
        .await
        .unwrap();
    step(host).await;
    stops.recv().await.unwrap()
}

async fn keyed_stopped(
    store: &mut TestStore<managed_servers::State, managed_servers::Action>,
    host: &mut Host,
    peer: &mut TcpStream,
    reply: Reply,
    id: &'static str,
) {
    reply.send(Ok(())).await.unwrap();
    step(host).await;
    // Independently observe release before TestStore commits the keyed result.
    eof(peer);
    let _ = store
        .receive_with(
            move |a| *a == keyed(id, Action::Stopped),
            move |s| {
                proxy_state(s, id).phase = Phase::Stopped;
            },
        )
        .await
        .unwrap();
}

#[test]
fn keyed_failed_cleanup_retains_membership_and_releases_only_its_socket() {
    check(async {
        let (alpha, mut alpha_peer, alpha_stops, alpha_events) = fixture_for("alpha", [17, 29]);
        let (beta, mut beta_peer, beta_stops, beta_events) = fixture_for("beta", [43, 61]);
        let clients = Arc::new(SocketClients([Arc::clone(&alpha), Arc::clone(&beta)]));
        let (state, reducer, owners) = managed_servers::compose(
            [config_for("alpha"), config_for("beta")],
            Arc::<SocketClients>::clone(&clients),
        )
        .unwrap();
        let mut hosts = owners
            .into_iter()
            .map(|owner| Box::pin(owner.run()) as Host);
        let mut alpha_host = hosts.next().unwrap();
        let mut beta_host = hosts.next().unwrap();
        assert!(hosts.next().is_none());
        drop(hosts);
        let mut store = TestStore::new(state, reducer);
        keyed_start(&mut store, &mut alpha_host, "alpha").await;
        keyed_start(&mut store, &mut beta_host, "beta").await;

        let reply = keyed_stop(&mut store, &mut alpha_host, &alpha_stops, "alpha").await;
        assert!(alpha_events.is_closed());
        assert!(!beta_events.is_closed());
        // Exhaustive send asserts neither membership nor either child's state changes.
        let _ = store
            .send(managed_servers::Action::Remove("alpha".into()))
            .await
            .unwrap();
        alpha.round_trip(&mut alpha_peer);
        beta.round_trip(&mut beta_peer);
        let error = Error::StopFailed("alpha socket busy".into());
        reply.send(Err(error.clone())).await.unwrap();
        step(&mut alpha_host).await;
        let _ = store
            .receive_with(
                move |a| *a == keyed("alpha", Action::Failed(error.clone())),
                |s| {
                    proxy_state(s, "alpha").phase = Phase::Failed;
                    proxy_state(s, "alpha").last_error =
                        Some("StopFailed(\"alpha socket busy\")".into());
                },
            )
            .await
            .unwrap();
        let _ = store
            .send(managed_servers::Action::Remove("alpha".into()))
            .await
            .unwrap();
        alpha.round_trip(&mut alpha_peer);
        beta.round_trip(&mut beta_peer);

        let reply = keyed_stop(&mut store, &mut alpha_host, &alpha_stops, "alpha").await;
        keyed_stopped(&mut store, &mut alpha_host, &mut alpha_peer, reply, "alpha").await;
        let _ = store
            .send_with(managed_servers::Action::Remove("alpha".into()), |s| {
                s.servers.remove(&"alpha".to_owned());
            })
            .await
            .unwrap();
        assert!(!beta_events.is_closed());
        assert!(beta_stops.is_empty());
        beta.round_trip(&mut beta_peer);

        let reply = keyed_stop(&mut store, &mut beta_host, &beta_stops, "beta").await;
        assert!(beta_events.is_closed());
        keyed_stopped(&mut store, &mut beta_host, &mut beta_peer, reply, "beta").await;
        store.finish().await.unwrap();
        drop(store);
        for host in [alpha_host, beta_host] {
            let recovery = host.await;
            assert!(!recovery.is_pending());
            assert_eq!(recovery.last_error(), None);
        }
        assert!(alpha_stops.is_empty());
        assert!(beta_stops.is_empty());
        // Explicit retention prevents host destruction masking a missing native release.
        drop((clients, alpha, beta));
    });
}

#[test]
fn cancelled_reply_and_closed_store_do_not_cancel_native_cleanup() {
    check(async {
        let (client, mut peer, stops, events) = fixture();
        let (control, owner) = channel(config(), Arc::<SocketClient>::clone(&client));
        let mut host: Host = Box::pin(owner.run());
        let mut store = TestStore::new(State::default(), ManagedProxy::new(control));
        start(&mut store, &mut host).await;
        let stop = store
            .send_with(Action::Stop, |s| s.phase = Phase::Stopping)
            .await
            .unwrap();
        step(&mut host).await;
        let reply = stops.recv().await.unwrap();
        stop.cancel().await.unwrap();
        store.finish().await.unwrap();
        drop(store);
        assert!(events.is_closed());
        step(&mut host).await;
        client.round_trip(&mut peer);
        reply.send(Ok(())).await.unwrap();
        let recovery = host.await;
        assert!(!recovery.is_pending());
        assert_eq!(recovery.last_error(), None);
        eof(&mut peer);
        assert!(stops.is_empty());
        drop(client);
    });
}

#[test]
fn failed_cleanup_after_store_closure_retains_recovery_and_native_socket() {
    check(async {
        let (client, mut peer, stops, events) = fixture();
        let (control, owner) = channel(config(), Arc::<SocketClient>::clone(&client));
        let mut host: Host = Box::pin(owner.run());
        let mut store = TestStore::new(State::default(), ManagedProxy::new(control));
        start(&mut store, &mut host).await;
        let stop = store
            .send_with(Action::Stop, |s| s.phase = Phase::Stopping)
            .await
            .unwrap();
        step(&mut host).await;
        let reply = stops.recv().await.unwrap();
        stop.cancel().await.unwrap();
        store.finish().await.unwrap();
        drop(store);
        assert!(events.is_closed());
        let error = Error::StopFailed("still owns socket".into());
        reply.send(Err(error.clone())).await.unwrap();
        let mut recovery = host.await;
        assert!(recovery.is_pending());
        assert_eq!(recovery.last_error(), Some(&error));
        client.round_trip(&mut peer);
        let mut retry = Box::pin(recovery.retry());
        assert!(futures_util::poll!(retry.as_mut()).is_pending());
        client.round_trip(&mut peer);
        stops.recv().await.unwrap().send(Ok(())).await.unwrap();
        retry.as_mut().await.unwrap();
        drop(retry);
        eof(&mut peer);
        assert!(!recovery.is_pending());
        assert_eq!(recovery.last_error(), None);
        assert!(stops.is_empty());
        drop(client);
    });
}
