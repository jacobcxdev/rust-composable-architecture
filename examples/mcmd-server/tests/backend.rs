//! Tests backend reconciliation with independent expected snapshots and gated replies.
//!
//! `TestStore` drives the real reducer. Queued responses control external completion;
//! no network service or remote rollback is exercised here.

use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

use async_channel::{Receiver, Sender};
use futures_executor::block_on;
use rca_example_mcmd_server::backend::{
    Action, Backend, BackendClient, BackendError, BackendFuture, BackendKey, Phase, State, Target,
};
use rca_test::TestStore;

#[derive(Debug, Eq, PartialEq)]
enum Call {
    Start(String),
    Stop(String),
}

type Response = Result<(), BackendError>;

#[derive(Default)]
struct Client {
    calls: Mutex<Vec<Call>>,
    starts: Mutex<VecDeque<Receiver<Response>>>,
    stops: Mutex<VecDeque<Receiver<Response>>>,
}

impl Client {
    fn queue_start(&self) -> Sender<Response> {
        let (sender, receiver) = async_channel::bounded(1);
        self.starts.lock().unwrap().push_back(receiver);
        sender
    }

    fn queue_stop(&self) -> Sender<Response> {
        let (sender, receiver) = async_channel::bounded(1);
        self.stops.lock().unwrap().push_back(receiver);
        sender
    }

    fn assert_calls(&self, expected: &[Call]) {
        assert_eq!(*self.calls.lock().unwrap(), expected);
    }
}

impl BackendClient for Client {
    fn start(&self, server_id: String) -> BackendFuture {
        self.calls.lock().unwrap().push(Call::Start(server_id));
        let response = self
            .starts
            .lock()
            .unwrap()
            .pop_front()
            .expect("unexpected start call");
        Box::pin(async move { response.recv().await.expect("start response retained") })
    }

    fn stop(&self, server_id: String) -> BackendFuture {
        self.calls.lock().unwrap().push(Call::Stop(server_id));
        let response = self
            .stops
            .lock()
            .unwrap()
            .pop_front()
            .expect("unexpected stop call");
        Box::pin(async move { response.recv().await.expect("stop response retained") })
    }
}

fn state(phase: Phase, target: Target) -> State {
    State {
        server_id: "s1".into(),
        phase,
        target,
        last_error: None,
    }
}

fn store(state: State, client: &Arc<Client>) -> TestStore<State, Action> {
    TestStore::builder(state, Backend)
        .with_dependency::<BackendKey>(Arc::clone(client) as Arc<dyn BackendClient>)
        .build()
}

// Removing the request target change, phase change, client call or delegate must fail.
#[test]
fn start_success_emits_started() {
    block_on(async {
        let client = Arc::new(Client::default());
        let response = client.queue_start();
        let mut initial = state(Phase::Stopped, Target::Stopped);
        initial.last_error = Some("previous".into());
        let mut store = store(initial, &client);
        let _ = store
            .send_with(Action::RequestStart, |s| s.target = Target::Running)
            .await
            .unwrap();
        let _ = store
            .receive_with(
                |a| *a == Action::Reconcile,
                |s| {
                    s.phase = Phase::Starting;
                    s.last_error = None;
                },
            )
            .await
            .unwrap();
        store
            .assert_state(|s| assert_eq!(s.phase, Phase::Starting))
            .await
            .unwrap();
        client.assert_calls(&[Call::Start("s1".into())]);
        response.try_send(Ok(())).unwrap();
        let _ = store
            .receive_with(
                |a| *a == Action::StartFinished(Ok(())),
                |s| s.phase = Phase::Running,
            )
            .await
            .unwrap();
        let _ = store.receive(|a| *a == Action::Started).await.unwrap();
        store.finish().await.unwrap();
        client.assert_calls(&[Call::Start("s1".into())]);
    });
}

#[test]
fn stop_success_emits_stopped() {
    block_on(async {
        let client = Arc::new(Client::default());
        let response = client.queue_stop();
        let mut store = store(state(Phase::Running, Target::Running), &client);
        let _ = store
            .send_with(Action::RequestStop, |s| s.target = Target::Stopped)
            .await
            .unwrap();
        let _ = store
            .receive_with(|a| *a == Action::Reconcile, |s| s.phase = Phase::Stopping)
            .await
            .unwrap();
        store
            .assert_state(|s| assert_eq!(s.phase, Phase::Stopping))
            .await
            .unwrap();
        client.assert_calls(&[Call::Stop("s1".into())]);
        response.try_send(Ok(())).unwrap();
        let _ = store
            .receive_with(
                |a| *a == Action::StopFinished(Ok(())),
                |s| s.phase = Phase::Stopped,
            )
            .await
            .unwrap();
        let _ = store.receive(|a| *a == Action::Stopped).await.unwrap();
        store.finish().await.unwrap();
        client.assert_calls(&[Call::Stop("s1".into())]);
    });
}

async fn begin_start(store: &mut TestStore<State, Action>) {
    let _ = store
        .send_with(Action::RequestStart, |s| s.target = Target::Running)
        .await
        .unwrap();
    let _ = store
        .receive_with(
            |a| *a == Action::Reconcile,
            |s| {
                s.phase = Phase::Starting;
                s.last_error = None;
            },
        )
        .await
        .unwrap();
}

async fn begin_stop(store: &mut TestStore<State, Action>) {
    let _ = store
        .send_with(Action::RequestStop, |s| s.target = Target::Stopped)
        .await
        .unwrap();
    let _ = store
        .receive_with(
            |a| *a == Action::Reconcile,
            |s| {
                s.phase = Phase::Stopping;
                s.last_error = None;
            },
        )
        .await
        .unwrap();
}

// Ignoring failures or reporting success after a target reversal must fail.
#[test]
fn start_failure_preserves_error_after_target_reversal() {
    block_on(async {
        let client = Arc::new(Client::default());
        let response = client.queue_start();
        let mut store = store(state(Phase::Stopped, Target::Stopped), &client);
        begin_start(&mut store).await;
        let _ = store
            .send_with(Action::RequestStop, |s| s.target = Target::Stopped)
            .await
            .unwrap();
        let _ = store.receive(|a| *a == Action::Reconcile).await.unwrap();
        response
            .try_send(Err(BackendError("start failed".into())))
            .unwrap();
        let _ = store
            .receive_with(
                |a| *a == Action::StartFinished(Err(BackendError("start failed".into()))),
                |s| {
                    s.phase = Phase::Error;
                    s.last_error = Some("start failed".into());
                },
            )
            .await
            .unwrap();
        let _ = store
            .send_with(Action::RequestStart, |s| s.target = Target::Running)
            .await
            .unwrap();
        let _ = store.receive(|a| *a == Action::Reconcile).await.unwrap();
        store.finish().await.unwrap();
        client.assert_calls(&[Call::Start("s1".into())]);
    });
}

#[test]
fn stop_failure_preserves_error_after_target_reversal() {
    block_on(async {
        let client = Arc::new(Client::default());
        let response = client.queue_stop();
        let mut store = store(state(Phase::Running, Target::Running), &client);
        begin_stop(&mut store).await;
        let _ = store
            .send_with(Action::RequestStart, |s| s.target = Target::Running)
            .await
            .unwrap();
        let _ = store.receive(|a| *a == Action::Reconcile).await.unwrap();
        response
            .try_send(Err(BackendError("stop failed".into())))
            .unwrap();
        let _ = store
            .receive_with(
                |a| *a == Action::StopFinished(Err(BackendError("stop failed".into()))),
                |s| {
                    s.phase = Phase::Error;
                    s.last_error = Some("stop failed".into());
                },
            )
            .await
            .unwrap();
        let _ = store
            .send_with(Action::RequestStop, |s| s.target = Target::Stopped)
            .await
            .unwrap();
        let _ = store.receive(|a| *a == Action::Reconcile).await.unwrap();
        store.finish().await.unwrap();
        client.assert_calls(&[Call::Stop("s1".into())]);
    });
}

// Starting in a stable/error phase without a target mismatch must not call I/O.
#[test]
fn stable_and_error_reconciliation_is_idle() {
    block_on(async {
        for (phase, target) in [
            (Phase::Stopped, Target::Stopped),
            (Phase::Running, Target::Running),
            (Phase::Error, Target::Stopped),
            (Phase::Error, Target::Running),
        ] {
            let client = Arc::new(Client::default());
            let mut store = store(state(phase, target), &client);
            let _ = store.send(Action::Reconcile).await.unwrap();
            store.finish().await.unwrap();
            client.assert_calls(&[]);
        }
    });
}

// Allowing a second start while Starting triggers an unexpected client call.
#[test]
fn repeated_start_during_pending_start_does_not_duplicate_work() {
    block_on(async {
        let client = Arc::new(Client::default());
        let response = client.queue_start();
        let mut store = store(state(Phase::Stopped, Target::Stopped), &client);
        begin_start(&mut store).await;
        for _ in 0..2 {
            let _ = store.send(Action::RequestStart).await.unwrap();
            let _ = store.receive(|a| *a == Action::Reconcile).await.unwrap();
        }
        store
            .assert_state(|s| assert_eq!(s.phase, Phase::Starting))
            .await
            .unwrap();
        client.assert_calls(&[Call::Start("s1".into())]);
        response.try_send(Ok(())).unwrap();
        let _ = store
            .receive_with(
                |a| *a == Action::StartFinished(Ok(())),
                |s| s.phase = Phase::Running,
            )
            .await
            .unwrap();
        let _ = store.receive(|a| *a == Action::Started).await.unwrap();
        store.finish().await.unwrap();
    });
}

#[test]
fn repeated_stop_during_pending_stop_does_not_duplicate_work() {
    block_on(async {
        let client = Arc::new(Client::default());
        let response = client.queue_stop();
        let mut store = store(state(Phase::Running, Target::Running), &client);
        begin_stop(&mut store).await;
        for _ in 0..2 {
            let _ = store.send(Action::RequestStop).await.unwrap();
            let _ = store.receive(|a| *a == Action::Reconcile).await.unwrap();
        }
        store
            .assert_state(|s| assert_eq!(s.phase, Phase::Stopping))
            .await
            .unwrap();
        client.assert_calls(&[Call::Stop("s1".into())]);
        response.try_send(Ok(())).unwrap();
        let _ = store
            .receive_with(
                |a| *a == Action::StopFinished(Ok(())),
                |s| s.phase = Phase::Stopped,
            )
            .await
            .unwrap();
        let _ = store.receive(|a| *a == Action::Stopped).await.unwrap();
        store.finish().await.unwrap();
    });
}

// Issuing stop before start completes, or emitting Started despite stopped target, fails.
#[test]
fn stop_during_start_waits_then_stops_without_started_delegate() {
    block_on(async {
        let client = Arc::new(Client::default());
        let started = client.queue_start();
        let stopped = client.queue_stop();
        let mut store = store(state(Phase::Stopped, Target::Stopped), &client);
        begin_start(&mut store).await;
        let _ = store
            .send_with(Action::RequestStop, |s| s.target = Target::Stopped)
            .await
            .unwrap();
        let _ = store.receive(|a| *a == Action::Reconcile).await.unwrap();
        client.assert_calls(&[Call::Start("s1".into())]);
        started.try_send(Ok(())).unwrap();
        let _ = store
            .receive_with(
                |a| *a == Action::StartFinished(Ok(())),
                |s| s.phase = Phase::Running,
            )
            .await
            .unwrap();
        let _ = store
            .receive_with(|a| *a == Action::Reconcile, |s| s.phase = Phase::Stopping)
            .await
            .unwrap();
        store
            .assert_state(|s| assert_eq!(s.phase, Phase::Stopping))
            .await
            .unwrap();
        client.assert_calls(&[Call::Start("s1".into()), Call::Stop("s1".into())]);
        stopped.try_send(Ok(())).unwrap();
        let _ = store
            .receive_with(
                |a| *a == Action::StopFinished(Ok(())),
                |s| s.phase = Phase::Stopped,
            )
            .await
            .unwrap();
        let _ = store.receive(|a| *a == Action::Stopped).await.unwrap();
        store.finish().await.unwrap();
    });
}

// Dropping the latest start target, or restarting before Stopped, fails.
#[test]
fn start_during_stop_restarts_after_stopped_delegate() {
    block_on(async {
        let client = Arc::new(Client::default());
        let stopped = client.queue_stop();
        let started = client.queue_start();
        let mut store = store(state(Phase::Running, Target::Running), &client);
        begin_stop(&mut store).await;
        let _ = store
            .send_with(Action::RequestStart, |s| s.target = Target::Running)
            .await
            .unwrap();
        let _ = store.receive(|a| *a == Action::Reconcile).await.unwrap();
        client.assert_calls(&[Call::Stop("s1".into())]);
        stopped.try_send(Ok(())).unwrap();
        let _ = store
            .receive_with(
                |a| *a == Action::StopFinished(Ok(())),
                |s| s.phase = Phase::Stopped,
            )
            .await
            .unwrap();
        let _ = store.receive(|a| *a == Action::Stopped).await.unwrap();
        let _ = store
            .receive_with(|a| *a == Action::Reconcile, |s| s.phase = Phase::Starting)
            .await
            .unwrap();
        store
            .assert_state(|s| assert_eq!(s.phase, Phase::Starting))
            .await
            .unwrap();
        client.assert_calls(&[Call::Stop("s1".into()), Call::Start("s1".into())]);
        started.try_send(Ok(())).unwrap();
        let _ = store
            .receive_with(
                |a| *a == Action::StartFinished(Ok(())),
                |s| s.phase = Phase::Running,
            )
            .await
            .unwrap();
        let _ = store.receive(|a| *a == Action::Started).await.unwrap();
        store.finish().await.unwrap();
    });
}

#[test]
fn latest_target_during_start_avoids_unnecessary_stop() {
    block_on(async {
        let client = Arc::new(Client::default());
        let started = client.queue_start();
        let mut store = store(state(Phase::Stopped, Target::Stopped), &client);
        begin_start(&mut store).await;
        let _ = store
            .send_with(Action::RequestStop, |s| s.target = Target::Stopped)
            .await
            .unwrap();
        let _ = store.receive(|a| *a == Action::Reconcile).await.unwrap();
        let _ = store
            .send_with(Action::RequestStart, |s| s.target = Target::Running)
            .await
            .unwrap();
        let _ = store.receive(|a| *a == Action::Reconcile).await.unwrap();
        started.try_send(Ok(())).unwrap();
        let _ = store
            .receive_with(
                |a| *a == Action::StartFinished(Ok(())),
                |s| s.phase = Phase::Running,
            )
            .await
            .unwrap();
        let _ = store.receive(|a| *a == Action::Started).await.unwrap();
        store.finish().await.unwrap();
        client.assert_calls(&[Call::Start("s1".into())]);
    });
}

#[test]
fn latest_target_during_stop_avoids_unnecessary_restart() {
    block_on(async {
        let client = Arc::new(Client::default());
        let stopped = client.queue_stop();
        let mut store = store(state(Phase::Running, Target::Running), &client);
        begin_stop(&mut store).await;
        let _ = store
            .send_with(Action::RequestStart, |s| s.target = Target::Running)
            .await
            .unwrap();
        let _ = store.receive(|a| *a == Action::Reconcile).await.unwrap();
        let _ = store
            .send_with(Action::RequestStop, |s| s.target = Target::Stopped)
            .await
            .unwrap();
        let _ = store.receive(|a| *a == Action::Reconcile).await.unwrap();
        stopped.try_send(Ok(())).unwrap();
        let _ = store
            .receive_with(
                |a| *a == Action::StopFinished(Ok(())),
                |s| s.phase = Phase::Stopped,
            )
            .await
            .unwrap();
        let _ = store.receive(|a| *a == Action::Stopped).await.unwrap();
        store.finish().await.unwrap();
        client.assert_calls(&[Call::Stop("s1".into())]);
    });
}

// Removing completion phase guards corrupts these states.
#[test]
fn irrelevant_completion_kinds_leave_state_unchanged() {
    block_on(async {
        for phase in [
            Phase::Stopped,
            Phase::Running,
            Phase::Starting,
            Phase::Stopping,
            Phase::Error,
        ] {
            let client = Arc::new(Client::default());
            let mut store = store(state(phase, Target::Stopped), &client);
            if phase != Phase::Starting {
                let _ = store.send(Action::StartFinished(Ok(()))).await.unwrap();
                let _ = store
                    .send(Action::StartFinished(Err(BackendError(
                        "stale start".into(),
                    ))))
                    .await
                    .unwrap();
            }
            if phase != Phase::Stopping {
                let _ = store.send(Action::StopFinished(Ok(()))).await.unwrap();
                let _ = store
                    .send(Action::StopFinished(Err(BackendError("stale stop".into()))))
                    .await
                    .unwrap();
            }
            store.finish().await.unwrap();
            client.assert_calls(&[]);
        }
    });
}

// Ambient/global dependency storage would cross-wire these pending operations.
#[test]
fn stores_keep_backend_clients_and_results_separate() {
    block_on(async {
        let first = Arc::new(Client::default());
        let second = Arc::new(Client::default());
        let first_response = first.queue_start();
        let second_response = second.queue_start();
        let mut first_store = store(state(Phase::Stopped, Target::Stopped), &first);
        let mut second_state = state(Phase::Stopped, Target::Stopped);
        second_state.server_id = "s2".into();
        let mut second_store = store(second_state, &second);
        begin_start(&mut first_store).await;
        begin_start(&mut second_store).await;
        first_store
            .assert_state(|s| assert_eq!(s.phase, Phase::Starting))
            .await
            .unwrap();
        second_store
            .assert_state(|s| assert_eq!(s.phase, Phase::Starting))
            .await
            .unwrap();
        first.assert_calls(&[Call::Start("s1".into())]);
        second.assert_calls(&[Call::Start("s2".into())]);
        second_response
            .try_send(Err(BackendError("second failed".into())))
            .unwrap();
        let _ = second_store
            .receive_with(
                |a| *a == Action::StartFinished(Err(BackendError("second failed".into()))),
                |s| {
                    s.phase = Phase::Error;
                    s.last_error = Some("second failed".into());
                },
            )
            .await
            .unwrap();
        second_store.finish().await.unwrap();
        first_response.try_send(Ok(())).unwrap();
        let _ = first_store
            .receive_with(
                |a| *a == Action::StartFinished(Ok(())),
                |s| s.phase = Phase::Running,
            )
            .await
            .unwrap();
        let _ = first_store
            .receive(|a| *a == Action::Started)
            .await
            .unwrap();
        first_store.finish().await.unwrap();
    });
}
