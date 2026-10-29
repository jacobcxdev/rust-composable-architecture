//! Checks owner admission, reply handoff and retained cleanup authority.
//!
//! Tests use Tokio tasks for caller cancellation and explicit polls for handoff order.
//! The outer deadline detects stalled cooperative futures. It does not set operation timing.

use std::{sync::Arc, time::Duration};

use async_channel::{Receiver, Sender};
use rca_example_mcmd_server::{
    managed_proxy::{Client, Error, Events, StartFuture, StopFuture, channel},
    proxy::Config,
};

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

async fn started(calls: &Receiver<Call>) {
    let Call::Start(actual, reply) = calls.recv().await.unwrap() else {
        panic!("expected start")
    };
    assert_eq!(actual, config());
    reply
        .send(Ok(Box::pin(futures_util::stream::pending())))
        .await
        .unwrap();
}

async fn stop_reply(calls: &Receiver<Call>) -> Sender<Result<(), Error>> {
    let Call::Stop(id, reply) = calls.recv().await.unwrap() else {
        panic!("expected stop")
    };
    assert_eq!(id, "alpha");
    reply
}

#[test]
fn failed_stop_blocks_restart_until_explicit_retry_succeeds() {
    check(async {
        let (send, calls) = async_channel::bounded(1);
        let (control, owner) = channel(config(), Arc::new(Gated(send)));
        let host = tokio::spawn(owner.run());
        let start = tokio::spawn({
            let control = control.clone();
            async move { control.start().await }
        });
        started(&calls).await;
        let events = start.await.unwrap().unwrap();
        let stop = tokio::spawn({
            let control = control.clone();
            async move { control.stop().await }
        });
        let reply = stop_reply(&calls).await;
        assert!(!stop.is_finished(), "admission cannot confirm cleanup");
        reply
            .send(Err(Error::StopFailed("busy\n\"socket\"".into())))
            .await
            .unwrap();
        assert_eq!(
            stop.await.unwrap(),
            Err(Error::StopFailed("busy\n\"socket\"".into()))
        );
        assert!(matches!(control.start().await, Err(Error::CleanupPending)));
        assert!(calls.is_empty());
        let retry = tokio::spawn({
            let control = control.clone();
            async move { control.stop().await }
        });
        stop_reply(&calls).await.send(Ok(())).await.unwrap();
        assert_eq!(retry.await.unwrap(), Ok(()));
        drop(events);
        drop(control);
        let recovery = host.await.unwrap();
        assert!(!recovery.is_pending());
        assert_eq!(recovery.last_error(), None);
        assert!(calls.is_empty());
    });
}

#[test]
fn abandoned_start_retains_failed_cleanup_and_recovery_can_retry() {
    check(async {
        let (send, calls) = async_channel::bounded(1);
        let (control, owner) = channel(config(), Arc::new(Gated(send)));
        let host = tokio::spawn(owner.run());
        let start = tokio::spawn({
            let control = control.clone();
            async move { control.start().await }
        });
        let Call::Start(actual, reply) = calls.recv().await.unwrap() else {
            panic!("expected start")
        };
        assert_eq!(actual, config());
        start.abort();
        assert!(matches!(start.await, Err(error) if error.is_cancelled()));
        reply
            .send(Ok(Box::pin(futures_util::stream::pending())))
            .await
            .unwrap();
        stop_reply(&calls)
            .await
            .send(Err(Error::StopFailed("retained".into())))
            .await
            .unwrap();
        drop(control);
        let mut recovery = host.await.unwrap();
        assert!(recovery.is_pending());
        assert_eq!(
            recovery.last_error(),
            Some(&Error::StopFailed("retained".into()))
        );
        let (result, ()) = futures_util::future::join(recovery.retry(), async {
            stop_reply(&calls).await.send(Ok(())).await.unwrap();
        })
        .await;
        assert_eq!(result, Ok(()));
        assert!(!recovery.is_pending());
    });
}

#[test]
fn failed_start_keeps_partial_cleanup_authority() {
    check(async {
        let (send, calls) = async_channel::bounded(1);
        let (control, owner) = channel(config(), Arc::new(Gated(send)));
        let host = tokio::spawn(owner.run());
        let start = tokio::spawn({
            let control = control.clone();
            async move { control.start().await }
        });
        let Call::Start(_, reply) = calls.recv().await.unwrap() else {
            panic!("expected start")
        };
        reply
            .send(Err(Error::StartFailed("partial".into())))
            .await
            .unwrap();
        assert!(
            matches!(start.await.unwrap(), Err(Error::StartFailed(message)) if message == "partial")
        );
        drop(control);
        let recovery = host.await.unwrap();
        assert!(recovery.is_pending());
        assert_eq!(
            recovery.last_error(),
            Some(&Error::StartFailed("partial".into()))
        );
        assert!(
            calls.is_empty(),
            "failed operations are not silently retried"
        );
    });
}

#[test]
fn closed_controls_clean_up_live_session_once() {
    check(async {
        let (send, calls) = async_channel::bounded(1);
        let (control, owner) = channel(config(), Arc::new(Gated(send)));
        let host = tokio::spawn(owner.run());
        let start = tokio::spawn({
            let control = control.clone();
            async move { control.start().await }
        });
        started(&calls).await;
        let events = start.await.unwrap().unwrap();
        drop(control);
        let reply = stop_reply(&calls).await;
        assert!(!host.is_finished());
        reply.send(Ok(())).await.unwrap();
        assert!(!host.await.unwrap().is_pending());
        assert!(calls.is_empty());
        drop(events);
    });
}

#[test]
fn cancelled_stop_wait_retains_failed_operation_and_cancelled_retry_authority() {
    check(async {
        let (send, calls) = async_channel::bounded(1);
        let (control, owner) = channel(config(), Arc::new(Gated(send)));
        let host = tokio::spawn(owner.run());
        let start = tokio::spawn({
            let control = control.clone();
            async move { control.start().await }
        });
        started(&calls).await;
        let events = start.await.unwrap().unwrap();
        let stop = tokio::spawn({
            let control = control.clone();
            async move { control.stop().await }
        });
        let reply = stop_reply(&calls).await;
        stop.abort();
        assert!(stop.await.unwrap_err().is_cancelled());
        drop(control);
        reply
            .send(Err(Error::Other("uncertain".into())))
            .await
            .unwrap();
        let mut recovery = host.await.unwrap();
        assert!(recovery.is_pending());
        assert_eq!(
            recovery.last_error(),
            Some(&Error::Other("uncertain".into()))
        );
        {
            let retry = recovery.retry();
            futures_util::pin_mut!(retry);
            assert!(futures_util::poll!(retry.as_mut()).is_pending());
        };
        assert!(recovery.is_pending());
        assert_eq!(
            recovery.last_error(),
            Some(&Error::Other("uncertain".into()))
        );
        assert!(stop_reply(&calls).await.send(Ok(())).await.is_err());
        let (result, ()) = futures_util::future::join(recovery.retry(), async {
            stop_reply(&calls).await.send(Ok(())).await.unwrap();
        })
        .await;
        assert_eq!(result, Ok(()));
        assert!(!recovery.is_pending());
        drop(events);
    });
}

#[test]
fn bounded_admission_reports_full_and_closed() {
    check(async {
        let (send, _calls) = async_channel::bounded(1);
        let (control, owner) = channel(config(), Arc::new(Gated(send)));
        let waiting = control.stop();
        futures_util::pin_mut!(waiting);
        assert!(futures_util::poll!(waiting.as_mut()).is_pending());
        assert_eq!(control.stop().await, Err(Error::Full));
        drop(owner);
        assert_eq!(waiting.await, Err(Error::Closed));
        assert!(matches!(control.start().await, Err(Error::Closed)));
    });
}

#[test]
fn buffered_stop_reply_survives_owner_closure() {
    check(async {
        let (send, calls) = async_channel::bounded(1);
        let (control, owner) = channel(config(), Arc::new(Gated(send)));
        let mut host = Box::pin(owner.run());
        let stop = control.stop();
        futures_util::pin_mut!(stop);
        assert!(futures_util::poll!(stop.as_mut()).is_pending());
        assert!(futures_util::poll!(host.as_mut()).is_pending());

        // No cleanup was needed, but the caller has not consumed its buffered reply.
        // Both that reply and command closure are ready when it next polls.
        drop(host);
        assert_eq!(stop.await, Ok(()));
        assert!(calls.is_empty());
    });
}

#[test]
fn stop_waits_behind_pending_start_without_overlapping_external_calls() {
    check(async {
        let (send, calls) = async_channel::bounded(1);
        let (control, owner) = channel(config(), Arc::new(Gated(send)));
        let mut host = Box::pin(owner.run());
        {
            let start = control.start();
            futures_util::pin_mut!(start);
            assert!(futures_util::poll!(start.as_mut()).is_pending());
            assert!(futures_util::poll!(host.as_mut()).is_pending());
            let Call::Start(actual, reply) = calls.try_recv().expect("start entered the client")
            else {
                panic!("expected start")
            };
            assert_eq!(actual, config());

            let stop = control.stop();
            futures_util::pin_mut!(stop);
            assert!(futures_util::poll!(stop.as_mut()).is_pending());
            // Give the owner another poll while acquisition still holds the external gate.
            assert!(futures_util::poll!(host.as_mut()).is_pending());
            assert!(calls.is_empty(), "stop cannot overlap acquisition");
            reply
                .send(Ok(Box::pin(futures_util::stream::pending())))
                .await
                .unwrap();
            assert!(futures_util::poll!(host.as_mut()).is_pending());
            let events = start.await.unwrap();
            stop_reply(&calls).await.send(Ok(())).await.unwrap();
            assert!(futures_util::poll!(host.as_mut()).is_pending());
            assert_eq!(stop.await, Ok(()));
            drop(events);
        };
        drop(control);
        let recovery = host.await;
        assert!(!recovery.is_pending());
        assert_eq!(recovery.last_error(), None);
        assert!(calls.is_empty());
    });
}

#[test]
fn cancelling_buffered_start_reply_triggers_cleanup_without_closing_controls() {
    check(async {
        let (send, calls) = async_channel::bounded(1);
        let (control, owner) = channel(config(), Arc::new(Gated(send)));
        let mut host = Box::pin(owner.run());
        {
            let start = control.start();
            futures_util::pin_mut!(start);
            assert!(futures_util::poll!(start.as_mut()).is_pending());
            assert!(futures_util::poll!(host.as_mut()).is_pending());
            started(&calls).await;
            // Successful reply is buffered; the caller never observes it.
            assert!(futures_util::poll!(host.as_mut()).is_pending());
        };
        assert!(futures_util::poll!(host.as_mut()).is_pending());
        let Call::Stop(id, reply) = calls
            .try_recv()
            .expect("abandoned buffered reply must clean up")
        else {
            panic!("expected stop")
        };
        assert_eq!(id, "alpha");
        reply.send(Ok(())).await.unwrap();
        drop(control);
        assert!(!host.await.is_pending());
    });
}

#[test]
fn stop_does_not_wait_for_start_reply_consumption() {
    check(async {
        let (send, calls) = async_channel::bounded(1);
        let (control, owner) = channel(config(), Arc::new(Gated(send)));
        let mut host = Box::pin(owner.run());
        {
            let start = control.start();
            futures_util::pin_mut!(start);
            assert!(futures_util::poll!(start.as_mut()).is_pending());
            assert!(futures_util::poll!(host.as_mut()).is_pending());
            started(&calls).await;
            assert!(futures_util::poll!(host.as_mut()).is_pending());
            let stop = control.stop();
            futures_util::pin_mut!(stop);
            assert!(futures_util::poll!(stop.as_mut()).is_pending());
            assert!(futures_util::poll!(host.as_mut()).is_pending());
            let Call::Stop(id, reply) = calls
                .try_recv()
                .expect("stop must not wait for start consumer")
            else {
                panic!("expected stop")
            };
            assert_eq!(id, "alpha");
            reply.send(Ok(())).await.unwrap();
            assert!(futures_util::poll!(host.as_mut()).is_pending());
            assert_eq!(stop.await, Ok(()));
        };
        drop(control);
        assert!(!host.await.is_pending());
        assert!(calls.is_empty());
    });
}

#[test]
fn abandoned_old_handoff_cannot_stop_replacement() {
    check(async {
        let (send, calls) = async_channel::bounded(1);
        let (control, owner) = channel(config(), Arc::new(Gated(send)));
        let mut host = Box::pin(owner.run());
        let mut old = Box::pin(control.start());
        assert!(futures_util::poll!(old.as_mut()).is_pending());
        assert!(futures_util::poll!(host.as_mut()).is_pending());
        started(&calls).await;
        assert!(futures_util::poll!(host.as_mut()).is_pending());
        {
            let stop = control.stop();
            futures_util::pin_mut!(stop);
            assert!(futures_util::poll!(stop.as_mut()).is_pending());
            assert!(futures_util::poll!(host.as_mut()).is_pending());
            stop_reply(&calls).await.send(Ok(())).await.unwrap();
            assert!(futures_util::poll!(host.as_mut()).is_pending());
            assert_eq!(stop.await, Ok(()));
        };
        let events = {
            let replacement = control.start();
            futures_util::pin_mut!(replacement);
            assert!(futures_util::poll!(replacement.as_mut()).is_pending());
            assert!(futures_util::poll!(host.as_mut()).is_pending());
            started(&calls).await;
            assert!(futures_util::poll!(host.as_mut()).is_pending());
            replacement.await.unwrap()
        };
        drop(old);
        assert!(futures_util::poll!(host.as_mut()).is_pending());
        assert!(
            calls.is_empty(),
            "old hand-off cannot clean up the replacement"
        );
        drop(events);
        drop(control);
        assert!(futures_util::poll!(host.as_mut()).is_pending());
        stop_reply(&calls).await.send(Ok(())).await.unwrap();
        assert!(!host.await.is_pending());
    });
}
