//! Native committed-send measurements with independently checked work counts.

#[path = "../../common.rs"]
mod common;

use rca_core::{Effect, ReduceContext, Reducer, Store};
use rca_tokio::TokioRuntime;

struct Counter {
    gate: Option<common::Gate>,
}

impl Reducer for Counter {
    type State = u64;
    type Action = u8;

    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &Self::Action,
        _context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        if let Some(gate) = &self.gate {
            gate.before_reduce();
        }
        *state += u64::from(*action);
        if let Some(gate) = &self.gate {
            gate.reduced();
        }
        Effect::none()
    }
}

/// Owns the native store and its host until extraction completes.
struct Adapter {
    runtime: tokio::runtime::Runtime,
    store: Option<Store<u64, u8>>,
}

impl Adapter {
    fn new(gate: Option<common::Gate>) -> Result<Self, &'static str> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|_| "Tokio runtime construction failed")?;
        let store = {
            let _entered = runtime.enter();
            let scheduler = TokioRuntime::current().map_err(|_| "Tokio adapter failed")?;
            Store::builder(0_u64, Counter { gate }, scheduler)
                .mailbox_capacity(1)
                .build()
                .map_err(|_| "Store construction failed")?
        };
        Ok(Self {
            runtime,
            store: Some(store),
        })
    }

    fn send_committed(&mut self) -> Result<(), &'static str> {
        let task = self
            .runtime
            .block_on(
                self.store
                    .as_ref()
                    .ok_or("Store was already extracted")?
                    .send(1_u8),
            )
            .map_err(|_| "committed send failed")?;
        drop(task);
        Ok(())
    }

    #[cfg(test)]
    fn send_enqueue_only(&mut self) -> Result<(), &'static str> {
        let receipt = self
            .store
            .as_ref()
            .ok_or("Store was already extracted")?
            .try_send(1_u8)
            .map_err(|_| "action admission failed")?;
        drop(receipt);
        Ok(())
    }

    fn finish(mut self) -> Result<u64, &'static str> {
        let store = self.store.take().ok_or("Store was already extracted")?;
        let final_state = self
            .runtime
            .block_on(store.shutdown_into_state())
            .map_err(|_| "state extraction failed")?;
        drop(self.runtime);
        Ok(final_state)
    }
}

fn run(mode: common::Mode) -> Result<String, &'static str> {
    #[cfg(feature = "allocation-measurement")]
    if mode == common::Mode::Allocations {
        return common::allocations::run(
            "rca",
            || Adapter::new(None),
            Adapter::send_committed,
            Adapter::finish,
        );
    }
    if mode == common::Mode::Lifecycle {
        return common::run_lifecycle(
            "rca",
            || Adapter::new(None),
            Adapter::send_committed,
            Adapter::finish,
        );
    }
    let measured = mode == common::Mode::Operations;
    let mut adapter = Adapter::new(None)?;
    let (work, timing) = common::run_work(|| adapter.send_committed(), measured)?;
    let final_state = adapter.finish()?;
    let ledger = work.finish("rca", final_state, true, true)?;
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
            let mut adapter = Adapter::new(Some(gate)).expect("adapter built");
            adapter.send_committed().expect("one action commits");
            let value = adapter.finish().expect("state extracted");
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
            "{\"schema\":\"composable-counter-work-v1\",\"engine\":\"rca\",\"warmup_expected\":20,\"warmup_attempted\":20,\"warmup_completed\":20,\"operations_expected\":10000,\"operations_attempted\":10000,\"operations_completed\":10000,\"initial\":0,\"final\":10020,\"errors\":0,\"state_extracted\":true,\"host_stopped\":true}"
        );
    }

    #[test]
    fn committed_send_observes_reducer_completion_at_return() {
        let (gate, control) = common::gate();
        let completed = control.completions();
        let (returned_tx, returned_rx) = mpsc::channel();
        let worker = thread::spawn(move || {
            let mut adapter = Adapter::new(Some(gate)).expect("adapter built");
            adapter.send_committed().expect("committed send");
            let observed = completed.load(std::sync::atomic::Ordering::Acquire);
            returned_tx
                .send(observed)
                .expect("controller receives result");
            adapter.finish().expect("state extracted")
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
            let mut adapter = Adapter::new(Some(gate)).expect("adapter built");
            adapter.send_enqueue_only().expect("enqueue admitted");
            let observed = completed.load(std::sync::atomic::Ordering::Acquire);
            returned_tx
                .send(observed)
                .expect("controller receives result");
            adapter.finish().expect("state extracted")
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
