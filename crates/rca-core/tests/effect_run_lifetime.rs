//! Checks deferred factory execution, pinned future storage and exact capture release.

#[expect(dead_code, reason = "run lifetime tests manually poll runtime tasks")]
mod support;

use std::cell::Cell;
use std::marker::PhantomPinned;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll, Waker};

use rca_core::{Effect, ReduceContext, Reducer, Store};

/// Counts destruction whether the factory runs or its stored capture is discarded.
struct Capture(Arc<AtomicUsize>);

impl Drop for Capture {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

/// A non-`Sync`, non-`Unpin` future records its address on every poll.
struct PendingRun {
    _capture: Capture,
    address: Cell<Option<usize>>,
    polls: Arc<AtomicUsize>,
    _pinned: PhantomPinned,
}

impl Future for PendingRun {
    type Output = ();

    fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<()> {
        let this = self.as_ref().get_ref();
        let address = std::ptr::from_ref(this).addr();
        if let Some(previous) = this.address.replace(Some(address)) {
            assert_eq!(previous, address, "pinned user future moved between polls");
        }
        this.polls.fetch_add(1, Ordering::SeqCst);
        Poll::Pending
    }
}

/// Selects direct or twice-mapped work, with an optional deferred factory panic.
struct Feature {
    factories: Arc<AtomicUsize>,
    drops: Arc<AtomicUsize>,
    polls: Arc<AtomicUsize>,
    mapped: bool,
    panic_in_factory: bool,
}

impl Reducer for Feature {
    type State = ();
    type Action = ();

    fn reduce(&mut self, (): &mut (), (): &(), _: &mut ReduceContext<'_>) -> Effect<()> {
        let factories = Arc::clone(&self.factories);
        let polls = Arc::clone(&self.polls);
        let capture = Capture(Arc::clone(&self.drops));
        let panic_in_factory = self.panic_in_factory;
        let effect = Effect::run(move |_sender| {
            factories.fetch_add(1, Ordering::SeqCst);
            assert!(!panic_in_factory, "intentional run factory panic");
            PendingRun {
                _capture: capture,
                address: Cell::new(None),
                polls,
                _pinned: PhantomPinned,
            }
        });
        if self.mapped {
            effect.map(|()| ()).map(|()| ())
        } else {
            effect
        }
    }
}

#[test]
fn unpolled_run_releases_capture_without_invoking_factory() {
    for (cancel, mapped) in [(false, false), (false, true), (true, false), (true, true)] {
        let runtime = support::ManualRuntime::new();
        let factories = Arc::new(AtomicUsize::new(0));
        let drops = Arc::new(AtomicUsize::new(0));
        let store = Store::new(
            (),
            Feature {
                factories: Arc::clone(&factories),
                drops: Arc::clone(&drops),
                polls: Arc::new(AtomicUsize::new(0)),
                mapped,
                panic_in_factory: false,
            },
            runtime.runtime(),
        );
        let mut actor = runtime.take_only_queued_task();
        let receipt = store.try_send(()).unwrap();
        let mut cx = Context::from_waker(Waker::noop());
        assert!(actor.as_mut().poll(&mut cx).is_pending());
        let task = futures_executor::block_on(receipt.finish()).unwrap();
        let mut run = runtime.take_only_queued_task();
        assert_eq!(factories.load(Ordering::SeqCst), 0);
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        if cancel {
            task.cancel();
            assert!(actor.as_mut().poll(&mut cx).is_pending());
            assert!(run.as_mut().poll(&mut cx).is_ready());
        }
        drop(run);
        assert_eq!(factories.load(Ordering::SeqCst), 0);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }
}

#[test]
fn pending_non_unpin_run_survives_handle_move_and_drops_once_on_cancellation() {
    for mapped in [false, true] {
        let runtime = support::ManualRuntime::new();
        let factories = Arc::new(AtomicUsize::new(0));
        let drops = Arc::new(AtomicUsize::new(0));
        let polls = Arc::new(AtomicUsize::new(0));
        let store = Store::new(
            (),
            Feature {
                factories: Arc::clone(&factories),
                drops: Arc::clone(&drops),
                polls: Arc::clone(&polls),
                mapped,
                panic_in_factory: false,
            },
            runtime.runtime(),
        );
        let mut actor = runtime.take_only_queued_task();
        let receipt = store.try_send(()).unwrap();
        let mut cx = Context::from_waker(Waker::noop());
        assert!(actor.as_mut().poll(&mut cx).is_pending());
        let task = futures_executor::block_on(receipt.finish()).unwrap();
        let mut run = runtime.take_only_queued_task();
        assert_eq!(factories.load(Ordering::SeqCst), 0);
        assert!(run.as_mut().poll(&mut cx).is_pending());
        let mut moved = Box::new(run);
        assert!(moved.as_mut().as_mut().poll(&mut cx).is_pending());
        assert_eq!(factories.load(Ordering::SeqCst), 1);
        assert_eq!(polls.load(Ordering::SeqCst), 2);
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        task.cancel();
        assert!(actor.as_mut().poll(&mut cx).is_pending());
        assert!(moved.as_mut().as_mut().poll(&mut cx).is_ready());
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        drop(moved);
        assert_eq!(polls.load(Ordering::SeqCst), 2);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }
}

#[test]
fn factory_panic_is_deferred_to_runtime_poll_and_releases_capture_once() {
    for mapped in [false, true] {
        let runtime = support::ManualRuntime::new();
        let factories = Arc::new(AtomicUsize::new(0));
        let drops = Arc::new(AtomicUsize::new(0));
        let polls = Arc::new(AtomicUsize::new(0));
        let store = Store::new(
            (),
            Feature {
                factories: Arc::clone(&factories),
                drops: Arc::clone(&drops),
                polls: Arc::clone(&polls),
                mapped,
                panic_in_factory: true,
            },
            runtime.runtime(),
        );
        let mut actor = runtime.take_only_queued_task();
        let receipt = store.try_send(()).unwrap();
        let mut cx = Context::from_waker(Waker::noop());
        assert!(actor.as_mut().poll(&mut cx).is_pending());
        let task = futures_executor::block_on(receipt.finish()).unwrap();
        assert_eq!(factories.load(Ordering::SeqCst), 0);
        let mut run = runtime.take_only_queued_task();
        let panic = catch_unwind(AssertUnwindSafe(move || {
            run.as_mut().poll(&mut Context::from_waker(Waker::noop()))
        }));
        let panic = panic.expect_err("factory panic must propagate from the runtime poll");
        assert_eq!(
            panic.downcast_ref::<&str>(),
            Some(&"intentional run factory panic")
        );
        assert_eq!(factories.load(Ordering::SeqCst), 1);
        assert_eq!(polls.load(Ordering::SeqCst), 0);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        drop(actor);
        assert!(task.is_finished());
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }
}
