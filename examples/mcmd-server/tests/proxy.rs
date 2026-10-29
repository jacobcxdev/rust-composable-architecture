//! Checks local lease ownership through the real reducer and deterministic `TestStore`.
//!
//! Reply channels gate acquisition; the resulting stream owns the same lease.
//! Native socket release has separate coverage in `proxy_socket`.

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
use rca_example_mcmd_server::proxy::{
    AcquireFuture, Action, Config, Phase, Proxy, ProxyClient, ProxyError, ProxyEvent, ProxyKey,
    ProxySession, State,
};
use rca_test::TestStore;

type Acquisition = Result<(), ProxyError>;
type LeaseLog = Arc<Mutex<Vec<(&'static str, usize)>>>;

/// Records release for both cancelled acquisition and completed stream ownership.
struct Lease {
    id: usize,
    log: LeaseLog,
}

impl Drop for Lease {
    fn drop(&mut self) {
        self.log.lock().unwrap().push(("released", self.id));
    }
}

/// Owns the acquired lease for exactly as long as event delivery remains live.
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
    result: Receiver<Acquisition>,
    events: Receiver<ProxyEvent>,
}

#[derive(Default)]
struct Client {
    calls: Mutex<Vec<Config>>,
    pending: Mutex<VecDeque<Pending>>,
    log: LeaseLog,
}

impl Client {
    fn queue(&self) -> (Sender<Acquisition>, Sender<ProxyEvent>) {
        let (result_tx, result) = async_channel::bounded(1);
        let (event_tx, events) = async_channel::bounded(4);
        self.pending
            .lock()
            .unwrap()
            .push_back(Pending { result, events });
        (result_tx, event_tx)
    }

    fn assert_log(&self, expected: &[(&str, usize)]) {
        let actual = self.log.lock().unwrap().clone();
        assert_eq!(actual, expected);
    }

    fn assert_calls(&self, expected: &[Config]) {
        assert_eq!(*self.calls.lock().unwrap(), expected);
    }
}

impl ProxyClient for Client {
    fn acquire(&self, config: Config) -> AcquireFuture {
        let mut calls = self.calls.lock().unwrap();
        calls.push(config);
        let id = calls.len();
        drop(calls);
        let pending = self
            .pending
            .lock()
            .unwrap()
            .pop_front()
            .expect("unexpected acquisition");
        self.log.lock().unwrap().push(("acquired", id));
        let lease = Lease {
            id,
            log: Arc::clone(&self.log),
        };
        Box::pin(async move {
            pending
                .result
                .recv()
                .await
                .expect("acquisition response retained")?;
            Ok(Box::pin(Session {
                events: Box::pin(pending.events),
                _lease: lease,
            }) as ProxySession)
        })
    }
}

fn config() -> Config {
    Config {
        server_id: "s1".into(),
        host: "127.0.0.1".into(),
        java_ports: vec![25565],
        bedrock_ports: vec![19132],
    }
}

fn store(client: &Arc<Client>) -> TestStore<State, Action> {
    TestStore::builder(
        State {
            config: config(),
            phase: Phase::Stopped,
            last_error: None,
        },
        Proxy,
    )
    .with_dependency::<ProxyKey>(Arc::clone(client) as Arc<dyn ProxyClient>)
    .build()
}

async fn start(store: &mut TestStore<State, Action>) {
    let _ = store
        .send_with(Action::Start, |s| {
            s.phase = Phase::Starting;
            s.last_error = None;
        })
        .await
        .unwrap();
    store
        .assert_state(|s| assert_eq!(s.phase, Phase::Starting))
        .await
        .unwrap();
}

async fn ready(store: &mut TestStore<State, Action>) {
    let _ = store
        .receive_with(|a| *a == Action::Ready, |s| s.phase = Phase::Listening)
        .await
        .unwrap();
}

async fn stop(store: &mut TestStore<State, Action>) {
    let _ = store
        .send_with(Action::Stop, |s| {
            s.phase = Phase::Stopped;
            s.last_error = None;
        })
        .await
        .unwrap();
    store
        .assert_state(|s| assert_eq!(s.phase, Phase::Stopped))
        .await
        .unwrap();
}

#[test]
fn failed_lease_log_assertion_preserves_live_session_cleanup() {
    block_on(async {
        let client = Arc::new(Client::default());
        let (acquired, events) = client.queue();
        let mut store = store(&client);
        start(&mut store).await;
        acquired.try_send(Ok(())).unwrap();
        ready(&mut store).await;
        assert!(
            !events.is_closed(),
            "the assertion must run with a live lease"
        );

        let mismatch = catch_unwind(AssertUnwindSafe(|| client.assert_log(&[])));
        let poisoned = client.log.is_poisoned();
        // Keep the failing control safe: Lease::drop still uses this mutex.
        // Clearing it here permits cleanup without hiding whether the helper poisoned it.
        client.log.clear_poison();

        stop(&mut store).await;
        assert!(events.is_closed());
        client.assert_log(&[("acquired", 1), ("released", 1)]);
        store.finish().await.unwrap();
        assert!(mismatch.is_err(), "the deliberately wrong log must fail");
        assert!(!poisoned, "an oracle panic must not poison lease cleanup");
    });
}

// Missing Start state, wrong ordering, duplicate acquisition or missing cancellation fails.
#[test]
fn ready_precedes_events_and_stop_releases_session() {
    block_on(async {
        let client = Arc::new(Client::default());
        let (acquired, events) = client.queue();
        let mut store = store(&client);
        start(&mut store).await;
        client.assert_log(&[("acquired", 1)]);
        events.try_send(ProxyEvent::JoinObserved).unwrap();
        acquired.try_send(Ok(())).unwrap();
        ready(&mut store).await;
        let _ = store
            .receive(|a| *a == Action::Event(ProxyEvent::JoinObserved))
            .await
            .unwrap();
        let _ = store.send(Action::Start).await.unwrap();
        client.assert_calls(&[config()]);
        stop(&mut store).await;
        assert!(events.is_closed());
        client.assert_log(&[("acquired", 1), ("released", 1)]);
        store.finish().await.unwrap();
    });
}

#[test]
fn pending_acquisition_cancels_and_restart_is_fresh() {
    block_on(async {
        let client = Arc::new(Client::default());
        let (old_result, old_events) = client.queue();
        let mut store = store(&client);
        start(&mut store).await;
        let _ = store.send(Action::Start).await.unwrap();
        client.assert_calls(&[config()]);
        stop(&mut store).await;
        assert!(old_result.is_closed());
        assert!(old_events.is_closed());
        client.assert_log(&[("acquired", 1), ("released", 1)]);
        let (result, events) = client.queue();
        start(&mut store).await;
        assert!(old_result.try_send(Ok(())).is_err());
        result.try_send(Ok(())).unwrap();
        ready(&mut store).await;
        events.try_send(ProxyEvent::JoinObserved).unwrap();
        let _ = store
            .receive(|a| *a == Action::Event(ProxyEvent::JoinObserved))
            .await
            .unwrap();
        stop(&mut store).await;
        client.assert_calls(&[config(), config()]);
        client.assert_log(&[
            ("acquired", 1),
            ("released", 1),
            ("acquired", 2),
            ("released", 2),
        ]);
        store.finish().await.unwrap();
    });
}

#[test]
fn failure_releases_lease_and_explicit_retry_clears_error() {
    block_on(async {
        let client = Arc::new(Client::default());
        let (result, events) = client.queue();
        let mut store = store(&client);
        start(&mut store).await;
        result
            .try_send(Err(ProxyError::StartFailed("bind failed".into())))
            .unwrap();
        let _ = store
            .receive_with(
                |a| *a == Action::Failed(ProxyError::StartFailed("bind failed".into())),
                |s| {
                    s.phase = Phase::Failed;
                    s.last_error = Some("StartFailed(\"bind failed\")".into());
                },
            )
            .await
            .unwrap();
        assert!(events.is_closed());
        client.assert_log(&[("acquired", 1), ("released", 1)]);
        client.assert_calls(&[config()]);
        store
            .assert_state(|s| assert_eq!(s.phase, Phase::Failed))
            .await
            .unwrap();
        let (result, _events) = client.queue();
        start(&mut store).await;
        result.try_send(Ok(())).unwrap();
        ready(&mut store).await;
        stop(&mut store).await;
        client.assert_calls(&[config(), config()]);
        client.assert_log(&[
            ("acquired", 1),
            ("released", 1),
            ("acquired", 2),
            ("released", 2),
        ]);
        store.finish().await.unwrap();
    });
}

#[test]
fn eof_releases_before_ended_and_allows_restart() {
    block_on(async {
        let client = Arc::new(Client::default());
        let (result, events) = client.queue();
        let mut store = store(&client);
        start(&mut store).await;
        result.try_send(Ok(())).unwrap();
        ready(&mut store).await;
        drop(events);
        let _ = store
            .receive_with(|a| *a == Action::Ended, |s| s.phase = Phase::Stopped)
            .await
            .unwrap();
        client.assert_log(&[("acquired", 1), ("released", 1)]);
        let (result, _events) = client.queue();
        start(&mut store).await;
        result.try_send(Ok(())).unwrap();
        ready(&mut store).await;
        stop(&mut store).await;
        client.assert_calls(&[config(), config()]);
        client.assert_log(&[
            ("acquired", 1),
            ("released", 1),
            ("acquired", 2),
            ("released", 2),
        ]);
        store.finish().await.unwrap();
    });
}

#[test]
fn inactive_actions_do_not_acquire() {
    block_on(async {
        let client = Arc::new(Client::default());
        let mut store = store(&client);
        for action in [
            Action::Ready,
            Action::Failed(ProxyError::Other("late".into())),
            Action::Ended,
            Action::Event(ProxyEvent::JoinObserved),
            Action::Stop,
            Action::Stop,
        ] {
            let _ = store.send(action).await.unwrap();
        }
        client.assert_calls(&[]);
        client.assert_log(&[]);
        store.finish().await.unwrap();
    });
}

#[test]
fn stores_own_independent_sessions() {
    block_on(async {
        let first = Arc::new(Client::default());
        let second = Arc::new(Client::default());
        let (first_result, first_events) = first.queue();
        let (second_result, second_events) = second.queue();
        let mut first_store = store(&first);
        let mut second_store = store(&second);
        start(&mut first_store).await;
        start(&mut second_store).await;
        first_result.try_send(Ok(())).unwrap();
        second_result.try_send(Ok(())).unwrap();
        ready(&mut first_store).await;
        ready(&mut second_store).await;
        stop(&mut first_store).await;
        assert!(first_events.is_closed());
        first.assert_log(&[("acquired", 1), ("released", 1)]);
        second.assert_log(&[("acquired", 1)]);
        second_events.try_send(ProxyEvent::JoinObserved).unwrap();
        let _ = second_store
            .receive(|a| *a == Action::Event(ProxyEvent::JoinObserved))
            .await
            .unwrap();
        stop(&mut second_store).await;
        second.assert_log(&[("acquired", 1), ("released", 1)]);
        first.assert_calls(&[config()]);
        second.assert_calls(&[config()]);
        first_store.finish().await.unwrap();
        second_store.finish().await.unwrap();
    });
}
