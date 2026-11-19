//! Producer ownership shared by the three native ingress fixtures.

use std::sync::{mpsc, Arc, Condvar, Mutex};
use std::thread::JoinHandle;

/// Holds all four producers until the controller releases them.
pub struct StartGate {
    state: Mutex<StartState>,
    changed: Condvar,
}

/// Counts arrivals and distinguishes normal release from partial-launch cancellation.
struct StartState {
    arrived: usize,
    run: Option<bool>,
}

impl StartGate {
    fn new() -> Self {
        Self {
            state: Mutex::new(StartState {
                arrived: 0,
                run: None,
            }),
            changed: Condvar::new(),
        }
    }

    /// Returns whether this producer can submit its action.
    pub fn wait(&self) -> bool {
        let mut state = self.state.lock().expect("start gate is not poisoned");
        state.arrived += 1;
        self.changed.notify_all();
        let state = self
            .changed
            .wait_while(state, |state| state.run.is_none())
            .expect("start gate is not poisoned");
        state.run.expect("start decision is available")
    }

    /// Releases the complete group from the same start boundary.
    pub fn release(&self) {
        let state = self.state.lock().expect("start gate is not poisoned");
        let mut state = self
            .changed
            .wait_while(state, |state| state.arrived < 4 && state.run.is_none())
            .expect("start gate is not poisoned");
        if state.run.is_none() {
            state.run = Some(true);
        }
        self.changed.notify_all();
    }

    /// Cancels producers that did not reach a complete launch group.
    fn cancel(&self) {
        let mut state = self.state.lock().expect("start gate is not poisoned");
        if state.run.is_none() {
            state.run = Some(false);
        }
        self.changed.notify_all();
    }
}

/// Releases the reducer gate even if setup exits early.
struct Release(Option<mpsc::Sender<()>>);

impl Drop for Release {
    fn drop(&mut self) {
        if let Some(send) = self.0.take() {
            let _ = send.send(());
        }
    }
}

/// Owns producer threads and releases their reducer before joining them.
pub struct Producers {
    release: Option<Release>,
    /// The common boundary before any producer submits an action.
    pub start: Arc<StartGate>,
    /// Every successfully launched producer remains owned until it joins.
    pub workers: Vec<JoinHandle<()>>,
}

impl Producers {
    /// Takes ownership of the reducer release signal.
    pub fn new(release: mpsc::Sender<()>) -> Self {
        Self {
            release: Some(Release(Some(release))),
            start: Arc::new(StartGate::new()),
            workers: Vec::new(),
        }
    }

    /// Cancels unstarted work, then joins every producer before reporting failure.
    pub fn finish(&mut self) -> Result<(), &'static str> {
        self.start.cancel();
        drop(self.release.take());

        let mut panicked = false;
        for worker in self.workers.drain(..) {
            panicked |= worker.join().is_err();
        }
        if panicked {
            Err("producer panicked")
        } else {
            Ok(())
        }
    }
}

impl Drop for Producers {
    fn drop(&mut self) {
        let _ = self.finish();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    use super::*;

    #[test]
    fn partial_launch_drops_and_joins_waiting_producers() {
        let (release, _released) = mpsc::channel();
        let mut producers = Producers::new(release);
        let start = Arc::clone(&producers.start);
        let worker_start = Arc::clone(&start);
        let submitted = Arc::new(AtomicBool::new(false));
        let worker_submitted = Arc::clone(&submitted);
        producers.workers.push(std::thread::spawn(move || {
            if worker_start.wait() {
                worker_submitted.store(true, Ordering::Release);
            }
        }));

        // This models unwind after only one of four thread launches succeeds.
        let (finished, completion) = mpsc::channel();
        let cleanup = std::thread::spawn(move || {
            drop(producers);
            finished.send(()).expect("controller receives cleanup");
        });
        let completed = completion.recv_timeout(Duration::from_secs(2));

        if completed.is_err() {
            // Rescue the original five-party barrier before asserting failure.
            // This also releases a future regression that forgets cancellation.
            let rescuers: Vec<_> = (0..3)
                .map(|_| {
                    let start = Arc::clone(&start);
                    std::thread::spawn(move || start.wait())
                })
                .collect();
            start.release();
            for rescuer in rescuers {
                rescuer.join().expect("rescue producer joined");
            }
        }
        cleanup.join().expect("cleanup thread joined");

        assert!(completed.is_ok(), "partial launch blocked producer cleanup");
        assert!(
            !submitted.load(Ordering::Acquire),
            "cancelled producer submitted work"
        );
    }

    #[test]
    fn cancellation_also_rejects_a_producer_that_arrives_later() {
        let start = StartGate::new();
        start.cancel();
        assert!(!start.wait());
    }

    #[test]
    fn finish_joins_all_workers_before_reporting_a_panic() {
        let (release, released) = mpsc::channel();
        let mut producers = Producers::new(release);
        let completed = Arc::new(AtomicBool::new(false));
        let worker_completed = Arc::clone(&completed);
        producers.workers.push(std::thread::spawn(|| {
            panic!("intentional producer panic");
        }));
        producers.workers.push(std::thread::spawn(move || {
            worker_completed.store(true, Ordering::Release);
        }));

        assert_eq!(producers.finish(), Err("producer panicked"));
        assert_eq!(released.try_recv(), Ok(()));
        assert!(completed.load(Ordering::Acquire));
        assert!(producers.workers.is_empty());
    }
}
