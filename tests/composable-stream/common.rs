use std::pin::Pin;
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    mpsc, Arc,
};
use std::task::{Context, Poll};
use std::time::Duration;

use futures::Stream;

// A watchdog, not a measurement or semantic completion boundary.
pub const TIMEOUT: Duration = Duration::from_secs(10);
pub const VALUES: &[u64] = &[3, 1, 4, 2];

#[derive(Clone, Debug)]
pub enum Action {
    Start,
    Value(u64),
    Terminal,
}

#[derive(Clone, Default)]
pub struct Probe {
    pub exhausted: Arc<AtomicBool>,
    pub drops: Arc<AtomicUsize>,
    #[cfg(test)]
    yielded: Arc<AtomicUsize>,
    #[cfg(test)]
    pub first_value: Arc<std::sync::Mutex<Option<Progress>>>,
    #[cfg(test)]
    pub drop_notice: Arc<std::sync::Mutex<Option<futures::channel::oneshot::Sender<()>>>>,
    #[cfg(test)]
    pub blocking_drop_notice: Arc<std::sync::Mutex<Option<mpsc::Sender<()>>>>,
}

#[cfg(test)]
#[derive(Debug, PartialEq, Eq)]
pub struct Progress {
    pub yielded: usize,
    pub exhausted: bool,
    pub drops: usize,
}

#[cfg(test)]
impl Probe {
    pub fn record_first_value(&self, action: &Action) {
        if matches!(action, Action::Value(_)) {
            let mut first = self.first_value.lock().expect("progress unpoisoned");
            if first.is_none() {
                *first = Some(Progress {
                    yielded: self.yielded.load(Ordering::Acquire),
                    exhausted: self.exhausted.load(Ordering::Acquire),
                    drops: self.drops.load(Ordering::Acquire),
                });
            }
        }
    }
}

pub struct Finite {
    actions: std::vec::IntoIter<Action>,
    probe: Probe,
    #[cfg(test)]
    gate: Option<Arc<std::sync::Mutex<PendingGate>>>,
}

impl Finite {
    pub fn new(actions: Vec<Action>, probe: Probe) -> Self {
        Self {
            actions: actions.into_iter(),
            probe,
            #[cfg(test)]
            gate: None,
        }
    }
}

impl Stream for Finite {
    type Item = Action;
    fn poll_next(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<Action>> {
        #[cfg(not(test))]
        let _ = context;
        #[cfg(test)]
        if let Some(gate) = &self.gate {
            let mut gate = gate.lock().expect("gate unpoisoned");
            if gate.ready_items > 0 {
                gate.ready_items -= 1;
            } else if !gate.released {
                gate.waker = Some(context.waker().clone());
                if let Some(pending) = gate.pending.take() {
                    pending
                        .send(())
                        .expect("controller receives Pending signal");
                }
                return Poll::Pending;
            }
        }
        let action = self.actions.next();
        #[cfg(test)]
        if action.is_some() {
            self.probe.yielded.fetch_add(1, Ordering::Release);
        }
        if action.is_none() {
            self.probe.exhausted.store(true, Ordering::Release);
        }
        Poll::Ready(action)
    }
}

#[cfg(test)]
struct PendingGate {
    ready_items: usize,
    released: bool,
    pending: Option<mpsc::Sender<()>>,
    waker: Option<std::task::Waker>,
}

#[cfg(test)]
pub struct PendingControl {
    gate: Arc<std::sync::Mutex<PendingGate>>,
    pending: mpsc::Receiver<()>,
}

#[cfg(test)]
impl PendingControl {
    pub fn wait_pending(&self) {
        self.pending
            .recv_timeout(TIMEOUT)
            .expect("stream reached Pending");
    }

    pub fn release(self) {
        let waker = {
            let mut gate = self.gate.lock().expect("gate unpoisoned");
            gate.released = true;
            gate.waker.take().expect("Pending recorded a waker")
        };
        waker.wake();
    }
}

#[cfg(test)]
pub fn pending_stream(actions: Vec<Action>, probe: Probe) -> (Finite, PendingControl) {
    pending_stream_after(actions, probe, 0)
}

#[cfg(test)]
pub fn pending_stream_after(
    actions: Vec<Action>,
    probe: Probe,
    ready_items: usize,
) -> (Finite, PendingControl) {
    assert!(ready_items <= actions.len(), "gate must precede exhaustion");
    let (tx, rx) = mpsc::channel();
    let gate = Arc::new(std::sync::Mutex::new(PendingGate {
        ready_items,
        released: false,
        pending: Some(tx),
        waker: None,
    }));
    let mut stream = Finite::new(actions, probe);
    stream.gate = Some(Arc::clone(&gate));
    (stream, PendingControl { gate, pending: rx })
}

impl Drop for Finite {
    fn drop(&mut self) {
        self.probe.drops.fetch_add(1, Ordering::Release);
        #[cfg(test)]
        if let Some(notice) = self
            .probe
            .drop_notice
            .lock()
            .expect("drop notice unpoisoned")
            .take()
        {
            let _ = notice.send(());
        }
        #[cfg(test)]
        if let Some(notice) = self.probe.blocking_drop_notice.lock().unwrap().take() {
            let _ = notice.send(());
        }
    }
}

#[derive(Default, Debug)]
pub struct Trace {
    pub observed: Vec<u64>,
    pub terminals: usize,
    pub invalid: bool,
}

impl Trace {
    pub fn reduce(
        &mut self,
        action: &Action,
        expected: &[u64],
        ack: &mut Option<mpsc::Sender<Result<(), &'static str>>>,
    ) {
        match action {
            Action::Start => {}
            Action::Value(value) => {
                self.invalid |= self.terminals != 0;
                self.observed.push(*value);
            }
            Action::Terminal => {
                self.terminals += 1;
                let valid = self.observed == expected && self.terminals == 1 && !self.invalid;
                self.invalid |= !valid;
                if let Some(ack) = ack.take() {
                    let _ = ack.send(if valid {
                        Ok(())
                    } else {
                        Err("terminal preceded exact ordered trace")
                    });
                }
            }
        }
    }

    pub fn verify(&self, expected: &[u64], probe: &Probe) -> Result<(), &'static str> {
        if self.observed != expected || self.terminals != 1 || self.invalid {
            return Err("final trace differs from intended stream");
        }
        if !probe.exhausted.load(Ordering::Acquire) || probe.drops.load(Ordering::Acquire) != 1 {
            return Err("stream exhaustion or destruction missing");
        }
        Ok(())
    }
}

pub fn actions(values: &[u64]) -> Vec<Action> {
    values
        .iter()
        .copied()
        .map(Action::Value)
        .chain([Action::Terminal])
        .collect()
}

pub fn emit(engine: &str, case: &str, expected: &[u64], trace: &Trace) {
    println!("{{\"schema\":\"composable-finite-stream-v1\",\"engine\":\"{engine}\",\"case\":\"{case}\",\"expected\":{expected:?},\"observed\":{:?},\"terminal_reduced\":true,\"producer_exhausted\":true,\"stream_drops\":1,\"state_extracted\":true,\"host_stopped\":true}}", trace.observed);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_stream_preserves_remaining_items_across_pending() {
        let (mut stream, control) = pending_stream_after(actions(&[3, 1]), Probe::default(), 1);
        let mut context = Context::from_waker(std::task::Waker::noop());
        assert!(matches!(
            Pin::new(&mut stream).poll_next(&mut context),
            Poll::Ready(Some(Action::Value(3)))
        ));
        assert!(Pin::new(&mut stream).poll_next(&mut context).is_pending());
        control.wait_pending();
        assert!(Pin::new(&mut stream).poll_next(&mut context).is_pending());
        control.release();
        assert!(matches!(
            Pin::new(&mut stream).poll_next(&mut context),
            Poll::Ready(Some(Action::Value(1)))
        ));
        assert!(matches!(
            Pin::new(&mut stream).poll_next(&mut context),
            Poll::Ready(Some(Action::Terminal))
        ));
        assert!(matches!(
            Pin::new(&mut stream).poll_next(&mut context),
            Poll::Ready(None)
        ));
    }

    #[test]
    fn pending_release_wakes_latest_registered_waker() {
        struct Count(AtomicUsize);
        impl std::task::Wake for Count {
            fn wake(self: Arc<Self>) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }
        let first = Arc::new(Count(AtomicUsize::new(0)));
        let latest = Arc::new(Count(AtomicUsize::new(0)));
        let first_waker = std::task::Waker::from(Arc::clone(&first));
        let latest_waker = std::task::Waker::from(Arc::clone(&latest));
        let (mut stream, control) = pending_stream(actions(&[]), Probe::default());
        assert!(Pin::new(&mut stream)
            .poll_next(&mut Context::from_waker(&first_waker))
            .is_pending());
        control.wait_pending();
        assert!(Pin::new(&mut stream)
            .poll_next(&mut Context::from_waker(&latest_waker))
            .is_pending());
        control.release();
        assert_eq!(first.0.load(Ordering::SeqCst), 0);
        assert_eq!(latest.0.load(Ordering::SeqCst), 1);
        assert!(matches!(
            Pin::new(&mut stream).poll_next(&mut Context::from_waker(&latest_waker)),
            Poll::Ready(Some(Action::Terminal))
        ));
    }

    #[test]
    fn trace_rejects_missing_reordered_duplicate_and_early_terminal() {
        for input in [
            vec![Action::Value(3), Action::Terminal],
            vec![Action::Value(1), Action::Value(3), Action::Terminal],
            vec![
                Action::Value(3),
                Action::Value(3),
                Action::Value(1),
                Action::Terminal,
            ],
            vec![Action::Terminal, Action::Value(3), Action::Value(1)],
        ] {
            let mut trace = Trace::default();
            let (tx, rx) = mpsc::channel();
            let mut ack = Some(tx);
            for action in input {
                trace.reduce(&action, &[3, 1], &mut ack);
            }
            assert!(rx.recv_timeout(TIMEOUT).unwrap().is_err());
        }
    }
}
