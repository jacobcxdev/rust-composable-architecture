//! Shared finite-future observations; task ownership remains adapter-specific.

use std::{
    future::Future,
    pin::Pin,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc, Arc, Mutex,
    },
    task::{Context, Poll, Waker},
    time::Duration,
};

pub const TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone, Default)]
pub struct Probe {
    pub ready: Arc<AtomicBool>,
    pub drops: Arc<AtomicUsize>,
}

#[derive(Default)]
struct Gate {
    released: bool,
    waker: Option<Waker>,
}

pub struct FiniteFuture {
    gate: Arc<Mutex<Gate>>,
    probe: Probe,
    pending: Option<mpsc::Sender<()>>,
    dropped: mpsc::Sender<()>,
}

pub struct Control {
    gate: Arc<Mutex<Gate>>,
    pending: mpsc::Receiver<()>,
    dropped: mpsc::Receiver<()>,
}

pub fn pending_future(probe: Probe) -> (FiniteFuture, Control) {
    let gate = Arc::new(Mutex::new(Gate::default()));
    let (pending_tx, pending) = mpsc::channel();
    let (dropped_tx, dropped) = mpsc::channel();
    (
        FiniteFuture {
            gate: Arc::clone(&gate),
            probe,
            pending: Some(pending_tx),
            dropped: dropped_tx,
        },
        Control {
            gate,
            pending,
            dropped,
        },
    )
}

impl Control {
    pub fn wait_pending(&self) {
        self.pending
            .recv_timeout(TIMEOUT)
            .expect("future reached Pending");
    }

    pub fn wait_dropped(&self) {
        self.dropped
            .recv_timeout(TIMEOUT)
            .expect("future dropped before host teardown");
    }

    pub fn release(&self) {
        let waker = {
            let mut gate = self.gate.lock().unwrap();
            gate.released = true;
            gate.waker.take().expect("Pending registered a waker")
        };
        waker.wake();
    }
}

impl Future for FiniteFuture {
    type Output = u64;
    fn poll(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<u64> {
        let this = self.get_mut();
        let mut gate = this.gate.lock().unwrap();
        if !gate.released {
            gate.waker = Some(context.waker().clone());
            if let Some(pending) = this.pending.take() {
                pending.send(()).expect("controller receives Pending");
            }
            return Poll::Pending;
        }
        this.probe.ready.store(true, Ordering::Release);
        Poll::Ready(37)
    }
}

impl Drop for FiniteFuture {
    fn drop(&mut self) {
        self.probe.drops.fetch_add(1, Ordering::Release);
        let _ = self.dropped.send(());
    }
}

pub fn verify_probe(probe: &Probe, ready: bool) {
    assert_eq!(probe.ready.load(Ordering::Acquire), ready);
    assert_eq!(probe.drops.load(Ordering::Acquire), 1);
}

#[test]
fn withheld_gate_is_pending_and_release_wakes_latest_registration() {
    struct Count(AtomicUsize);
    impl std::task::Wake for Count {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
    let first = Arc::new(Count(AtomicUsize::new(0)));
    let latest = Arc::new(Count(AtomicUsize::new(0)));
    let first_waker = Waker::from(Arc::clone(&first));
    let latest_waker = Waker::from(Arc::clone(&latest));
    let probe = Probe::default();
    let (mut future, control) = pending_future(probe.clone());
    assert!(Pin::new(&mut future)
        .poll(&mut Context::from_waker(&first_waker))
        .is_pending());
    control.wait_pending();
    assert!(Pin::new(&mut future)
        .poll(&mut Context::from_waker(&latest_waker))
        .is_pending());
    control.release();
    assert_eq!(first.0.load(Ordering::SeqCst), 0);
    assert_eq!(latest.0.load(Ordering::SeqCst), 1);
    assert_eq!(
        Pin::new(&mut future).poll(&mut Context::from_waker(&latest_waker)),
        Poll::Ready(37)
    );
    drop(future);
    control.wait_dropped();
    verify_probe(&probe, true);
}
