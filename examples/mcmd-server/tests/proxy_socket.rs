//! Tests local socket ownership, not a production proxy transport or remote cleanup.

use std::{
    collections::VecDeque,
    io::{ErrorKind, Read, Write},
    net::{TcpListener, TcpStream},
    pin::Pin,
    sync::{Arc, Mutex, mpsc},
    task::{Context, Poll, Wake, Waker},
    time::Duration,
};

use futures_core::Stream;
use futures_executor::block_on;
use rca_core::{KeyedAction, MonotonicInstant, Runtime, RuntimeDriver, RuntimeFuture, Store};
use rca_example_mcmd_server::{proxy, server, servers};
use rca_test::TestStore;

const LIMIT: Duration = Duration::from_secs(3);

/// Signals release only after dropping this fixture's owned native socket handle.
struct SocketOwner {
    socket: Option<TcpStream>,
    released: mpsc::Sender<()>,
}

impl Drop for SocketOwner {
    fn drop(&mut self) {
        drop(self.socket.take());
        let _ = self.released.send(());
    }
}

/// Checks physical EOF independently of the owner's release acknowledgement.
struct Peer {
    socket: TcpStream,
    released: mpsc::Receiver<()>,
}

fn pair() -> (SocketOwner, Peer) {
    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
    let peer = TcpStream::connect_timeout(&listener.local_addr().unwrap(), LIMIT).unwrap();
    let (socket, _) = listener.accept().unwrap();
    drop(listener);
    peer.set_read_timeout(Some(LIMIT)).unwrap();
    socket.set_write_timeout(Some(LIMIT)).unwrap();
    let (released, receive) = mpsc::channel();
    (
        SocketOwner {
            socket: Some(socket),
            released,
        },
        Peer {
            socket: peer,
            released: receive,
        },
    )
}

impl Peer {
    /// Requires both the drop acknowledgement and native EOF; either alone is insufficient.
    fn closed(&mut self) -> Result<(), &'static str> {
        self.released
            .recv_timeout(LIMIT)
            .map_err(|_error| "release acknowledgement missing")?;
        self.eof()
    }

    fn eof(&mut self) -> Result<(), &'static str> {
        let mut byte = [0_u8; 1];
        match self.socket.read(&mut byte) {
            Ok(0) => Ok(()),
            Ok(_) => Err("unexpected socket data"),
            Err(error) if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                Err("peer remained open")
            }
            Err(_) => Err("socket read failed"),
        }
    }

    fn pong(&mut self) {
        let mut byte = [0_u8; 1];
        self.socket.read_exact(&mut byte).unwrap();
        assert_eq!(byte, [7]);
    }
}

#[test]
fn retained_handle_is_not_socket_release() {
    let (owner, mut peer) = pair();
    let retained = owner.socket.as_ref().unwrap().try_clone().unwrap();
    peer.socket
        .set_read_timeout(Some(Duration::from_millis(100)))
        .unwrap();
    drop(owner);
    assert_eq!(peer.closed(), Err("peer remained open"));
    drop(retained);
    peer.socket.set_read_timeout(Some(LIMIT)).unwrap();
    assert_eq!(peer.eof(), Ok(()));
}

/// Owns the socket while the reducer polls command-driven probe traffic.
struct Session {
    owner: SocketOwner,
    commands: Pin<Box<async_channel::Receiver<()>>>,
    listening: Option<mpsc::Sender<()>>,
}

impl Stream for Session {
    type Item = proxy::ProxyEvent;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if let Some(listening) = self.listening.take() {
            let _ = listening.send(());
        }
        loop {
            match self.commands.as_mut().poll_next(cx) {
                Poll::Ready(Some(())) => {
                    self.owner.socket.as_mut().unwrap().write_all(&[7]).unwrap();
                }
                Poll::Ready(None) => return Poll::Ready(None),
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

/// Retains a partially acquired resource until the test opens its completion gate.
struct Acquisition {
    session: Session,
    gate: async_channel::Receiver<()>,
    acquired: mpsc::Sender<()>,
}

#[derive(Default)]
struct Client(Mutex<VecDeque<Acquisition>>);

struct Control {
    peer: Peer,
    gate: async_channel::Sender<()>,
    commands: async_channel::Sender<()>,
    acquired: mpsc::Receiver<()>,
    listening: mpsc::Receiver<()>,
}

impl Client {
    fn queue(&self) -> Control {
        let (owner, peer) = pair();
        let (gate, receive_gate) = async_channel::bounded(1);
        let (commands, receive_commands) = async_channel::bounded(1);
        let (acquired, receive_acquired) = mpsc::channel();
        let (listening, receive_listening) = mpsc::channel();
        self.0.lock().unwrap().push_back(Acquisition {
            session: Session {
                owner,
                commands: Box::pin(receive_commands),
                listening: Some(listening),
            },
            gate: receive_gate,
            acquired,
        });
        Control {
            peer,
            gate,
            commands,
            acquired: receive_acquired,
            listening: receive_listening,
        }
    }
}

impl proxy::ProxyClient for Client {
    fn acquire(&self, _: proxy::Config) -> proxy::AcquireFuture {
        let acquisition = self.0.lock().unwrap().pop_front().unwrap();
        Box::pin(async move {
            acquisition.acquired.send(()).unwrap();
            acquisition.gate.recv().await.unwrap();
            Ok(Box::pin(acquisition.session) as proxy::ProxySession)
        })
    }
}

fn config(id: &str) -> proxy::Config {
    proxy::Config {
        server_id: id.into(),
        host: "127.0.0.1".into(),
        java_ports: vec![],
        bedrock_ports: vec![],
    }
}

fn state(id: &str) -> proxy::State {
    proxy::State {
        config: config(id),
        phase: proxy::Phase::Stopped,
        last_error: None,
    }
}

fn store(client: &Arc<Client>) -> TestStore<proxy::State, proxy::Action> {
    TestStore::builder(state("one"), proxy::Proxy)
        .with_dependency::<proxy::ProxyKey>(Arc::clone(client) as Arc<dyn proxy::ProxyClient>)
        .build()
}

async fn start(store: &mut TestStore<proxy::State, proxy::Action>, control: &Control) {
    drop(
        store
            .send_with(proxy::Action::Start, |s| s.phase = proxy::Phase::Starting)
            .await
            .unwrap(),
    );
    store
        .assert_state(|s| assert_eq!(s.phase, proxy::Phase::Starting))
        .await
        .unwrap();
    control.acquired.recv_timeout(LIMIT).unwrap();
}

async fn ready(store: &mut TestStore<proxy::State, proxy::Action>, control: &Control) {
    control.gate.try_send(()).unwrap();
    drop(
        store
            .receive_with(
                |a| *a == proxy::Action::Ready,
                |s| s.phase = proxy::Phase::Listening,
            )
            .await
            .unwrap(),
    );
    store
        .assert_state(|s| assert_eq!(s.phase, proxy::Phase::Listening))
        .await
        .unwrap();
    control.listening.recv_timeout(LIMIT).unwrap();
}

#[test]
fn cancelling_gated_acquisition_closes_owned_socket() {
    block_on(async {
        let client = Arc::new(Client::default());
        let mut control = client.queue();
        let mut store = store(&client);
        start(&mut store, &control).await;
        drop(
            store
                .send_with(proxy::Action::Stop, |s| s.phase = proxy::Phase::Stopped)
                .await
                .unwrap(),
        );
        store
            .assert_state(|s| assert_eq!(s.phase, proxy::Phase::Stopped))
            .await
            .unwrap();
        assert_eq!(control.peer.closed(), Ok(()));
        assert!(control.gate.is_closed());
        assert!(control.commands.is_closed());
        store.finish().await.unwrap();
    });
}

#[test]
fn stream_eof_closes_socket_before_ended_commit() {
    block_on(async {
        let client = Arc::new(Client::default());
        let mut control = client.queue();
        let mut store = store(&client);
        start(&mut store, &control).await;
        ready(&mut store, &control).await;
        drop(control.commands);
        // Rejecting the matcher leaves Ended at the assertion gate, before reduction.
        store
            .receive(|a| {
                assert_eq!(*a, proxy::Action::Ended);
                false
            })
            .await
            .expect_err("Ended remains gated");
        assert_eq!(control.peer.closed(), Ok(()));
        drop(
            store
                .receive_with(
                    |a| *a == proxy::Action::Ended,
                    |s| s.phase = proxy::Phase::Stopped,
                )
                .await
                .unwrap(),
        );
        store.finish().await.unwrap();
    });
}

fn keyed(id: &str, action: proxy::Action) -> servers::Action {
    servers::Action::Server(KeyedAction::new(id.into(), server::Action::Proxy(action)))
}

async fn keyed_start(
    store: &mut TestStore<servers::State, servers::Action>,
    id: &'static str,
    control: &Control,
) {
    drop(
        store
            .send_with(servers::Action::Add(config(id)), move |s| {
                s.servers.insert(id.into(), server::State::new(config(id)));
            })
            .await
            .unwrap(),
    );
    drop(
        store
            .send_with(keyed(id, proxy::Action::Start), move |s| {
                s.servers.get_mut(&id.into()).unwrap().proxy.phase = proxy::Phase::Starting;
            })
            .await
            .unwrap(),
    );
    store
        .assert_state(move |s| {
            assert_eq!(
                s.servers.get(&id.into()).unwrap().proxy.phase,
                proxy::Phase::Starting
            );
        })
        .await
        .unwrap();
    control.acquired.recv_timeout(LIMIT).unwrap();
    control.gate.try_send(()).unwrap();
    drop(
        store
            .receive_with(
                move |a| *a == keyed(id, proxy::Action::Ready),
                move |s| {
                    s.servers.get_mut(&id.into()).unwrap().proxy.phase = proxy::Phase::Listening;
                },
            )
            .await
            .unwrap(),
    );
    store
        .assert_state(move |s| {
            assert_eq!(
                s.servers.get(&id.into()).unwrap().proxy.phase,
                proxy::Phase::Listening
            );
        })
        .await
        .unwrap();
    control.listening.recv_timeout(LIMIT).unwrap();
}

async fn forget(store: &mut TestStore<servers::State, servers::Action>, id: &'static str) {
    drop(
        store
            .send_with(servers::Action::Forget(id.into()), move |s| {
                s.servers.remove(&id.into());
            })
            .await
            .unwrap(),
    );
    store
        .assert_state(move |s| assert!(!s.servers.contains_id(&id.into())))
        .await
        .unwrap();
}

#[test]
fn keyed_removal_closes_only_old_socket_and_replacement_is_live() {
    block_on(async {
        let client = Arc::new(Client::default());
        let mut first = client.queue();
        let mut sibling = client.queue();
        let mut replacement = client.queue();
        let mut store = TestStore::builder(servers::State::default(), servers::reducer())
            .with_dependency::<proxy::ProxyKey>(Arc::clone(&client) as Arc<dyn proxy::ProxyClient>)
            .build();
        keyed_start(&mut store, "one", &first).await;
        keyed_start(&mut store, "two", &sibling).await;
        forget(&mut store, "one").await;
        assert_eq!(first.peer.closed(), Ok(()));
        keyed_start(&mut store, "one", &replacement).await;
        sibling.commands.try_send(()).unwrap();
        replacement.commands.try_send(()).unwrap();
        store
            .assert_state(|s| assert_eq!(s.servers.len(), 2))
            .await
            .unwrap();
        sibling.peer.pong();
        replacement.peer.pong();
        forget(&mut store, "one").await;
        assert_eq!(replacement.peer.closed(), Ok(()));
        forget(&mut store, "two").await;
        assert_eq!(sibling.peer.closed(), Ok(()));
        store.finish().await.unwrap();
    });
}

/// Runs the public Store on a separately owned Tokio host for the shutdown proof.
#[derive(Debug)]
struct Driver {
    handle: tokio::runtime::Handle,
    origin: tokio::time::Instant,
}

impl RuntimeDriver for Driver {
    fn spawn(&self, task: RuntimeFuture) {
        drop(self.handle.spawn(task));
    }

    fn now(&self) -> MonotonicInstant {
        MonotonicInstant::from_elapsed(self.origin.elapsed())
    }

    fn sleep_until(&self, deadline: MonotonicInstant) -> RuntimeFuture {
        let deadline = self.origin + deadline.elapsed();
        Box::pin(async move { tokio::time::sleep_until(deadline).await })
    }

    fn yield_now(&self) -> RuntimeFuture {
        Box::pin(tokio::task::yield_now())
    }
}

/// Stops and joins the native host on both explicit completion and assertion unwind.
struct Host {
    stop: Option<mpsc::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Host {
    fn new() -> (Self, Runtime) {
        let (send, receive) = mpsc::channel();
        let (stop, stopped) = mpsc::channel();
        let thread = std::thread::spawn(move || {
            let host = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .enable_all()
                .build()
                .unwrap();
            send.send(Runtime::new(Driver {
                handle: host.handle().clone(),
                origin: tokio::time::Instant::now(),
            }))
            .unwrap();
            let _ = stopped.recv();
            drop(host);
        });
        let guard = Self {
            stop: Some(stop),
            thread: Some(thread),
        };
        (guard, receive.recv_timeout(LIMIT).unwrap())
    }

    fn join(mut self) {
        drop(self.stop.take());
        self.thread.take().unwrap().join().unwrap();
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        drop(self.stop.take());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Unparks the test thread when a Store operation can make progress.
struct ThreadWake(std::thread::Thread);

impl Wake for ThreadWake {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }
}

/// Bridges a native-runtime future to this test thread within a fixed failure bound.
fn wait<F: Future>(future: F) -> F::Output {
    let mut future = std::pin::pin!(future);
    let waker = Waker::from(Arc::new(ThreadWake(std::thread::current())));
    let deadline = std::time::Instant::now() + LIMIT;
    loop {
        if let Poll::Ready(value) = future.as_mut().poll(&mut Context::from_waker(&waker)) {
            return value;
        }
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        assert!(!remaining.is_zero(), "native operation deadline expired");
        std::thread::park_timeout(remaining);
    }
}

#[test]
fn store_shutdown_releases_live_socket_before_owned_host_join() {
    let client = Arc::new(Client::default());
    let mut control = client.queue();
    let (host, runtime) = Host::new();
    let store = Store::builder(state("one"), proxy::Proxy, runtime)
        .with_dependency::<proxy::ProxyKey>(Arc::clone(&client) as Arc<dyn proxy::ProxyClient>)
        .build()
        .unwrap();
    drop(wait(store.send(proxy::Action::Start)).unwrap());
    control.acquired.recv_timeout(LIMIT).unwrap();
    control.gate.try_send(()).unwrap();
    control.listening.recv_timeout(LIMIT).unwrap();
    control.commands.try_send(()).unwrap();
    control.peer.pong();
    wait(store.shutdown()).unwrap();
    assert_eq!(control.peer.closed(), Ok(()));
    drop(store);
    host.join();
}
