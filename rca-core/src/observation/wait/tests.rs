//! Checks predicate-wait cleanup, terminal precedence and measured storage.

use std::cell::Cell;
use std::rc::Rc;
use std::task::{Context, Poll, Waker};

use super::*;
use crate::effect::lineage::LineageState;
use crate::observation::local::PredicateWait as LocalPredicateWait;
use crate::observation::{
    ObservationId, ObservationSignal, TakeFuture, TakeOperation, TakeOutcome, TakeReply,
};
use crate::store::Local;

/// Polls on the owner thread without scheduling further work.
fn poll<F: Future + Unpin>(future: &mut F) -> Poll<F::Output> {
    Pin::new(future).poll(&mut Context::from_waker(Waker::noop()))
}

/// Reads owner-local values through the real observation consumer.
///
/// The observation owns listener registration, coalescing and terminal cleanup.
struct Selection {
    latest: Rc<Cell<(u64, bool)>>,
    owner: std::thread::ThreadId,
    drops: Rc<Cell<usize>>,
}

impl TakeOperation<bool, Local> for Selection {
    fn take(self: Box<Self>) -> TakeFuture<bool, Local> {
        assert_eq!(std::thread::current().id(), self.owner);
        let (version, value) = self.latest.get();
        let outcome = TakeOutcome::<bool, Local> {
            reply: TakeReply::Value { version, value },
            operation: self,
        };
        TakeFuture::<bool, Local>::Deferred(Box::pin(std::future::ready(Ok(outcome))))
    }
}

impl Drop for Selection {
    fn drop(&mut self) {
        assert_eq!(std::thread::current().id(), self.owner);
        self.drops.set(self.drops.get() + 1);
    }
}

/// Publishes the latest version and records destruction of its selection.
struct Publication {
    signal: Arc<ObservationSignal>,
    latest: Rc<Cell<(u64, bool)>>,
    drops: Rc<Cell<usize>>,
}

impl Publication {
    /// Pairs an owner-local publisher with the real observation being tested.
    fn new(initial: bool) -> (Self, crate::observation::local::Observation<bool>) {
        let signal = ObservationSignal::new(std::sync::Weak::new());
        let latest = Rc::new(Cell::new((0, initial)));
        let drops = Rc::new(Cell::new(0));
        let observation = crate::observation::local::Observation::new(
            ObservationId::new(1),
            Arc::clone(&signal),
            initial,
            0,
            Box::new(Selection {
                latest: Rc::clone(&latest),
                owner: std::thread::current().id(),
                drops: Rc::clone(&drops),
            }),
        );
        (
            Self {
                signal,
                latest,
                drops,
            },
            observation,
        )
    }

    /// Stores a newer value before notifying the observation.
    fn publish(&self, value: bool) {
        let version = self.latest.get().0 + 1;
        self.latest.set((version, value));
        self.signal.publish(version);
    }
}

/// Pairs wait cancellation with the action whose work it can cancel.
fn causal_work() -> (Arc<WaitCancellation>, Arc<LineageState>) {
    let lineage = LineageState::new(std::sync::Weak::new());
    let cancellation = Arc::new(WaitCancellation::default());
    cancellation.install(ActionTask::live(Arc::clone(&lineage)));
    (cancellation, lineage)
}

#[test]
fn local_cancel_before_poll_drops_capture_without_running_future() {
    /// Records destruction and rejects cleanup away from the owner thread.
    struct Capture(Rc<Cell<usize>>, std::thread::ThreadId);
    impl Drop for Capture {
        fn drop(&mut self) {
            assert_eq!(std::thread::current().id(), self.1);
            self.0.set(self.0.get() + 1);
        }
    }
    let owner = std::thread::current().id();
    let drops = Rc::new(Cell::new(0));
    let capture = Capture(Rc::clone(&drops), owner);
    let mut wait = LocalPredicateWait::new(
        async move {
            let _capture = capture;
            panic!("cancelled unpolled wait must not run");
        },
        None,
    );
    wait.cancel();
    assert_eq!(drops.get(), 1);
    assert_eq!(
        Pin::new(&mut wait).poll(&mut Context::from_waker(Waker::noop())),
        Poll::Ready(PredicateWaitOutcome::Cancelled)
    );
    wait.cancel();
    drop(wait);
    assert_eq!(drops.get(), 1);
}

#[test]
fn local_cancel_releases_pending_observation_and_cancels_only_its_work() {
    let (publication, observation) = Publication::new(true);
    let (cancellation, lineage) = causal_work();
    let (_, sibling) = causal_work();
    let mut wait = LocalPredicateWait::new(
        crate::observation::local::wait_for_false(observation, || None),
        Some(Arc::clone(&cancellation)),
    );
    assert!(poll(&mut wait).is_pending());
    wait.cancel();
    assert!(cancellation.is_cancelled());
    assert!(lineage.is_cancelled());
    assert!(!sibling.is_cancelled());
    assert!(!publication.signal.is_active());
    assert_eq!(publication.drops.get(), 1);
    assert_eq!(
        poll(&mut wait),
        Poll::Ready(PredicateWaitOutcome::Cancelled)
    );
}

#[test]
fn local_drop_retires_observation_without_cancelling_causal_work() {
    let (publication, observation) = Publication::new(true);
    let (cancellation, lineage) = causal_work();
    let mut wait = LocalPredicateWait::new(
        crate::observation::local::wait_for_false(observation, || None),
        Some(Arc::clone(&cancellation)),
    );
    assert!(poll(&mut wait).is_pending());
    drop(wait);
    assert!(!cancellation.is_cancelled());
    assert!(!lineage.is_cancelled());
    assert!(!publication.signal.is_active());
    assert_eq!(publication.drops.get(), 1);
}

#[test]
fn local_completion_is_sticky_and_drops_observation_before_returning() {
    let (publication, observation) = Publication::new(false);
    let (cancellation, lineage) = causal_work();
    let calls = Rc::new(Cell::new(0));
    let terminal_calls = Rc::clone(&calls);
    let mut wait = LocalPredicateWait::new(
        crate::observation::local::wait_for_false(observation, move || {
            terminal_calls.set(terminal_calls.get() + 1);
            None
        }),
        Some(Arc::clone(&cancellation)),
    );
    assert_eq!(
        poll(&mut wait),
        Poll::Ready(PredicateWaitOutcome::Satisfied)
    );
    assert!(!publication.signal.is_active());
    assert_eq!(publication.drops.get(), 1);
    wait.cancel();
    assert_eq!(
        poll(&mut wait),
        Poll::Ready(PredicateWaitOutcome::Satisfied)
    );
    assert_eq!(calls.get(), 1);
    assert!(!cancellation.is_cancelled());
    assert!(!lineage.is_cancelled());
}

#[test]
fn local_false_selection_respects_terminal_precedence() {
    for outcome in [
        PredicateWaitOutcome::ScopeEnded,
        PredicateWaitOutcome::StoreStopped,
    ] {
        let (publication, observation) = Publication::new(false);
        let terminal = Rc::new(outcome);
        let mut wait = LocalPredicateWait::new(
            crate::observation::local::wait_for_false(observation, move || Some(*terminal)),
            None,
        );
        assert_eq!(poll(&mut wait), Poll::Ready(outcome));
        assert!(!publication.signal.is_active());
        assert_eq!(publication.drops.get(), 1);
    }
}

#[test]
fn local_transient_false_coalesces_before_consumption() {
    let (publication, observation) = Publication::new(true);
    let mut wait = LocalPredicateWait::new(
        crate::observation::local::wait_for_false(observation, || None),
        None,
    );
    assert!(poll(&mut wait).is_pending());
    publication.publish(false);
    publication.publish(true);
    assert!(poll(&mut wait).is_pending());
    assert!(publication.signal.is_active());
    publication.publish(false);
    assert_eq!(
        poll(&mut wait),
        Poll::Ready(PredicateWaitOutcome::Satisfied)
    );
    assert_eq!(publication.drops.get(), 1);
}

#[test]
fn local_remote_closure_only_notifies_and_owner_retires_payload() {
    for terminal in [None, Some(PredicateWaitOutcome::ScopeEnded)] {
        let (publication, observation) = Publication::new(true);
        let mut wait = LocalPredicateWait::new(
            crate::observation::local::wait_for_false(observation, move || terminal),
            None,
        );
        assert!(poll(&mut wait).is_pending());
        let signal = Arc::clone(&publication.signal);
        std::thread::spawn(move || signal.deactivate())
            .join()
            .unwrap();
        assert_eq!(publication.drops.get(), 0);
        assert_eq!(
            poll(&mut wait),
            Poll::Ready(terminal.unwrap_or(PredicateWaitOutcome::StoreStopped))
        );
        assert_eq!(publication.drops.get(), 1);
    }
}

#[test]
fn local_terminal_panic_unwinds_observation_on_owner() {
    let (publication, observation) = Publication::new(false);
    let mut wait = LocalPredicateWait::new(
        crate::observation::local::wait_for_false(observation, || panic!("terminal callback")),
        None,
    );
    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| poll(&mut wait)));
    assert_eq!(
        panic
            .expect_err("terminal callback must panic")
            .downcast_ref::<&str>(),
        Some(&"terminal callback")
    );
    drop(wait);
    assert!(!publication.signal.is_active());
    assert_eq!(publication.drops.get(), 1);
}

#[test]
fn local_cancel_drops_future_outside_cancellation_lock() {
    /// Re-enters cancellation state while the owned future is being dropped.
    struct ReentrantDrop(Arc<WaitCancellation>, Rc<Cell<bool>>);
    impl Drop for ReentrantDrop {
        fn drop(&mut self) {
            assert!(self.0.is_cancelled());
            self.1.set(true);
        }
    }
    let cancellation = Arc::new(WaitCancellation::default());
    let dropped = Rc::new(Cell::new(false));
    let capture = ReentrantDrop(Arc::clone(&cancellation), Rc::clone(&dropped));
    let mut wait = LocalPredicateWait::new(
        async move {
            let _capture = capture;
            std::future::pending().await
        },
        Some(cancellation),
    );
    assert!(poll(&mut wait).is_pending());
    wait.cancel();
    assert!(dropped.get());
}

/// Returns a fixed poll result and can panic when its storage is released.
struct DropProbe {
    drops: Rc<Cell<usize>>,
    outcome: Poll<PredicateWaitOutcome>,
    panic: bool,
}

impl Future for DropProbe {
    type Output = PredicateWaitOutcome;

    fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Self::Output> {
        self.outcome
    }
}

impl Drop for DropProbe {
    fn drop(&mut self) {
        self.drops.set(self.drops.get() + 1);
        assert!(!self.panic, "predicate future destructor panic");
    }
}

// Cleanup must not leave the wait without both a future and a terminal outcome.
#[test]
fn local_cancel_remains_terminal_after_future_destructor_panics() {
    let drops = Rc::new(Cell::new(0));
    let mut wait = LocalPredicateWait::new(
        DropProbe {
            drops: Rc::clone(&drops),
            outcome: Poll::Pending,
            panic: true,
        },
        None,
    );
    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| wait.cancel()));
    assert_eq!(
        panic
            .expect_err("destructor must panic")
            .downcast_ref::<&str>(),
        Some(&"predicate future destructor panic")
    );
    assert_eq!(drops.get(), 1);
    assert_eq!(
        poll(&mut wait),
        Poll::Ready(PredicateWaitOutcome::Cancelled)
    );
    wait.cancel();
    drop(wait);
    assert_eq!(drops.get(), 1);
}

#[test]
fn local_ready_outcome_survives_future_destructor_panic() {
    let drops = Rc::new(Cell::new(0));
    let (cancellation, lineage) = causal_work();
    let mut wait = LocalPredicateWait::new(
        DropProbe {
            drops: Rc::clone(&drops),
            outcome: Poll::Ready(PredicateWaitOutcome::Satisfied),
            panic: true,
        },
        Some(cancellation),
    );
    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| poll(&mut wait)));
    assert_eq!(
        panic
            .expect_err("destructor must panic")
            .downcast_ref::<&str>(),
        Some(&"predicate future destructor panic")
    );
    assert_eq!(drops.get(), 1);
    assert_eq!(
        poll(&mut wait),
        Poll::Ready(PredicateWaitOutcome::Satisfied)
    );
    wait.cancel();
    assert!(!lineage.is_cancelled());
    drop(wait);
    assert_eq!(drops.get(), 1);
}

// Actor cancellation can invoke a user waker before the owned future is dropped.
#[test]
fn local_cancel_wake_panic_still_retires_future_and_records_outcome() {
    /// Panics when action cancellation wakes the actor's registered listener.
    struct PanicWake;
    impl std::task::Wake for PanicWake {
        fn wake(self: Arc<Self>) {
            panic!("actor wake panic");
        }
    }
    let control = crate::effect::lineage::ControlWake::new();
    let lineage = LineageState::new(Arc::downgrade(&control));
    let cancellation = Arc::new(WaitCancellation::default());
    cancellation.install(ActionTask::live(Arc::clone(&lineage)));
    let mut listener = control.event().listen();
    let waker = Waker::from(Arc::new(PanicWake));
    assert!(
        Pin::new(&mut listener)
            .poll(&mut Context::from_waker(&waker))
            .is_pending()
    );
    let drops = Rc::new(Cell::new(0));
    let mut wait = LocalPredicateWait::new(
        DropProbe {
            drops: Rc::clone(&drops),
            outcome: Poll::Pending,
            panic: false,
        },
        Some(Arc::clone(&cancellation)),
    );
    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| wait.cancel()));
    assert_eq!(
        panic
            .expect_err("registered actor waker must panic")
            .downcast_ref::<&str>(),
        Some(&"actor wake panic")
    );
    assert!(cancellation.is_cancelled());
    assert!(lineage.is_cancelled());
    assert_eq!(drops.get(), 1);
    assert_eq!(
        poll(&mut wait),
        Poll::Ready(PredicateWaitOutcome::Cancelled)
    );
    wait.cancel();
    drop(wait);
    assert_eq!(drops.get(), 1);
}
