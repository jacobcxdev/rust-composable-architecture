//! Host closure rejects new work and releases actor owners before reporting failure.
//!
//! Reentrant destructors call the real scheduler: they must run outside its lock.
//! Retained driver and task handles must not extend actor-state lifetime after close.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use rca_core::{Effect, ReduceContext, Reducer, Runtime, Store};
use rca_test::DeterministicRuntime;

struct State {
    runtime: Runtime,
    drops: Arc<AtomicUsize>,
}

impl Drop for State {
    fn drop(&mut self) {
        // Re-entering the scheduler proves rejected actors are dropped outside its lock.
        let _ = self.runtime.now();
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}

struct Feature;

impl Reducer for Feature {
    type State = State;
    type Action = ();

    fn reduce(
        &mut self,
        _state: &mut State,
        _action: &(),
        _context: &mut ReduceContext<'_>,
    ) -> Effect<()> {
        Effect::none()
    }
}

#[test]
fn retained_runtime_closes_store_created_after_host_teardown() {
    let host = DeterministicRuntime::new();
    let runtime = host.runtime();
    drop(host);
    let drops = Arc::new(AtomicUsize::new(0));

    let store = Store::new(
        State {
            runtime: runtime.clone(),
            drops: Arc::clone(&drops),
        },
        Feature,
        runtime,
    );

    assert!(store.is_closed());
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    assert!(!store.try_send(()).expect_err("closed actor").is_full());
}

struct SpawnOnDrop {
    runtime: Runtime,
    drops: Arc<AtomicUsize>,
    store: Arc<Mutex<Option<Store<State, ()>>>>,
}

impl Drop for SpawnOnDrop {
    fn drop(&mut self) {
        let store = Store::new(
            State {
                runtime: self.runtime.clone(),
                drops: Arc::clone(&self.drops),
            },
            Feature,
            self.runtime.clone(),
        );
        *self.store.lock().expect("result lock") = Some(store);
    }
}

#[test]
fn teardown_rejects_reentrant_actor_spawn_from_task_destructor() {
    let host = DeterministicRuntime::new();
    let drops = Arc::new(AtomicUsize::new(0));
    let store = Arc::new(Mutex::new(None));
    let probe = SpawnOnDrop {
        runtime: host.runtime(),
        drops: Arc::clone(&drops),
        store: Arc::clone(&store),
    };
    host.spawn_task(Box::pin(async move {
        let _probe = probe;
        std::future::pending::<()>().await;
    }));
    host.run_until_stalled().expect("pending task stalls");

    drop(host);

    let store = store
        .lock()
        .expect("result lock")
        .take()
        .expect("spawn ran");
    assert!(store.is_closed());
    assert_eq!(drops.load(Ordering::SeqCst), 1);
}

#[test]
fn host_closed_by_first_task_drops_unpolled_batch_siblings_outside_lock() {
    let host = DeterministicRuntime::new();
    let retained_runtime = host.runtime();
    let drain = host.drain_deadlines(0);
    let last_host = host.clone();
    host.spawn_task(Box::pin(async move {
        drop(last_host);
        std::future::pending::<()>().await;
    }));

    let polls = Arc::new(AtomicUsize::new(0));
    let drops = Arc::new(AtomicUsize::new(0));
    let probe = State {
        runtime: retained_runtime.clone(),
        drops: Arc::clone(&drops),
    };
    let sibling_polls = Arc::clone(&polls);
    host.spawn_task(Box::pin(async move {
        // State's destructor re-enters the scheduler, even when never polled.
        let _probe = probe;
        sibling_polls.fetch_add(1, Ordering::SeqCst);
        std::future::pending::<()>().await;
    }));
    drop(host);
    assert_eq!(drops.load(Ordering::SeqCst), 0);

    let error = futures_executor::block_on(drain).expect_err("last host closes during drain");
    assert!(error.is_host_closed());
    assert_eq!(error.polls(), 1);
    assert!(error.live_task_ids().is_empty());
    assert!(error.ready_task_ids().is_empty());
    assert!(error.pending_sleeps().is_empty());
    assert_eq!(polls.load(Ordering::SeqCst), 0);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    drop(retained_runtime);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
}

/// Abort wake failures must still release actor state through retained task handles.
mod test_store_abort {
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    };
    use std::task::{Context, Wake, Waker};

    use futures_executor::block_on;
    use rca_core::{Feature, FeatureContext, Task, TaskContext};
    use rca_test::TestStore;

    struct CountedState(Arc<AtomicUsize>);

    impl Drop for CountedState {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    struct ExportContext(Arc<OnceLock<TaskContext<CountedState, bool>>>);

    impl Feature for ExportContext {
        type State = CountedState;
        type Action = bool;

        fn reduce(
            &mut self,
            _: &mut CountedState,
            action: &bool,
            context: &mut FeatureContext<'_, CountedState, bool>,
        ) {
            if *action {
                let exported = Arc::clone(&self.0);
                context.task(Task::run(move |context| async move {
                    exported.set(context).unwrap();
                    std::future::pending::<()>().await;
                }));
            }
        }
    }

    struct PanicWake(AtomicBool);

    impl Wake for PanicWake {
        fn wake(self: Arc<Self>) {
            self.wake_by_ref();
        }

        fn wake_by_ref(self: &Arc<Self>) {
            assert!(!self.0.swap(false, Ordering::SeqCst), "blocked send wake");
        }
    }

    #[test]
    fn abort_wake_panic_still_discards_scheduler_owned_actor() {
        let drops = Arc::new(AtomicUsize::new(0));
        let exported = Arc::new(OnceLock::new());
        let mut store = TestStore::projected_feature_builder(
            CountedState(Arc::clone(&drops)),
            ExportContext(Arc::clone(&exported)),
            |_| 0_u8,
        )
        .mailbox_capacity(1)
        .build();
        let retained_runtime = block_on(store.send(true)).unwrap();
        let context = exported.get().unwrap().clone();

        // Existing debt makes Drop inspect no further ready work. The mailbox
        // therefore remains full until abort closes it and notifies its sender.
        let mismatch = catch_unwind(AssertUnwindSafe(|| {
            block_on(store.send_with(false, |expected| *expected = 1))
        }))
        .expect_err("the incorrect snapshot must leave assertion debt");
        assert!(
            mismatch
                .downcast_ref::<String>()
                .unwrap()
                .contains("TestStore state mismatch")
        );
        let mut first = Box::pin(context.send(false));
        assert!(
            first
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending()
        );
        let mut blocked = Box::pin(context.send(false));
        let waker = Waker::from(Arc::new(PanicWake(AtomicBool::new(true))));
        assert!(
            blocked
                .as_mut()
                .poll(&mut Context::from_waker(&waker))
                .is_pending()
        );

        let panic = catch_unwind(AssertUnwindSafe(|| drop(store)))
            .expect_err("abort must propagate the sender's original wake panic");
        let released_before_last_runtime = drops.load(Ordering::SeqCst);
        drop(first);
        drop(blocked);
        drop(context);
        drop(retained_runtime);

        assert_eq!(panic.downcast_ref::<&str>(), Some(&"blocked send wake"));
        assert_eq!(released_before_last_runtime, 1);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }
}
