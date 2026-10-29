//! Preserves each legacy engine's cancellation-owning task handle.

use super::future_ownership::{pending_future, verify_probe, FiniteFuture, Probe, TIMEOUT};
use composable::{effects::Task, Effects, Reducer, Store};
use std::sync::mpsc;

#[derive(Clone, Copy)]
enum Ownership {
    Cancel,
    Drop,
    Detach,
}

enum Action {
    Start,
    Handle(Ownership),
    Completed(u64),
}

struct FutureReducer {
    future: Option<FiniteFuture>,
    task: Option<Task>,
    trace: Vec<u64>,
    acknowledged: Option<mpsc::Sender<bool>>,
    handled: mpsc::Sender<()>,
}

impl Reducer for FutureReducer {
    type Action = Action;
    type Output = Vec<u64>;

    fn reduce(&mut self, action: Action, effects: impl Effects<Action>) {
        match action {
            Action::Start => {
                let future = self.future.take().unwrap();
                self.task = Some(effects.task(futures::stream::once(async move {
                    Action::Completed(future.await)
                })));
            }
            Action::Handle(ownership) => {
                let task = self.task.take().unwrap();
                match ownership {
                    Ownership::Cancel => task.cancel(),
                    Ownership::Drop => drop(task),
                    Ownership::Detach => task.detach(),
                }
                self.handled.send(()).unwrap();
            }
            Action::Completed(value) => {
                self.trace.push(value);
                if let Some(acknowledged) = self.acknowledged.take() {
                    acknowledged.send(self.trace == [37]).unwrap();
                }
            }
        }
    }
}

impl From<FutureReducer> for Vec<u64> {
    fn from(reducer: FutureReducer) -> Self {
        reducer.trace
    }
}

fn exercise(ownership: Ownership) {
    let probe = Probe::default();
    let (future, control) = pending_future(probe.clone());
    let (ack_tx, ack_rx) = mpsc::channel();
    let (handled_tx, handled_rx) = mpsc::channel();
    let store = Store::with_initial(FutureReducer {
        future: Some(future),
        task: None,
        trace: Vec::new(),
        acknowledged: Some(ack_tx),
        handled: handled_tx,
    });
    store.send(Action::Start);
    control.wait_pending();
    store.send(Action::Handle(ownership));
    handled_rx
        .recv_timeout(TIMEOUT)
        .expect("ownership operation reduced");
    let continues = matches!(ownership, Ownership::Detach);
    if continues {
        control.release();
    }
    // This must precede into_inner: host teardown cannot supply the proof.
    control.wait_dropped();
    verify_probe(&probe, continues);
    if continues {
        assert!(ack_rx.recv_timeout(TIMEOUT).expect("terminal reduced"));
    } else {
        assert!(matches!(ack_rx.try_recv(), Err(mpsc::TryRecvError::Empty)));
    }
    let state: Vec<u64> = store.into_inner();
    assert_eq!(state, if continues { vec![37] } else { vec![] });
    // Retained control/waker resources are not claimed to be destroyed above.
    drop(control);
}

#[test]
fn explicit_cancel_drops_pending_future_before_host_teardown() {
    if crate::child_process::run_in_child(
        "ownership::explicit_cancel_drops_pending_future_before_host_teardown",
    ) {
        return;
    }

    exercise(Ownership::Cancel);
}

#[test]
fn dropping_task_cancels_pending_future_before_host_teardown() {
    if crate::child_process::run_in_child(
        "ownership::dropping_task_cancels_pending_future_before_host_teardown",
    ) {
        return;
    }

    exercise(Ownership::Drop);
}

#[test]
fn detached_task_completes_after_release_without_handle() {
    if crate::child_process::run_in_child(
        "ownership::detached_task_completes_after_release_without_handle",
    ) {
        return;
    }

    exercise(Ownership::Detach);
}
