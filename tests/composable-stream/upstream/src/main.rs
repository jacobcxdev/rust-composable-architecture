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

#[path = "../../legacy_ready.rs"]
mod ready;

#[cfg(test)]
#[path = "../../future_ownership.rs"]
mod future_ownership;
#[cfg(test)]
#[path = "../../legacy_ownership.rs"]
mod ownership;
#[cfg(test)]
#[path = "../../legacy_stream_ownership.rs"]
mod stream_ownership;

use common::{Action, Finite, Probe, Trace};
use composable::{Effects, Reducer, Store};
use std::sync::mpsc;

struct StreamReducer {
    #[cfg(test)]
    probe: Probe,
    trace: Trace,
    expected: Vec<u64>,
    stream: Option<Finite>,
    ack: Option<mpsc::Sender<Result<(), &'static str>>>,
}

impl Reducer for StreamReducer {
    type Action = Action;
    type Output = Trace;
    fn reduce(&mut self, action: Action, effects: impl Effects<Action>) {
        if matches!(action, Action::Start) {
            effects.stream(self.stream.take().expect("one start per fixture"));
        } else {
            #[cfg(test)]
            self.probe.record_first_value(&action);
            self.trace.reduce(&action, &self.expected, &mut self.ack);
        }
    }
}

impl From<StreamReducer> for Trace {
    fn from(reducer: StreamReducer) -> Self {
        reducer.trace
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
    let (tx, rx) = mpsc::channel();
    let store = Store::with_initial(StreamReducer {
        #[cfg(test)]
        probe: probe.clone(),
        trace: Trace::default(),
        expected: values.to_vec(),
        stream: Some(stream),
        ack: Some(tx),
    });
    store.send(Action::Start);
    let acknowledged = rx
        .recv_timeout(common::TIMEOUT)
        .map_err(|_| "terminal acknowledgement missing");
    // Extract only after terminal acknowledgement. Even an invalid workload
    // is joined before reporting its validation error.
    let state: Trace = store.into_inner();
    acknowledged??;
    state.verify(values, probe)?;
    Ok(state)
}

fn main() {
    if std::env::args_os().len() == 2
        && std::env::args_os().nth(1).as_deref()
            == Some(std::ffi::OsStr::new("--measure-ready-future-allocations"))
    {
        #[cfg(feature = "allocation-measurement")]
        match ready_allocations::run("composable", "result-acknowledgement") {
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
            Ok((trace, probe)) => trace.emit("composable", "result-acknowledgement", &probe),
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
            Ok(trace) => common::emit("composable", case, values, &trace),
            Err(error) => {
                eprintln!("{error}");
                std::process::exit(1);
            }
        }
    }
}

#[cfg(test)]
mod tests {
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
                yielded: 5,
                exhausted: true,
                drops: 1
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
}
