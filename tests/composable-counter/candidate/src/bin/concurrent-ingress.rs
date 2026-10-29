//! Native overload semantics with a gated reducer and literal final trace.

#[path = "../../../concurrent.rs"]
mod concurrent;

use concurrent::Producers;
use rca_core::{Effect, ReduceContext, Reducer, Store};
use rca_tokio::TokioRuntime;
use std::future::Future;
use std::sync::Mutex;
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    mpsc, Arc,
};
use std::task::{Context, Poll, Wake, Waker};
use std::time::Duration;

/// A deadlock watchdog, not a latency requirement.
const TIMEOUT: Duration = Duration::from_secs(10);

/// Separates the reducer gate, producer work and the final commit barrier.
enum Action {
    Gate(mpsc::Sender<()>, mpsc::Receiver<()>),
    Item(u8),
    Barrier,
}

fn apply(
    action: &Action,
    trace: &mut Vec<(u8, u64)>,
    commits: &AtomicUsize,
    gate_failed: &AtomicBool,
) {
    match action {
        Action::Gate(entered, release) => {
            let _ = entered.send(());
            if release.recv_timeout(TIMEOUT).is_err() {
                gate_failed.store(true, Ordering::Release);
            }
        }
        Action::Item(id) => {
            trace.push((*id, 3 * u64::from(*id) + 7));
            commits.fetch_add(1, Ordering::Release);
        }
        Action::Barrier => {}
    }
}

/// Checks exact IDs and values without assuming inter-producer order.
fn verify(
    mut trace: Vec<(u8, u64)>,
    blocked: usize,
    gate_failed: bool,
) -> Result<(), &'static str> {
    if gate_failed {
        return Err("gate deadline expired");
    }
    if blocked != 0 {
        return Err("enqueue is not completion");
    }
    trace.sort_unstable();
    if trace != [(0, 7), (1, 10), (2, 13), (3, 16)] {
        return Err("incorrect final trace");
    }
    Ok(())
}

/// Records state changes independently of producer admission acknowledgements.
struct Counter {
    commits: Arc<AtomicUsize>,
    gate_failed: Arc<AtomicBool>,
}

impl Reducer for Counter {
    type State = Vec<(u8, u64)>;
    type Action = Action;
    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &Action,
        _context: &mut ReduceContext<'_>,
    ) -> Effect<Action> {
        apply(action, state, &self.commits, &self.gate_failed);
        Effect::none()
    }
}

/// Wakes the controller without introducing another executor into the fixture.
struct ThreadWake(std::thread::Thread);

impl Wake for ThreadWake {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }
}

/// Drives one receipt with a parking waker and a fixed watchdog.
fn wait<F: Future>(future: F) -> Result<F::Output, &'static str> {
    let mut future = std::pin::pin!(future);
    let waker = Waker::from(Arc::new(ThreadWake(std::thread::current())));
    let deadline = std::time::Instant::now() + TIMEOUT;
    loop {
        if let Poll::Ready(value) = future.as_mut().poll(&mut Context::from_waker(&waker)) {
            return Ok(value);
        }
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            return Err("receipt deadline expired");
        }
        std::thread::park_timeout(remaining);
    }
}

/// Shares the controller stop request with the dedicated host future.
type StopState = Arc<Mutex<(bool, Option<Waker>)>>;

/// Requests host shutdown on both successful and failed controller paths.
struct Stop(StopState);

impl Drop for Stop {
    fn drop(&mut self) {
        let waker = {
            let mut state = self.0.lock().unwrap();
            state.0 = true;
            state.1.take()
        };
        if let Some(waker) = waker {
            waker.wake();
        }
    }
}

/// Exercises native admission, committed work and joined host teardown.
fn run(ids: [u8; 4], enqueue_as_completion: bool) -> Result<(), &'static str> {
    let commits = Arc::new(AtomicUsize::new(0));
    let failed = Arc::new(AtomicBool::new(false));
    let host_commits = Arc::clone(&commits);
    let host_failed = Arc::clone(&failed);
    let stop_state = Arc::new(Mutex::new((false, None)));
    let host_stop = Arc::clone(&stop_state);
    let stop = Stop(stop_state);
    let (store_tx, store_rx) = mpsc::channel();
    let host = std::thread::spawn(move || -> Result<Vec<(u8, u64)>, &'static str> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|_| "runtime build failed")?;
        let store = {
            let _entered = runtime.enter();
            Store::builder(
                vec![],
                Counter {
                    commits: host_commits,
                    gate_failed: host_failed,
                },
                TokioRuntime::current().map_err(|_| "runtime adapter failed")?,
            )
            .mailbox_capacity(2)
            .build()
            .map_err(|_| "store build failed")?
        };
        store_tx
            .send(store.clone())
            .map_err(|_| "controller gone")?;
        let result = runtime.block_on(async {
            std::future::poll_fn(|cx| {
                let mut state = host_stop.lock().unwrap();
                if state.0 {
                    Poll::Ready(())
                } else {
                    state.1 = Some(cx.waker().clone());
                    Poll::Pending
                }
            })
            .await;
            store
                .shutdown_into_state()
                .await
                .map_err(|_| "extraction failed")
        });
        drop(runtime);
        result
    });
    let outcome = (|| -> Result<usize, &'static str> {
        let store = store_rx
            .recv_timeout(TIMEOUT)
            .map_err(|_| "host did not start")?;
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let mut producers = Producers::new(release_tx);
        // Hold the reducer before all producers reach the same start boundary.
        let gate = store
            .try_send(Action::Gate(entered_tx, release_rx))
            .map_err(|_| "gate admission failed")?;
        entered_rx
            .recv_timeout(TIMEOUT)
            .map_err(|_| "gate not entered")?;
        let start = Arc::clone(&producers.start);
        let (sent, received) = mpsc::channel();
        for id in ids {
            let store = store.clone();
            let start = Arc::clone(&start);
            let sent = sent.clone();
            let worker = std::thread::Builder::new()
                .spawn(move || {
                    if !start.wait() {
                        return;
                    }
                    let result = store.try_send(Action::Item(id));
                    let _ = sent.send((id, result));
                })
                .map_err(|_| "producer launch failed")?;
            producers.workers.push(worker);
        }
        start.release();
        drop(sent);
        let mut receipts = Vec::new();
        let mut rejected = Vec::new();
        for _ in 0..4 {
            let (id, result) = received
                .recv_timeout(TIMEOUT)
                .map_err(|_| "producer did not return")?;
            match result {
                Ok(receipt) => receipts.push((id, Box::pin(receipt.finish()))),
                Err(error) => {
                    if !error.is_full() {
                        return Err("unexpected admission error");
                    }
                    match error.into_action() {
                        Action::Item(retained) if retained == id => rejected.push(retained),
                        _ => return Err("retained identity changed"),
                    }
                }
            }
        }
        let mut invalid_pending = false;
        for (_, receipt) in &mut receipts {
            if receipt
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_ready()
            {
                invalid_pending = true;
            }
        }
        let blocked = commits.load(Ordering::Acquire);
        producers.finish()?;
        let admission_valid = receipts.len() == 2 && rejected.len() == 2 && !invalid_pending;
        if !admission_valid {
            return Err("incorrect saturation outcome");
        }
        drop(wait(gate.finish())?.map_err(|_| "gate commit failed")?);
        for (_, receipt) in receipts {
            drop(wait(receipt)?.map_err(|_| "admitted commit failed")?);
        }
        for id in rejected {
            drop(wait(store.send(Action::Item(id)))?.map_err(|_| "retry commit failed")?);
        }
        drop(wait(store.send(Action::Barrier))?.map_err(|_| "barrier commit failed")?);
        // Fault injection selects the wrong checkpoint, not a fabricated observed count.
        if enqueue_as_completion && blocked != 4 {
            return Err("enqueue is not completion");
        }
        Ok(blocked)
    })();

    drop(stop);
    let trace = host.join().map_err(|_| "host panicked")??;
    verify(trace, outcome?, failed.load(Ordering::Acquire))
}

fn main() -> std::process::ExitCode {
    match run([0, 1, 2, 3], false) {
        Ok(()) => {
            println!(
                r#"{{"schema":"composable-concurrent-ingress-v1","engine":"rca","producers":4,"policy":"bounded-try-send-retry","capacity":2,"initial_admitted":2,"initial_full":2,"pending_receipts":2,"committed_while_blocked":0,"caller_retries":2,"final_trace":[[0,7],[1,10],[2,13],[3,16]],"state_extracted":true,"host_stopped":true}}"#
            );
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
    #[test]
    fn native_saturation_completes_exact_ids() {
        assert_eq!(super::run([0, 1, 2, 3], false), Ok(()));
    }
    #[test]
    fn enqueue_is_not_completion() {
        assert_eq!(
            super::run([0, 1, 2, 3], true),
            Err("enqueue is not completion")
        );
    }
    #[test]
    fn omitted_id_is_rejected() {
        assert_eq!(
            super::run([0, 1, 2, 4], false),
            Err("incorrect final trace")
        );
    }
    #[test]
    fn duplicate_id_is_rejected() {
        assert_eq!(
            super::run([0, 1, 2, 2], false),
            Err("incorrect final trace")
        );
    }
}
