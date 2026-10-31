//! Checks reply cancellation without transferring external operation ownership.
//!
//! `TestStore` drives RCA deterministically. Tests poll the independent owner explicitly.
//! Tokio supplies a cooperative deadline, not RCA's scheduler.
//! These channel fixtures prove protocol behaviour, not native resource release.

use std::{pin::Pin, sync::Arc, time::Duration};

use async_channel::{Receiver, Sender};
use rca_example_mcmd_server::{
    managed_proxy::{
        Action, Client, Error, Events, ManagedProxy, Phase, Recovery, StartFuture, State,
        StopFuture, channel,
    },
    proxy::{Config, ProxyEvent},
};
use rca_test::TestStore;

/// Records client entry and transfers the operation's completion gate.
enum Call {
    Start(Config, Sender<Result<Events, Error>>),
    Stop(String, Sender<Result<(), Error>>),
}

/// Holds each client operation until its test supplies a result.
struct Gated(Sender<Call>);

impl Client for Gated {
    fn start(&self, config: Config) -> StartFuture {
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

fn config() -> Config {
    Config {
        server_id: "alpha".into(),
        host: "127.0.0.1".into(),
        java_ports: vec![25565],
        bedrock_ports: vec![19132],
    }
}

/// Bounds cooperative waits without controlling the tested operation order.
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

/// Holds an external owner polled separately from `TestStore`'s deterministic runtime.
type Host = Pin<Box<dyn Future<Output = Recovery>>>;

async fn step(host: &mut Host) {
    assert!(futures_util::poll!(host.as_mut()).is_pending());
}

/// Completes acquisition and verifies that Ready precedes the first event.
async fn start(
    store: &mut TestStore<State, Action>,
    calls: &Receiver<Call>,
    host: &mut Host,
) -> Sender<ProxyEvent> {
    let _ = store
        .send_with(Action::Start, |s| s.phase = Phase::Starting)
        .await
        .unwrap();
    step(host).await;
    let Call::Start(actual, reply) = calls.recv().await.unwrap() else {
        panic!("expected start")
    };
    assert_eq!(actual, config());
    let (send, events) = async_channel::bounded(2);
    // A queued event must not overtake Ready.
    send.send(ProxyEvent::JoinObserved).await.unwrap();
    reply.send(Ok(Box::pin(events))).await.unwrap();
    step(host).await;
    let _ = store
        .receive_with(|a| *a == Action::Ready, |s| s.phase = Phase::Listening)
        .await
        .unwrap();
    let _ = store
        .receive(|a| *a == Action::Event(ProxyEvent::JoinObserved))
        .await
        .unwrap();
    send
}

async fn stop_reply(calls: &Receiver<Call>, host: &mut Host) -> Sender<Result<(), Error>> {
    step(host).await;
    let Call::Stop(id, reply) = calls.recv().await.unwrap() else {
        panic!("expected stop")
    };
    assert_eq!(id, "alpha");
    reply
}

#[test]
fn stop_waits_for_release_and_typed_failure_allows_explicit_retry() {
    check(async {
        let (send, calls) = async_channel::bounded(1);
        let (control, owner) = channel(config(), Arc::new(Gated(send)));
        let mut host: Host = Box::pin(owner.run());
        let mut store = TestStore::new(State::default(), ManagedProxy::new(control));
        let events = start(&mut store, &calls, &mut host).await;
        let _ = store
            .send_with(Action::Stop, |s| s.phase = Phase::Stopping)
            .await
            .unwrap();
        assert!(events.is_closed());
        let reply = stop_reply(&calls, &mut host).await;
        let _ = store.send(Action::Start).await.unwrap();
        store
            .assert_state(|s| assert_eq!(s.phase, Phase::Stopping))
            .await
            .unwrap();
        let error = Error::StopFailed("busy\n\"socket\"".into());
        reply.send(Err(error.clone())).await.unwrap();
        step(&mut host).await;
        let expected = error.clone();
        let diagnostic = error.to_string();
        let _ = store
            .receive_with(
                move |a| *a == Action::Failed(expected.clone()),
                move |s| {
                    s.phase = Phase::Failed;
                    s.last_error = Some(diagnostic);
                },
            )
            .await
            .unwrap();
        assert_eq!(error.to_string(), "StopFailed(\"busy\\n\\\"socket\\\"\")");
        let _ = store
            .send_with(Action::Stop, |s| {
                s.phase = Phase::Stopping;
                s.last_error = None;
            })
            .await
            .unwrap();
        stop_reply(&calls, &mut host)
            .await
            .send(Ok(()))
            .await
            .unwrap();
        step(&mut host).await;
        let _ = store
            .receive_with(|a| *a == Action::Stopped, |s| s.phase = Phase::Stopped)
            .await
            .unwrap();
        store.finish().await.unwrap();
        drop(store);
        assert!(!host.await.is_pending());
        assert!(calls.is_empty());
    });
}

#[test]
fn eof_requests_cleanup_instead_of_claiming_release() {
    check(async {
        let (send, calls) = async_channel::bounded(1);
        let (control, owner) = channel(config(), Arc::new(Gated(send)));
        let mut host: Host = Box::pin(owner.run());
        let mut store = TestStore::new(State::default(), ManagedProxy::new(control));
        drop(start(&mut store, &calls, &mut host).await);
        let _ = store
            .receive_with(|a| *a == Action::Ended, |s| s.phase = Phase::Stopping)
            .await
            .unwrap();
        let reply = stop_reply(&calls, &mut host).await;
        store
            .assert_state(|s| assert_eq!(s.phase, Phase::Stopping))
            .await
            .unwrap();
        reply.send(Ok(())).await.unwrap();
        step(&mut host).await;
        let _ = store
            .receive_with(|a| *a == Action::Stopped, |s| s.phase = Phase::Stopped)
            .await
            .unwrap();
        store.finish().await.unwrap();
        drop(store);
        assert!(!host.await.is_pending());
    });
}

#[test]
fn cancelling_stop_reply_then_finishing_store_leaves_host_cleanup_running() {
    check(async {
        let (send, calls) = async_channel::bounded(1);
        let (control, owner) = channel(config(), Arc::new(Gated(send)));
        let mut host: Host = Box::pin(owner.run());
        let mut store = TestStore::new(State::default(), ManagedProxy::new(control));
        let events = start(&mut store, &calls, &mut host).await;
        let stop = store
            .send_with(Action::Stop, |s| s.phase = Phase::Stopping)
            .await
            .unwrap();
        let reply = stop_reply(&calls, &mut host).await;
        stop.cancel().await.unwrap();
        store.finish().await.unwrap();
        drop(store);
        assert!(events.is_closed());
        step(&mut host).await;
        reply
            .send(Err(Error::StopFailed("still live".into())))
            .await
            .unwrap();
        let recovery = host.await;
        assert!(recovery.is_pending());
        assert_eq!(
            recovery.last_error(),
            Some(&Error::StopFailed("still live".into()))
        );
    });
}

#[test]
fn cancelled_stop_reply_can_be_reconciled_without_closing_store() {
    check(async {
        let (send, calls) = async_channel::bounded(1);
        let (control, owner) = channel(config(), Arc::new(Gated(send)));
        let mut host: Host = Box::pin(owner.run());
        let mut store = TestStore::new(State::default(), ManagedProxy::new(control));
        let events = start(&mut store, &calls, &mut host).await;
        let stop = store
            .send_with(Action::Stop, |s| s.phase = Phase::Stopping)
            .await
            .unwrap();
        let reply = stop_reply(&calls, &mut host).await;
        stop.cancel().await.unwrap();
        stop.finish().await.unwrap();
        assert!(events.is_closed());

        // The owner confirms release even though the cancelled effect cannot deliver its reply.
        reply.send(Ok(())).await.unwrap();
        step(&mut host).await;
        store
            .assert_state(|s| assert_eq!(s.phase, Phase::Stopping))
            .await
            .unwrap();
        assert!(calls.is_empty());

        let retry = store.send(Action::Stop).await.unwrap();
        assert!(
            !retry.is_finished(),
            "a fresh Stop must request confirmation instead of ignoring the cancelled reply"
        );
        step(&mut host).await;
        assert!(
            calls.is_empty(),
            "confirmed cleanup needs no second external stop"
        );
        let _ = store
            .receive_with(|a| *a == Action::Stopped, |s| s.phase = Phase::Stopped)
            .await
            .unwrap();
        retry.finish().await.unwrap();

        let replacement_events = start(&mut store, &calls, &mut host).await;
        let _ = store
            .send_with(Action::Stop, |s| s.phase = Phase::Stopping)
            .await
            .unwrap();
        assert!(replacement_events.is_closed());
        stop_reply(&calls, &mut host)
            .await
            .send(Ok(()))
            .await
            .unwrap();
        step(&mut host).await;
        let _ = store
            .receive_with(|a| *a == Action::Stopped, |s| s.phase = Phase::Stopped)
            .await
            .unwrap();
        store.finish().await.unwrap();
        drop(store);
        let recovery = host.await;
        assert!(!recovery.is_pending());
        assert_eq!(recovery.last_error(), None);
        assert!(calls.is_empty());
    });
}

#[test]
fn latest_stop_reply_rejects_old_failure_and_confirms_retry() {
    check(async {
        let (send, calls) = async_channel::bounded(1);
        let (control, owner) = channel(config(), Arc::new(Gated(send)));
        let mut host: Host = Box::pin(owner.run());
        let mut store = TestStore::new(State::default(), ManagedProxy::new(control));
        let events = start(&mut store, &calls, &mut host).await;
        let first = store
            .send_with(Action::Stop, |s| s.phase = Phase::Stopping)
            .await
            .unwrap();
        let first_reply = stop_reply(&calls, &mut host).await;
        assert!(events.is_closed());

        let latest = store.send(Action::Stop).await.unwrap();
        first.finish().await.unwrap();
        assert!(!latest.is_finished());
        step(&mut host).await;
        assert!(
            calls.is_empty(),
            "the first external stop still owns its gate"
        );

        // The superseded reply cannot fail the latest request, but its owner operation still runs.
        first_reply
            .send(Err(Error::StopFailed("old failure".into())))
            .await
            .unwrap();
        let latest_reply = stop_reply(&calls, &mut host).await;
        store
            .assert_state(|s| {
                assert_eq!(s.phase, Phase::Stopping);
                assert_eq!(s.last_error, None);
            })
            .await
            .unwrap();
        assert!(!latest.is_finished());

        latest_reply.send(Ok(())).await.unwrap();
        step(&mut host).await;
        let _ = store
            .receive_with(|a| *a == Action::Stopped, |s| s.phase = Phase::Stopped)
            .await
            .unwrap();
        latest.finish().await.unwrap();
        store.finish().await.unwrap();
        drop(store);
        let recovery = host.await;
        assert!(!recovery.is_pending());
        assert_eq!(recovery.last_error(), None);
        assert!(calls.is_empty());
    });
}

#[test]
fn full_stop_reply_preserves_latest_failure_until_explicit_retry() {
    check(async {
        let (send, calls) = async_channel::bounded(1);
        let (control, owner) = channel(config(), Arc::new(Gated(send)));
        let mut host: Host = Box::pin(owner.run());
        let mut store = TestStore::new(State::default(), ManagedProxy::new(control));
        let events = start(&mut store, &calls, &mut host).await;
        let first = store
            .send_with(Action::Stop, |s| s.phase = Phase::Stopping)
            .await
            .unwrap();
        let first_reply = stop_reply(&calls, &mut host).await;
        assert!(events.is_closed());

        // One stop runs, one remains queued, and a third must report bounded admission failure.
        let queued = store.send(Action::Stop).await.unwrap();
        first.finish().await.unwrap();
        let full = store.send(Action::Stop).await.unwrap();
        queued.finish().await.unwrap();
        let _ = store
            .receive_with(
                |a| *a == Action::Failed(Error::Full),
                |s| {
                    s.phase = Phase::Failed;
                    s.last_error = Some(Error::Full.to_string());
                },
            )
            .await
            .unwrap();
        full.finish().await.unwrap();
        step(&mut host).await;
        assert!(calls.is_empty(), "admitted cleanup still awaits its result");

        first_reply.send(Ok(())).await.unwrap();
        step(&mut host).await;
        assert!(calls.is_empty(), "the queued stop reuses confirmed cleanup");
        store
            .assert_state(|s| {
                assert_eq!(s.phase, Phase::Failed);
                assert_eq!(s.last_error, Some(Error::Full.to_string()));
            })
            .await
            .unwrap();

        let retry = store
            .send_with(Action::Stop, |s| {
                s.phase = Phase::Stopping;
                s.last_error = None;
            })
            .await
            .unwrap();
        assert!(!retry.is_finished());
        step(&mut host).await;
        let _ = store
            .receive_with(|a| *a == Action::Stopped, |s| s.phase = Phase::Stopped)
            .await
            .unwrap();
        retry.finish().await.unwrap();
        store.finish().await.unwrap();
        drop(store);
        let recovery = host.await;
        assert!(!recovery.is_pending());
        assert_eq!(recovery.last_error(), None);
        assert!(calls.is_empty());
    });
}

#[test]
fn failed_acquisition_requires_cleanup_before_fresh_start() {
    for error in [
        Error::StartFailed("partial\n\"bind\"".into()),
        Error::Other("transport".into()),
    ] {
        check(async {
            let (send, calls) = async_channel::bounded(1);
            let (control, owner) = channel(config(), Arc::new(Gated(send)));
            let mut host: Host = Box::pin(owner.run());
            let mut store = TestStore::new(State::default(), ManagedProxy::new(control));
            let _ = store
                .send_with(Action::Start, |s| s.phase = Phase::Starting)
                .await
                .unwrap();
            step(&mut host).await;
            let Call::Start(actual, reply) = calls.recv().await.unwrap() else {
                panic!("expected start")
            };
            assert_eq!(actual, config());
            reply.send(Err(error.clone())).await.unwrap();
            step(&mut host).await;
            let diagnostic = error.to_string();
            let _ = store
                .receive_with(
                    move |a| *a == Action::Failed(error.clone()),
                    move |s| {
                        s.phase = Phase::Failed;
                        s.last_error = Some(diagnostic);
                    },
                )
                .await
                .unwrap();
            let _ = store.send(Action::Start).await.unwrap();
            assert!(calls.is_empty());
            let _ = store
                .send_with(Action::Stop, |s| {
                    s.phase = Phase::Stopping;
                    s.last_error = None;
                })
                .await
                .unwrap();
            stop_reply(&calls, &mut host)
                .await
                .send(Ok(()))
                .await
                .unwrap();
            step(&mut host).await;
            let _ = store
                .receive_with(|a| *a == Action::Stopped, |s| s.phase = Phase::Stopped)
                .await
                .unwrap();
            let events = start(&mut store, &calls, &mut host).await;
            let _ = store
                .send_with(Action::Stop, |s| s.phase = Phase::Stopping)
                .await
                .unwrap();
            assert!(events.is_closed());
            stop_reply(&calls, &mut host)
                .await
                .send(Ok(()))
                .await
                .unwrap();
            step(&mut host).await;
            let _ = store
                .receive_with(|a| *a == Action::Stopped, |s| s.phase = Phase::Stopped)
                .await
                .unwrap();
            store.finish().await.unwrap();
            drop(store);
            assert!(!host.await.is_pending());
        });
    }
}

#[test]
fn stop_during_acquisition_cancels_delivery_but_waits_for_external_release() {
    check(async {
        let (send, calls) = async_channel::bounded(1);
        let (control, owner) = channel(config(), Arc::new(Gated(send)));
        let mut host: Host = Box::pin(owner.run());
        let mut store = TestStore::new(State::default(), ManagedProxy::new(control));
        let _ = store
            .send_with(Action::Start, |s| s.phase = Phase::Starting)
            .await
            .unwrap();
        step(&mut host).await;
        let Call::Start(_, reply) = calls.recv().await.unwrap() else {
            panic!("expected start")
        };
        let _ = store
            .send_with(Action::Stop, |s| s.phase = Phase::Stopping)
            .await
            .unwrap();
        let (events, receive) = async_channel::bounded(1);
        reply.send(Ok(Box::pin(receive))).await.unwrap();
        let reply = stop_reply(&calls, &mut host).await;
        assert!(events.is_closed());
        store
            .assert_state(|s| assert_eq!(s.phase, Phase::Stopping))
            .await
            .unwrap();
        reply.send(Ok(())).await.unwrap();
        step(&mut host).await;
        let _ = store
            .receive_with(|a| *a == Action::Stopped, |s| s.phase = Phase::Stopped)
            .await
            .unwrap();
        store.finish().await.unwrap();
        drop(store);
        assert!(!host.await.is_pending());
        assert!(
            calls.is_empty(),
            "orphan cleanup also satisfies the queued Stop"
        );
    });
}
