//! Checks thread-owned resources behind bounded Send control messages.
//!
//! These fixtures do not call native audio APIs.

use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use rca_core::{Effect, ReduceContext, Reducer, Store};
use rca_tokio::TokioRuntime;
use tokio::sync::oneshot;

/// Reports owner-thread identity and release count without sharing a local handle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Outcome {
    Opened(thread::ThreadId),
    Closed(thread::ThreadId, usize),
    Full,
    Unavailable,
}

/// Requests a local operation or records the host's owned summary.
enum Action {
    Open,
    Close,
    Finished(Outcome),
}

/// Sends bounded commands whose reply channels do not own the local resource.
enum Request {
    Open(oneshot::Sender<Outcome>),
    Close(oneshot::Sender<Outcome>),
}

/// Keeps blocking host work off the executor through non-blocking admission.
struct Control(mpsc::SyncSender<Request>);

impl Reducer for Control {
    type State = Vec<Outcome>;
    type Action = Action;

    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &Action,
        _context: &mut ReduceContext<'_>,
    ) -> Effect<Action> {
        let close = match action {
            Action::Open => false,
            Action::Close => true,
            Action::Finished(outcome) => {
                // Only owned summaries cross the host boundary; no local handle escapes.
                state.push(*outcome);
                return Effect::none();
            }
        };
        let host = self.0.clone();
        Effect::run(move |sender| async move {
            let (reply, receive) = oneshot::channel();
            let request = if close {
                Request::Close(reply)
            } else {
                Request::Open(reply)
            };
            let outcome = match host.try_send(request) {
                Ok(()) => receive.await.unwrap_or(Outcome::Unavailable),
                Err(mpsc::TrySendError::Full(_)) => Outcome::Full,
                Err(mpsc::TrySendError::Disconnected(_)) => Outcome::Unavailable,
            };
            let _ = sender.send(Action::Finished(outcome)).await;
        })
    }
}

/// Is deliberately non-Send and asserts destruction on its construction thread.
struct LocalResource {
    owner: Rc<thread::ThreadId>,
    releases: Arc<AtomicUsize>,
}

impl Drop for LocalResource {
    fn drop(&mut self) {
        assert_eq!(*self.owner, thread::current().id());
        self.releases.fetch_add(1, Ordering::SeqCst);
    }
}

/// Checks local release before close acknowledgement, with or without a cancelled opener.
fn qualify_host(cancel_open: bool) {
    // Keep runtime and channel owners inside the scope: failed assertions drop
    // them before the implicit join, and the scope resumes the original panic.
    thread::scope(|scope| {
        // Watchdogs detect stalled progress; they make no latency guarantee.
        let watchdog = Duration::from_secs(15);
        let (commands, receive) = mpsc::sync_channel(1);
        let (opened, was_opened) = oneshot::channel();
        let (release_reply, await_release) = mpsc::sync_channel(1);
        let releases = Arc::new(AtomicUsize::new(0));
        let host_releases = Arc::clone(&releases);
        let worker = scope.spawn(move || {
            let id = thread::current().id();
            let Request::Open(reply) = receive.recv_timeout(watchdog).expect("receive open") else {
                panic!("expected open");
            };
            // Rc makes this resource genuinely !Send; construction and Drop stay here.
            let resource = LocalResource {
                owner: Rc::new(id),
                releases: host_releases,
            };
            opened.send(id).expect("report local construction");
            await_release
                .recv_timeout(watchdog)
                .expect("release open reply");
            let cancelled_reply = reply.send(Outcome::Opened(id)).is_err();
            let Request::Close(reply) = receive.recv_timeout(watchdog).expect("receive close")
            else {
                panic!("expected close");
            };
            let count = Arc::clone(&resource.releases);
            drop(resource);
            reply
                .send(Outcome::Closed(id, count.load(Ordering::SeqCst)))
                .expect("report release");
            cancelled_reply
        });

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("host runtime");
        runtime.block_on(async {
            tokio::time::timeout(watchdog, async {
                let store = Store::new(
                    Vec::new(),
                    Control(commands),
                    TokioRuntime::current().expect("adapt runtime"),
                );
                let open = store.send(Action::Open).await.expect("commit open");
                let resource_thread = was_opened.await.expect("resource constructed");
                assert_ne!(resource_thread, thread::current().id());
                if cancel_open {
                    open.cancel();
                    open.finish().await;
                    assert!(open.is_cancelled());
                    assert_eq!(releases.load(Ordering::SeqCst), 0);
                }
                release_reply.send(()).expect("let host reply");
                open.finish().await;
                store
                    .send(Action::Close)
                    .await
                    .expect("commit close")
                    .finish()
                    .await;
                // The worker can exit only after publishing its post-release reply.
                assert_eq!(worker.join().expect("local host exited"), cancel_open);
                store
                    .send(Action::Open)
                    .await
                    .expect("commit after host loss")
                    .finish()
                    .await;
                let state = store
                    .shutdown_into_state()
                    .await
                    .expect("extract control state");
                let mut expected = Vec::new();
                if !cancel_open {
                    expected.push(Outcome::Opened(resource_thread));
                }
                expected.extend([Outcome::Closed(resource_thread, 1), Outcome::Unavailable]);
                assert_eq!(state, expected);
                assert_eq!(releases.load(Ordering::SeqCst), 1);
            })
            .await
            .expect("control progress watchdog");
        });
    });
}

#[test]
fn local_resource_is_released_before_control_completion() {
    qualify_host(false);
}

#[test]
fn cancelled_caller_does_not_claim_external_resource_teardown() {
    qualify_host(true);
}

#[tokio::test]
async fn full_host_queue_becomes_explicit_domain_outcome() {
    let (commands, receive) = mpsc::sync_channel(1);
    let (reply, _response) = oneshot::channel();
    commands
        .try_send(Request::Close(reply))
        .expect("fill queue");
    let store = Store::new(
        Vec::new(),
        Control(commands),
        TokioRuntime::current().expect("adapt runtime"),
    );
    tokio::time::timeout(Duration::from_secs(15), async {
        store
            .send(Action::Open)
            .await
            .expect("commit open")
            .finish()
            .await;
        assert_eq!(
            store.shutdown_into_state().await.expect("extract state"),
            [Outcome::Full]
        );
    })
    .await
    .expect("full queue must not block control");
    assert!(matches!(
        receive.try_recv().expect("original request retained"),
        Request::Close(_)
    ));
    assert!(receive.try_recv().is_err());
}
