//! Observes native legacy stream ownership before stopping the host.

use super::common::{self, Finite, Probe, Trace, TIMEOUT};
use composable::{effects::Task, Effects, Reducer, Store};
use futures::StreamExt;
use std::sync::{atomic::Ordering, mpsc};

#[derive(Clone, Copy)]
enum Ownership {
    Cancel,
    Drop,
    Detach,
}

enum Action {
    Start,
    Handle(Ownership),
    Stream(common::Action),
    Inspect(mpsc::Sender<(Vec<u64>, usize, bool)>),
}

struct StreamReducer {
    stream: Option<Finite>,
    task: Option<Task>,
    trace: Trace,
    expected: Vec<u64>,
    acknowledged: Option<mpsc::Sender<Result<(), &'static str>>>,
    handled: mpsc::Sender<()>,
    cancel_on_first: bool,
    probe: Probe,
}

impl Reducer for StreamReducer {
    type Action = Action;
    type Output = Trace;

    fn reduce(&mut self, action: Action, effects: impl Effects<Action>) {
        match action {
            Action::Start => {
                let stream = self.stream.take().unwrap();
                // Unlike effects.stream, task exposes the cancellation-owning handle.
                self.task = Some(effects.task(stream.map(Action::Stream)));
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
            Action::Stream(action) => {
                self.probe.record_first_value(&action);
                if self.cancel_on_first && matches!(action, common::Action::Value(_)) {
                    self.cancel_on_first = false;
                    self.task.take().unwrap().cancel();
                    self.handled.send(()).unwrap();
                }
                self.trace
                    .reduce(&action, &self.expected, &mut self.acknowledged);
            }
            Action::Inspect(snapshot) => {
                snapshot
                    .send((
                        self.trace.observed.clone(),
                        self.trace.terminals,
                        self.trace.invalid,
                    ))
                    .unwrap();
            }
        }
    }
}

impl From<StreamReducer> for Trace {
    fn from(reducer: StreamReducer) -> Self {
        reducer.trace
    }
}

fn exercise(ownership: Ownership, values: &[u64], prefix: &[u64]) {
    let probe = Probe::default();
    let (drop_tx, drop_rx) = mpsc::channel();
    *probe.blocking_drop_notice.lock().unwrap() = Some(drop_tx);
    let (stream, control) =
        common::pending_stream_after(common::actions(values), probe.clone(), prefix.len());
    let (ack_tx, ack_rx) = mpsc::channel();
    let (handled_tx, handled_rx) = mpsc::channel();
    let store = Store::with_initial(StreamReducer {
        stream: Some(stream),
        task: None,
        trace: Trace::default(),
        expected: values.to_vec(),
        acknowledged: Some(ack_tx),
        handled: handled_tx,
        cancel_on_first: false,
        probe: probe.clone(),
    });
    store.send(Action::Start);
    control.wait_pending();
    let inspect = || {
        let (snapshot_tx, snapshot_rx) = mpsc::channel();
        store.send(Action::Inspect(snapshot_tx));
        snapshot_rx
            .recv_timeout(TIMEOUT)
            .expect("live reducer inspected")
    };
    assert_eq!(inspect(), (prefix.to_vec(), 0, false));
    store.send(Action::Handle(ownership));
    handled_rx
        .recv_timeout(TIMEOUT)
        .expect("ownership operation reduced");
    let continues = matches!(ownership, Ownership::Detach);
    let control = if continues {
        control.release();
        None
    } else {
        Some(control)
    };
    // Host teardown must not supply this observation. The cancelled branch
    // still owns its external gate/waker and never releases the stream.
    drop_rx
        .recv_timeout(TIMEOUT)
        .expect("stream dropped before host teardown");
    assert_eq!(probe.drops.load(Ordering::Acquire), 1);
    assert_eq!(probe.exhausted.load(Ordering::Acquire), continues);
    if continues {
        ack_rx
            .recv_timeout(TIMEOUT)
            .expect("terminal reduced")
            .expect("exact terminal trace");
    } else {
        assert!(matches!(ack_rx.try_recv(), Err(mpsc::TryRecvError::Empty)));
    }
    let expected = if continues {
        (values.to_vec(), 1, false)
    } else {
        (prefix.to_vec(), 0, false)
    };
    assert_eq!(inspect(), expected);
    let trace: Trace = store.into_inner();
    if continues {
        trace.verify(values, &probe).unwrap();
    }
    assert_eq!((trace.observed, trace.terminals, trace.invalid), expected);
    drop(control);
}

#[test]
fn explicit_cancel_drops_pending_stream_before_host_teardown() {
    if crate::child_process::run_in_child(
        "stream_ownership::explicit_cancel_drops_pending_stream_before_host_teardown",
    ) {
        return;
    }

    for values in [&[][..], &[3, 1, 4, 2][..]] {
        exercise(Ownership::Cancel, values, &[]);
    }
}

#[test]
fn dropping_task_drops_pending_stream_before_host_teardown() {
    if crate::child_process::run_in_child(
        "stream_ownership::dropping_task_drops_pending_stream_before_host_teardown",
    ) {
        return;
    }

    for values in [&[][..], &[3, 1, 4, 2][..]] {
        exercise(Ownership::Drop, values, &[]);
    }
}

#[test]
fn detached_pending_stream_completes_after_release_without_handle() {
    if crate::child_process::run_in_child(
        "stream_ownership::detached_pending_stream_completes_after_release_without_handle",
    ) {
        return;
    }

    for values in [&[][..], &[3, 1, 4, 2][..]] {
        exercise(Ownership::Detach, values, &[]);
    }
}

#[test]
fn cancelling_partially_delivered_stream_preserves_committed_prefix() {
    if crate::child_process::run_in_child(
        "stream_ownership::cancelling_partially_delivered_stream_preserves_committed_prefix",
    ) {
        return;
    }

    exercise(Ownership::Cancel, &[3, 1, 4, 2], &[3]);
}

#[test]
fn dropping_partially_delivered_stream_preserves_committed_prefix() {
    if crate::child_process::run_in_child(
        "stream_ownership::dropping_partially_delivered_stream_preserves_committed_prefix",
    ) {
        return;
    }

    exercise(Ownership::Drop, &[3, 1, 4, 2], &[3]);
}

#[test]
fn detaching_partially_delivered_stream_preserves_remaining_delivery() {
    if crate::child_process::run_in_child(
        "stream_ownership::detaching_partially_delivered_stream_preserves_remaining_delivery",
    ) {
        return;
    }

    exercise(Ownership::Detach, &[3, 1, 4, 2], &[3]);
}

#[test]
fn cancelling_at_first_value_preserves_already_queued_second_value() {
    if crate::child_process::run_in_child(
        "stream_ownership::cancelling_at_first_value_preserves_already_queued_second_value",
    ) {
        return;
    }

    let probe = Probe::default();
    let (drop_tx, drop_rx) = mpsc::channel();
    *probe.blocking_drop_notice.lock().unwrap() = Some(drop_tx);
    let (stream, control) =
        common::pending_stream_after(common::actions(common::VALUES), probe.clone(), 2);
    let (ack_tx, ack_rx) = mpsc::channel();
    let (handled_tx, handled_rx) = mpsc::channel();
    let store = Store::with_initial(StreamReducer {
        stream: Some(stream),
        task: None,
        trace: Trace::default(),
        expected: common::VALUES.to_vec(),
        acknowledged: Some(ack_tx),
        handled: handled_tx,
        cancel_on_first: true,
        probe: probe.clone(),
    });
    store.send(Action::Start);
    // The producer queues both values before yielding to the reducer. Holding
    // this gate prevents exhaustion from masquerading as cancellation.
    control.wait_pending();
    handled_rx.recv_timeout(TIMEOUT).unwrap();
    assert_eq!(
        *probe.first_value.lock().unwrap(),
        Some(common::Progress {
            yielded: 2,
            exhausted: false,
            drops: 0
        })
    );
    drop_rx.recv_timeout(TIMEOUT).unwrap();
    assert_eq!(probe.drops.load(Ordering::Acquire), 1);
    assert!(!probe.exhausted.load(Ordering::Acquire));
    let (snapshot_tx, snapshot_rx) = mpsc::channel();
    store.send(Action::Inspect(snapshot_tx));
    assert_eq!(
        snapshot_rx.recv_timeout(TIMEOUT).unwrap(),
        (vec![3, 1], 0, false)
    );
    assert!(matches!(ack_rx.try_recv(), Err(mpsc::TryRecvError::Empty)));
    let trace: Trace = store.into_inner();
    assert_eq!(
        (trace.observed, trace.terminals, trace.invalid),
        (vec![3, 1], 0, false)
    );
    drop(control);
}
