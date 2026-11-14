//! Exercises mailbox admission, without `TestStore`'s pre-reduction assertion gate.

use std::{
    cell::Cell,
    collections::VecDeque,
    future::poll_fn,
    pin::Pin,
    sync::{Arc, Mutex, mpsc},
    task::{Context, Poll},
    time::Duration,
};

use futures_core::Stream;
use rca_core::{
    Effect, MonotonicInstant, ReduceContext, Reducer, Runtime, RuntimeDriver, RuntimeFuture, Store,
};
use rca_example_mcmd_server::{backend, proxy, server};

const LIMIT: Duration = Duration::from_secs(10);

thread_local! {
    // Consumed after the same task poll returns, never by another worker.
    static OLD_SESSION_POLL: Cell<Option<bool>> = const { Cell::new(None) };
}

/// Reports the task poll containing a marked stream poll, without changing its result.
#[derive(Debug)]
struct Driver {
    handle: tokio::runtime::Handle,
    origin: tokio::time::Instant,
    polled: mpsc::Sender<(bool, bool)>,
}

impl RuntimeDriver for Driver {
    fn spawn(&self, mut task: RuntimeFuture) {
        let polled = self.polled.clone();
        drop(self.handle.spawn(poll_fn(move |cx| {
            let result = task.as_mut().poll(cx);
            if let Some(yielded) = OLD_SESSION_POLL.replace(None) {
                let _ = polled.send((yielded, result.is_pending()));
            }
            result
        })));
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

/// Marks old-session progress on the worker that polls the real event stream.
struct Session {
    events: Pin<Box<async_channel::Receiver<proxy::ProxyEvent>>>,
    old: bool,
    pending_notified: bool,
}

impl Stream for Session {
    type Item = proxy::ProxyEvent;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let result = self.events.as_mut().poll_next(cx);
        if self.old && matches!(result, Poll::Ready(Some(_))) {
            OLD_SESSION_POLL.set(Some(true));
        } else if self.old && result.is_pending() && !self.pending_notified {
            self.pending_notified = true;
            OLD_SESSION_POLL.set(Some(false));
        }
        result
    }
}

struct Client {
    sessions: Mutex<VecDeque<Session>>,
    starts: Mutex<Vec<String>>,
}

impl proxy::ProxyClient for Client {
    fn acquire(&self, _: proxy::Config) -> proxy::AcquireFuture {
        let session = self.sessions.lock().unwrap().pop_front().unwrap();
        Box::pin(async move { Ok(Box::pin(session) as proxy::ProxySession) })
    }
}

impl backend::BackendClient for Client {
    fn start(&self, server_id: String) -> backend::BackendFuture {
        self.starts.lock().unwrap().push(server_id);
        // Avoid introducing successful-start polling work into this queue proof.
        Box::pin(async { Err(backend::BackendError("controlled failure".into())) })
    }

    fn stop(&self, _: String) -> backend::BackendFuture {
        panic!("no backend stop expected")
    }
}

#[derive(Debug)]
enum Action {
    Server(server::Action),
    Block,
    Probe,
}

/// Blocks one actor turn and records actual reductions around the inner reducer.
struct Wrapper<R> {
    inner: R,
    entered: mpsc::Sender<()>,
    release: mpsc::Receiver<()>,
    reduced: mpsc::Sender<&'static str>,
    joins: Arc<Mutex<usize>>,
}

impl<R: Reducer<State = server::State, Action = server::Action>> Reducer for Wrapper<R> {
    type State = server::State;
    type Action = Action;

    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &Action,
        context: &mut ReduceContext<'_>,
    ) -> Effect<Action> {
        match action {
            Action::Server(action) => {
                if matches!(action, server::Action::Proxy(proxy::Action::Event(_))) {
                    *self.joins.lock().unwrap() += 1;
                }
                let effect = self
                    .inner
                    .reduce(state, action, context)
                    .map(Action::Server);
                let notice = match action {
                    server::Action::Proxy(proxy::Action::Ready) => Some("ready"),
                    server::Action::Backend(backend::Action::StartFinished(_)) => {
                        Some("backend finished")
                    }
                    _ => None,
                };
                if let Some(notice) = notice {
                    let _ = self.reduced.send(notice);
                }
                effect
            }
            Action::Block => {
                self.entered.send(()).unwrap();
                self.release
                    .recv_timeout(LIMIT)
                    .expect("controller releases actor");
                Effect::none()
            }
            Action::Probe => Effect::none(),
        }
    }
}

/// Releases the blocked actor before runtime teardown, including assertion unwind.
struct Release(mpsc::Sender<()>);

impl Drop for Release {
    fn drop(&mut self) {
        let _ = self.0.send(());
    }
}

/// Bounds asynchronous Store operations; actor-side blocking has its own timeout.
fn drive<F: Future>(host: &tokio::runtime::Runtime, future: F) -> F::Output {
    host.block_on(async {
        tokio::time::timeout(LIMIT, future)
            .await
            .expect("bounded store operation")
    })
}

fn proxy_action(action: proxy::Action) -> Action {
    Action::Server(server::Action::Proxy(action))
}

#[test]
fn stop_invalidates_real_old_event_already_admitted_to_mailbox() {
    let host = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_time()
        .build()
        .unwrap();
    let (polled_tx, polled_rx) = mpsc::channel();
    let runtime = Runtime::new(Driver {
        handle: host.handle().clone(),
        origin: tokio::time::Instant::now(),
        polled: polled_tx,
    });
    let (old_tx, old_rx) = async_channel::bounded(1);
    let (new_tx, new_rx) = async_channel::bounded(1);
    let client = Arc::new(Client {
        sessions: Mutex::new(VecDeque::from([
            Session {
                events: Box::pin(old_rx),
                old: true,
                pending_notified: false,
            },
            Session {
                events: Box::pin(new_rx),
                old: false,
                pending_notified: false,
            },
        ])),
        starts: Mutex::new(Vec::new()),
    });
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let release = Release(release_tx);
    let (reduced_tx, reduced_rx) = mpsc::channel();
    let joins = Arc::new(Mutex::new(0));
    let store = Store::builder(
        server::State::new(proxy::Config {
            server_id: "alpha".into(),
            host: "localhost".into(),
            java_ports: vec![25565],
            bedrock_ports: vec![],
        }),
        Wrapper {
            inner: server::reducer(),
            entered: entered_tx,
            release: release_rx,
            reduced: reduced_tx,
            joins: Arc::clone(&joins),
        },
        runtime,
    )
    .mailbox_capacity(2)
    .with_dependency::<proxy::ProxyKey>(Arc::clone(&client) as Arc<dyn proxy::ProxyClient>)
    .with_dependency::<backend::BackendKey>(Arc::clone(&client) as Arc<dyn backend::BackendClient>)
    .build()
    .unwrap();
    let old_task = drive(&host, store.send(proxy_action(proxy::Action::Start))).unwrap();
    assert_eq!(reduced_rx.recv_timeout(LIMIT).unwrap(), "ready");
    // Let the acquisition/Ready handshake finish and the real stream park
    // before blocking an actor worker. This avoids stranding an initial poll
    // in that worker's Tokio-local scheduling slot.
    assert_eq!(polled_rx.recv_timeout(LIMIT).unwrap(), (false, true));
    let blocked = store.try_send(Action::Block).unwrap();
    entered_rx.recv_timeout(LIMIT).unwrap();
    let stopped = store.try_send(proxy_action(proxy::Action::Stop)).unwrap();
    old_tx.try_send(proxy::ProxyEvent::JoinObserved).unwrap();
    // The actor cannot drain Stop. Only the real old-session effect can occupy
    // the second slot. Wait until its marked poll returns, then probe ONCE.
    let pending = polled_rx.recv_timeout(LIMIT);
    let probe = store.try_send(Action::Probe);
    // Release even when subsequent assertions fail; Drop also handles earlier unwind.
    drop(release);
    assert_eq!(pending.unwrap(), (true, true));
    assert!(probe.unwrap_err().is_full());
    drop(drive(&host, blocked.finish()).unwrap());
    drop(drive(&host, stopped.finish()).unwrap());
    let stopped_state = drive(&host, store.read(Clone::clone)).unwrap();
    assert_eq!(
        *joins.lock().unwrap(),
        0,
        "old event must not reach reducer"
    );
    assert_eq!(stopped_state.proxy.phase, proxy::Phase::Stopped);
    assert_eq!(stopped_state.backend.phase, backend::Phase::Stopped);
    assert_eq!(stopped_state.backend.target, backend::Target::Stopped);
    assert!(client.starts.lock().unwrap().is_empty());
    drive(&host, old_task.finish());
    assert!(old_tx.is_closed());

    let new_task = drive(&host, store.send(proxy_action(proxy::Action::Start))).unwrap();
    assert_eq!(reduced_rx.recv_timeout(LIMIT).unwrap(), "ready");
    new_tx.try_send(proxy::ProxyEvent::JoinObserved).unwrap();
    assert_eq!(reduced_rx.recv_timeout(LIMIT).unwrap(), "backend finished");
    assert_eq!(*client.starts.lock().unwrap(), ["alpha"]);
    assert_eq!(*joins.lock().unwrap(), 1);
    let fresh = drive(&host, store.read(Clone::clone)).unwrap();
    assert_eq!(fresh.proxy.phase, proxy::Phase::Listening);
    assert_eq!(fresh.backend.phase, backend::Phase::Error);
    assert_eq!(
        fresh.backend.last_error.as_deref(),
        Some("controlled failure")
    );
    drop(drive(&host, store.send(proxy_action(proxy::Action::Stop))).unwrap());
    drive(&host, new_task.finish());
    assert!(new_tx.is_closed());
    drive(&host, store.shutdown()).unwrap();
}
