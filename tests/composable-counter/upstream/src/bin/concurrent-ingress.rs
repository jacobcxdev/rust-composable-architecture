//! Native overload semantics with a gated reducer and literal final trace.

#[path = "../../../concurrent.rs"]
mod concurrent;

use composable::{Effects, Reducer, Store};
use concurrent::Producers;
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    mpsc, Arc,
};
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
    trace: Vec<(u8, u64)>,
    commits: Arc<AtomicUsize>,
    gate_failed: Arc<AtomicBool>,
}

impl Reducer for Counter {
    type Action = Action;
    type Output = Vec<(u8, u64)>;
    fn reduce(&mut self, action: Action, _effects: impl Effects<Action>) {
        apply(&action, &mut self.trace, &self.commits, &self.gate_failed);
    }
}
impl From<Counter> for Vec<(u8, u64)> {
    fn from(counter: Counter) -> Self {
        counter.trace
    }
}

/// Exercises native admission, committed work and joined host teardown.
fn run(ids: [u8; 4], enqueue_as_completion: bool) -> Result<(), &'static str> {
    let commits = Arc::new(AtomicUsize::new(0));
    let failed = Arc::new(AtomicBool::new(false));
    let store = Arc::new(Store::with_initial(Counter {
        trace: vec![],
        commits: Arc::clone(&commits),
        gate_failed: Arc::clone(&failed),
    }));
    let outcome = (|| -> Result<usize, &'static str> {
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let mut producers = Producers::new(release_tx);
        store.send(Action::Gate(entered_tx, release_rx));
        entered_rx
            .recv_timeout(TIMEOUT)
            .map_err(|_| "gate not entered")?;
        let start = Arc::clone(&producers.start);
        let (sent, received) = mpsc::channel();
        for id in ids {
            let store = Arc::clone(&store);
            let start = Arc::clone(&start);
            let sent = sent.clone();
            let worker = std::thread::Builder::new()
                .spawn(move || {
                    if !start.wait() {
                        return;
                    }
                    store.send(Action::Item(id));
                    let _ = sent.send(id);
                })
                .map_err(|_| "producer launch failed")?;
            producers.workers.push(worker);
        }
        start.release();
        drop(sent);
        let mut enqueued = Vec::new();
        for _ in 0..4 {
            enqueued.push(
                received
                    .recv_timeout(TIMEOUT)
                    .map_err(|_| "producer did not return")?,
            );
        }
        let blocked = commits.load(Ordering::Acquire);
        producers.finish()?;
        enqueued.sort_unstable();
        let mut expected = ids;
        expected.sort_unstable();
        if enqueued != expected {
            return Err("producer identity changed");
        }
        // Fault injection treats all returned enqueues as a completion checkpoint.
        if enqueue_as_completion && blocked != 4 {
            return Err("enqueue is not completion");
        }
        Ok(blocked)
    })();
    store.sync(Action::Barrier);
    let store = Arc::try_unwrap(store).map_err(|_| "producer retained store")?;
    let trace = store.into_inner();
    verify(trace, outcome?, failed.load(Ordering::Acquire))
}

fn main() -> std::process::ExitCode {
    match run([0, 1, 2, 3], false) {
        Ok(()) => {
            println!(
                r#"{{"schema":"composable-concurrent-ingress-v1","engine":"composable","producers":4,"policy":"unbounded-enqueue","capacity":null,"initial_admitted":4,"initial_full":0,"pending_receipts":0,"committed_while_blocked":0,"caller_retries":0,"final_trace":[[0,7],[1,10],[2,13],[3,16]],"state_extracted":true,"host_stopped":true}}"#
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
