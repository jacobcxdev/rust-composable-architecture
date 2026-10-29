//! Checks host-owned stop outcomes after caller cancellation or Store shutdown.
//!
//! These fixtures do not establish native resource release or durable cleanup.

use std::time::Duration;

use rca_core::{Effect, ReduceContext, Reducer, Store};
use rca_tokio::TokioRuntime;
use tokio::sync::{mpsc, oneshot};

/// A progress watchdog, not a cleanup latency contract.
const LIMIT: Duration = Duration::from_secs(15);

/// Preserves host completion and transport rejection as different domain outcomes.
#[derive(Clone, Debug, Eq, PartialEq)]
enum Outcome {
    Stopped,
    StopFailed(String),
    WrongSession,
    Full,
    Unavailable,
}

/// Records control progress without treating admission as completed cleanup.
#[derive(Clone, Debug, Eq, PartialEq)]
enum Phase {
    Running,
    Stopping,
    Stopped,
    Failed(Outcome),
}

/// Starts one stop attempt or records the outcome returned by its host.
enum Action {
    Stop,
    Finished(Outcome),
}

/// Carries session authority and a caller-owned reply channel, not cleanup ownership.
struct Request {
    session: u64,
    reply: oneshot::Sender<Outcome>,
}

/// Sends bounded control requests while the external host owns admitted work.
struct Control {
    session: u64,
    commands: mpsc::Sender<Request>,
}

impl Reducer for Control {
    type State = Phase;
    type Action = Action;

    fn reduce(
        &mut self,
        state: &mut Phase,
        action: &Action,
        _context: &mut ReduceContext<'_>,
    ) -> Effect<Action> {
        match action {
            Action::Stop if *state == Phase::Running => {
                *state = Phase::Stopping;
                let commands = self.commands.clone();
                let session = self.session;
                Effect::run(move |sender| async move {
                    let (reply, response) = oneshot::channel();
                    let outcome = match commands.try_send(Request { session, reply }) {
                        Ok(()) => response.await.unwrap_or(Outcome::Unavailable),
                        Err(mpsc::error::TrySendError::Full(_)) => Outcome::Full,
                        Err(mpsc::error::TrySendError::Closed(_)) => Outcome::Unavailable,
                    };
                    let _ = sender.send(Action::Finished(outcome)).await;
                })
            }
            Action::Finished(outcome) if *state == Phase::Stopping => {
                *state = match outcome {
                    Outcome::Stopped => Phase::Stopped,
                    outcome => Phase::Failed(outcome.clone()),
                };
                Effect::none()
            }
            _ => Effect::none(),
        }
    }
}

/// Keeps the host's outcome even when no caller remains to receive it.
#[derive(Debug, Eq, PartialEq)]
struct Report {
    outcome: Outcome,
    release_confirmed: bool,
    reply_delivered: bool,
}

/// Accepts session 7 and awaits completion independently of the reply receiver.
async fn run_host(
    mut commands: mpsc::Receiver<Request>,
    accepted: oneshot::Sender<()>,
    completion: oneshot::Receiver<Outcome>,
) -> Report {
    let request = commands.recv().await.expect("one controlled stop request");
    let outcome = if request.session == 7 {
        accepted.send(()).expect("test observes admission");
        // This future belongs to the host, not to the caller awaiting its reply.
        completion.await.unwrap_or(Outcome::Unavailable)
    } else {
        Outcome::WrongSession
    };
    let mut report = Report {
        outcome: outcome.clone(),
        release_confirmed: outcome == Outcome::Stopped,
        reply_delivered: false,
    };
    report.reply_delivered = request.reply.send(outcome).is_ok();
    report
}

/// Chooses whether the caller survives long enough to receive the host's outcome.
#[derive(Clone, Copy)]
enum Caller {
    Waiting,
    Cancelled,
    Shutdown,
}

/// Checks admission, caller termination, host completion and final control state.
async fn qualify(caller: Caller, outcome: Outcome) {
    tokio::time::timeout(LIMIT, async {
        let (commands, receive) = mpsc::channel(1);
        let (accepted, admission) = oneshot::channel();
        let (complete, completion) = oneshot::channel();
        let host = tokio::spawn(run_host(receive, accepted, completion));
        let store = Store::new(
            Phase::Running,
            Control {
                session: 7,
                commands,
            },
            TokioRuntime::current().unwrap(),
        );
        let task = store.send(Action::Stop).await.unwrap();
        admission.await.unwrap();
        assert_eq!(store.read(Clone::clone).await.unwrap(), Phase::Stopping);
        assert!(!host.is_finished(), "admission is not completed cleanup");

        let mut live_store = Some(store);
        let mut stopped_state = None;
        match caller {
            Caller::Waiting => {}
            Caller::Cancelled => {
                task.cancel();
                task.finish().await;
                assert!(task.is_cancelled());
            }
            Caller::Shutdown => {
                stopped_state = Some(
                    live_store
                        .take()
                        .unwrap()
                        .shutdown_into_state()
                        .await
                        .unwrap(),
                );
                task.finish().await;
                assert_eq!(stopped_state, Some(Phase::Stopping));
            }
        }
        assert!(
            !host.is_finished(),
            "caller lifetime must not finish cleanup"
        );
        complete
            .send(outcome.clone())
            .expect("host still owns operation");
        let report = host.await.unwrap();
        assert_eq!(
            report,
            Report {
                outcome: outcome.clone(),
                release_confirmed: outcome == Outcome::Stopped,
                reply_delivered: matches!(caller, Caller::Waiting),
            }
        );
        task.finish().await;
        let state = if let Some(store) = live_store {
            store.shutdown_into_state().await.unwrap()
        } else {
            stopped_state.unwrap()
        };
        let expected = match caller {
            Caller::Waiting => match outcome {
                Outcome::Stopped => Phase::Stopped,
                outcome => Phase::Failed(outcome),
            },
            Caller::Cancelled | Caller::Shutdown => Phase::Stopping,
        };
        assert_eq!(state, expected);
    })
    .await
    .expect("cleanup ownership progress");
}

#[tokio::test]
async fn waiting_caller_receives_only_completed_cleanup() {
    qualify(Caller::Waiting, Outcome::Stopped).await;
    qualify(
        Caller::Waiting,
        Outcome::StopFailed("remote \"busy\"\nretry".into()),
    )
    .await;
}

#[tokio::test]
async fn cancelled_caller_leaves_cleanup_and_outcome_with_host() {
    qualify(Caller::Cancelled, Outcome::Stopped).await;
    qualify(
        Caller::Cancelled,
        Outcome::StopFailed("remote \"busy\"\nretry".into()),
    )
    .await;
}

#[tokio::test]
async fn store_shutdown_leaves_cleanup_and_outcome_with_host() {
    qualify(Caller::Shutdown, Outcome::Stopped).await;
    qualify(
        Caller::Shutdown,
        Outcome::StopFailed("remote \"busy\"\nretry".into()),
    )
    .await;
}

#[tokio::test]
async fn stale_session_cannot_stop_current_session() {
    tokio::time::timeout(LIMIT, async {
        let (commands, receive) = mpsc::channel(1);
        let (accepted, admission) = oneshot::channel();
        let (complete, completion) = oneshot::channel();
        let host = tokio::spawn(run_host(receive, accepted, completion));
        let store = Store::new(
            Phase::Running,
            Control {
                session: 6,
                commands,
            },
            TokioRuntime::current().unwrap(),
        );
        store.send(Action::Stop).await.unwrap().finish().await;
        assert_eq!(
            store.shutdown_into_state().await.unwrap(),
            Phase::Failed(Outcome::WrongSession)
        );
        assert_eq!(
            host.await.unwrap(),
            Report {
                outcome: Outcome::WrongSession,
                release_confirmed: false,
                reply_delivered: true,
            }
        );
        assert!(
            admission.await.is_err(),
            "stale token must not enter cleanup"
        );
        assert!(complete.is_closed(), "stop result must not be consumed");
    })
    .await
    .expect("stale session progress");
}

#[tokio::test]
async fn full_queue_reports_rejection_without_replacing_accepted_work() {
    tokio::time::timeout(LIMIT, async {
        let (commands, mut receive) = mpsc::channel(1);
        let (reply, _response) = oneshot::channel();
        commands.try_send(Request { session: 99, reply }).unwrap();
        let store = Store::new(
            Phase::Running,
            Control {
                session: 7,
                commands,
            },
            TokioRuntime::current().unwrap(),
        );
        store.send(Action::Stop).await.unwrap().finish().await;
        assert_eq!(
            store.shutdown_into_state().await.unwrap(),
            Phase::Failed(Outcome::Full)
        );
        assert_eq!(receive.try_recv().unwrap().session, 99);
        assert!(receive.try_recv().is_err());
    })
    .await
    .expect("full queue progress");
}

#[tokio::test]
async fn closed_host_is_not_successful_cleanup() {
    tokio::time::timeout(LIMIT, async {
        let (commands, receive) = mpsc::channel(1);
        drop(receive);
        let store = Store::new(
            Phase::Running,
            Control {
                session: 7,
                commands,
            },
            TokioRuntime::current().unwrap(),
        );
        store.send(Action::Stop).await.unwrap().finish().await;
        assert_eq!(
            store.shutdown_into_state().await.unwrap(),
            Phase::Failed(Outcome::Unavailable)
        );
    })
    .await
    .expect("closed host progress");
}

#[tokio::test]
async fn admitted_request_without_reply_is_not_successful_cleanup() {
    tokio::time::timeout(LIMIT, async {
        let (commands, mut receive) = mpsc::channel(1);
        let store = Store::new(
            Phase::Running,
            Control {
                session: 7,
                commands,
            },
            TokioRuntime::current().unwrap(),
        );
        let task = store.send(Action::Stop).await.unwrap();
        let request = receive.recv().await.expect("request was admitted");
        assert_eq!(request.session, 7);
        assert_eq!(store.read(Clone::clone).await.unwrap(), Phase::Stopping);

        // Losing an admitted reply differs from rejecting the command at admission.
        drop(request);
        drop(receive);
        task.finish().await;
        assert_eq!(
            store.shutdown_into_state().await.unwrap(),
            Phase::Failed(Outcome::Unavailable)
        );
    })
    .await
    .expect("lost reply progress");
}

#[tokio::test]
async fn host_retains_unavailable_outcome_when_completion_source_closes() {
    tokio::time::timeout(LIMIT, async {
        let (commands, receive) = mpsc::channel(1);
        let (accepted, admission) = oneshot::channel();
        let (complete, completion) = oneshot::channel();
        let host = tokio::spawn(run_host(receive, accepted, completion));
        let store = Store::new(
            Phase::Running,
            Control {
                session: 7,
                commands,
            },
            TokioRuntime::current().unwrap(),
        );
        let task = store.send(Action::Stop).await.unwrap();
        admission.await.unwrap();
        assert_eq!(store.read(Clone::clone).await.unwrap(), Phase::Stopping);

        drop(complete);
        assert_eq!(
            host.await.unwrap(),
            Report {
                outcome: Outcome::Unavailable,
                release_confirmed: false,
                reply_delivered: true,
            }
        );
        task.finish().await;
        assert_eq!(
            store.shutdown_into_state().await.unwrap(),
            Phase::Failed(Outcome::Unavailable)
        );
    })
    .await
    .expect("closed completion source progress");
}
