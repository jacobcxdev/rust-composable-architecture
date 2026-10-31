//! Checks assertion obligations, effect-action ordering and cleanup during unwinding.

#[path = "support/child_process.rs"]
mod child_process;

use std::any::Any;
use std::panic::{AssertUnwindSafe, catch_unwind};

use futures_executor::block_on;
use rca_core::{Effect, ReduceContext, Reducer};
use rca_test::TestStore;

/// Exercises independent fields and effect actions that must be received in order.
enum Action {
    Increment,
    Both,
    Start,
    StartBoth,
    Chain,
}

/// Provides state changes and chained effects for full and projected stores.
struct Feature;

/// Borrows either standard string panic payload without allocating another message.
fn panic_message(payload: &(dyn Any + Send)) -> &str {
    payload
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| payload.downcast_ref::<&str>().copied())
        .expect("expected a string panic payload")
}

#[test]
fn normal_drop_rejects_pending_action() {
    let mut store = TestStore::new((0, 0), Feature);
    let _task = block_on(store.send(Action::Start)).unwrap();
    let panic = catch_unwind(AssertUnwindSafe(|| drop(store))).unwrap_err();
    assert!(panic_message(panic.as_ref()).starts_with(
        "TestStore teardown failed: TestStore finished with RCA-owned work pending at "
    ));
}

/// Leaves either a live timer or an external future for teardown to cancel.
struct PendingWork;

impl Reducer for PendingWork {
    type State = ();
    type Action = bool;
    fn reduce(&mut self, (): &mut (), action: &bool, _: &mut ReduceContext<'_>) -> Effect<bool> {
        if *action {
            Effect::interval(std::time::Duration::from_secs(1), || true)
        } else {
            Effect::run(|_| std::future::pending())
        }
    }
}

#[test]
fn normal_drop_rejects_interval_and_external_pending_work() {
    for timer in [false, true] {
        let mut store = TestStore::new((), PendingWork);
        let task = block_on(store.send(timer)).unwrap();
        let panic = catch_unwind(AssertUnwindSafe(|| drop(store))).unwrap_err();
        assert!(panic_message(panic.as_ref()).starts_with(
            "TestStore teardown failed: TestStore finished with RCA-owned work pending at "
        ));
        assert!(task.is_finished());
        assert!(task.is_cancelled());
    }
}

#[test]
fn failed_finish_does_not_discharge_drop_obligation() {
    let mut store = TestStore::new((), PendingWork);
    let _task = block_on(store.send(false)).unwrap();
    assert!(
        block_on(store.finish())
            .unwrap_err()
            .finish_diagnostics()
            .is_some()
    );
    let panic = catch_unwind(AssertUnwindSafe(|| drop(store))).unwrap_err();
    assert!(panic_message(panic.as_ref()).starts_with(
        "TestStore teardown failed: TestStore finished with RCA-owned work pending at "
    ));
}

#[test]
fn idle_and_explicitly_finished_stores_drop_cleanly() {
    drop(TestStore::new((0, 0), Feature));
    let mut store = TestStore::new((0, 0), Feature);
    block_on(store.finish()).unwrap();
    drop(store);
}

/// Runs in a separate process so a second panic cannot abort the parent test host.
fn panic_teardown_child(mode: &str) -> ! {
    use std::panic::resume_unwind;
    use std::sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    };
    /// Counts the owned effect resource released while the original failure unwinds.
    struct Probe(Arc<AtomicUsize>);
    impl Drop for Probe {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
    /// Starts pending owned work before triggering a projection or assertion failure.
    struct ResourceFeature(Arc<AtomicUsize>);
    impl Reducer for ResourceFeature {
        type State = u32;
        type Action = bool;
        fn reduce(
            &mut self,
            state: &mut u32,
            action: &bool,
            _: &mut ReduceContext<'_>,
        ) -> Effect<bool> {
            if *action {
                *state = 1;
                Effect::none()
            } else {
                let probe = Probe(Arc::clone(&self.0));
                Effect::run(move |_| async move {
                    let _probe = probe;
                    std::future::pending::<()>().await;
                })
            }
        }
    }
    let drops = Arc::new(AtomicUsize::new(0));
    let panic = catch_unwind(AssertUnwindSafe(|| {
        let fail = Arc::new(AtomicBool::new(false));
        let capture = Arc::clone(&fail);
        let mut store =
            TestStore::projected(0, ResourceFeature(Arc::clone(&drops)), move |state| {
                assert!(
                    !capture.load(Ordering::SeqCst),
                    "original projection failure"
                );
                *state
            });
        let _task = block_on(store.send(false)).unwrap();
        fail.store(mode == "projection", Ordering::SeqCst);
        let _task = block_on(store.send(true)).unwrap();
    }))
    .unwrap_err();
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    eprintln!("effect resource released exactly once");
    resume_unwind(panic);
}

#[test]
fn panic_teardown_subprocess_preserves_failure_and_releases_effect_once() {
    const CHILD: &str = "RCA_EXHAUSTIVITY_PANIC_CHILD";
    if let Ok(mode) = std::env::var(CHILD) {
        panic_teardown_child(&mode);
    }
    for mode in ["projection", "assertion"] {
        use std::process::Command;
        use std::time::Duration;
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "panic_teardown_subprocess_preserves_failure_and_releases_effect_once",
                "--nocapture",
            ])
            .env(CHILD, mode);
        // This bounds a broken child process; it is not a performance measurement.
        let output = child_process::capture(&mut command, Duration::from_secs(5))
            .unwrap_or_else(|error| panic!("teardown child capture failed ({mode}): {error}"));
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(101), "{stderr}");
        assert!(
            stderr.contains("effect resource released exactly once"),
            "{stderr}"
        );
        assert!(
            stderr.contains(if mode == "projection" {
                "original projection failure"
            } else {
                "state mismatch"
            }),
            "{stderr}"
        );
        assert!(!stderr.contains("TestStore teardown failed"), "{stderr}");
    }
}

#[test]
fn unpolled_send_has_no_obligation() {
    let mut store = TestStore::new((0, 0), Feature);
    drop(store.send_with(Action::Increment, |_| panic!("unpolled update ran")));
    block_on(store.assert_state_eq((0, 0))).unwrap();
    block_on(store.finish()).unwrap();
}

#[test]
fn non_exhaustive_skip_cancels_action_and_compares_unchanged_snapshot() {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    let captures = Arc::new(AtomicUsize::new(0));
    let count = Arc::clone(&captures);
    let mut store = TestStore::projected_builder((0, 0), Feature, move |state| {
        count.fetch_add(1, Ordering::SeqCst);
        *state
    })
    .with_exhaustivity(rca_test::Exhaustivity::NonExhaustive)
    .build();
    let _task = block_on(store.send(Action::Start)).unwrap();
    assert_eq!(captures.load(Ordering::SeqCst), 2);
    block_on(store.skip_received()).unwrap();
    assert_eq!(
        captures.load(Ordering::SeqCst),
        3,
        "skip must compare its unchanged snapshot"
    );
    block_on(store.assert_state_eq((0, 0))).unwrap();
    block_on(store.finish()).unwrap();
}

#[test]
fn non_exhaustive_skip_does_not_hide_unrelated_live_work() {
    /// Pairs a skipped action with an independent effect that remains live.
    struct Pending;
    impl Reducer for Pending {
        type State = ();
        type Action = bool;
        fn reduce(
            &mut self,
            (): &mut (),
            action: &bool,
            _: &mut ReduceContext<'_>,
        ) -> Effect<bool> {
            assert!(*action, "skipped action must not reduce");
            Effect::merge([Effect::send(false), Effect::run(|_| std::future::pending())])
        }
    }
    let mut store =
        TestStore::with_exhaustivity((), Pending, rca_test::Exhaustivity::NonExhaustive);
    let _task = block_on(store.send(true)).unwrap();
    block_on(store.skip_received()).unwrap();
    let error = block_on(store.finish()).unwrap_err();
    assert_ne!(
        error.finish_diagnostics().unwrap().runtime().live_task_ids,
        [] as [u64; 0]
    );
    assert!(!error.is_incomplete_assertion());
    let panic = catch_unwind(AssertUnwindSafe(|| drop(store))).unwrap_err();
    assert!(panic_message(panic.as_ref()).starts_with(
        "TestStore teardown failed: TestStore finished with RCA-owned work pending at "
    ));
}

#[test]
#[should_panic(expected = "state mismatch")]
fn skipped_action_cancellation_still_checks_state() {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    /// Changes the live source only when a queued effect action is released.
    struct DropAction(Option<Arc<AtomicUsize>>);
    impl Drop for DropAction {
        fn drop(&mut self) {
            if let Some(value) = &self.0 {
                value.store(1, Ordering::SeqCst);
            }
        }
    }
    /// Sends the drop-sensitive action without modifying the source during reduction.
    struct OnDrop;
    impl Reducer for OnDrop {
        type State = Arc<AtomicUsize>;
        type Action = DropAction;
        fn reduce(
            &mut self,
            state: &mut Self::State,
            action: &DropAction,
            _: &mut ReduceContext<'_>,
        ) -> Effect<DropAction> {
            assert!(action.0.is_none(), "skipped action must not reduce");
            Effect::send(DropAction(Some(Arc::clone(state))))
        }
    }
    // The projection is an independent integer; cancellation changes the live source only.
    let mut store = TestStore::projected_builder(Arc::new(AtomicUsize::new(0)), OnDrop, |state| {
        state.load(Ordering::SeqCst)
    })
    .with_exhaustivity(rca_test::Exhaustivity::NonExhaustive)
    .build();
    let _task = block_on(store.send(DropAction(None))).unwrap();
    block_on(store.skip_received()).unwrap();
}

#[test]
fn drive_failure_retains_admitted_assertion() {
    /// Exhausts the poll budget without completing its admitted action's effect.
    struct SelfWake;
    impl Reducer for SelfWake {
        type State = ();
        type Action = ();
        fn reduce(&mut self, (): &mut (), (): &(), _: &mut ReduceContext<'_>) -> Effect<()> {
            Effect::run(|_| {
                std::future::poll_fn(|cx| {
                    cx.waker().wake_by_ref();
                    std::task::Poll::Pending
                })
            })
        }
    }
    let mut store = TestStore::with_step_budget((), SelfWake, 7);
    let origin_line = line!() + 1;
    let error = block_on(store.send(())).unwrap_err();
    assert!(error.drive_error().is_some());
    assert!(!error.is_incomplete_assertion());
    let incomplete = block_on(store.finish()).unwrap_err();
    assert!(incomplete.is_incomplete_assertion());
    assert_eq!(incomplete.origin().line(), origin_line);
    assert!(format!("{incomplete:?}").contains("IncompleteAssertion"));
    assert!(incomplete.to_string().contains("send"));
    assert!(std::error::Error::source(&incomplete).is_none());
    let panic = catch_unwind(AssertUnwindSafe(|| drop(store))).unwrap_err();
    assert!(
        panic_message(panic.as_ref())
            .starts_with("TestStore teardown failed: TestStore incomplete assertion for send at ")
    );
}

impl Reducer for Feature {
    type State = (u32, u32);
    type Action = Action;

    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &Action,
        _: &mut ReduceContext<'_>,
    ) -> Effect<Action> {
        match action {
            Action::Increment => state.0 += 1,
            Action::Both => {
                state.0 += 1;
                state.1 += 1;
            }
            Action::Start => return Effect::send(Action::Increment),
            Action::StartBoth => return Effect::send(Action::Both),
            Action::Chain => {
                state.0 += 1;
                return Effect::send(Action::Increment);
            }
        }
        Effect::none()
    }
}

#[test]
#[should_panic(expected = "state mismatch")]
fn send_without_update_rejects_changed_state() {
    block_on(async {
        let mut store = TestStore::new((0, 0), Feature);
        let _task = store.send(Action::Increment).await.unwrap();
    });
}

#[test]
#[should_panic(expected = "state mismatch")]
fn receive_without_update_rejects_changed_state() {
    block_on(async {
        let mut store = TestStore::new((0, 0), Feature);
        let _task = store.send(Action::Start).await.unwrap();
        let _task = store
            .receive(|a| matches!(a, Action::Increment))
            .await
            .unwrap();
    });
}

#[test]
#[should_panic(expected = "state mismatch")]
fn send_update_does_not_hide_second_field() {
    block_on(async {
        let mut store = TestStore::new((0, 0), Feature);
        let _task = store.send_with(Action::Both, |s| s.0 = 1).await.unwrap();
    });
}

#[test]
#[should_panic(expected = "state mismatch")]
fn receive_update_does_not_hide_second_field() {
    block_on(async {
        let mut store = TestStore::new((0, 0), Feature);
        let _task = store.send(Action::StartBoth).await.unwrap();
        let _task = store
            .receive_with(|a| matches!(a, Action::Both), |s| s.0 = 1)
            .await
            .unwrap();
    });
}

#[test]
fn matching_sends_advance_verified_baseline() {
    block_on(async {
        let mut store = TestStore::new((0, 0), Feature);
        let _task = store
            .send_with(Action::Increment, |s| s.0 += 1)
            .await
            .unwrap();
        store.assert_state_eq((1, 0)).await.unwrap();
        let _task = store
            .send_with(Action::Increment, |s| s.0 += 1)
            .await
            .unwrap();
        store.assert_state_eq((2, 0)).await.unwrap();
        store.finish().await.unwrap();
    });
}

#[test]
fn matching_receives_advance_verified_baseline() {
    block_on(async {
        let mut store = TestStore::new((0, 0), Feature);
        for _ in 0..2 {
            let _task = store.send(Action::Start).await.unwrap();
            let _task = store
                .receive_with(|a| matches!(a, Action::Increment), |s| s.0 += 1)
                .await
                .unwrap();
        }
        store.assert_state_eq((2, 0)).await.unwrap();
        store.finish().await.unwrap();
    });
}

#[test]
fn gate_preserves_intermediate_state_and_rejected_matcher() {
    /// Produces two distinct intermediate values, not just one final sum.
    struct Chain;
    impl Reducer for Chain {
        type State = (u32, u32);
        type Action = Action;
        fn reduce(
            &mut self,
            state: &mut Self::State,
            action: &Action,
            context: &mut ReduceContext<'_>,
        ) -> Effect<Action> {
            if matches!(action, Action::Start) {
                Effect::send(Action::Chain)
            } else {
                Feature.reduce(state, action, context)
            }
        }
    }
    block_on(async {
        let mut store = TestStore::new((0, 0), Chain);
        let _task = store.send(Action::Start).await.unwrap();
        let error = store
            .receive_with(|_| false, |_| panic!("rejected update ran"))
            .await
            .unwrap_err();
        assert!(error.is_action_mismatch());
        let _task = store
            .receive_with(|a| matches!(a, Action::Chain), |s| s.0 = 1)
            .await
            .unwrap();
        store.assert_state_eq((1, 0)).await.unwrap();
        assert!(
            store
                .send(Action::Increment)
                .await
                .unwrap_err()
                .is_unreceived_action()
        );
        let _task = store
            .receive_with(|a| matches!(a, Action::Increment), |s| s.0 += 1)
            .await
            .unwrap();
        store.assert_state_eq((2, 0)).await.unwrap();
        store.finish().await.unwrap();
    });
}

#[test]
fn move_only_values_and_non_sync_snapshot_keep_futures_send() {
    use std::cell::Cell;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    /// Uses `Cell` to require `Send` without providing `Sync` or `Clone`.
    struct State {
        count: Cell<u32>,
    }
    /// Omits `Clone` and `Debug` so the test proves neither bound is required.
    enum MoveAction {
        Increment,
        Start,
    }
    /// Gives the reducer the same `Send`-only ownership boundary as its state.
    struct MoveFeature(Cell<u32>);
    impl Reducer for MoveFeature {
        type State = State;
        type Action = MoveAction;
        fn reduce(
            &mut self,
            state: &mut State,
            action: &MoveAction,
            _: &mut ReduceContext<'_>,
        ) -> Effect<MoveAction> {
            self.0.set(self.0.get() + 1);
            match action {
                MoveAction::Increment => {
                    state.count.set(state.count.get() + 1);
                    Effect::none()
                }
                MoveAction::Start => Effect::send(MoveAction::Increment),
            }
        }
    }
    /// Consumes unpolled futures to check their transfer bound without executing them.
    fn assert_send<T: Send>(_: T) {}
    /// Checks the builder's transfer bound without consuming its later runtime witness.
    fn assert_builder_send<T: Send>(_: &T) {}
    let projections = Arc::new(AtomicUsize::new(0));
    let captures = Arc::clone(&projections);
    let builder = TestStore::projected_builder(
        State {
            count: Cell::new(0),
        },
        MoveFeature(Cell::new(0)),
        move |s| {
            captures.fetch_add(1, Ordering::SeqCst);
            Cell::new(s.count.get())
        },
    )
    .with_exhaustivity(rca_test::Exhaustivity::NonExhaustive)
    .with_step_budget(1000);
    assert_eq!(projections.load(Ordering::SeqCst), 1);
    assert_builder_send(&builder);
    let mut store = builder.build();
    assert_eq!(projections.load(Ordering::SeqCst), 1);
    assert_send(store.send(MoveAction::Increment));
    assert_send(store.send_with(MoveAction::Increment, |_| {}));
    assert_send(store.receive(|_| true));
    assert_send(store.receive_with(|_| true, |_| {}));
    assert_send(store.assert_state(|_| {}));
    assert_send(store.skip_received());
    assert_send(store.finish());
    let updates = Arc::new(AtomicUsize::new(0));
    let called = Arc::clone(&updates);
    block_on(async {
        let _task = store
            .send_with(MoveAction::Increment, move |s| {
                called.fetch_add(1, Ordering::SeqCst);
                assert_eq!(s.get(), 0);
                s.set(1);
            })
            .await
            .unwrap();
        store
            .assert_state(|s| assert_eq!(s.count.get(), 1))
            .await
            .unwrap();
        let _task = store.send(MoveAction::Start).await.unwrap();
        let received = Arc::clone(&updates);
        let _task = store
            .receive_with(
                |a| matches!(a, MoveAction::Increment),
                move |s| {
                    received.fetch_add(1, Ordering::SeqCst);
                    assert_eq!(s.get(), 1);
                    s.set(2);
                },
            )
            .await
            .unwrap();
        store
            .assert_state(|s| assert_eq!(s.count.get(), 2))
            .await
            .unwrap();
        store.finish().await.unwrap();
    });
    assert_eq!(updates.load(Ordering::SeqCst), 2);
    assert_eq!(projections.load(Ordering::SeqCst), 4);
}

#[test]
fn send_builder_supports_owned_transfer_across_await() {
    /// Preserves the value after checking that the owned transfer is allowed.
    fn assert_send<T: Send>(value: T) -> T {
        value
    }
    let builder = TestStore::builder((0, 0), Feature);
    // Deferred mount configuration can own Send-only captures. The consumed
    // builder is Send, not Sync; completed Store/TestStore retain Sync.
    let transferred = assert_send(async move {
        std::future::ready(()).await;
        builder
    });
    let builder = block_on(transferred);
    let projected =
        TestStore::projected_builder((0, 0), Feature, |state| std::cell::Cell::new(state.0));
    let projected = assert_send(projected);
    block_on(builder.build().finish()).unwrap();
    block_on(projected.build().finish()).unwrap();
}

#[test]
#[should_panic(expected = "positive")]
fn builder_rejects_zero_budget() {
    let _builder = TestStore::builder((0, 0), Feature).with_step_budget(0);
}

#[test]
fn user_snapshot_panics_propagate_without_poisoning_comparison_storage() {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    /// Selects a user callback panic independently of the store's own comparison.
    #[derive(Clone)]
    struct Snapshot {
        value: u32,
        panic_eq: Arc<AtomicBool>,
        panic_debug: Arc<AtomicBool>,
    }
    impl PartialEq for Snapshot {
        fn eq(&self, other: &Self) -> bool {
            assert!(!self.panic_eq.load(Ordering::SeqCst), "equality panic");
            self.value == other.value
        }
    }
    impl std::fmt::Debug for Snapshot {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            assert!(!self.panic_debug.load(Ordering::SeqCst), "debug panic");
            self.value.fmt(f)
        }
    }
    for mode in ["projection", "equality", "debug"] {
        let fail = Arc::new(AtomicBool::new(false));
        let panic_eq = Arc::new(AtomicBool::new(false));
        let panic_debug = Arc::new(AtomicBool::new(false));
        let projection = Arc::clone(&fail);
        let eq = Arc::clone(&panic_eq);
        let debug = Arc::clone(&panic_debug);
        let mut store = TestStore::projected((0, 0), Feature, move |s| {
            assert!(!projection.load(Ordering::SeqCst), "projection panic");
            Snapshot {
                value: s.0,
                panic_eq: Arc::clone(&eq),
                panic_debug: Arc::clone(&debug),
            }
        });
        match mode {
            "projection" => fail.store(true, Ordering::SeqCst),
            "equality" => panic_eq.store(true, Ordering::SeqCst),
            _ => panic_debug.store(true, Ordering::SeqCst),
        }
        let panic =
            catch_unwind(AssertUnwindSafe(|| block_on(store.send(Action::Increment)))).unwrap_err();
        assert_eq!(panic_message(panic.as_ref()), format!("{mode} panic"));
        assert!(
            block_on(store.finish())
                .unwrap_err()
                .is_incomplete_assertion()
        );
        // A caught assertion leaves a durable obligation on normal scope exit.
        let panic = catch_unwind(AssertUnwindSafe(|| drop(store))).unwrap_err();
        assert!(
            panic_message(panic.as_ref()).starts_with(
                "TestStore teardown failed: TestStore incomplete assertion for send at "
            )
        );
    }
}

#[test]
fn builders_preserve_dependency_overlays() {
    use rca_core::DependencyKey;
    /// Has a live value distinct from every explicitly configured test value.
    struct Key;
    impl DependencyKey for Key {
        type Value = u32;
        fn live_value() -> u32 {
            99
        }
    }
    /// Exposes the effective dependency as state, independently of the builder API.
    struct Read;
    impl Reducer for Read {
        type State = u32;
        type Action = ();
        fn reduce(
            &mut self,
            state: &mut u32,
            (): &(),
            context: &mut ReduceContext<'_>,
        ) -> Effect<()> {
            *state = *context.dependency::<Key>();
            Effect::none()
        }
    }
    block_on(async {
        /// Keeps the configured builder available after its compile-time bound check.
        fn assert_send<T: Send>(_: &T) {}
        let full = TestStore::builder(0, Read)
            .with_dependency::<Key>(5)
            .with_dependency::<Key>(7)
            .with_store_dependency::<Key>(3)
            .with_exhaustivity(rca_test::Exhaustivity::NonExhaustive)
            .with_step_budget(1000);
        assert_send(&full);
        let mut full = full.build();
        let _task = full.send_with((), |s| *s = 7).await.unwrap();
        full.finish().await.unwrap();
        let mut projected = TestStore::projected_builder(0, Read, |s| *s)
            .with_store_dependency::<Key>(3)
            .with_dependency::<Key>(7)
            .build();
        let _task = projected.send_with((), |s| *s = 7).await.unwrap();
        projected.finish().await.unwrap();
        let mut root = TestStore::builder(0, Read)
            .with_store_dependency::<Key>(1)
            .with_store_dependency::<Key>(3)
            .build();
        let _task = root.send_with((), |s| *s = 3).await.unwrap();
        root.finish().await.unwrap();
    });
}

#[test]
fn mismatch_diagnostics_identify_coverage_origin_values_and_runtime() {
    for projected in [false, true] {
        let mut store = if projected {
            TestStore::projected((0, 0), Feature, |s| *s)
        } else {
            TestStore::new((0, 0), Feature)
        };
        let origin_line = line!() + 1;
        let future = store.send(Action::Increment);
        let panic = catch_unwind(AssertUnwindSafe(|| block_on(future))).unwrap_err();
        let message = panic.downcast_ref::<String>().unwrap();
        assert!(message.contains(&format!("{}:{origin_line}:", file!())));
        assert!(message.contains(if projected {
            "projected coverage"
        } else {
            "full-state coverage"
        }));
        assert!(message.contains("expected:\n  1 | (\n- 2 |     0,\n  3 |     0,\n  4 | )"));
        assert!(message.contains("actual:\n  1 | (\n+ 2 |     1,\n  3 |     0,\n  4 | )"));
        assert!(message.contains("runtime: Some("));
        let panic = catch_unwind(AssertUnwindSafe(|| drop(store))).unwrap_err();
        assert!(
            panic_message(panic.as_ref()).starts_with(
                "TestStore teardown failed: TestStore incomplete assertion for send at "
            )
        );
    }
}

#[test]
fn receive_matcher_panic_preserves_pending_action_and_cleanup() {
    let mut store = TestStore::new((0, 0), Feature);
    let _task = block_on(store.send(Action::Start)).unwrap();
    let panic = catch_unwind(AssertUnwindSafe(|| {
        block_on(store.receive_with(|_| panic!("matcher panic"), |_| panic!("update ran")))
    }))
    .unwrap_err();
    assert_eq!(panic.downcast_ref::<&str>(), Some(&"matcher panic"));
    block_on(async {
        let _task = store
            .receive_with(|a| matches!(a, Action::Increment), |s| s.0 = 1)
            .await
            .unwrap();
        store.assert_state_eq((1, 0)).await.unwrap();
        store.finish().await.unwrap();
    });
}
