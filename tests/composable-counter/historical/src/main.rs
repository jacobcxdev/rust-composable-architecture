//! Native committed-send measurements with independently checked work counts.

#[path = "../../common.rs"]
mod common;

use composable::{Effects, Reducer, Store};

struct Counter {
    value: u64,
    gate: Option<common::Gate>,
}

impl Reducer for Counter {
    type Action = u8;
    type Output = u64;

    fn reduce(&mut self, action: Self::Action, _effects: impl Effects<Self::Action>) {
        if let Some(gate) = &self.gate {
            gate.before_reduce();
        }
        self.value += u64::from(action);
        if let Some(gate) = &self.gate {
            gate.reduced();
        }
    }
}

impl From<Counter> for u64 {
    fn from(counter: Counter) -> Self {
        counter.value
    }
}

/// Owns the native store and its host until extraction completes.
struct Adapter {
    store: Store<Counter>,
}

impl Adapter {
    fn new(gate: Option<common::Gate>) -> Self {
        Self {
            store: Store::with_initial(Counter { value: 0, gate }),
        }
    }

    fn send_committed(&self) {
        self.store.sync(1_u8);
    }

    #[cfg(test)]
    fn send_enqueue_only(&self) {
        self.store.send(1_u8);
    }

    fn finish(self) -> u64 {
        self.store.into_inner()
    }
}

fn run(mode: common::Mode) -> Result<String, &'static str> {
    #[cfg(feature = "allocation-measurement")]
    if mode == common::Mode::Allocations {
        return common::allocations::run(
            "rca-0.7",
            || Ok(Adapter::new(None)),
            |adapter| {
                adapter.send_committed();
                Ok(())
            },
            |adapter| Ok(adapter.finish()),
        );
    }
    if mode == common::Mode::Lifecycle {
        return common::run_lifecycle(
            "rca-0.7",
            || Ok(Adapter::new(None)),
            |adapter| {
                adapter.send_committed();
                Ok(())
            },
            |adapter| Ok(adapter.finish()),
        );
    }
    let measured = mode == common::Mode::Operations;
    let adapter = Adapter::new(None);
    let (work, timing) = common::run_work(
        || {
            adapter.send_committed();
            Ok(())
        },
        measured,
    )?;
    let final_state = adapter.finish();
    let ledger = work.finish("rca-0.7", final_state, true, true)?;
    Ok(match timing {
        Some(timing) => timing.finish(&ledger),
        None => ledger,
    })
}

fn main() {
    match common::mode(std::env::args_os().skip(1)).and_then(run) {
        Ok(ledger) => println!("{ledger}"),
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;
    use std::thread;

    use super::{common, Adapter};

    #[test]
    fn lifecycle_mode_completes_one_increment_and_teardown() {
        let output = super::run(common::Mode::Lifecycle).expect("completed lifecycle");
        assert!(output.starts_with("{\"schema\":\"composable-counter-lifecycle-v1\""));
        assert!(output.contains("\"operations_completed\":1,"));
        assert!(output.contains("\"final\":1,"));
        assert!(output.contains("\"state_extracted\":true,\"host_stopped\":true,"));
    }

    #[test]
    fn extraction_observes_gated_reducer_teardown() {
        let (gate, control) = common::drop_gate();
        let (returned_tx, returned_rx) = mpsc::channel();
        let worker = thread::spawn(move || {
            let adapter = Adapter::new(Some(gate));
            adapter.send_committed();
            let value = adapter.finish();
            returned_tx.send(value).expect("controller receives state");
        });

        let entered = control.wait_entered();
        // This snapshot detects an already-reported return, not every possible
        // pre-emption between finish returning and its notification being sent.
        let premature = returned_rx.try_recv().ok();
        control.release();
        let value = returned_rx.recv_timeout(common::PROOF_TIMEOUT);
        worker.join().expect("adapter worker joined");

        assert!(entered, "reducer teardown did not reach the gate");
        assert!(premature.is_none(), "extraction returned before teardown");
        assert_eq!(value.expect("extraction completed"), 1);
        assert!(!control.timed_out());
    }

    #[test]
    fn exact_counter_work() {
        assert_eq!(
            super::run(common::Mode::Correctness).expect("completed work"),
            "{\"schema\":\"composable-counter-work-v1\",\"engine\":\"rca-0.7\",\"warmup_expected\":20,\"warmup_attempted\":20,\"warmup_completed\":20,\"operations_expected\":10000,\"operations_attempted\":10000,\"operations_completed\":10000,\"initial\":0,\"final\":10020,\"errors\":0,\"state_extracted\":true,\"host_stopped\":true}"
        );
    }

    #[test]
    fn committed_send_observes_reducer_completion_at_return() {
        let (gate, control) = common::gate();
        let completed = control.completions();
        let (returned_tx, returned_rx) = mpsc::channel();
        let worker = thread::spawn(move || {
            let adapter = Adapter::new(Some(gate));
            adapter.send_committed();
            let observed = completed.load(std::sync::atomic::Ordering::Acquire);
            returned_tx
                .send(observed)
                .expect("controller receives result");
            adapter.finish()
        });

        let entered = control.wait_entered();
        let premature = returned_rx.try_recv().ok();
        control.release();
        let returned = returned_rx.recv_timeout(common::PROOF_TIMEOUT);
        let final_state = worker.join().expect("worker joined");

        assert!(entered, "reducer did not enter before bounded wait");
        assert!(
            premature.is_none(),
            "send returned while reduction was gated"
        );
        assert_eq!(returned.expect("send returned"), 1);
        assert_eq!(final_state, 1);
        assert!(!control.timed_out());
    }

    #[test]
    fn enqueue_only_control_is_rejected_at_return() {
        let (gate, control) = common::gate();
        let completed = control.completions();
        let (returned_tx, returned_rx) = mpsc::channel();
        let worker = thread::spawn(move || {
            let adapter = Adapter::new(Some(gate));
            adapter.send_enqueue_only();
            let observed = completed.load(std::sync::atomic::Ordering::Acquire);
            returned_tx
                .send(observed)
                .expect("controller receives result");
            adapter.finish()
        });

        let returned = returned_rx.recv_timeout(common::PROOF_TIMEOUT);
        let entered = control.wait_entered();
        control.release();
        let final_state = worker.join().expect("worker joined");

        assert!(entered, "reducer did not enter before bounded wait");
        let observed = returned.expect("enqueue call returned");
        assert_eq!(observed, 0);
        assert_ne!(observed, 1);
        assert_eq!(final_state, 1);
        assert!(!control.timed_out());
    }
}
