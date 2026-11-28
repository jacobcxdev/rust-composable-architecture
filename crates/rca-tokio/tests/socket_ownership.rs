//! Checks when a socket owned by a Store effect closes at its peer.

use std::{
    collections::VecDeque,
    io::{Read, Write},
    net::TcpStream,
    time::Duration,
};

use rca_core::{
    Effect, KeyedAction, KeyedActionPath, KeyedState, KeyedStatePath, Reduce, ReduceContext,
    Reducer, Store, for_each,
};
use rca_test::TestStore;
use rca_tokio::TokioRuntime;
use tokio::sync::{mpsc, oneshot};

#[path = "support/loopback.rs"]
mod loopback;

/// Bounds a broken handshake without choosing the order of successful events.
async fn bounded(test: impl Future<Output = ()>) {
    tokio::time::timeout(Duration::from_secs(15), test)
        .await
        .expect("socket test timed out");
}

/// Moves the socket into the effect before it waits for permission to write.
struct Session {
    /// Only the effect owns this end after it starts.
    socket: TcpStream,
    /// Confirms that cancellation will drop a running future.
    acquired: oneshot::Sender<()>,
    /// Holds acquisition open until the test allows normal traffic.
    gate: oneshot::Receiver<()>,
    /// Closing this channel requests a normal end rather than cancellation.
    writes: mpsc::UnboundedReceiver<u8>,
}

/// Drives the effect and observes bytes through the opposite socket.
struct Peer {
    /// Receives markers and the final EOF independently of Store state.
    socket: TcpStream,
    /// Waits for the effect to take ownership.
    acquired: oneshot::Receiver<()>,
    /// Allows traffic on this connection.
    gate: oneshot::Sender<()>,
    /// Requests markers from the effect without retaining its socket.
    writes: mpsc::UnboundedSender<u8>,
}

/// Gives the effect and the test different ends of the same connection.
fn session() -> (Session, Peer) {
    let (socket, peer) = loopback::pair();
    let (acquired, started) = oneshot::channel();
    let (release, gate) = oneshot::channel();
    let (send, writes) = mpsc::unbounded_channel();
    (
        Session {
            socket,
            acquired,
            gate,
            writes,
        },
        Peer {
            socket: peer,
            acquired: started,
            gate: release,
            writes: send,
        },
    )
}

/// Requires the selected effect to deliver a fresh byte at its own peer.
fn assert_live(peer: &mut TcpStream, writes: &mpsc::UnboundedSender<u8>, marker: u8) {
    writes.send(marker).unwrap();
    let mut byte = [0];
    peer.read_exact(&mut byte).unwrap();
    assert_eq!(byte, [marker]);
}

/// Starts an effect or reports that its input ended normally.
#[derive(Debug, Eq, PartialEq)]
enum Action {
    /// Transfers the next prepared session to the effect.
    Start,
    /// Records normal completion in state.
    Ended,
}

/// Each start consumes a fresh session, even when a key is reused.
struct Sessions(VecDeque<Session>);

impl Reducer for Sessions {
    type State = bool;
    type Action = Action;

    fn reduce(
        &mut self,
        state: &mut bool,
        action: &Action,
        _context: &mut ReduceContext<'_>,
    ) -> Effect<Action> {
        match action {
            Action::Start => {
                let Session {
                    mut socket,
                    acquired,
                    gate,
                    mut writes,
                } = self.0.pop_front().expect("one socket per start");
                Effect::run(move |sender| async move {
                    acquired.send(()).unwrap();
                    gate.await.unwrap();
                    while let Some(byte) = writes.recv().await {
                        socket.write_all(&[byte]).unwrap();
                    }
                    drop(socket);
                    let _ = sender.send(Action::Ended).await;
                })
            }
            Action::Ended => {
                *state = true;
                Effect::none()
            }
        }
    }
}

#[test]
fn dropping_one_handle_does_not_close_the_peer() {
    let (socket, mut peer) = loopback::pair();
    let mut retained = socket.try_clone().unwrap();
    drop(socket);
    retained.write_all(&[7]).unwrap();
    let mut byte = [0];
    peer.read_exact(&mut byte).unwrap();
    assert_eq!(byte, [7]);
    drop(retained);
    loopback::assert_eof(&mut peer);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelling_pending_acquisition_releases_its_socket() {
    bounded(async {
        let (session, mut peer) = session();
        let store = Store::new(
            false,
            Sessions([session].into()),
            TokioRuntime::current().unwrap(),
        );
        let task = store.send(Action::Start).await.unwrap();
        peer.acquired.await.unwrap();
        task.cancel();
        task.finish().await;
        loopback::assert_eof(&mut peer.socket);
        assert!(peer.gate.is_closed() && peer.writes.is_closed());
        assert!(!store.shutdown_into_state().await.unwrap());
    })
    .await;
}

#[tokio::test]
async fn normal_completion_closes_the_socket_before_the_ended_action_commits() {
    bounded(async {
        let (session, mut peer) = session();
        let mut store = TestStore::new(false, Sessions([session].into()));
        peer.gate.send(()).unwrap();
        drop(peer.writes);
        let _ = store.send(Action::Start).await.unwrap();
        store
            .receive(|action| {
                assert_eq!(*action, Action::Ended);
                false
            })
            .await
            .expect_err("the terminal action must still be pending");
        loopback::assert_eof(&mut peer.socket);
        let _ = store
            .receive_with(|a| *a == Action::Ended, |ended| *ended = true)
            .await
            .unwrap();
        store.finish().await.unwrap();
    })
    .await;
}

/// Changes membership or addresses a specific child.
enum Roster {
    /// Inserts a stopped child.
    Add(u8),
    /// Ends the named child's lifetime.
    Remove(u8),
    /// Routes a command within the current lifetime.
    Child(KeyedAction<u8, Action>),
}

/// Creates a fresh lifetime for each new child. Removal cancels only that child's work.
fn roster(sessions: Sessions) -> impl Reducer<State = KeyedState<u8, bool>, Action = Roster> {
    for_each(
        Reduce::new(|state: &mut KeyedState<u8, bool>, action: &Roster, _| {
            match action {
                Roster::Add(id) => {
                    state.insert(*id, false);
                }
                Roster::Remove(id) => {
                    state.remove(id);
                }
                Roster::Child(_) => {}
            }
            Effect::none()
        }),
        KeyedStatePath::new(|state| state, |state| state),
        KeyedActionPath::new(
            |action| match action {
                Roster::Child(child) => Some(child),
                _ => None,
            },
            Roster::Child,
        ),
        sessions,
    )
}

/// Adds a child and waits until its effect owns the socket.
async fn start(
    store: &Store<KeyedState<u8, bool>, Roster>,
    id: u8,
    peer: Peer,
) -> (TcpStream, mpsc::UnboundedSender<u8>) {
    let _ = store.send(Roster::Add(id)).await.unwrap();
    let _ = store
        .send(Roster::Child(KeyedAction::new(id, Action::Start)))
        .await
        .unwrap();
    peer.acquired.await.unwrap();
    peer.gate.send(()).unwrap();
    (peer.socket, peer.writes)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn removal_releases_only_the_old_lifetime_and_shutdown_releases_the_rest() {
    bounded(async {
        let (old, first) = session();
        let (other, sibling) = session();
        let (new, replacement) = session();
        let store = Store::new(
            KeyedState::default(),
            roster(Sessions([old, other, new].into())),
            TokioRuntime::current().unwrap(),
        );
        let (mut first, first_writes) = start(&store, 1, first).await;
        let (mut sibling, sibling_writes) = start(&store, 2, sibling).await;
        assert_live(&mut first, &first_writes, 1);
        let _ = store.send(Roster::Remove(1)).await.unwrap();
        loopback::assert_eof(&mut first);
        assert!(first_writes.is_closed());

        let (mut replacement, new_writes) = start(&store, 1, replacement).await;
        assert_live(&mut sibling, &sibling_writes, 2);
        assert_live(&mut replacement, &new_writes, 3);
        let _ = store.send(Roster::Remove(1)).await.unwrap();
        loopback::assert_eof(&mut replacement);
        assert_live(&mut sibling, &sibling_writes, 4);

        store.shutdown().await.unwrap();
        loopback::assert_eof(&mut sibling);
        assert!(sibling_writes.is_closed());
    })
    .await;
}
