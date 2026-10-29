//! A literal sum application without measurement code or allocator instrumentation.

use rca_core::{Effect, ReduceContext, Reducer, Store};
use rca_tokio::TokioRuntime;

/// Implements the counter, with a release witness only in tests.
struct Counter {
    #[cfg(test)]
    dropped: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl Reducer for Counter {
    type State = u64;
    type Action = u8;
    fn reduce(
        &mut self,
        state: &mut u64,
        action: &u8,
        _context: &mut ReduceContext<'_>,
    ) -> Effect<u8> {
        *state += u64::from(*action);
        Effect::none()
    }
}

/// Returns the final sum only after state extraction and native host teardown.
fn run(
    inputs: &[u8],
    #[cfg(test)] dropped: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> Result<u64, &'static str> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| "runtime construction failed")?;
    let store = {
        let _entered = runtime.enter();
        let scheduler = TokioRuntime::current().map_err(|_| "runtime adapter failed")?;
        Store::builder(
            0_u64,
            Counter {
                #[cfg(test)]
                dropped,
            },
            scheduler,
        )
        .mailbox_capacity(1)
        .build()
        .map_err(|_| "store construction failed")?
    };
    for &input in inputs {
        let receipt = runtime
            .block_on(store.send(input))
            .map_err(|_| "committed send failed")?;
        drop(receipt);
    }
    let value = runtime
        .block_on(store.shutdown_into_state())
        .map_err(|_| "state extraction failed")?;
    drop(runtime);

    checked(value)
}

const INPUTS: [u8; 7] = [3, 1, 7, 2, 9, 4, 16];

#[cfg(test)]
impl Drop for Counter {
    fn drop(&mut self) {
        self.dropped
            .store(true, std::sync::atomic::Ordering::Release);
    }
}

/// Checks a literal expected total, not another copy of the reducer algorithm.
fn checked(value: u64) -> Result<u64, &'static str> {
    if value == 42 {
        Ok(value)
    } else {
        Err("unexpected final counter")
    }
}

fn main() -> std::process::ExitCode {
    match run(
        &INPUTS,
        #[cfg(test)]
        std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
    ) {
        Ok(value) => {
            println!("{value}");
            std::process::ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("{error}");
            std::process::ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc,
    };
    use std::time::Duration;

    fn exercise(inputs: &[u8]) -> Result<u64, &'static str> {
        let inputs = inputs.to_vec();
        let dropped = Arc::new(AtomicBool::new(false));
        let witness = Arc::clone(&dropped);
        let (send, receive) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            let result = super::run(&inputs, witness);
            send.send(result).expect("test receives result");
        });
        let result = receive
            .recv_timeout(Duration::from_secs(10))
            .expect("native workload completed");
        worker.join().expect("workload worker joined");
        assert!(dropped.load(Ordering::Acquire), "reducer was not released");
        result
    }

    #[test]
    fn native_workload_checks_literal_total_and_releases_reducer() {
        assert_eq!(exercise(&super::INPUTS), Ok(42));
    }

    #[test]
    fn omitted_input_is_rejected_after_release() {
        assert_eq!(
            exercise(&[3, 7, 2, 9, 4, 16]),
            Err("unexpected final counter")
        );
    }

    #[test]
    fn duplicate_input_is_rejected_after_release() {
        assert_eq!(
            exercise(&[3, 1, 7, 2, 9, 4, 16, 3]),
            Err("unexpected final counter")
        );
    }
}
