#![cfg(feature = "local")]
//! Owner-local gates share metadata without imposing Send on domain values.

use rca_core::InputGate;
use rca_core::local::{
    Feature, FeatureContext, OptionalMount, RootMount, Runtime, RuntimeDriver, RuntimeFuture,
};
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::task::{Context, Poll, Waker};

/// Polls owner-local work without advancing its fixed zero clock.
///
/// Elapsed waits complete. Future waits remain pending, and yield requests one
/// wake. Tests must stop their Stores and drain tasks before releasing the driver.
#[derive(Clone, Default)]
struct Driver(Rc<RefCell<Vec<RuntimeFuture>>>);

impl std::fmt::Debug for Driver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("LocalGateDriver")
    }
}

impl RuntimeDriver for Driver {
    fn spawn(&self, task: RuntimeFuture) {
        self.0.borrow_mut().push(task);
    }
    fn now(&self) -> rca_core::MonotonicInstant {
        rca_core::MonotonicInstant::ZERO
    }
    fn sleep_until(&self, deadline: rca_core::MonotonicInstant) -> RuntimeFuture {
        if deadline <= self.now() {
            Box::pin(std::future::ready(()))
        } else {
            Box::pin(std::future::pending())
        }
    }
    fn yield_now(&self) -> RuntimeFuture {
        let mut yielded = false;
        Box::pin(std::future::poll_fn(move |context| {
            if yielded {
                Poll::Ready(())
            } else {
                yielded = true;
                context.waker().wake_by_ref();
                Poll::Pending
            }
        }))
    }
}

impl Driver {
    /// Polls an operation and queued tasks within a finite test budget.
    ///
    /// Each task batch leaves the queue before polling, allowing local re-entry.
    /// The 32-poll limit detects a stalled test, not a runtime scheduling promise.
    ///
    /// # Panics
    ///
    /// Panics if the operation remains pending after the final poll.
    fn finish<F: Future>(&self, future: F) -> F::Output {
        let mut future = std::pin::pin!(future);
        for _ in 0..32 {
            if let Poll::Ready(value) = future
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
            {
                return value;
            }
            let pending = std::mem::take(&mut *self.0.borrow_mut());
            for mut task in pending {
                if task
                    .as_mut()
                    .poll(&mut Context::from_waker(Waker::noop()))
                    .is_pending()
                {
                    self.0.borrow_mut().push(task);
                }
            }
        }
        panic!("finite Local gate operation");
    }
}

#[test]
fn driver_contract_sleep_completes_only_elapsed_deadlines() {
    use rca_core::MonotonicInstant;
    use std::time::Duration;

    let driver = Driver::default();
    let mut context = Context::from_waker(Waker::noop());
    let mut elapsed = driver.sleep_until(MonotonicInstant::ZERO);
    assert!(elapsed.as_mut().poll(&mut context).is_ready());

    for deadline in [
        MonotonicInstant::from_elapsed(Duration::from_nanos(1)),
        MonotonicInstant::MAX,
    ] {
        let mut future = driver.sleep_until(deadline);
        assert!(future.as_mut().poll(&mut context).is_pending());
        assert!(future.as_mut().poll(&mut context).is_pending());
    }
}

#[test]
fn driver_contract_yield_wakes_once_before_completion() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::Wake;

    /// Counts scheduling requests without running another task.
    #[derive(Default)]
    struct WakeCount(AtomicUsize);

    impl Wake for WakeCount {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    let notifications = Arc::new(WakeCount::default());
    let waker = Waker::from(Arc::clone(&notifications));
    let mut context = Context::from_waker(&waker);
    let mut future = Driver::default().yield_now();

    assert_eq!(notifications.0.load(Ordering::SeqCst), 0);
    assert!(future.as_mut().poll(&mut context).is_pending());
    assert_eq!(notifications.0.load(Ordering::SeqCst), 1);
    assert!(future.as_mut().poll(&mut context).is_ready());
    assert_eq!(notifications.0.load(Ordering::SeqCst), 1);
}

/// Counts mutable child selection independently of input reduction.
struct Root {
    child: Option<Rc<Cell<usize>>>,
    selections: Rc<Cell<usize>>,
}

enum RootAction {
    Gate(InputGate, bool),
}

/// Retains the caller's non-Send value across rejected admission.
struct Input(Rc<Cell<usize>>);

struct RootFeature;
struct ChildFeature;

impl Feature for RootFeature {
    type State = Root;
    type Action = RootAction;
    fn reduce(
        &mut self,
        _: &mut Root,
        action: &RootAction,
        context: &mut FeatureContext<'_, Root, RootAction>,
    ) {
        let RootAction::Gate(gate, open) = action;
        context.set_input_open(gate, *open).unwrap();
    }
}

impl Feature for ChildFeature {
    type State = Rc<Cell<usize>>;
    type Action = Input;
    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &Input,
        _: &mut FeatureContext<'_, Self::State, Input>,
    ) {
        state.set(state.get() + action.0.get());
    }
}

#[test]
fn local_owned_input_rejects_before_selection_and_reopens_without_remount() {
    let driver = Driver::default();
    let selections = Rc::new(Cell::new(0));
    let makes = Rc::new(Cell::new(0));
    let made = std::rc::Rc::clone(&makes);
    let (builder, site) = RootMount::new(RootFeature)
        .builder(
            Root {
                child: Some(Rc::new(Cell::new(4))),
                selections: std::rc::Rc::clone(&selections),
            },
            Runtime::new(driver.clone()),
        )
        .mount_with_site(OptionalMount::new(
            |root: &Root| root.child.as_ref(),
            |root: &mut Root| {
                root.selections.set(root.selections.get() + 1);
                root.child.as_mut()
            },
            |root: &Root| {
                root.child.as_ref().map(|_| rca_core::MountIdentity {
                    key: (),
                    replacement: 0,
                    configuration: 0,
                })
            },
            move |_| {
                made.set(made.get() + 1);
                ChildFeature
            },
        ));
    let store = builder.build().unwrap();
    driver.finish(store.ready()).unwrap();
    let child = driver.finish(store.acquire(&site)).unwrap().unwrap();
    let acquisition = store.input_gate(&site);
    drop(site);
    let gate = driver.finish(acquisition).unwrap().unwrap();
    let _ = driver
        .finish(store.send(RootAction::Gate(gate.clone(), false)))
        .unwrap();
    selections.set(0);
    let value = Rc::new(Cell::new(3));
    let identity = std::rc::Rc::clone(&value);
    let error = driver.finish(child.send(Input(value))).unwrap_err();
    let rca_core::TaskSendError::Rejected { action, error } = error else {
        panic!("unentered input owns its value")
    };
    assert!(Rc::ptr_eq(&identity, &action.0));
    assert_eq!(error.reason(), rca_core::TaskAccessReason::InputClosed);
    assert_eq!(error.progress(), rca_core::TaskProgress::NotStarted);
    assert_eq!(selections.get(), 0);
    let _ = driver.finish(child.update(|state| state.set(5))).unwrap();
    assert_eq!(driver.finish(child.read(|state| state.get())).unwrap(), 5);
    let _ = driver
        .finish(store.send(RootAction::Gate(gate, true)))
        .unwrap();
    let _ = driver.finish(child.send(action)).unwrap();
    assert_eq!(driver.finish(child.read(|state| state.get())).unwrap(), 8);
    assert_eq!(makes.get(), 1);

    driver.finish(store.shutdown()).unwrap();
    drop(store);
    let remaining = std::mem::take(&mut *driver.0.borrow_mut());
    let remaining_count = remaining.len();
    drop(remaining);
    assert_eq!(remaining_count, 0, "the test must dispose of its actor");
    let queue = Rc::downgrade(&driver.0);
    drop(driver);
    assert!(
        queue.upgrade().is_none(),
        "no driver handle retains the queue"
    );
}
