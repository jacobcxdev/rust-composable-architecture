#![cfg(feature = "local")]

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::task::{Context, Poll, Waker};
use std::thread::{self, ThreadId};

use rca_core::local::{
    Feature, FeatureContext, MountSite, OptionalMount, Runtime, RuntimeDriver, RuntimeFuture,
    Store, Task,
};
use rca_core::{MonotonicInstant, MountIdentity, TaskAccessReason, TaskProgress};

/// Polls tasks only on the owner thread; its fixed clock never advances.
#[derive(Clone)]
struct Driver {
    /// Rejects accidental cross-thread task polling in this Local fixture.
    owner: ThreadId,
    /// Holds work until an explicit fixture turn takes it out of the borrow.
    tasks: Rc<RefCell<Vec<RuntimeFuture>>>,
}

impl Driver {
    fn new() -> Self {
        Self {
            owner: thread::current().id(),
            tasks: Rc::default(),
        }
    }

    /// Polls each task once outside the queue borrow; newly spawned work waits.
    fn turn(&self) {
        assert_eq!(thread::current().id(), self.owner);
        let pending = std::mem::take(&mut *self.tasks.borrow_mut());
        for mut future in pending {
            if future
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending()
            {
                self.tasks.borrow_mut().push(future);
            }
        }
    }

    /// Bounds owner-thread progress to 64 turns for finite requests.
    fn finish<F: Future>(&self, future: F) -> F::Output {
        let mut future = std::pin::pin!(future);
        for _ in 0..64 {
            if let Poll::Ready(value) = future
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
            {
                return value;
            }
            self.turn();
        }
        panic!("finite local worker operation did not finish");
    }
}

impl std::fmt::Debug for Driver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("LocalWorkerDriver")
    }
}

impl RuntimeDriver for Driver {
    fn spawn(&self, task: RuntimeFuture) {
        assert_eq!(thread::current().id(), self.owner);
        self.tasks.borrow_mut().push(task);
    }

    fn now(&self) -> MonotonicInstant {
        MonotonicInstant::ZERO
    }

    fn sleep_until(&self, deadline: MonotonicInstant) -> RuntimeFuture {
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

#[test]
fn local_worker_driver_honours_elapsed_and_future_sleep() {
    let driver = Driver::new();
    let mut context = Context::from_waker(Waker::noop());
    let mut elapsed = driver.sleep_until(driver.now());
    assert!(elapsed.as_mut().poll(&mut context).is_ready());
    for deadline in [
        driver
            .now()
            .saturating_add(std::time::Duration::from_nanos(1)),
        MonotonicInstant::MAX,
    ] {
        let mut future = driver.sleep_until(deadline);
        assert!(future.as_mut().poll(&mut context).is_pending());
    }
    assert!(driver.tasks.borrow().is_empty());
}

#[test]
fn local_worker_driver_yields_once_and_wakes_once() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::Wake;

    /// Counts scheduling notifications without polling the actor from the waker.
    struct WakeCount(AtomicUsize);
    impl Wake for WakeCount {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
        fn wake_by_ref(self: &Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    let driver = Driver::new();
    let count = Arc::new(WakeCount(AtomicUsize::new(0)));
    let waker = Waker::from(Arc::clone(&count));
    let mut context = Context::from_waker(&waker);
    let mut yield_once = driver.yield_now();
    assert!(yield_once.as_mut().poll(&mut context).is_pending());
    assert_eq!(count.0.load(Ordering::SeqCst), 1);
    assert!(yield_once.as_mut().poll(&mut context).is_ready());
    assert_eq!(count.0.load(Ordering::SeqCst), 1);
    assert!(driver.tasks.borrow().is_empty());
}

/// Suspends both managed and externally retained work until explicit release.
#[derive(Default)]
struct Gate {
    /// Allows each suspended continuation to leave its wait.
    released: Cell<bool>,
    /// Proves a real poll reached the gate before replacement.
    pending: Cell<usize>,
    /// Wakes the most recent waiter after release without polling it inline.
    waker: RefCell<Option<Waker>>,
}

impl Gate {
    /// Records a pending poll and stores its waker for owner-thread release.
    async fn wait(&self) {
        std::future::poll_fn(|context| {
            if self.released.get() {
                Poll::Ready(())
            } else {
                self.pending.set(self.pending.get() + 1);
                *self.waker.borrow_mut() = Some(context.waker().clone());
                Poll::Pending
            }
        })
        .await;
    }

    /// Wakes the last waiter; manual fixture turns drive actual progress.
    fn release(&self) {
        self.released.set(true);
        if let Some(waker) = self.waker.borrow_mut().take() {
            waker.wake();
        }
    }
}

/// Carries a non-Send value whose use and disposal must stay on one thread.
struct Interpreter {
    owner: ThreadId,
    /// Distinguishes the retired value from its same-key successor.
    offset: usize,
    /// Counts final disposal, including externally retained continuation owners.
    drops: Rc<Cell<usize>>,
}

impl Interpreter {
    fn evaluate(&self, input: &Rc<usize>) -> usize {
        assert_eq!(thread::current().id(), self.owner);
        **input + self.offset
    }
}

impl Drop for Interpreter {
    fn drop(&mut self) {
        assert_eq!(thread::current().id(), self.owner);
        self.drops.set(self.drops.get() + 1);
    }
}

/// Owns the interpreter across the worker's suspended continuation.
struct Capture {
    interpreter: Rc<Interpreter>,
    drops: Rc<Cell<usize>>,
}

impl Drop for Capture {
    fn drop(&mut self) {
        assert_eq!(thread::current().id(), self.interpreter.owner);
        self.drops.set(self.drops.get() + 1);
    }
}

/// Separates stable key from incarnation so same-key replacement retires work.
struct WorkerState {
    /// Stable key remains unchanged across replacement.
    id: u64,
    /// Changes on replacement to retire the old worker's authority.
    incarnation: u64,
    value: usize,
    interpreter: Rc<Interpreter>,
}

/// Keeps the currently installed worker and counts root replacements.
struct RootState {
    replacements: usize,
    worker: Option<WorkerState>,
}

enum RootAction {
    Replace(Rc<Interpreter>),
}

/// Carries the suspension gate, input and independent ownership counters.
struct Start {
    gate: Rc<Gate>,
    input: Rc<usize>,
    /// Counts updates that reached the currently installed worker.
    updates: Rc<Cell<usize>>,
    /// Separately tracks disposal of the actor-owned worker continuation.
    capture_drops: Rc<Cell<usize>>,
}

struct Root;
struct Worker;

impl Feature for Root {
    type State = RootState;
    type Action = RootAction;

    fn update(
        &mut self,
        state: &mut RootState,
        action: &RootAction,
        _: &mut FeatureContext<'_, RootState, RootAction>,
    ) {
        let RootAction::Replace(interpreter) = action;
        let previous = state.worker.as_ref().unwrap();
        state.worker = Some(WorkerState {
            id: previous.id,
            incarnation: previous.incarnation + 1,
            value: 100,
            interpreter: Rc::clone(interpreter),
        });
        state.replacements += 1;
    }
}

impl Feature for Worker {
    type State = WorkerState;
    type Action = Start;

    fn update(
        &mut self,
        state: &mut WorkerState,
        action: &Start,
        context: &mut FeatureContext<'_, WorkerState, Start>,
    ) {
        let capture = Capture {
            interpreter: Rc::clone(&state.interpreter),
            drops: Rc::clone(&action.capture_drops),
        };
        let gate = Rc::clone(&action.gate);
        let input = Rc::clone(&action.input);
        let updates = Rc::clone(&action.updates);
        context.task(Task::<WorkerState, Start>::run(move |context| async move {
            assert_eq!(thread::current().id(), capture.interpreter.owner);
            gate.wait().await;
            let evaluated = capture.interpreter.evaluate(&input);
            let current = context.read(|state| Rc::new(state.value)).await.unwrap();
            let expected = *current + evaluated;
            let committed = context
                .update(move |state| {
                    updates.set(updates.get() + 1);
                    state.value = expected;
                    Rc::new(state.value)
                })
                .await
                .unwrap();
            assert_eq!(**committed.value(), expected);
            drop(capture);
        }));
    }
}

/// Addresses the current worker without retaining a specific incarnation.
type Site = MountSite<RootState, WorkerState, Start, RootAction>;

/// Installs one worker and returns its weak, reacquirable site.
fn mounted(driver: &Driver, interpreter: Rc<Interpreter>) -> (Store<RootState, RootAction>, Site) {
    let mount = OptionalMount::new(
        |root: &RootState| root.worker.as_ref(),
        |root: &mut RootState| root.worker.as_mut(),
        |root| {
            root.worker.as_ref().map(|worker| MountIdentity {
                key: worker.id,
                replacement: worker.incarnation,
                configuration: 0,
            })
        },
        |_| Worker,
    );
    let (builder, site) = Store::builder(
        RootState {
            replacements: 0,
            worker: Some(WorkerState {
                id: 7,
                incarnation: 0,
                value: 0,
                interpreter,
            }),
        },
        Root,
        Runtime::new(driver.clone()),
    )
    .mount_with_site(mount);
    (builder.build().unwrap(), site)
}

/// Creates an owner-thread interpreter with a separately observed final drop.
fn interpreter(offset: usize, drops: &Rc<Cell<usize>>) -> Rc<Interpreter> {
    Rc::new(Interpreter {
        owner: thread::current().id(),
        offset,
        drops: Rc::clone(drops),
    })
}

/// Reads key, incarnation and value without retaining the interpreter.
fn state_values(state: &RootState) -> (usize, Option<(u64, u64, usize, usize)>) {
    (
        state.replacements,
        state.worker.as_ref().map(|worker| {
            assert_eq!(thread::current().id(), worker.interpreter.owner);
            (
                worker.id,
                worker.incarnation,
                worker.value,
                worker.interpreter.offset,
            )
        }),
    )
}

#[test]
fn local_interpreter_resumes_after_observed_pending_and_updates_live_state() {
    let driver = Driver::new();
    let interpreter_drops = Rc::new(Cell::new(0));
    let capture_drops = Rc::new(Cell::new(0));
    let updates = Rc::new(Cell::new(0));
    let gate = Rc::new(Gate::default());
    let (store, site) = mounted(&driver, interpreter(3, &interpreter_drops));
    driver.finish(store.ready()).unwrap();
    let context = driver.finish(store.acquire(&site)).unwrap().unwrap();
    let task = driver
        .finish(context.send(Start {
            gate: Rc::clone(&gate),
            input: Rc::new(4),
            updates: Rc::clone(&updates),
            capture_drops: Rc::clone(&capture_drops),
        }))
        .unwrap();
    driver.turn();
    assert!(
        gate.pending.get() > 0,
        "worker really suspended before release"
    );
    assert_eq!((updates.get(), capture_drops.get()), (0, 0));
    driver
        .finish(context.update(|state| state.value = 5))
        .unwrap();
    assert_eq!(
        driver.finish(store.read(state_values)).unwrap(),
        (0, Some((7, 0, 5, 3)))
    );
    gate.release();
    driver.finish(task.finish());
    assert_eq!(
        driver.finish(store.read(state_values)).unwrap(),
        (0, Some((7, 0, 12, 3)))
    );
    assert_eq!(
        (updates.get(), capture_drops.get(), interpreter_drops.get()),
        (1, 1, 0)
    );
    let state = driver.finish(store.shutdown_into_state()).unwrap();
    assert_eq!(state_values(&state), (0, Some((7, 0, 12, 3))));
    drop(state);
    assert_eq!(interpreter_drops.get(), 1);
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "One Local worker replacement sequence preserves cancellation and retained-after-await rejection"
)]
fn same_id_replacement_cancels_local_worker_and_fences_retained_after_await_update() {
    let driver = Driver::new();
    let old_drops = Rc::new(Cell::new(0));
    let successor_drops = Rc::new(Cell::new(0));
    let managed_drops = Rc::new(Cell::new(0));
    let retained_drops = Rc::new(Cell::new(0));
    let managed_updates = Rc::new(Cell::new(0));
    let stale_updates = Rc::new(Cell::new(0));
    let gate = Rc::new(Gate::default());
    let old = interpreter(3, &old_drops);
    let old_weak = Rc::downgrade(&old);
    let (store, site) = mounted(&driver, old);
    driver.finish(store.ready()).unwrap();
    let original = driver.finish(store.acquire(&site)).unwrap().unwrap();
    let managed = driver
        .finish(original.send(Start {
            gate: Rc::clone(&gate),
            input: Rc::new(4),
            updates: Rc::clone(&managed_updates),
            capture_drops: Rc::clone(&managed_drops),
        }))
        .unwrap();
    driver.turn();
    assert!(gate.pending.get() > 0);

    // External ownership is intentional: actor cancellation cannot destroy this
    // continuation. It retains the original context, never reacquiring the site.
    let capture = Capture {
        interpreter: driver
            .finish(original.read(|state| Rc::clone(&state.interpreter)))
            .unwrap(),
        drops: Rc::clone(&retained_drops),
    };
    let captured_context = original.clone();
    let wait = Rc::clone(&gate);
    let callbacks = Rc::clone(&stale_updates);
    let mut retained = std::pin::pin!(async move {
        wait.wait().await;
        assert_eq!(capture.interpreter.evaluate(&Rc::new(4)), 7);
        let result = captured_context
            .update(move |state| {
                callbacks.set(callbacks.get() + 1);
                state.value = 999;
            })
            .await;
        drop(capture);
        result
    });
    let before = gate.pending.get();
    assert!(
        retained
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_pending()
    );
    assert_eq!(gate.pending.get(), before + 1);
    assert_eq!(
        driver.finish(store.read(state_values)).unwrap(),
        (0, Some((7, 0, 0, 3)))
    );

    let replace = driver
        .finish(store.send(RootAction::Replace(interpreter(20, &successor_drops))))
        .unwrap();
    driver.finish(replace.finish());
    driver.finish(managed.finish());
    assert_eq!(
        managed_drops.get(),
        1,
        "actor-owned suspended body was physically destroyed"
    );
    assert_eq!(
        (
            managed_updates.get(),
            stale_updates.get(),
            retained_drops.get()
        ),
        (0, 0, 0)
    );
    assert_eq!(
        old_drops.get(),
        0,
        "only the external continuation still owns the old interpreter"
    );
    assert!(old_weak.upgrade().is_some());
    assert_eq!(
        driver.finish(store.read(state_values)).unwrap(),
        (1, Some((7, 1, 100, 20)))
    );

    gate.release();
    let Err(error) = driver.finish(retained.as_mut()) else {
        panic!("old captured authority must not update the same-ID successor");
    };
    assert_eq!(error.reason(), TaskAccessReason::ScopeEnded);
    assert_eq!(error.progress(), TaskProgress::NotStarted);
    assert_eq!((managed_updates.get(), stale_updates.get()), (0, 0));
    assert_eq!(
        (managed_drops.get(), retained_drops.get(), old_drops.get()),
        (1, 1, 1)
    );
    assert!(old_weak.upgrade().is_none());
    assert_eq!(
        driver.finish(store.read(state_values)).unwrap(),
        (1, Some((7, 1, 100, 20)))
    );
    let successor = driver.finish(store.acquire(&site)).unwrap().unwrap();
    driver
        .finish(successor.update(|state| state.value += 1))
        .unwrap();
    assert_eq!(
        driver.finish(store.read(state_values)).unwrap(),
        (1, Some((7, 1, 101, 20)))
    );
    let state = driver.finish(store.shutdown_into_state()).unwrap();
    assert_eq!(state_values(&state), (1, Some((7, 1, 101, 20))));
    drop(state);
    assert_eq!((old_drops.get(), successor_drops.get()), (1, 1));
}
