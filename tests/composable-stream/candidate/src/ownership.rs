//! Exercises root causal ownership, not scoped cancellation identities.

use super::future_ownership::{pending_future, verify_probe, FiniteFuture, Probe, TIMEOUT};
use rca_core::{Effect, ReduceContext, Reducer, Store};
use rca_tokio::TokioRuntime;
use std::sync::mpsc;

enum Action {
    Start,
    Completed(u64),
}

struct FutureReducer {
    future: Option<FiniteFuture>,
    acknowledged: Option<mpsc::Sender<bool>>,
    post_commit: Option<mpsc::Sender<()>>,
}

impl Reducer for FutureReducer {
    type State = Vec<u64>;
    type Action = Action;

    fn reduce(
        &mut self,
        state: &mut Vec<u64>,
        action: &Action,
        _: &mut ReduceContext<'_>,
    ) -> Effect<Action> {
        match action {
            Action::Start => {
                let future = self.future.take().unwrap();
                let post_commit = self.post_commit.take().unwrap();
                Effect::run(move |sender| async move {
                    sender
                        .send(Action::Completed(future.await))
                        .await
                        .expect("live completion delivery");
                    // This observes send's commit acknowledgement, not task termination.
                    post_commit
                        .send(())
                        .expect("controller awaits committed delivery");
                })
            }
            Action::Completed(value) => {
                state.push(*value);
                if let Some(acknowledged) = self.acknowledged.take() {
                    acknowledged.send(state == &[37]).unwrap();
                }
                Effect::none()
            }
        }
    }
}

fn exercise(cancel: bool) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let probe = Probe::default();
        let (future, control) = pending_future(probe.clone());
        let (ack_tx, ack_rx) = mpsc::channel();
        let (post_commit_tx, post_commit_rx) = mpsc::channel();
        let store = Store::builder(
            Vec::new(),
            FutureReducer {
                future: Some(future),
                acknowledged: Some(ack_tx),
                post_commit: Some(post_commit_tx),
            },
            TokioRuntime::current().unwrap(),
        )
        .build()
        .unwrap();
        let task = store.send(Action::Start).await.unwrap();
        let (tx, rx) = futures::channel::oneshot::channel();
        let controller = std::thread::spawn(move || {
            control.wait_pending();
            let task = if cancel {
                task.cancel();
                assert!(task.is_cancelled());
                Some(task)
            } else {
                drop(task); // No observation handle remains.
                control.release();
                None
            };
            control.wait_dropped();
            verify_probe(&probe, !cancel);
            if cancel {
                assert!(matches!(ack_rx.try_recv(), Err(mpsc::TryRecvError::Empty)));
            } else {
                assert!(ack_rx.recv_timeout(TIMEOUT).expect("terminal reduced"));
                post_commit_rx
                    .recv_timeout(TIMEOUT)
                    .expect("effect observed committed delivery");
            }
            assert!(tx.send((task, control)).is_ok());
        });
        let (task, control) = tokio::time::timeout(TIMEOUT, rx)
            .await
            .expect("ownership watchdog")
            .expect("controller completed");
        controller.join().expect("controller joined");
        if let Some(task) = task {
            tokio::time::timeout(TIMEOUT, task.finish())
                .await
                .expect("cancelled causal task finished");
        }
        let expected = if cancel { vec![] } else { vec![37] };
        assert_eq!(store.read(Clone::clone).await.unwrap(), expected);
        assert_eq!(store.shutdown_into_state().await.unwrap(), expected);
        drop(control);
    });
}

#[test]
fn explicit_cancel_drops_pending_future_before_host_teardown() {
    if crate::child_process::run_in_child(
        "ownership::explicit_cancel_drops_pending_future_before_host_teardown",
    ) {
        return;
    }

    exercise(true);
}

#[test]
fn dropping_all_observation_handles_preserves_pending_future() {
    if crate::child_process::run_in_child(
        "ownership::dropping_all_observation_handles_preserves_pending_future",
    ) {
        return;
    }

    exercise(false);
}
