//! Exercises real Local actors with owner-thread state, effects and query results.
//!
//! The manual host polls queued tasks in bounded rounds. Wake counts are checked
//! separately; successful completion alone does not prove wake-driven scheduling.

#![cfg(feature = "local")]

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::rc::{Rc, Weak};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};
use std::thread::{self, ThreadId};
use std::time::Duration;

use rca_core::MonotonicInstant;
use rca_core::local::{
    Effect, Feature, FeatureContext, Runtime, RuntimeDriver, RuntimeFuture, Store, Task,
};

/// Owns the queue and clock; retained runtime handles hold only weak references.
struct HostInner {
    owner: ThreadId,
    now: Cell<MonotonicInstant>,
    tasks: RefCell<VecDeque<RuntimeFuture>>,
    wake: Arc<HostWake>,
}

/// Counts notifications without polling or retaining owner-local work.
#[derive(Default)]
struct HostWake(AtomicUsize);
impl std::task::Wake for HostWake {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }
    fn wake_by_ref(self: &Arc<Self>) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

/// Rejects work after host loss by dropping it on the calling owner thread.
struct Driver(Weak<HostInner>);

impl std::fmt::Debug for Driver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("LocalStoreDriver")
    }
}

impl RuntimeDriver for Driver {
    fn spawn(&self, task: RuntimeFuture) {
        if let Some(host) = self.0.upgrade() {
            assert_eq!(thread::current().id(), host.owner);
            host.tasks.borrow_mut().push_back(task);
        }
    }

    fn now(&self) -> MonotonicInstant {
        self.0
            .upgrade()
            .map_or(MonotonicInstant::ZERO, |host| host.now.get())
    }

    fn sleep_until(&self, deadline: MonotonicInstant) -> RuntimeFuture {
        let host = Weak::clone(&self.0);
        Box::pin(std::future::poll_fn(move |_| {
            if host.upgrade().is_some_and(|host| host.now.get() < deadline) {
                Poll::Pending
            } else {
                Poll::Ready(())
            }
        }))
    }

    fn yield_now(&self) -> RuntimeFuture {
        let mut yielded = false;
        Box::pin(std::future::poll_fn(move |cx| {
            if yielded {
                Poll::Ready(())
            } else {
                yielded = true;
                cx.waker().wake_by_ref();
                Poll::Pending
            }
        }))
    }
}

/// Drives finite scenarios explicitly, not a general-purpose wake-driven executor.
struct Host(Rc<HostInner>);

impl Host {
    /// Starts an empty queue at zero on the current thread.
    fn new() -> Self {
        Self(Rc::new(HostInner {
            owner: thread::current().id(),
            now: Cell::new(MonotonicInstant::ZERO),
            tasks: RefCell::new(VecDeque::new()),
            wake: Arc::new(HostWake::default()),
        }))
    }

    /// Returns a runtime handle that cannot extend this host's lifetime.
    fn runtime(&self) -> Runtime {
        Runtime::new(Driver(Rc::downgrade(&self.0)))
    }

    /// Moves the clock forwards and allows the fixture's finite timer chain to settle.
    fn advance_to(&self, elapsed: Duration) {
        let next = MonotonicInstant::from_elapsed(elapsed);
        assert!(next >= self.0.now.get());
        self.0.now.set(next);
        for _ in 0..8 {
            self.turn();
        }
    }

    /// Polls each task present at entry once, outside the queue's mutable borrow.
    fn turn(&self) {
        let count = self.0.tasks.borrow().len();
        for _ in 0..count {
            let mut task = self.0.tasks.borrow_mut().pop_front().unwrap();
            let waker = Waker::from(Arc::clone(&self.0.wake));
            let mut cx = Context::from_waker(&waker);
            if task.as_mut().poll(&mut cx).is_pending() {
                self.0.tasks.borrow_mut().push_back(task);
            }
        }
    }

    /// Requires a finite operation to finish within 64 manually driven rounds.
    fn run<F: Future>(&self, future: F) -> F::Output {
        let mut future = std::pin::pin!(future);
        let mut cx = Context::from_waker(Waker::noop());
        for _ in 0..64 {
            if let Poll::Ready(value) = future.as_mut().poll(&mut cx) {
                return value;
            }
            self.turn();
        }
        panic!("finite local actor fixture did not complete")
    }
}

/// Detects destruction away from the owning thread and records the final release.
struct Interpreter {
    owner: ThreadId,
    drops: Rc<Cell<usize>>,
}

/// Retains a local payload and can deliberately fail during reduction.
struct OwnedFeature {
    interpreter: Rc<Interpreter>,
    panic: bool,
}
impl Feature for OwnedFeature {
    type State = Rc<Interpreter>;
    type Action = Rc<usize>;
    fn update(
        &mut self,
        _: &mut Self::State,
        _: &Self::Action,
        _: &mut FeatureContext<'_, Self::State, Self::Action>,
    ) {
        assert_eq!(thread::current().id(), self.interpreter.owner);
        assert!(!self.panic, "intentional local reducer panic");
    }
}
/// Shares an owner-thread payload while keeping its destruction count outside it.
fn interpreter(drops: &Rc<Cell<usize>>) -> Rc<Interpreter> {
    Rc::new(Interpreter {
        owner: thread::current().id(),
        drops: Rc::clone(drops),
    })
}

#[test]
fn local_default_constructor_keeps_capacity_and_owned_values_without_polling() {
    use rca_core::local::Reduce;

    let host = Host::new();
    let state = Rc::new(Cell::new(0));
    let action = Rc::new(1);
    let reducer = Reduce::new(
        |state: &mut Rc<Cell<usize>>, action: &Rc<usize>, _: &mut rca_core::ReduceContext<'_>| {
            state.set(state.get() + **action);
            Effect::none()
        },
    );
    let store = Store::new(Rc::clone(&state), reducer, host.runtime());
    assert_eq!(host.0.tasks.borrow().len(), 1);
    for _ in 0..rca_core::DEFAULT_MAILBOX_CAPACITY {
        drop(
            store
                .try_send(Rc::clone(&action))
                .expect("default mailbox has room"),
        );
    }
    let error = store
        .try_send(Rc::clone(&action))
        .expect_err("default mailbox is exactly full");
    assert!(error.is_full());
    drop(error);
    assert_eq!(state.get(), 0);
    assert_eq!(Rc::strong_count(&state), 2);
    drop(store);
    host.turn();
    assert!(host.0.tasks.borrow().is_empty());
    assert_eq!(Rc::strong_count(&state), 1);
    assert_eq!(Rc::strong_count(&action), 1);
}

#[cfg(feature = "test-util")]
#[test]
fn local_builder_rejections_preserve_gate_and_release_owned_inputs() {
    use rca_core::testing::local::TransactionGate;

    let host = Host::new();
    let drops = Rc::new(Cell::new(0));
    let state = interpreter(&drops);
    let feature = interpreter(&drops);
    let gate = TransactionGate::new();
    let build = |capacity| {
        Store::builder(
            Rc::clone(&state),
            OwnedFeature {
                interpreter: Rc::clone(&feature),
                panic: false,
            },
            host.runtime(),
        )
        .mailbox_capacity(capacity)
        .transaction_gate(gate.clone())
        .build()
    };
    assert!(build(0).unwrap_err().is_zero_capacity());
    assert!(host.0.tasks.borrow().is_empty());
    assert_eq!(
        (Rc::strong_count(&state), Rc::strong_count(&feature)),
        (1, 1)
    );
    let first = build(1).expect("invalid capacity leaves the gate available");
    assert!(build(0).unwrap_err().is_zero_capacity());
    assert!(build(1).unwrap_err().is_assertion_gate_attached());
    assert_eq!(host.0.tasks.borrow().len(), 1);
    assert_eq!(
        (Rc::strong_count(&state), Rc::strong_count(&feature)),
        (2, 2)
    );
    drop(host.0.tasks.borrow_mut().pop_front().unwrap());
    assert_eq!(
        (Rc::strong_count(&state), Rc::strong_count(&feature)),
        (1, 1)
    );
    let second = build(1).expect("unpolled actor teardown releases the gate");
    drop((first, second));
    host.turn();
    assert!(host.0.tasks.borrow().is_empty());
    assert_eq!(
        (Rc::strong_count(&state), Rc::strong_count(&feature)),
        (1, 1)
    );
    assert_eq!(drops.get(), 0);
    drop((state, feature));
    assert_eq!(drops.get(), 2);
}

#[test]
fn local_builder_debug_keeps_configuration_lazy_and_releases_unbuilt_captures() {
    use rca_core::local::Reduce;

    for capacity in [0, 3] {
        let host = Host::new();
        let state_drops = Rc::new(Cell::new(0));
        let reducer_drops = Rc::new(Cell::new(0));
        let calls = Rc::new(Cell::new(0));
        let observed = Rc::clone(&calls);
        let capture = interpreter(&reducer_drops);
        let reducer = Reduce::new(
            move |_: &mut (Rc<Interpreter>, String),
                  (): &(),
                  _: &mut rca_core::ReduceContext<'_>| {
                assert_eq!(thread::current().id(), capture.owner);
                observed.set(observed.get() + 1);
                Effect::none()
            },
        );
        let builder = Store::builder(
            (
                interpreter(&state_drops),
                String::from("private-local-state"),
            ),
            reducer,
            host.runtime(),
        )
        .mailbox_capacity(capacity);
        for debug in [format!("{builder:?}"), format!("{builder:#?}")] {
            assert!(debug.starts_with("StoreBuilder"));
            assert!(debug.contains("LocalStoreDriver"));
            assert!(debug.contains(&format!("mailbox_capacity: {capacity}")));
            assert!(!debug.contains("private-local-state") && !debug.contains("Interpreter"));
        }
        assert!(host.0.tasks.borrow().is_empty());
        assert_eq!(
            (calls.get(), state_drops.get(), reducer_drops.get()),
            (0, 0, 0)
        );
        if capacity == 0 {
            assert!(builder.build().unwrap_err().is_zero_capacity());
        } else {
            drop(builder);
        }
        assert!(host.0.tasks.borrow().is_empty());
        assert_eq!(
            (calls.get(), state_drops.get(), reducer_drops.get()),
            (0, 1, 1)
        );
    }
}

#[test]
fn local_feature_debug_preserves_owned_reduction_effect_and_store_lifecycle() {
    use rca_core::local::Reduce;

    let host = Host::new();
    let calls = Rc::new(Cell::new(0));
    let reductions = Rc::clone(&calls);
    let starts = Rc::new(Cell::new(0));
    let operations = Rc::clone(&starts);
    let private = Rc::new(String::from("private-local-reducer-capture"));
    let weak = Rc::downgrade(&private);
    let feature = Reduce::new(
        move |state: &mut Vec<usize>, action: &Rc<usize>, _: &mut rca_core::ReduceContext<'_>| {
            reductions.set(reductions.get() + 1);
            state.push(**action);
            if **action != 1 {
                return Effect::none();
            }
            let capture = Rc::clone(&private);
            let running = Rc::clone(&operations);
            let observed_reductions = Rc::clone(&reductions);
            let effect = Effect::run(move |sender| async move {
                running.set(running.get() + 1);
                for debug in [format!("{sender:?}"), format!("{sender:#?}")] {
                    assert!(debug.starts_with("EffectSender") && debug.contains(".."));
                    assert!(!debug.contains(capture.as_str()));
                }
                assert_eq!((running.get(), observed_reductions.get()), (1, 1));
                sender.send(Rc::new(2)).await.unwrap();
            });
            for debug in [format!("{effect:?}"), format!("{effect:#?}")] {
                assert!(debug.starts_with("Effect") && debug.contains(".."));
                assert!(!debug.contains(private.as_str()));
            }
            assert_eq!(operations.get(), 0);
            effect
        },
    );
    for debug in [format!("{feature:?}"), format!("{feature:#?}")] {
        assert!(debug.starts_with("Reduce"));
        assert!(!debug.contains("private-local-reducer-capture"));
    }
    let builder = Store::builder(Vec::new(), feature, host.runtime()).mailbox_capacity(1);
    for debug in [format!("{builder:?}"), format!("{builder:#?}")] {
        assert!(debug.starts_with("StoreBuilder"));
        assert!(debug.contains("LocalStoreDriver"));
        assert!(debug.contains("mailbox_capacity: 1"));
        assert!(!debug.contains("private-local-reducer-capture"));
    }
    assert_eq!((calls.get(), starts.get()), (0, 0));
    assert!(host.0.tasks.borrow().is_empty());
    assert!(weak.upgrade().is_some());
    let store = builder.build().unwrap();
    let retained = store.clone();
    drop(store);
    for debug in [format!("{retained:?}"), format!("{retained:#?}")] {
        assert!(debug.starts_with("Store") && debug.contains("closed: false"));
        assert!(!debug.contains("private-local-reducer-capture"));
    }
    assert_eq!((calls.get(), starts.get()), (0, 0));
    let task = host.run(retained.send(Rc::new(1))).unwrap();
    host.run(task.finish());
    assert_eq!(host.run(retained.read(Clone::clone)).unwrap(), [1, 2]);
    assert_eq!((calls.get(), starts.get()), (2, 1));
    host.run(retained.shutdown()).unwrap();
    for debug in [format!("{retained:?}"), format!("{retained:#?}")] {
        assert!(debug.starts_with("Store") && debug.contains("closed: true"));
        assert!(!debug.contains("private-local-reducer-capture"));
    }
    assert!(weak.upgrade().is_none());
    assert_eq!((calls.get(), starts.get()), (2, 1));
    assert!(host.0.tasks.borrow().is_empty());
}

#[test]
fn closed_local_predicate_waits_do_not_select_or_retain_rejected_actions() {
    let host = Host::new();
    let state_drops = Rc::new(Cell::new(0));
    let feature_drops = Rc::new(Cell::new(0));
    let store = Store::new(
        interpreter(&state_drops),
        OwnedFeature {
            interpreter: interpreter(&feature_drops),
            panic: false,
        },
        host.runtime(),
    );
    host.run(store.shutdown()).unwrap();
    assert_eq!((state_drops.get(), feature_drops.get()), (1, 1));
    let calls = Rc::new(Cell::new(0));
    let selected = Rc::clone(&calls);
    assert_eq!(
        host.run(store.wait_while(move |_| {
            selected.set(selected.get() + 1);
            true
        })),
        rca_core::PredicateWaitOutcome::StoreStopped
    );
    let action = Rc::new(7);
    let weak = Rc::downgrade(&action);
    let selected = Rc::clone(&calls);
    let wait = store.send_and_wait_while(action, move |_| {
        selected.set(selected.get() + 1);
        true
    });
    assert!(weak.upgrade().is_some());
    assert_eq!(host.run(wait), rca_core::PredicateWaitOutcome::StoreStopped);
    assert!(weak.upgrade().is_none());
    assert_eq!(calls.get(), 0);
    assert_eq!(Rc::strong_count(&calls), 1);
    assert!(host.0.tasks.borrow().is_empty());
}

#[test]
fn retained_runtime_rejects_a_new_actor_after_host_teardown_on_owner() {
    let host = Host::new();
    let runtime = host.runtime();
    drop(host);
    let state_drops = Rc::new(Cell::new(0));
    let feature_drops = Rc::new(Cell::new(0));
    let store = Store::new(
        interpreter(&state_drops),
        OwnedFeature {
            interpreter: interpreter(&feature_drops),
            panic: false,
        },
        runtime,
    );
    assert!(store.is_closed());
    assert_eq!(state_drops.get(), 1);
    assert_eq!(feature_drops.get(), 1);
    store
        .try_send(Rc::new(1))
        .expect_err("closed store rejects admission");
    drop(store);
    assert_eq!((state_drops.get(), feature_drops.get()), (1, 1));
}

#[test]
fn bounded_local_admission_preserves_rejected_rc_action_and_final_owner_cleanup() {
    let host = Host::new();
    let state_drops = Rc::new(Cell::new(0));
    let feature_drops = Rc::new(Cell::new(0));
    let store = Store::builder(
        interpreter(&state_drops),
        OwnedFeature {
            interpreter: interpreter(&feature_drops),
            panic: false,
        },
        host.runtime(),
    )
    .mailbox_capacity(1)
    .build()
    .unwrap();
    let admitted = store.try_send(Rc::new(1)).unwrap();
    let action = Rc::new(2);
    let rejected = store.try_send(Rc::clone(&action)).unwrap_err();
    assert!(Rc::ptr_eq(&rejected.into_action(), &action));
    let mut closed = std::pin::pin!(store.closed());
    assert!(
        closed
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_pending()
    );
    drop(store);
    assert!(
        closed
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_ready()
    );
    host.turn();
    host.run(admitted.finish())
        .expect_err("final owner cleanup rejects pending admission");
    assert_eq!((state_drops.get(), feature_drops.get()), (1, 1));
}

// Catches a Local actor exhausting ready input before yielding to its owner host.
#[test]
fn local_ready_ingress_yields_before_exhaustion_with_and_without_timer() {
    use futures_util::FutureExt as _;

    const BACKLOG: usize = 129;
    #[derive(Debug)]
    enum BudgetAction {
        ArmTimer,
        Commit(Rc<usize>),
        TimerFired,
    }

    for with_timer in [false, true] {
        let host = Host::new();
        let count = Rc::new(Cell::new(0));
        let reductions = Rc::clone(&count);
        let owner = thread::current().id();
        let reducer = rca_core::local::Reduce::new(
            move |state: &mut Vec<Rc<usize>>,
                  action: &BudgetAction,
                  _: &mut rca_core::ReduceContext<'_>| {
                assert_eq!(thread::current().id(), owner);
                match action {
                    BudgetAction::ArmTimer => {
                        Effect::send_at(MonotonicInstant::MAX, BudgetAction::TimerFired)
                    }
                    BudgetAction::Commit(value) => {
                        state.push(Rc::clone(value));
                        reductions.set(reductions.get() + 1);
                        Effect::none()
                    }
                    BudgetAction::TimerFired => panic!("saturated timer must not fire"),
                }
            },
        );
        let store = Store::builder(Vec::new(), reducer, host.runtime())
            .mailbox_capacity(BACKLOG + 1)
            .build()
            .unwrap();
        let timer = with_timer.then(|| store.try_send(BudgetAction::ArmTimer).unwrap());
        let values: Vec<_> = (0..BACKLOG).map(Rc::new).collect();
        let mut receipts: Vec<_> = values
            .iter()
            .map(|value| {
                store
                    .try_send(BudgetAction::Commit(Rc::clone(value)))
                    .unwrap()
            })
            .collect();
        let mut last_receipt = Box::pin(receipts.pop().unwrap().finish());
        let mut cx = Context::from_waker(Waker::noop());
        assert!(last_receipt.as_mut().poll(&mut cx).is_pending());

        // The existing host polls each queued task once; only the actor is queued.
        assert_eq!(host.0.tasks.borrow().len(), 1);
        host.turn();
        assert!(count.get() > 0 && count.get() < BACKLOG);
        assert!(last_receipt.as_mut().poll(&mut cx).is_pending());
        for _ in 0..BACKLOG {
            if count.get() == BACKLOG {
                break;
            }
            host.turn();
        }
        assert_eq!(count.get(), BACKLOG);
        assert!(last_receipt.now_or_never().unwrap().unwrap().is_finished());
        let timer_task = timer.map(|receipt| receipt.finish().now_or_never().unwrap().unwrap());
        for receipt in receipts {
            let task = receipt.finish().now_or_never().unwrap().unwrap();
            assert!(task.is_finished());
            assert!(!task.is_cancelled());
        }
        let state = host.run(store.shutdown_into_state()).unwrap();
        assert_eq!(state.len(), BACKLOG);
        for (actual, expected) in state.iter().zip(&values) {
            assert!(
                Rc::ptr_eq(actual, expected),
                "state handoff preserves FIFO ownership"
            );
        }
        if let Some(task) = timer_task {
            assert!(task.is_finished());
            assert!(
                task.is_cancelled(),
                "shutdown retired the active Local timer"
            );
        }
        assert!(host.0.tasks.borrow().is_empty());
    }
}

#[cfg(feature = "test-util")]
#[test]
fn local_runtime_snapshots_are_ordered_and_stop_after_graceful_or_host_closure() {
    use futures_util::FutureExt as _;

    for host_loss in [false, true] {
        let host = Host::new();
        let state_drops = Rc::new(Cell::new(0));
        let feature_drops = Rc::new(Cell::new(0));
        let store = Store::new(
            interpreter(&state_drops),
            OwnedFeature {
                interpreter: interpreter(&feature_drops),
                panic: false,
            },
            host.runtime(),
        );
        host.run(store.ready()).unwrap();
        let before = host.run(store.runtime_snapshot()).unwrap();
        assert_eq!(before.commit_version, 0);
        let task = host.run(store.send(Rc::new(1))).unwrap();
        host.run(task.finish());
        let after = host.run(store.runtime_snapshot()).unwrap();
        assert_eq!(after.commit_version, 1);
        assert_eq!(after.live_task_ids, [] as [u64; 0]);
        assert_eq!(after.cancellation_count, 0);
        assert_eq!(after.timer_deadlines, [] as [MonotonicInstant; 0]);
        assert_eq!(after.observation_count, 0);
        let mut closed = Box::pin(store.closed());
        assert!(
            closed
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending()
        );
        if host_loss {
            drop(host);
        } else {
            host.run(store.shutdown()).unwrap();
        }
        assert_eq!(closed.now_or_never(), Some(()));
        assert!(store.is_closed());
        assert_eq!((state_drops.get(), feature_drops.get()), (1, 1));
        let _: rca_core::StoreStopped = store
            .runtime_snapshot()
            .now_or_never()
            .expect("closed snapshot needs no host progress")
            .expect_err("closed store cannot supply a snapshot");
    }
}

#[test]
fn local_state_handoff_follows_feature_drop_and_panic_closes_without_handoff() {
    let host = Host::new();
    let state_drops = Rc::new(Cell::new(0));
    let feature_drops = Rc::new(Cell::new(0));
    let store = Store::new(
        interpreter(&state_drops),
        OwnedFeature {
            interpreter: interpreter(&feature_drops),
            panic: false,
        },
        host.runtime(),
    );
    let state = host.run(store.shutdown_into_state()).unwrap();
    assert_eq!((state_drops.get(), feature_drops.get()), (0, 1));
    drop(state);
    assert_eq!(state_drops.get(), 1);

    let store = Store::new(
        interpreter(&state_drops),
        OwnedFeature {
            interpreter: interpreter(&feature_drops),
            panic: true,
        },
        host.runtime(),
    );
    let admitted = store.try_send(Rc::new(1)).unwrap();
    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| host.turn()));
    assert!(panic.is_err());
    assert!(store.is_closed());
    host.run(admitted.finish())
        .expect_err("reducer panic rejects pending admission");
    assert!(host.run(store.shutdown_into_state()).is_err());
    assert_eq!((state_drops.get(), feature_drops.get()), (2, 2));
}

#[test]
fn cancellation_drops_polled_and_unpolled_local_task_captures_on_owner() {
    /// Moves one local capture into a task whose cancellation must release it.
    struct PendingFeature {
        capture: Option<Rc<Interpreter>>,
        starts: Rc<Cell<usize>>,
    }
    impl Feature for PendingFeature {
        type State = ();
        type Action = ();
        fn update(&mut self, (): &mut (), (): &(), context: &mut FeatureContext<'_, (), ()>) {
            let capture = self.capture.take().unwrap();
            let starts = Rc::clone(&self.starts);
            context.task(Task::run(move |_| async move {
                starts.set(starts.get() + 1);
                std::future::pending::<()>().await;
                drop(capture);
            }));
        }
    }
    for poll_first in [false, true] {
        let host = Host::new();
        let drops = Rc::new(Cell::new(0));
        let starts = Rc::new(Cell::new(0));
        let store = Store::new(
            (),
            PendingFeature {
                capture: Some(interpreter(&drops)),
                starts: Rc::clone(&starts),
            },
            host.runtime(),
        );
        let task = host.run(store.send(())).unwrap();
        if poll_first {
            host.turn();
        }
        assert_eq!(starts.get(), usize::from(poll_first));
        task.cancel();
        host.run(task.finish());
        assert_eq!(drops.get(), 1);
        host.run(store.shutdown()).unwrap();
        assert_eq!(drops.get(), 1);
    }
}

#[test]
fn reentrant_feature_destructor_observes_closed_admission_and_panic_prevents_state_handoff() {
    type OwnerStore = Store<Rc<Interpreter>, Rc<usize>>;
    /// Attempts admission during feature destruction, after the actor closes ingress.
    struct Reentrant {
        held: Rc<RefCell<Option<OwnerStore>>>,
        calls: Rc<Cell<usize>>,
        owner: ThreadId,
        panic: bool,
    }
    impl Feature for Reentrant {
        type State = Rc<Interpreter>;
        type Action = Rc<usize>;
        fn update(
            &mut self,
            _: &mut Self::State,
            _: &Self::Action,
            _: &mut FeatureContext<'_, Self::State, Self::Action>,
        ) {
        }
    }
    impl Drop for Reentrant {
        fn drop(&mut self) {
            assert_eq!(thread::current().id(), self.owner);
            let store = self.held.borrow_mut().take().unwrap();
            // Admission closes before user destruction; lifecycle publication
            // remains the final ActorGuard responsibility after cleanup.
            store
                .try_send(Rc::new(1))
                .expect_err("admission closes before feature destruction");
            self.calls.set(self.calls.get() + 1);
            assert!(
                !self.panic,
                "intentional reentrant feature destructor panic"
            );
        }
    }
    for panic in [false, true] {
        let host = Host::new();
        let drops = Rc::new(Cell::new(0));
        let calls = Rc::new(Cell::new(0));
        let held = Rc::new(RefCell::new(None));
        let store = Store::new(
            interpreter(&drops),
            Reentrant {
                held: Rc::clone(&held),
                calls: Rc::clone(&calls),
                owner: thread::current().id(),
                panic,
            },
            host.runtime(),
        );
        *held.borrow_mut() = Some(store.clone());
        let mut handoff = std::pin::pin!(store.shutdown_into_state());
        let mut cx = Context::from_waker(Waker::noop());
        assert!(handoff.as_mut().poll(&mut cx).is_pending());
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| host.turn()));
        assert_eq!(result.is_err(), panic);
        assert_eq!(calls.get(), 1);
        let result = host.run(handoff);
        assert_eq!(result.is_err(), panic);
        if let Ok(state) = result {
            assert_eq!(drops.get(), 0);
            drop(state);
        }
        assert_eq!(drops.get(), 1);
        assert!(held.borrow().is_none());
    }
}

#[test]
fn remote_and_stale_actor_wakers_never_poll_or_drop_local_work_off_owner() {
    /// Exports only the task's thread-safe waker, retaining its payload locally.
    struct Suspended {
        capture: Option<Rc<Interpreter>>,
        polls: Rc<Cell<usize>>,
        waker: Arc<Mutex<Option<Waker>>>,
    }
    impl Feature for Suspended {
        type State = ();
        type Action = ();
        fn update(&mut self, (): &mut (), (): &(), context: &mut FeatureContext<'_, (), ()>) {
            let capture = self.capture.take().unwrap();
            let polls = Rc::clone(&self.polls);
            let waker = Arc::clone(&self.waker);
            context.task(Task::run(move |_| {
                std::future::poll_fn(move |cx| {
                    assert_eq!(thread::current().id(), capture.owner);
                    polls.set(polls.get() + 1);
                    *waker.lock().unwrap() = Some(cx.waker().clone());
                    Poll::Pending
                })
            }));
        }
    }
    let host = Host::new();
    let drops = Rc::new(Cell::new(0));
    let polls = Rc::new(Cell::new(0));
    let saved = Arc::new(Mutex::new(None));
    let store = Store::new(
        (),
        Suspended {
            capture: Some(interpreter(&drops)),
            polls: Rc::clone(&polls),
            waker: Arc::clone(&saved),
        },
        host.runtime(),
    );
    let task = host.run(store.send(())).unwrap();
    host.turn();
    assert_eq!(polls.get(), 1);
    let waker = saved.lock().unwrap().take().unwrap();
    let remote = waker.clone();
    thread::spawn(move || {
        remote.wake_by_ref();
        drop(remote);
    })
    .join()
    .unwrap();
    assert_eq!((polls.get(), drops.get()), (1, 0));
    host.turn();
    assert_eq!(polls.get(), 2);
    drop(host);
    assert!(store.is_closed());
    assert_eq!(drops.get(), 1);
    thread::spawn(move || {
        waker.wake_by_ref();
        drop(waker);
    })
    .join()
    .unwrap();
    assert_eq!((polls.get(), drops.get()), (2, 1));
    drop(task);
    drop(store);
}

impl Drop for Interpreter {
    fn drop(&mut self) {
        assert_eq!(thread::current().id(), self.owner);
        self.drops.set(self.drops.get() + 1);
    }
}

/// Holds application state together with a non-Send owner-thread resource.
struct State {
    interpreter: Rc<Interpreter>,
    value: usize,
}

/// Starts the typed task or delivers its owned local value.
enum Action {
    Start,
    Set(Rc<usize>),
}

/// Exercises send, read and update through one task while retaining local results.
struct Counter {
    owner: ThreadId,
    results: Rc<RefCell<Vec<Rc<usize>>>>,
}

impl Feature for Counter {
    type State = State;
    type Action = Action;

    fn update(
        &mut self,
        state: &mut State,
        action: &Action,
        context: &mut FeatureContext<'_, State, Action>,
    ) {
        assert_eq!(thread::current().id(), self.owner);
        let before = (state.value, self.results.borrow().len());
        for debug in [format!("{context:?}"), format!("{context:#?}")] {
            assert!(debug.starts_with("FeatureContext") && debug.contains(".."));
            assert!(!debug.contains("interpreter") && !debug.contains("results"));
        }
        assert_eq!((state.value, self.results.borrow().len()), before);
        match action {
            Action::Set(value) => state.value = **value,
            Action::Start => {
                let results = Rc::clone(&self.results);
                let interpreter = Rc::clone(&state.interpreter);
                context.task(Task::<State, Action>::run(move |context| async move {
                    assert_eq!(thread::current().id(), interpreter.owner);
                    let _ = context.send(Action::Set(Rc::new(7))).await.unwrap();
                    let read = context.read(|state| Rc::new(state.value)).await.unwrap();
                    let update = context
                        .update(|state| {
                            state.value += 1;
                            Rc::new(state.value)
                        })
                        .await
                        .unwrap();
                    results
                        .borrow_mut()
                        .extend([read, Rc::clone(update.value())]);
                }));
            }
        }
    }
}

#[test]
fn real_local_actor_owns_rc_state_action_feature_task_and_query_results() {
    let host = Host::new();
    let drops = Rc::new(Cell::new(0));
    let interpreter = Rc::new(Interpreter {
        owner: thread::current().id(),
        drops: Rc::clone(&drops),
    });
    let results = Rc::new(RefCell::new(Vec::new()));
    let store = Store::builder(
        State {
            interpreter: Rc::clone(&interpreter),
            value: 0,
        },
        Counter {
            owner: thread::current().id(),
            results: Rc::clone(&results),
        },
        host.runtime(),
    )
    .mailbox_capacity(1)
    .build()
    .unwrap();
    host.run(store.ready()).unwrap();
    let task = host.run(store.send(Action::Start)).unwrap();
    host.run(task.finish());
    assert_eq!(
        results
            .borrow()
            .iter()
            .map(|value| **value)
            .collect::<Vec<_>>(),
        [7, 8]
    );
    assert_eq!(
        *host.run(store.read(|state| Rc::new(state.value))).unwrap(),
        8
    );
    let state = host.run(store.shutdown_into_state()).unwrap();
    assert!(Rc::ptr_eq(&state.interpreter, &interpreter));
    drop(state);
    drop(interpreter);
    assert_eq!(drops.get(), 1);
}

#[test]
fn local_observation_and_predicate_callbacks_keep_rc_on_the_owner() {
    use rca_core::local::{Effect, Reduce};
    /// Makes owner identity and private data visible to the test, not Debug output.
    struct Publication {
        capture: Rc<()>,
        value: usize,
        private: &'static str,
    }
    let host = Host::new();
    let owner = thread::current().id();
    let capture = Rc::new(());
    let store = Store::new(
        0usize,
        Reduce::new(
            |state: &mut usize, (): &(), _: &mut rca_core::ReduceContext<'_>| {
                *state += 1;
                Effect::none()
            },
        ),
        host.runtime(),
    );
    let selector_capture = Rc::clone(&capture);
    let selections = Rc::new(Cell::new(0));
    let selected = Rc::clone(&selections);
    let mut observation = host
        .run(store.observe(move |state| {
            assert_eq!(thread::current().id(), owner);
            selected.set(selected.get() + 1);
            Publication {
                capture: Rc::clone(&selector_capture),
                value: *state,
                private: "private-local-publication",
            }
        }))
        .unwrap();
    let debug = format!("{observation:?}");
    assert!(debug.starts_with("Observation {"));
    assert!(debug.contains("active: true"));
    assert!(!debug.contains("Publication") && !debug.contains("capture"));
    assert!(!debug.contains("private-local-publication"));
    assert_eq!(selections.get(), 1);
    let initial = host.run(observation.next()).unwrap();
    assert!(Rc::ptr_eq(&initial.capture, &capture));
    assert_eq!(initial.value, 0);
    assert_eq!(initial.private, "private-local-publication");
    assert_eq!(format!("{observation:?}"), debug);
    assert_eq!(selections.get(), 1);
    let _ = host.run(store.send(())).unwrap();
    assert_eq!(host.run(observation.next()).unwrap().value, 1);
    let predicate_capture = Rc::clone(&capture);
    assert_eq!(
        host.run(store.wait_while(move |state| {
            assert_eq!(thread::current().id(), owner);
            assert!(Rc::ptr_eq(&predicate_capture, &capture));
            *state < 1
        })),
        rca_core::PredicateWaitOutcome::Satisfied
    );
    assert_eq!(
        host.run(store.send_and_wait_while((), |state| *state < 2)),
        rca_core::PredicateWaitOutcome::Satisfied
    );
    host.run(store.shutdown()).unwrap();
    assert!(host.run(observation.next()).is_none());
}

#[cfg(feature = "futures-stream")]
#[test]
fn local_observation_stream_preserves_initial_then_latest_owned_publication_and_closes() {
    use futures_util::StreamExt;
    use rca_core::local::Reduce;

    let host = Host::new();
    let owner = thread::current().id();
    let selections = Rc::new(Cell::new(0));
    let selected = Rc::clone(&selections);
    let store = Store::new(
        0usize,
        Reduce::new(
            |state: &mut usize, (): &(), _: &mut rca_core::ReduceContext<'_>| {
                *state += 1;
                Effect::none()
            },
        ),
        host.runtime(),
    );
    let mut values = host
        .run(store.observe(move |state| {
            assert_eq!(thread::current().id(), owner);
            selected.set(selected.get() + 1);
            Rc::new(Cell::new(*state))
        }))
        .unwrap();
    drop(host.run(store.send(())).unwrap());
    drop(host.run(store.send(())).unwrap());
    assert_eq!(selections.get(), 3);
    let before = format!("{values:?}");
    let initial = host.run(StreamExt::next(&mut values)).unwrap();
    let initial_owner = Rc::downgrade(&initial);
    assert_eq!(
        initial.get(),
        0,
        "Stream must retain the unread initial value"
    );
    assert_eq!(format!("{values:?}"), before);
    let latest = host.run(StreamExt::next(&mut values)).unwrap();
    let latest_owner = Rc::downgrade(&latest);
    assert_eq!(latest.get(), 2, "unread commits coalesce, not queue");
    assert!(!Rc::ptr_eq(&initial, &latest));
    let pending = {
        let mut next = std::pin::pin!(StreamExt::next(&mut values));
        next.as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_pending()
    };
    assert!(pending);
    host.run(store.shutdown()).unwrap();
    assert!(host.run(StreamExt::next(&mut values)).is_none());
    assert!(host.run(StreamExt::next(&mut values)).is_none());
    assert!(format!("{values:?}").contains("active: false"));
    assert_eq!(selections.get(), 3);
    drop(values);
    drop((initial, latest));
    assert!(initial_owner.upgrade().is_none());
    assert!(latest_owner.upgrade().is_none());
    assert_eq!(Rc::strong_count(&selections), 1);
    assert!(host.0.tasks.borrow().is_empty());
}

#[test]
fn local_predicate_debug_does_not_select_or_admit_its_owned_action() {
    use rca_core::local::Reduce;

    let host = Host::new();
    let reductions = Rc::new(Cell::new(0));
    let reduced = Rc::clone(&reductions);
    let store = Store::new(
        0usize,
        Reduce::new(
            move |state: &mut usize, action: &Rc<String>, _: &mut rca_core::ReduceContext<'_>| {
                assert_eq!(action.as_str(), "private-predicate-action");
                reduced.set(reduced.get() + 1);
                *state += 1;
                Effect::none()
            },
        ),
        host.runtime(),
    );
    let selections = Rc::new(Cell::new(0));
    let selected = Rc::clone(&selections);
    let action = Rc::new(String::from("private-predicate-action"));
    let retained = Rc::downgrade(&action);
    let mut wait = store.send_and_wait_while(action, move |state| {
        selected.set(selected.get() + 1);
        *state < 1
    });
    assert_eq!(format!("{wait:?}"), "PredicateWait { outcome: None, .. }");
    host.turn();
    assert_eq!(reductions.get(), 0, "formatting must not admit the action");
    assert_eq!(
        selections.get(),
        0,
        "formatting must not register the predicate"
    );
    assert!(retained.upgrade().is_some());
    assert_eq!(
        host.run(&mut wait),
        rca_core::PredicateWaitOutcome::Satisfied
    );
    assert_eq!(
        format!("{wait:?}"),
        "PredicateWait { outcome: Some(Satisfied), .. }"
    );
    assert_eq!((reductions.get(), selections.get()), (1, 1));
    assert!(retained.upgrade().is_none());
    // Completing the wait deactivates its observation; the actor's next control
    // turn removes the registry entry that owns the selector capture.
    host.turn();
    assert_eq!((reductions.get(), selections.get()), (1, 1));
    assert_eq!(Rc::strong_count(&selections), 1);

    let selected = Rc::clone(&selections);
    let action = Rc::new(String::from("private-predicate-action"));
    let retained = Rc::downgrade(&action);
    let mut wait = store.send_and_wait_while(action, move |state| {
        selected.set(selected.get() + 1);
        *state < 2
    });
    assert!(
        std::pin::Pin::new(&mut wait)
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_pending()
    );
    // Commit the action without resuming the waiter, then close the actor
    // before the waiter can register its observation.
    host.turn();
    assert_eq!((reductions.get(), selections.get()), (2, 1));
    assert!(retained.upgrade().is_none());
    host.run(store.shutdown()).unwrap();
    assert_eq!(
        host.run(&mut wait),
        rca_core::PredicateWaitOutcome::StoreStopped
    );
    assert_eq!(
        format!("{wait:?}"),
        "PredicateWait { outcome: Some(StoreStopped), .. }"
    );
    assert_eq!((reductions.get(), selections.get()), (2, 1));
    assert_eq!(Rc::strong_count(&selections), 1);
    assert!(host.0.tasks.borrow().is_empty());
}

#[test]
fn local_distinct_projection_suppresses_equal_values_and_releases_owned_captures() {
    use rca_core::local::Reduce;

    for close_store in [false, true] {
        let host = Host::new();
        let drops = Rc::new(Cell::new(0));
        let capture = interpreter(&drops);
        let selections = Rc::new(Cell::new(0));
        let calls = Rc::clone(&selections);
        let store = Store::new(
            0usize,
            Reduce::new(
                |state: &mut usize, (): &(), _: &mut rca_core::ReduceContext<'_>| {
                    *state += 1;
                    Effect::none()
                },
            ),
            host.runtime(),
        );
        let mut observation = host
            .run(store.observe_distinct(move |state| {
                assert_eq!(thread::current().id(), capture.owner);
                calls.set(calls.get() + 1);
                Rc::new(*state / 2)
            }))
            .unwrap();
        let initial = host.run(observation.next()).unwrap();
        assert_eq!(*initial, 0);
        let initial_weak = Rc::downgrade(&initial);
        drop(initial);
        assert_eq!(selections.get(), 1);
        drop(host.run(store.send(())).unwrap());
        assert_eq!(selections.get(), 2);
        let stayed_pending = {
            let mut next = std::pin::pin!(observation.next());
            assert!(
                next.as_mut()
                    .poll(&mut Context::from_waker(Waker::noop()))
                    .is_pending()
            );
            host.turn();
            next.as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending()
        };
        assert!(stayed_pending);
        drop(host.run(store.send(())).unwrap());
        let changed = host.run(observation.next()).unwrap();
        assert_eq!(*changed, 1);
        assert_eq!(selections.get(), 3);
        assert!(initial_weak.upgrade().is_none());
        let changed_weak = Rc::downgrade(&changed);
        drop(changed);
        if close_store {
            host.run(store.shutdown()).unwrap();
            assert!(host.run(observation.next()).is_none());
            drop(observation);
        } else {
            drop(observation);
            host.turn();
            assert_eq!(drops.get(), 1);
            drop(host.run(store.send(())).unwrap());
            assert_eq!(selections.get(), 3);
            host.run(store.shutdown()).unwrap();
        }
        assert_eq!(drops.get(), 1);
        assert!(changed_weak.upgrade().is_none());
        assert_eq!(Rc::strong_count(&selections), 1);
    }
}

/// Separates same-key coexistence, replacement and a numerically equal different key type.
enum CancellationKind {
    Coexist,
    Replace,
    OtherType,
}

/// Starts controlled pending effects, cancels their typed key or records completion.
enum CancellationAction {
    Start {
        value: Rc<usize>,
        ready: Rc<Cell<bool>>,
        kind: CancellationKind,
    },
    Cancel,
    Record(Rc<usize>),
}

/// Keeps pending effect values alive until explicit completion or cancellation.
struct CancellableFeature;

impl Feature for CancellableFeature {
    type State = Vec<usize>;
    type Action = CancellationAction;

    fn update(
        &mut self,
        state: &mut Self::State,
        action: &Self::Action,
        context: &mut FeatureContext<'_, Self::State, Self::Action>,
    ) {
        match action {
            CancellationAction::Start { value, ready, kind } => {
                let value = Rc::clone(value);
                let ready = Rc::clone(ready);
                let effect = Effect::future(async move {
                    std::future::poll_fn(|_| {
                        if ready.get() {
                            Poll::Ready(())
                        } else {
                            Poll::Pending
                        }
                    })
                    .await;
                    Some(CancellationAction::Record(value))
                });
                context.effect(match kind {
                    CancellationKind::Coexist => effect.cancellable(7_u8),
                    CancellationKind::Replace => effect.cancellable_in_flight(7_u8),
                    CancellationKind::OtherType => effect.cancellable(7_u16),
                });
            }
            CancellationAction::Cancel => context.effect(Effect::cancel(7_u8)),
            CancellationAction::Record(value) => state.push(**value),
        }
    }
}

#[test]
fn local_effect_cancellation_preserves_typed_generation_ownership() {
    let host = Host::new();
    let store = Store::new(Vec::new(), CancellableFeature, host.runtime());
    host.run(store.ready()).unwrap();
    let start = |value, kind| {
        let value = Rc::new(value);
        let weak = Rc::downgrade(&value);
        let ready = Rc::new(Cell::new(false));
        let task = host
            .run(store.send(CancellationAction::Start {
                value,
                ready: Rc::clone(&ready),
                kind,
            }))
            .unwrap();
        (task, ready, weak)
    };

    let (first, _, first_value) = start(1, CancellationKind::Coexist);
    let (second, _, second_value) = start(2, CancellationKind::Coexist);
    let (other, other_ready, other_value) = start(3, CancellationKind::OtherType);
    assert!(!first.is_finished() && !second.is_finished() && !other.is_finished());
    assert!(first_value.upgrade().is_some() && second_value.upgrade().is_some());

    let cancellation = host.run(store.send(CancellationAction::Cancel)).unwrap();
    host.run(cancellation.finish());
    host.run(first.finish());
    host.run(second.finish());
    assert!(first_value.upgrade().is_none() && second_value.upgrade().is_none());
    assert!(!other.is_finished());
    assert!(other_value.upgrade().is_some());

    let (old, _, old_value) = start(4, CancellationKind::Coexist);
    let (also_old, _, also_old_value) = start(5, CancellationKind::Coexist);
    let (replacement, replacement_ready, replacement_value) = start(6, CancellationKind::Replace);
    host.run(old.finish());
    host.run(also_old.finish());
    assert!(old_value.upgrade().is_none() && also_old_value.upgrade().is_none());
    assert!(!replacement.is_finished() && !other.is_finished());
    assert!(replacement_value.upgrade().is_some());

    replacement_ready.set(true);
    host.run(replacement.finish());
    assert!(replacement_value.upgrade().is_none());
    assert_eq!(host.run(store.read(Clone::clone)).unwrap(), [6]);
    other_ready.set(true);
    host.run(other.finish());
    assert!(other_value.upgrade().is_none());
    assert_eq!(host.run(store.shutdown_into_state()).unwrap(), [6, 3]);
}

/// Starts its owned programme once on None and records later emitted values.
struct TimerFeature(Option<Effect<Option<Rc<usize>>>>);

impl Feature for TimerFeature {
    type State = Vec<usize>;
    type Action = Option<Rc<usize>>;

    fn update(
        &mut self,
        state: &mut Self::State,
        action: &Self::Action,
        context: &mut FeatureContext<'_, Self::State, Self::Action>,
    ) {
        if let Some(value) = action {
            state.push(**value);
        } else {
            context.effect(self.0.take().expect("one timer programme starts"));
        }
    }
}

#[test]
fn local_absolute_deadline_is_not_rebased_by_predecessor_or_delay() {
    let host = Host::new();
    let runtime = host.runtime();
    let deadline = runtime.now().saturating_add(Duration::from_secs(6));
    let effect = Effect::concatenate([
        Effect::send_after(Duration::from_secs(5), Some(Rc::new(1))),
        Effect::send_at(deadline, Some(Rc::new(2))).delay(Duration::from_secs(2)),
    ]);
    let store = Store::new(Vec::new(), TimerFeature(Some(effect)), runtime);
    let task = host.run(store.send(None)).unwrap();

    host.advance_to(Duration::from_secs(4));
    assert_eq!(
        host.run(store.read(Clone::clone)).unwrap(),
        [] as [usize; 0]
    );
    host.advance_to(Duration::from_secs(5));
    assert_eq!(host.run(store.read(Clone::clone)).unwrap(), [1]);
    host.advance_to(Duration::from_secs(6));
    assert_eq!(host.run(store.read(Clone::clone)).unwrap(), [1]);
    assert!(!task.is_finished());
    host.advance_to(Duration::from_secs(7));
    host.run(task.finish());
    assert_eq!(host.run(store.shutdown_into_state()).unwrap(), [1, 2]);
}

#[test]
fn local_run_defers_factory_and_releases_each_capture_on_owner() {
    for poll_first in [false, true] {
        let host = Host::new();
        let calls = Rc::new(Cell::new(0));
        let called = Rc::clone(&calls);
        let factory_drops = Rc::new(Cell::new(0));
        let future_drops = Rc::new(Cell::new(0));
        let factory_capture = interpreter(&factory_drops);
        let future_capture = interpreter(&future_drops);
        let effect = Effect::run(move |_| {
            called.set(called.get() + 1);
            drop(factory_capture);
            std::future::poll_fn(move |_| {
                std::hint::black_box(&future_capture);
                Poll::Pending
            })
        });
        assert_eq!(
            (calls.get(), factory_drops.get(), future_drops.get()),
            (0, 0, 0)
        );
        let store = Store::new(Vec::new(), TimerFeature(Some(effect)), host.runtime());
        let task = host.run(store.send(None)).unwrap();
        assert_eq!(
            (calls.get(), factory_drops.get(), future_drops.get()),
            (0, 0, 0)
        );
        if poll_first {
            host.turn();
            assert_eq!(
                (calls.get(), factory_drops.get(), future_drops.get()),
                (1, 1, 0)
            );
        }
        task.cancel();
        // The actor must process the cancellation request before the host next
        // polls the effect. This host queues newly spawned work before the actor.
        assert_eq!(host.0.tasks.borrow().len(), 2);
        let body = host.0.tasks.borrow_mut().pop_front().unwrap();
        host.turn();
        host.0.tasks.borrow_mut().push_back(body);
        host.run(task.finish());
        assert_eq!(
            (calls.get(), factory_drops.get(), future_drops.get()),
            (usize::from(poll_first), 1, 1)
        );
        host.run(store.shutdown()).unwrap();
        assert!(host.0.tasks.borrow().is_empty());
    }
}

#[test]
fn local_run_factory_panic_is_deferred_and_releases_capture_on_owner() {
    for mapped in [false, true] {
        let host = Host::new();
        let calls = Rc::new(Cell::new(0));
        let called = Rc::clone(&calls);
        let drops = Rc::new(Cell::new(0));
        let capture = interpreter(&drops);
        let effect = Effect::run(move |_| -> std::future::Ready<()> {
            let _capture = capture;
            called.set(called.get() + 1);
            panic!("intentional local run factory panic");
        });
        let effect = if mapped {
            effect.map(std::convert::identity)
        } else {
            effect
        };
        let store = Store::new(Vec::new(), TimerFeature(Some(effect)), host.runtime());
        let task = host.run(store.send(None)).unwrap();
        assert_eq!((calls.get(), drops.get()), (0, 0));
        let mut body = host.0.tasks.borrow_mut().pop_front().unwrap();
        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            body.as_mut().poll(&mut Context::from_waker(Waker::noop()))
        }))
        .expect_err("factory panic must propagate from the first runtime poll");
        assert_eq!(
            panic.downcast_ref::<&str>(),
            Some(&"intentional local run factory panic")
        );
        assert_eq!((calls.get(), drops.get()), (1, 1));
        drop(store);
        host.0.tasks.borrow_mut().clear();
        assert!(task.is_finished());
        assert_eq!(drops.get(), 1);
    }
}

#[test]
fn local_positive_delay_defers_owned_operation_and_cancellation_releases_it() {
    for cancel in [false, true] {
        let host = Host::new();
        let drops = Rc::new(Cell::new(0));
        let capture = interpreter(&drops);
        let weak = Rc::downgrade(&capture);
        let calls = Rc::new(Cell::new(0));
        let operation_calls = Rc::clone(&calls);
        let effect = Effect::run(move |sender| {
            operation_calls.set(operation_calls.get() + 1);
            async move {
                assert_eq!(thread::current().id(), capture.owner);
                sender.send(Some(Rc::new(9))).await.unwrap();
                drop(capture);
            }
        })
        .delay(Duration::from_secs(3));
        let store = Store::new(Vec::new(), TimerFeature(Some(effect)), host.runtime());
        let task = host.run(store.send(None)).unwrap();

        host.advance_to(Duration::from_secs(2));
        assert_eq!((calls.get(), drops.get()), (0, 0));
        assert!(weak.upgrade().is_some());
        assert_eq!(
            host.run(store.read(Clone::clone)).unwrap(),
            [] as [usize; 0]
        );
        assert!(!task.is_finished());
        if cancel {
            task.cancel();
            host.run(task.finish());
            assert_eq!((calls.get(), drops.get()), (0, 1));
            assert!(weak.upgrade().is_none());
            host.advance_to(Duration::from_secs(8));
            assert_eq!(
                host.run(store.read(Clone::clone)).unwrap(),
                [] as [usize; 0]
            );
        } else {
            host.advance_to(Duration::from_secs(3));
            host.run(task.finish());
            assert_eq!((calls.get(), drops.get()), (1, 1));
            assert!(weak.upgrade().is_none());
            assert_eq!(host.run(store.read(Clone::clone)).unwrap(), [9]);
        }
        host.run(store.shutdown()).unwrap();
        assert_eq!(drops.get(), 1);
    }
}

#[test]
fn local_zero_and_empty_delays_finish_in_order_without_clock_advance() {
    let host = Host::new();
    let effect = Effect::concatenate([
        Effect::send(Some(Rc::new(1))).delay(Duration::ZERO),
        Effect::none().delay(Duration::from_secs(60)),
        Effect::send(Some(Rc::new(2))).delay(Duration::ZERO),
    ]);
    let store = Store::new(Vec::new(), TimerFeature(Some(effect)), host.runtime());
    let task = host.run(store.send(None)).unwrap();
    host.run(task.finish());
    assert!(!task.is_cancelled());
    assert_eq!(host.0.now.get(), MonotonicInstant::ZERO);
    assert_eq!(host.run(store.shutdown_into_state()).unwrap(), [1, 2]);
}

#[test]
#[cfg(feature = "test-util")]
fn local_reported_failure_retains_rc_cause_and_caller_when_delivery_is_unavailable() {
    /// Preserves the original non-Send failure payload through issue delivery.
    struct Cause(Rc<Interpreter>);

    for closed in [false, true] {
        let host = Host::new();
        let reporter = if closed {
            let recorder = rca_core::testing::IssueRecorder::new();
            let reporter = recorder.reporter();
            let owner = Store::builder(
                (),
                rca_core::local::Reduce::new(
                    |(): &mut (), (): &(), _: &mut rca_core::ReduceContext<'_>| Effect::none(),
                ),
                host.runtime(),
            )
            .issue_recorder(recorder)
            .build()
            .unwrap();
            host.run(owner.shutdown()).unwrap();
            reporter
        } else {
            rca_core::IssueReporter::default()
        };
        let drops = Rc::new(Cell::new(0));
        let original = interpreter(&drops);
        let identity = Rc::as_ptr(&original);
        let weak = Rc::downgrade(&original);
        let order = Rc::new(RefCell::new(Vec::new()));
        let operations = Rc::clone(&order);
        let factories = Rc::clone(&order);
        let consumers = Rc::clone(&order);
        let outcomes = Rc::new(RefCell::new(Vec::new()));
        let retained = Rc::clone(&outcomes);
        let line = line!() + 1;
        let effect = Effect::try_run_reported(
            move |_| async move {
                operations.borrow_mut().push("operation");
                Err(rca_core::EffectError::Failed(Cause(original)))
            },
            reporter,
            move |cause| {
                assert_eq!(Rc::as_ptr(&cause.0), identity);
                factories.borrow_mut().push("issue");
                rca_core::Issue::new("local.failed", "local operation failed")
                    .with_cause("owned local cause")
            },
            move |report| {
                consumers.borrow_mut().push("outcome");
                retained.borrow_mut().push(report);
            },
        );
        assert!(order.borrow().is_empty());
        let store = Store::new(Vec::new(), TimerFeature(Some(effect)), host.runtime());
        let task = host.run(store.send(None)).unwrap();
        host.run(task.finish());
        assert_eq!(*order.borrow(), ["operation", "issue", "outcome"]);
        let report = outcomes.borrow_mut().pop().unwrap();
        assert!(outcomes.borrow().is_empty());
        let cause = report.result.err().unwrap();
        assert_eq!(Rc::as_ptr(&cause.0), identity);
        let issue = match report.deliveries.as_slice() {
            [rca_core::IssueDelivery::Closed(issue)] if closed => issue,
            [rca_core::IssueDelivery::Disabled(issue)] if !closed => issue,
            _ => panic!("required diagnostic delivery evidence was lost"),
        };
        assert_eq!(issue.code, "local.failed");
        assert_eq!(issue.cause.as_deref(), Some("owned local cause"));
        assert_eq!(issue.location.file(), file!());
        assert_eq!(issue.location.line(), line);
        assert_eq!(drops.get(), 0);
        assert!(weak.upgrade().is_some());
        drop(cause);
        assert_eq!(drops.get(), 1);
        assert!(weak.upgrade().is_none());
        host.run(store.shutdown()).unwrap();
    }
}

#[test]
fn local_reported_success_and_explicit_cancellation_produce_no_issue_or_outcome() {
    for cancelled in [false, true] {
        let host = Host::new();
        let drops = Rc::new(Cell::new(0));
        let capture = interpreter(&drops);
        let calls = Rc::new(Cell::new(0));
        let operations = Rc::clone(&calls);
        let effect = Effect::try_run_reported(
            move |_| async move {
                operations.set(operations.get() + 1);
                assert_eq!(thread::current().id(), capture.owner);
                drop(capture);
                if cancelled {
                    Err(rca_core::EffectError::<Rc<Interpreter>>::Cancelled)
                } else {
                    Ok(())
                }
            },
            rca_core::IssueReporter::new(|_| panic!("no issue may be reported")),
            |_| panic!("no issue may be constructed"),
            |_| panic!("no failure outcome may be delivered"),
        );
        assert_eq!((calls.get(), drops.get()), (0, 0));
        let store = Store::new(Vec::new(), TimerFeature(Some(effect)), host.runtime());
        let task = host.run(store.send(None)).unwrap();
        host.run(task.finish());
        assert_eq!((calls.get(), drops.get()), (1, 1));
        assert_eq!(
            host.run(store.shutdown_into_state()).unwrap(),
            [] as [usize; 0]
        );
    }
}

#[test]
fn local_optional_future_none_finishes_without_emission_and_releases_capture() {
    let host = Host::new();
    let drops = Rc::new(Cell::new(0));
    let capture = interpreter(&drops);
    let weak = Rc::downgrade(&capture);
    let polls = Rc::new(Cell::new(0));
    let observed = Rc::clone(&polls);
    let effect = Effect::future(async move {
        observed.set(observed.get() + 1);
        assert_eq!(thread::current().id(), capture.owner);
        drop(capture);
        None
    });
    let store = Store::new(Vec::new(), TimerFeature(Some(effect)), host.runtime());
    let task = host.run(store.send(None)).unwrap();
    assert_eq!((polls.get(), drops.get()), (0, 0));
    assert!(!task.is_finished());

    host.run(task.finish());
    assert!(task.is_finished() && !task.is_cancelled());
    assert_eq!((polls.get(), drops.get()), (1, 1));
    assert!(weak.upgrade().is_none());
    assert_eq!(
        host.run(store.shutdown_into_state()).unwrap(),
        [] as [usize; 0]
    );
    assert!(host.0.tasks.borrow().is_empty());
}

#[test]
fn local_escaped_effect_sender_rejects_cancellation_and_owner_closure_before_publication() {
    for (cancel, wait_for_capacity) in [(false, false), (true, false), (false, true)] {
        let host = Host::new();
        let drops = Rc::new(Cell::new(0));
        let capture = interpreter(&drops);
        let capture_weak = Rc::downgrade(&capture);
        let exported = Rc::new(RefCell::new(None));
        let saved = Rc::clone(&exported);
        let starts = Rc::new(Cell::new(0));
        let observed_starts = Rc::clone(&starts);
        let effect = Effect::run(move |sender| async move {
            observed_starts.set(observed_starts.get() + 1);
            assert_eq!(thread::current().id(), capture.owner);
            *saved.borrow_mut() = Some(sender);
            std::future::pending::<()>().await;
            drop(capture);
        });
        let reductions = Rc::new(Cell::new(0));
        let observed_reductions = Rc::clone(&reductions);
        let mut effect = Some(effect);
        let reducer = rca_core::local::Reduce::new(
            move |state: &mut Vec<usize>,
                  action: &Option<Rc<usize>>,
                  _: &mut rca_core::ReduceContext<'_>| {
                if let Some(value) = action {
                    observed_reductions.set(observed_reductions.get() + 1);
                    state.push(**value);
                    Effect::none()
                } else {
                    effect.take().unwrap()
                }
            },
        );
        let mut store = Some(
            Store::builder(Vec::new(), reducer, host.runtime())
                .mailbox_capacity(1)
                .build()
                .unwrap(),
        );
        let task = host.run(store.as_ref().unwrap().send(None)).unwrap();
        host.turn();
        let sender = exported.borrow_mut().take().unwrap();
        assert_eq!((starts.get(), drops.get()), (1, 0));
        assert!(!task.is_finished());

        let queued = wait_for_capacity.then(|| {
            let action = Rc::new(3);
            let retained = Rc::downgrade(&action);
            let receipt = store.as_ref().unwrap().try_send(Some(action)).unwrap();
            (receipt, retained)
        });
        let action = Rc::new(7);
        let action_weak = Rc::downgrade(&action);
        let error = {
            let mut send = std::pin::pin!(sender.send(Some(action)));
            let mut cx = Context::from_waker(Waker::noop());
            if wait_for_capacity {
                assert!(send.as_mut().poll(&mut cx).is_pending());
                assert!(action_weak.upgrade().is_some());
                assert_eq!((starts.get(), reductions.get(), drops.get()), (1, 0, 0));
            }
            if cancel {
                task.cancel();
            } else {
                // No actor turn intervenes: even a sender waiting for capacity
                // must not keep the final Store owner alive.
                drop(store.take().unwrap());
            }
            let Poll::Ready(Err(error)) = send.as_mut().poll(&mut cx) else {
                panic!("invalidated effect admission rejects without an actor turn");
            };
            error
        };
        assert_eq!(error.is_cancelled(), cancel);
        assert_eq!(error.is_store_stopped(), !cancel);
        assert!(!error.is_input_closed());
        assert!(action_weak.upgrade().is_none());
        assert_eq!(reductions.get(), 0);

        host.run(task.finish());
        assert!(task.is_finished());
        // Owner shutdown invalidates registry work before the runtime necessarily
        // polls its aborted body. Drive that physical disposal on the owner thread.
        host.turn();
        assert_eq!(drops.get(), 1);
        assert!(capture_weak.upgrade().is_none());
        if let Some((receipt, retained)) = queued {
            host.run(receipt.finish())
                .expect_err("owner closure rejects the queued action without reduction");
            assert!(retained.upgrade().is_none());
        }
        if let Some(store) = store.take() {
            assert_eq!(
                host.run(store.shutdown_into_state()).unwrap(),
                [] as [usize; 0]
            );
        }
        drop(sender);
        assert!(host.0.tasks.borrow().is_empty());
        assert_eq!((starts.get(), reductions.get(), drops.get()), (1, 0, 1));
    }
}

#[test]
fn local_final_owner_close_survives_panicking_closed_waiter_with_retained_ingress() {
    use std::panic::{AssertUnwindSafe, catch_unwind};

    /// Counts the lifecycle wake before injecting the intended callback panic.
    struct ClosedWake(AtomicUsize);
    impl std::task::Wake for ClosedWake {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
            panic!("local closed waiter panic");
        }
    }

    let host = Host::new();
    let drops = Rc::new(Cell::new(0));
    let capture = interpreter(&drops);
    let capture_weak = Rc::downgrade(&capture);
    let exported = Rc::new(RefCell::new(None));
    let saved = Rc::clone(&exported);
    let effect = Effect::run(move |sender| async move {
        assert_eq!(thread::current().id(), capture.owner);
        *saved.borrow_mut() = Some(sender);
        std::future::pending::<()>().await;
        drop(capture);
    });
    let store = Store::builder(Vec::new(), TimerFeature(Some(effect)), host.runtime())
        .mailbox_capacity(1)
        .build()
        .unwrap();
    let task = host.run(store.send(None)).unwrap();
    host.turn();
    let sender = exported.borrow_mut().take().unwrap();
    let queued_action = Rc::new(3);
    let queued_weak = Rc::downgrade(&queued_action);
    let queued = store.try_send(Some(queued_action)).unwrap();
    let action = Rc::new(7);
    let identity = Rc::as_ptr(&action);
    let action_weak = Rc::downgrade(&action);
    let mut send = Box::pin(sender.send(Some(action)));
    let producer = Arc::new(HostWake::default());
    let producer_waker = Waker::from(Arc::clone(&producer));
    assert!(
        send.as_mut()
            .poll(&mut Context::from_waker(&producer_waker))
            .is_pending()
    );
    assert_eq!(Rc::as_ptr(&action_weak.upgrade().unwrap()), identity);
    let lifecycle = Arc::new(ClosedWake(AtomicUsize::new(0)));
    let lifecycle_waker = Waker::from(Arc::clone(&lifecycle));
    let mut closed = Box::pin(store.closed());
    assert!(
        closed
            .as_mut()
            .poll(&mut Context::from_waker(&lifecycle_waker))
            .is_pending()
    );
    let panic = catch_unwind(AssertUnwindSafe(|| drop(store))).unwrap_err();
    assert_eq!(
        panic.downcast_ref::<&str>(),
        Some(&"local closed waiter panic")
    );

    // Capture closure evidence before dropping the send can release retained ingress.
    let notifications = producer.0.load(Ordering::SeqCst);
    let mut cx = Context::from_waker(Waker::noop());
    let rejected_immediately = match send.as_mut().poll(&mut cx) {
        Poll::Ready(Err(error)) => error.is_store_stopped(),
        _ => false,
    };
    let released_immediately = action_weak.upgrade().is_none();

    // Cleanup remains bounded on the failing baseline and checks owner-thread drops.
    drop(send);
    host.run(task.finish());
    host.turn();
    host.run(queued.finish())
        .expect_err("owner closure must reject the queued action");
    assert!(queued_weak.upgrade().is_none());
    assert!(action_weak.upgrade().is_none());
    assert!(capture_weak.upgrade().is_none());
    assert_eq!(drops.get(), 1);
    assert!(closed.as_mut().poll(&mut cx).is_ready());
    assert!(host.0.tasks.borrow().is_empty());
    assert_eq!(lifecycle.0.load(Ordering::SeqCst), 1);
    assert_eq!(
        notifications, 1,
        "lifecycle unwind must wake the local producer"
    );
    assert!(
        rejected_immediately,
        "retained local ingress must already be closed"
    );
    assert!(released_immediately);
}

#[test]
fn local_fallible_effect_handles_only_failure_with_original_non_send_cause() {
    for failed in [false, true] {
        let host = Host::new();
        let drops = Rc::new(Cell::new(0));
        let cause = interpreter(&drops);
        let identity = Rc::as_ptr(&cause);
        let weak = Rc::downgrade(&cause);
        let operations = Rc::new(Cell::new(0));
        let observed_operations = Rc::clone(&operations);
        let handlers = Rc::new(Cell::new(0));
        let observed_handlers = Rc::clone(&handlers);
        let effect = Effect::try_run(
            move |_| async move {
                observed_operations.set(observed_operations.get() + 1);
                assert_eq!(thread::current().id(), cause.owner);
                if failed {
                    Err(rca_core::EffectError::Failed(cause))
                } else {
                    drop(cause);
                    Ok(())
                }
            },
            move |cause, sender| async move {
                assert_eq!(Rc::as_ptr(&cause), identity);
                observed_handlers.set(observed_handlers.get() + 1);
                sender.send(Some(Rc::new(9))).await.unwrap();
                drop(cause);
            },
        );
        let store = Store::new(Vec::new(), TimerFeature(Some(effect)), host.runtime());
        let task = host.run(store.send(None)).unwrap();
        assert_eq!((operations.get(), handlers.get(), drops.get()), (0, 0, 0));
        host.run(task.finish());
        assert!(task.is_finished() && !task.is_cancelled());
        assert_eq!(operations.get(), 1);
        assert_eq!(handlers.get(), usize::from(failed));
        assert_eq!(drops.get(), 1);
        assert!(weak.upgrade().is_none());
        assert_eq!(
            host.run(store.shutdown_into_state()).unwrap(),
            if failed { vec![9] } else { Vec::new() }
        );
        assert!(host.0.tasks.borrow().is_empty());
    }
}

#[test]
fn local_reported_failure_after_causal_cancellation_suppresses_issue_and_outcome() {
    let host = Host::new();
    let drops = Rc::new(Cell::new(0));
    let cause = interpreter(&drops);
    let weak = Rc::downgrade(&cause);
    let causal = Rc::new(RefCell::new(None::<rca_core::ActionTask>));
    let cancellation = Rc::clone(&causal);
    let calls = Rc::new(Cell::new(0));
    let observed = Rc::clone(&calls);
    let effect = Effect::try_run_reported(
        move |_| async move {
            observed.set(observed.get() + 1);
            assert_eq!(thread::current().id(), cause.owner);
            cancellation.borrow().as_ref().unwrap().cancel();
            Err(rca_core::EffectError::Failed(cause))
        },
        rca_core::IssueReporter::new(|_| panic!("cancelled failure cannot be reported")),
        |_| panic!("cancelled failure cannot enter its issue factory"),
        |_| panic!("cancelled failure cannot enter its outcome callback"),
    );
    let store = Store::new(Vec::new(), TimerFeature(Some(effect)), host.runtime());
    let task = host.run(store.send(None)).unwrap();
    assert_eq!((calls.get(), drops.get()), (0, 0));
    *causal.borrow_mut() = Some(task);
    host.turn();
    let task = causal.borrow_mut().take().unwrap();
    host.run(task.finish());
    assert!(task.is_finished() && task.is_cancelled());
    assert_eq!((calls.get(), drops.get()), (1, 1));
    assert!(weak.upgrade().is_none());
    assert_eq!(
        host.run(store.shutdown_into_state()).unwrap(),
        [] as [usize; 0]
    );
    assert!(host.0.tasks.borrow().is_empty());
}

#[test]
fn local_empty_effect_authoring_keeps_borrowed_action_types() {
    const EMPTY: Effect<()> = Effect::none();

    /// Requires the empty default to work without a static action bound.
    fn default_effect<A>() -> Effect<A> {
        Effect::default()
    }

    /// Exercises eager empty-iterator consumption with a borrowed action type.
    fn empty(action: &str) -> Effect<&str> {
        let calls = Cell::new(0);
        let effects = [action, action].into_iter().map(|_| {
            calls.set(calls.get() + 1);
            Effect::none()
        });
        let mut effect = Effect::merge(effects).named("empty", rca_core::OperationPolicy::Coexist);
        assert_eq!(calls.get(), 2);
        let retained = std::mem::take(&mut effect);
        drop(Effect::merge([effect, default_effect()]));
        retained
    }

    drop(EMPTY);
    let action = String::from("borrowed");
    drop(empty(&action));
}

#[test]
#[cfg(feature = "diagnostics")]
#[expect(
    clippy::too_many_lines,
    reason = "one configured actor keeps dependency, issue and owner-local lifecycle observations together"
)]
fn local_feature_configuration_reaches_reducer_effect_and_owner_lifecycle() {
    use rca_core::diagnostics::{CancellationCause, LifecycleEvent, LifecycleKind};
    use rca_core::{DependencyKey, DependencyValues};

    /// Is overridden after installing the complete dependency-value set.
    struct Amount;
    impl DependencyKey for Amount {
        type Value = usize;
    }
    /// Keeps the value supplied by the complete dependency-value set.
    struct Bias;
    impl DependencyKey for Bias {
        type Value = usize;
    }
    /// Links dependency reads, effect delivery and diagnostic ownership in one actor.
    struct ConfiguredFeature {
        cause: Option<Rc<Interpreter>>,
        trace: Rc<RefCell<Vec<&'static str>>>,
    }
    impl Feature for ConfiguredFeature {
        type State = Vec<usize>;
        type Action = Option<Rc<usize>>;

        fn update(
            &mut self,
            state: &mut Self::State,
            action: &Self::Action,
            context: &mut FeatureContext<'_, Self::State, Self::Action>,
        ) {
            if let Some(value) = action {
                self.trace.borrow_mut().push("commit");
                state.push(**value);
                return;
            }
            self.trace.borrow_mut().push("reduce");
            let reducer = context.reducer_context();
            let amount = *reducer.dependency::<Amount>();
            let bias = *reducer.dependency::<Bias>();
            state.push(amount);
            let reporter = reducer.issue_reporter();
            let cause = self.cause.take().expect("one configured operation");
            let identity = Rc::as_ptr(&cause);
            let operation = Rc::clone(&self.trace);
            let outcome = Rc::clone(&self.trace);
            context.effect(Effect::try_run_reported(
                move |sender| async move {
                    operation.borrow_mut().push("operation");
                    assert_eq!(thread::current().id(), cause.owner);
                    assert_eq!((amount, bias), (7, 5));
                    sender.send(Some(Rc::new(amount + bias))).await.unwrap();
                    Err(rca_core::EffectError::Failed(cause))
                },
                reporter,
                |_| {
                    rca_core::Issue::new("configured.failed", "configured Local operation failed")
                        .with_cause("original owner-local cause")
                },
                move |report| {
                    outcome.borrow_mut().push("outcome");
                    let cause = report.result.expect_err("the original operation failed");
                    assert_eq!(Rc::as_ptr(&cause), identity);
                    assert!(matches!(
                        report.deliveries.as_slice(),
                        [rca_core::IssueDelivery::Accepted]
                    ));
                },
            ));
        }
    }

    let host = Host::new();
    let owner = thread::current().id();
    let drops = Rc::new(Cell::new(0));
    let cause = interpreter(&drops);
    let cause_weak = Rc::downgrade(&cause);
    let trace = Rc::new(RefCell::new(Vec::new()));
    let events = Rc::new(RefCell::new(Vec::new()));
    let observed_events = Rc::clone(&events);
    let sink_capture = Rc::new(());
    let sink_weak = Rc::downgrade(&sink_capture);
    let issues = Arc::new(Mutex::new(Vec::new()));
    let observed_issues = Arc::clone(&issues);
    let issue_capture = Arc::new(());
    let issue_weak = Arc::downgrade(&issue_capture);
    let mut values = DependencyValues::new();
    values.set::<Amount>(2);
    values.set::<Bias>(5);
    let store = Store::builder(
        Vec::new(),
        ConfiguredFeature {
            cause: Some(cause),
            trace: Rc::clone(&trace),
        },
        host.runtime(),
    )
    .with_dependency_values(values)
    .with_dependency::<Amount>(7)
    .with_lifecycle_diagnostics(move |event| {
        assert_eq!(Rc::strong_count(&sink_capture), 1);
        assert_eq!(thread::current().id(), owner);
        observed_events.borrow_mut().push(*event);
    })
    .with_issue_handler(move |issue| {
        assert_eq!(Arc::strong_count(&issue_capture), 1);
        assert_eq!(thread::current().id(), owner);
        observed_issues.lock().unwrap().push(issue.clone());
    })
    .build()
    .unwrap();
    assert!(trace.borrow().is_empty());
    assert!(issues.lock().unwrap().is_empty());
    let task = host.run(store.send(None)).unwrap();
    host.run(task.finish());
    assert_eq!(host.run(store.read(Clone::clone)).unwrap(), [7, 12]);
    assert_eq!(
        *trace.borrow(),
        ["reduce", "operation", "commit", "outcome"]
    );
    let reported = issues.lock().unwrap();
    assert_eq!(reported.len(), 1);
    assert_eq!(reported[0].code, "configured.failed");
    assert_eq!(
        reported[0].cause.as_deref(),
        Some("original owner-local cause")
    );
    drop(reported);
    assert_eq!(drops.get(), 1);
    assert!(cause_weak.upgrade().is_none());
    assert!(sink_weak.upgrade().is_some());
    assert!(issue_weak.upgrade().is_some());
    assert_eq!(host.run(store.shutdown_into_state()).unwrap(), [7, 12]);
    assert!(sink_weak.upgrade().is_none());
    assert!(issue_weak.upgrade().is_none());
    let events = events.borrow();
    assert_eq!(
        events.iter().map(LifecycleEvent::kind).collect::<Vec<_>>(),
        [
            LifecycleKind::Installed {
                lifetime: 0,
                parent: None,
                configuration: Some(0),
            },
            LifecycleKind::IngressClosed,
            LifecycleKind::CancellationRequested {
                cause: CancellationCause::ActorStop,
                scope: None,
            },
            LifecycleKind::ActorStopped,
        ]
    );
    assert_eq!(events.last().unwrap().last_committed_version(), 2);
    assert_eq!(drops.get(), 1);
}
