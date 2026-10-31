#[path = "../../common.rs"]
mod common;

#[cfg(test)]
#[expect(
    dead_code,
    reason = "native tests use a subset of the shared child-process helper"
)]
mod child_process {
    include!(concat!(env!("CARGO_MANIFEST_DIR"), "/../child_process.rs"));
}

#[path = "../../ready_future.rs"]
mod ready_future;

#[cfg(feature = "allocation-measurement")]
#[path = "../../ready_allocations.rs"]
mod ready_allocations;

mod ready;

#[cfg(test)]
#[path = "../../future_ownership.rs"]
mod future_ownership;
#[cfg(test)]
mod ownership;

use common::{Action, Finite, Probe, Trace};
use futures::StreamExt;
use rca_core::{Effect, ReduceContext, Reducer, Store};
use rca_tokio::TokioRuntime;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    mpsc, Arc,
};

struct StreamReducer {
    #[cfg(test)]
    probe: Probe,
    expected: Vec<u64>,
    stream: Option<Finite>,
    ack: Option<mpsc::Sender<Result<(), &'static str>>>,
    failures: Arc<AtomicUsize>,
}

impl Reducer for StreamReducer {
    type State = Trace;
    type Action = Action;
    fn reduce(
        &mut self,
        state: &mut Trace,
        action: &Action,
        _: &mut ReduceContext<'_>,
    ) -> Effect<Action> {
        if matches!(action, Action::Start) {
            let stream = self.stream.take().expect("one start per fixture");
            let failures = Arc::clone(&self.failures);
            return Effect::run(move |sender| deliver(sender, stream, failures));
        }
        #[cfg(test)]
        self.probe.record_first_value(action);
        state.reduce(action, &self.expected, &mut self.ack);
        Effect::none()
    }
}

async fn deliver(
    sender: rca_core::EffectSender<Action>,
    mut stream: Finite,
    failures: Arc<AtomicUsize>,
) {
    while let Some(action) = stream.next().await {
        if sender.send(action).await.is_err() {
            failures.fetch_add(1, Ordering::Release);
            return;
        }
    }
}

fn run(values: &[u64]) -> Result<Trace, &'static str> {
    run_actions(values, common::actions(values))
}

fn run_actions(values: &[u64], actions: Vec<Action>) -> Result<Trace, &'static str> {
    let probe = Probe::default();
    run_stream(values, Finite::new(actions, probe.clone()), &probe)
}

fn run_stream(values: &[u64], stream: Finite, probe: &Probe) -> Result<Trace, &'static str> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| "runtime build failed")?;
    let failures = Arc::new(AtomicUsize::new(0));
    let (tx, rx) = mpsc::channel();
    let state = runtime.block_on(async {
        let store = Store::builder(
            Trace::default(),
            StreamReducer {
                #[cfg(test)]
                probe: probe.clone(),
                expected: values.to_vec(),
                stream: Some(stream),
                ack: Some(tx),
                failures: Arc::clone(&failures),
            },
            TokioRuntime::current().map_err(|_| "runtime adapter failed")?,
        )
        .build()
        .map_err(|_| "store build failed")?;
        let task = store
            .send(Action::Start)
            .await
            .map_err(|_| "start delivery failed")?;
        let completion = tokio::time::timeout(common::TIMEOUT, task.finish())
            .await
            .map_err(|_| "producer completion timed out");
        let acknowledged = rx
            .try_recv()
            .map_err(|_| "terminal acknowledgement missing");
        let state = store
            .shutdown_into_state()
            .await
            .map_err(|_| "state extraction failed")?;
        completion?;
        acknowledged??;
        Ok(state)
    })?;
    drop(runtime);
    if failures.load(Ordering::Acquire) != 0 {
        return Err("stream delivery failed");
    }
    state.verify(values, probe)?;
    Ok(state)
}

fn main() {
    if std::env::args_os().len() == 2
        && std::env::args_os().nth(1).as_deref()
            == Some(std::ffi::OsStr::new("--measure-ready-future-allocations"))
    {
        #[cfg(feature = "allocation-measurement")]
        match ready_allocations::run("rca", "causal-finish") {
            Ok(ledger) => println!("{ledger}"),
            Err(error) => {
                eprintln!("{error}");
                std::process::exit(1);
            }
        }
        #[cfg(not(feature = "allocation-measurement"))]
        {
            eprintln!("--measure-ready-future-allocations requires allocation-measurement");
            std::process::exit(1);
        }
        #[cfg(feature = "allocation-measurement")]
        return;
    }
    if std::env::args_os().len() == 2
        && std::env::args_os().nth(1).as_deref() == Some(std::ffi::OsStr::new("--ready-future"))
    {
        match ready::run(ready_future::Fault::None) {
            Ok((trace, probe)) => trace.emit("rca", "causal-finish", &probe),
            Err(error) => {
                eprintln!("{error}");
                std::process::exit(1);
            }
        }
        return;
    }
    if std::env::args_os().len() != 1 {
        eprintln!("expected no arguments or --ready-future or --measure-ready-future-allocations");
        std::process::exit(1);
    }
    for (case, values) in [("empty", &[][..]), ("ordered", common::VALUES)] {
        match run(values) {
            Ok(trace) => common::emit("rca", case, values, &trace),
            Err(error) => {
                eprintln!("{error}");
                std::process::exit(1);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    struct PendingFixture {
        store: super::Store<super::Trace, super::Action>,
        probe: super::Probe,
        control: super::common::PendingControl,
        ack: super::mpsc::Receiver<Result<(), &'static str>>,
        failures: super::Arc<super::AtomicUsize>,
    }

    fn pending_fixture(ready_items: usize) -> PendingFixture {
        use super::*;
        let probe = Probe::default();
        let (stream, control) = common::pending_stream_after(
            common::actions(common::VALUES),
            probe.clone(),
            ready_items,
        );
        let (tx, ack) = mpsc::channel();
        let failures = Arc::new(AtomicUsize::new(0));
        let store = Store::builder(
            Trace::default(),
            StreamReducer {
                probe: probe.clone(),
                expected: common::VALUES.to_vec(),
                stream: Some(stream),
                ack: Some(tx),
                failures: Arc::clone(&failures),
            },
            TokioRuntime::current().unwrap(),
        )
        .build()
        .unwrap();
        PendingFixture {
            store,
            probe,
            control,
            ack,
            failures,
        }
    }

    fn cancelled_stream_trace(ready_items: usize, expected: &[u64]) {
        use super::*;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let fixture = pending_fixture(ready_items);
            let task = fixture.store.send(Action::Start).await.unwrap();
            let (cancel_tx, cancel_rx) = futures::channel::oneshot::channel();
            let controller = std::thread::spawn(move || {
                fixture.control.wait_pending();
                assert!(cancel_tx.send((task, fixture.control)).is_ok());
            });
            let (task, control) = tokio::time::timeout(common::TIMEOUT, cancel_rx)
                .await
                .expect("cancellation watchdog")
                .expect("controller sent task");
            controller.join().expect("controller joined");
            assert_eq!(
                fixture
                    .store
                    .read(|state| state.observed.clone())
                    .await
                    .unwrap(),
                expected
            );
            task.cancel();
            assert!(task.is_cancelled());
            tokio::time::timeout(common::TIMEOUT, task.finish())
                .await
                .expect("cancelled task finished");
            assert_eq!(fixture.probe.drops.load(Ordering::Acquire), 1);
            assert!(!fixture.probe.exhausted.load(Ordering::Acquire));
            // The external controller still owns its gate and stored waker.
            // Stream destruction above does not claim those are destroyed.
            drop(control);
            let live = fixture
                .store
                .read(|state| (state.observed.clone(), state.terminals, state.invalid))
                .await
                .unwrap();
            assert_eq!(live, (expected.to_vec(), 0, false));
            let trace = fixture.store.shutdown_into_state().await.unwrap();
            assert_eq!(trace.observed, expected);
            assert_eq!(trace.terminals, 0);
            assert!(!trace.invalid);
            assert!(matches!(
                fixture.ack.try_recv(),
                Err(mpsc::TryRecvError::Disconnected)
            ));
            assert_eq!(fixture.failures.load(Ordering::Acquire), 0);
        });
    }

    #[test]
    fn cancelling_pending_stream_drops_it_without_delivering_actions() {
        if crate::child_process::run_in_child(
            "tests::cancelling_pending_stream_drops_it_without_delivering_actions",
        ) {
            return;
        }

        cancelled_stream_trace(0, &[]);
    }

    #[test]
    fn cancelling_partially_delivered_stream_preserves_committed_prefix() {
        if crate::child_process::run_in_child(
            "tests::cancelling_partially_delivered_stream_preserves_committed_prefix",
        ) {
            return;
        }

        cancelled_stream_trace(1, &[3]);
    }

    #[test]
    fn dropping_all_observation_handles_keeps_pending_stream_alive() {
        if crate::child_process::run_in_child(
            "tests::dropping_all_observation_handles_keeps_pending_stream_alive",
        ) {
            return;
        }

        use super::*;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let fixture = pending_fixture(0);
            let (drop_tx, drop_rx) = futures::channel::oneshot::channel();
            *fixture.probe.drop_notice.lock().unwrap() = Some(drop_tx);
            let task = fixture.store.send(Action::Start).await.unwrap();
            let (tx, rx) = futures::channel::oneshot::channel();
            let controller = std::thread::spawn(move || {
                fixture.control.wait_pending();
                drop(task);
                fixture.control.release();
                let acknowledged = fixture.ack.recv_timeout(common::TIMEOUT);
                tx.send(acknowledged)
                    .expect("driver receives acknowledgement");
            });
            tokio::time::timeout(common::TIMEOUT, rx)
                .await
                .expect("terminal watchdog")
                .expect("controller sent acknowledgement")
                .expect("terminal acknowledgement arrived")
                .expect("terminal validated trace");
            controller.join().expect("controller joined");
            tokio::time::timeout(common::TIMEOUT, drop_rx)
                .await
                .expect("stream drop watchdog")
                .expect("stream destroyed naturally");
            let trace = fixture.store.shutdown_into_state().await.unwrap();
            assert_eq!(trace.observed, [3, 1, 4, 2]);
            trace.verify(&[3, 1, 4, 2], &fixture.probe).unwrap();
            assert_eq!(fixture.failures.load(Ordering::Acquire), 0);
        });
    }

    #[test]
    fn producer_progress_at_first_value_entry() {
        if crate::child_process::run_in_child("tests::producer_progress_at_first_value_entry") {
            return;
        }

        let probe = super::Probe::default();
        let stream = super::Finite::new(super::common::actions(&[3, 1, 4, 2]), probe.clone());
        let trace =
            super::run_stream(&[3, 1, 4, 2], stream, &probe).expect("complete ordered stream");
        assert_eq!(trace.observed, [3, 1, 4, 2]);
        assert_eq!(
            *probe.first_value.lock().unwrap(),
            Some(super::common::Progress {
                yielded: 1,
                exhausted: false,
                drops: 0
            })
        );
    }

    #[test]
    fn pending_stream_resumes_after_controller_wake() {
        if crate::child_process::run_in_child("tests::pending_stream_resumes_after_controller_wake")
        {
            return;
        }

        for values in [&[][..], &[3, 1, 4, 2][..]] {
            let probe = super::Probe::default();
            let (stream, control) =
                super::common::pending_stream(super::common::actions(values), probe.clone());
            let controller = std::thread::spawn(move || {
                control.wait_pending();
                control.release();
            });
            let result = super::run_stream(values, stream, &probe);
            controller.join().expect("controller joined");
            assert_eq!(result.expect("woken stream completed").observed, values);
        }
    }

    #[test]
    fn ordered_and_empty_streams_complete() {
        if crate::child_process::run_in_child("tests::ordered_and_empty_streams_complete") {
            return;
        }

        assert_eq!(super::run(&[]).unwrap().observed, Vec::<u64>::new());
        assert_eq!(super::run(&[3, 1, 4, 2]).unwrap().observed, [3, 1, 4, 2]);
    }

    #[test]
    fn malformed_real_streams_are_rejected() {
        if crate::child_process::run_in_child("tests::malformed_real_streams_are_rejected") {
            return;
        }

        use super::Action::{Terminal, Value};
        for input in [
            vec![Value(3), Terminal],
            vec![Value(1), Value(3), Terminal],
            vec![Value(3), Value(3), Value(1), Terminal],
            vec![Terminal, Value(3), Value(1)],
            vec![Value(3), Value(1), Terminal, Terminal],
        ] {
            assert!(super::run_actions(&[3, 1], input).is_err());
        }
    }

    #[test]
    fn rejected_delivery_drops_stream_without_claiming_exhaustion() {
        if crate::child_process::run_in_child(
            "tests::rejected_delivery_drops_stream_without_claiming_exhaustion",
        ) {
            return;
        }

        use super::*;
        struct Capture(Option<mpsc::Sender<rca_core::EffectSender<Action>>>);
        impl Reducer for Capture {
            type State = ();
            type Action = Action;
            fn reduce(
                &mut self,
                _: &mut (),
                _: &Action,
                _: &mut ReduceContext<'_>,
            ) -> Effect<Action> {
                let tx = self.0.take().unwrap();
                Effect::run(move |sender| async move {
                    tx.send(sender).unwrap();
                })
            }
        }
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let (tx, rx) = mpsc::channel();
            let store = Store::builder((), Capture(Some(tx)), TokioRuntime::current().unwrap())
                .build()
                .unwrap();
            store.send(Action::Start).await.unwrap().finish().await;
            let sender = rx.try_recv().unwrap();
            store.shutdown_into_state().await.unwrap();
            let probe = Probe::default();
            let failures = Arc::new(AtomicUsize::new(0));
            super::deliver(
                sender,
                Finite::new(common::actions(&[3, 1]), probe.clone()),
                Arc::clone(&failures),
            )
            .await;
            assert_eq!(failures.load(Ordering::Acquire), 1);
            assert_eq!(probe.drops.load(Ordering::Acquire), 1);
            assert!(!probe.exhausted.load(Ordering::Acquire));
            assert!(Trace::default().verify(&[3, 1], &probe).is_err());
        });
    }
}
