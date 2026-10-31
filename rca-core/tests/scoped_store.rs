//! Checks scoped authority, callback ownership and observation ordering through public APIs.

#![expect(
    clippy::duplicate_mod,
    reason = "independently reusable test hosts each own the same private clock module"
)]

#[cfg(not(miri))]
#[path = "../src/allocation_test/child.rs"]
mod subprocess;
mod support;
#[path = "support/thread_runtime.rs"]
mod thread_runtime;

use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::task::{Context, Poll};
use std::time::Duration;

use rca_core::{
    ActionPath, CaseStatePath, Effect, OptionalStatePath, ReduceContext, Reducer, ScopedStore,
    StatePath, Store,
};

use support::ManualRuntime;

/// Separates required and optional children from parent-only commits.
#[derive(Default)]
struct AppState {
    child: ChildState,
    optional_child: Option<ChildState>,
    optional_reductions: usize,
}

/// Records identity, visible state and the exact sequence of committed inputs.
#[derive(Default)]
struct ChildState {
    id: usize,
    count: usize,
    commits: Vec<usize>,
}

/// Routes child work and explicit parent changes through one reducer.
enum AppAction {
    Child(ChildAction),
    OptionalChild(ChildAction),
    ChangeParentOnly,
    ChangeChildFromParent,
    ReplaceRequiredChild {
        id: usize,
        count: usize,
    },
    RemoveChild,
    InsertChild,
    #[cfg(feature = "test-util")]
    StartRemoval,
}

/// Distinguishes state changes from ordered admission records.
enum ChildAction {
    Increment,
    Add(usize),
    Record(usize),
}

/// Mutates only the addressed child; optional input requires a live child.
struct AppReducer;

impl Reducer for AppReducer {
    type State = AppState;
    type Action = AppAction;

    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &Self::Action,
        _context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        match action {
            AppAction::Child(ChildAction::Increment) | AppAction::ChangeChildFromParent => {
                state.child.count += 1;
            }
            AppAction::Child(ChildAction::Add(amount)) => state.child.count += amount,
            AppAction::Child(ChildAction::Record(id)) => state.child.commits.push(*id),
            AppAction::OptionalChild(ChildAction::Increment) => {
                state.optional_reductions += 1;
                state
                    .optional_child
                    .as_mut()
                    .expect("direct optional action requires child")
                    .count += 1;
            }
            AppAction::OptionalChild(ChildAction::Add(amount)) => {
                state.optional_reductions += 1;
                state
                    .optional_child
                    .as_mut()
                    .expect("direct optional action requires child")
                    .count += amount;
            }
            AppAction::OptionalChild(ChildAction::Record(id)) => state
                .optional_child
                .as_mut()
                .expect("direct optional action requires child")
                .commits
                .push(*id),
            AppAction::ReplaceRequiredChild { id, count } => {
                state.child = ChildState {
                    id: *id,
                    count: *count,
                    ..ChildState::default()
                };
            }
            AppAction::ChangeParentOnly => {}
            AppAction::RemoveChild => state.optional_child = None,
            AppAction::InsertChild => state.optional_child = Some(ChildState::default()),
            #[cfg(feature = "test-util")]
            AppAction::StartRemoval => return Effect::send(AppAction::RemoveChild),
        }
        Effect::none()
    }
}

/// Selects the required child without retaining a child value.
fn child_state_path() -> StatePath<
    AppState,
    ChildState,
    impl for<'a> Fn(&'a AppState) -> &'a ChildState + Clone + Send + Sync,
    impl for<'a> Fn(&'a mut AppState) -> &'a mut ChildState + Clone + Send + Sync,
> {
    StatePath::new(
        |state: &AppState| &state.child,
        |state: &mut AppState| &mut state.child,
    )
}

/// Embeds required-child input and rejects every other parent route.
fn child_action_path() -> ActionPath<
    AppAction,
    ChildAction,
    impl for<'a> Fn(&'a AppAction) -> Option<&'a ChildAction> + Clone + Send + Sync,
    impl Fn(ChildAction) -> AppAction + Clone + Send + Sync,
> {
    ActionPath::new(
        |action: &AppAction| match action {
            AppAction::Child(child) => Some(child),
            AppAction::OptionalChild(_)
            | AppAction::ChangeParentOnly
            | AppAction::ChangeChildFromParent
            | AppAction::ReplaceRequiredChild { .. }
            | AppAction::RemoveChild
            | AppAction::InsertChild => None,
            #[cfg(feature = "test-util")]
            AppAction::StartRemoval => None,
        },
        AppAction::Child,
    )
}

/// Selects the optional child while preserving absence.
#[expect(
    clippy::type_complexity,
    reason = "opaque path closures exercise the normal public construction surface"
)]
fn optional_child_state_path() -> OptionalStatePath<
    AppState,
    ChildState,
    impl for<'a> Fn(&'a AppState) -> Option<&'a ChildState> + Clone + Send + Sync,
    impl for<'a> Fn(&'a mut AppState) -> Option<&'a mut ChildState> + Clone + Send + Sync,
> {
    OptionalStatePath::new(
        |state: &AppState| state.optional_child.as_ref(),
        |state: &mut AppState| state.optional_child.as_mut(),
    )
}

/// Embeds optional-child input independently of current child presence.
fn optional_child_action_path() -> ActionPath<
    AppAction,
    ChildAction,
    impl for<'a> Fn(&'a AppAction) -> Option<&'a ChildAction> + Clone + Send + Sync,
    impl Fn(ChildAction) -> AppAction + Clone + Send + Sync,
> {
    ActionPath::new(
        |action: &AppAction| match action {
            AppAction::OptionalChild(child) => Some(child),
            AppAction::Child(_)
            | AppAction::ChangeParentOnly
            | AppAction::ChangeChildFromParent
            | AppAction::ReplaceRequiredChild { .. }
            | AppAction::RemoveChild
            | AppAction::InsertChild => None,
            #[cfg(feature = "test-util")]
            AppAction::StartRemoval => None,
        },
        AppAction::OptionalChild,
    )
}

/// Builds the required route without registering an optional lifetime.
fn child_handle(store: &Store<AppState, AppAction>) -> ScopedStore<ChildState, ChildAction> {
    store.scope(child_state_path(), child_action_path())
}

/// Drops the root `Store` before the runtime joins its actor thread.
struct Fixture {
    store: Store<AppState, AppAction>,
    runtime: ManualRuntime,
}

/// Starts an actor with only the required child present.
fn scoped_fixture() -> Fixture {
    let runtime = ManualRuntime::new();
    let store = Store::new(AppState::default(), AppReducer, runtime.runtime());
    runtime.start_all();
    Fixture { store, runtime }
}

/// Starts an actor with both child routes available.
fn optional_fixture() -> Fixture {
    let runtime = ManualRuntime::new();
    let state = AppState {
        optional_child: Some(ChildState::default()),
        ..AppState::default()
    };
    let store = Store::new(state, AppReducer, runtime.runtime());
    runtime.start_all();
    Fixture { store, runtime }
}

impl Fixture {
    /// Waits for client work while this fixture retains the running actor.
    fn block_on<F: std::future::Future>(&self, future: F) -> F::Output {
        std::hint::black_box(&self.runtime);
        futures_executor::block_on(future)
    }
}

fn assert_send<T: Send>(_: T) {}
fn assert_send_type<T: Send>() {}
fn assert_sync_type<T: Sync>() {}

#[test]
fn absent_identified_case_rejects_registration_then_accepts_inserted_child() {
    let runtime = ManualRuntime::new();
    let store = Store::new(AppState::default(), AppReducer, runtime.runtime());
    let mut actor = runtime.take_only_queued_task();
    let selections = Arc::new(AtomicUsize::new(0));

    let missing = drive_scoped_actor(
        &mut actor,
        store.scope_case_identified(
            CaseStatePath::new(
                |state: &AppState| state.optional_child.as_ref(),
                |state: &mut AppState| state.optional_child.as_mut(),
            ),
            optional_child_action_path(),
            {
                let selections = Arc::clone(&selections);
                move |child| {
                    selections.fetch_add(1, Ordering::SeqCst);
                    &child.id
                }
            },
        ),
    )
    .unwrap_err();
    assert!(missing.is_scope_ended());
    assert!(!store.is_closed());
    assert_eq!(selections.load(Ordering::SeqCst), 0);

    let inserted = drive_scoped_actor(&mut actor, store.send(AppAction::InsertChild)).unwrap();
    assert!(inserted.is_finished());
    let child = drive_scoped_actor(
        &mut actor,
        store.scope_case_identified(
            CaseStatePath::new(
                |state: &AppState| state.optional_child.as_ref(),
                |state: &mut AppState| state.optional_child.as_mut(),
            ),
            optional_child_action_path(),
            |child| &child.id,
        ),
    )
    .unwrap();
    let changed = drive_scoped_actor(&mut actor, child.send(ChildAction::Increment)).unwrap();
    assert!(changed.is_finished());
    assert_eq!(
        drive_scoped_actor(&mut actor, child.read(|state| (state.id, state.count))).unwrap(),
        (0, 1)
    );

    drop(child);
    let mut context = Context::from_waker(std::task::Waker::noop());
    let mut shutdown = std::pin::pin!(store.shutdown_into_state());
    assert!(shutdown.as_mut().poll(&mut context).is_pending());
    assert!(actor.as_mut().poll(&mut context).is_ready());
    let Poll::Ready(Ok(state)) = shutdown.as_mut().poll(&mut context) else {
        panic!("shutdown must return the state after the child action");
    };
    assert_eq!(state.optional_reductions, 1);
    assert_eq!(state.optional_child.unwrap().count, 1);
}

#[test]
fn scoped_handle_diagnostics_do_not_inspect_or_retain_user_captures() {
    let runtime = ManualRuntime::new();
    let store = Store::new(AppState::default(), AppReducer, runtime.runtime());
    let capture = Arc::new(());
    let held = Arc::clone(&capture);
    let path = StatePath::new(
        move |_state: &AppState| -> &ChildState {
            std::hint::black_box(&held);
            panic!("diagnostics must not project state");
        },
        |_state: &mut AppState| -> &mut ChildState {
            panic!("diagnostics must not mutate state");
        },
    );
    let child = store.scope(path, child_action_path());
    let weak = child.downgrade();

    // Neither the state nor action fixtures implement Debug.
    assert_eq!(Arc::strong_count(&capture), 2);
    assert_eq!(format!("{child:?}"), "ScopedStore { .. }");
    assert_eq!(format!("{weak:?}"), "WeakScopedStore { .. }");
    assert_eq!(Arc::strong_count(&capture), 2);
    assert!(weak.upgrade().is_some());

    drop(child);
    assert_eq!(Arc::strong_count(&capture), 1);
    assert!(weak.upgrade().is_none());
    assert_eq!(format!("{weak:?}"), "WeakScopedStore { .. }");
    assert!(!store.is_closed());
}

#[test]
fn registration_future_does_not_retain_unused_local_path_closures() {
    let runtime = ManualRuntime::new();
    let store = Store::new(AppState::default(), AppReducer, runtime.runtime());
    let local = std::rc::Rc::new(());
    let mutable_capture = std::rc::Rc::clone(&local);
    let path = OptionalStatePath::new(
        |state: &AppState| state.optional_child.as_ref(),
        move |state: &mut AppState| {
            std::hint::black_box(&mutable_capture);
            state.optional_child.as_mut()
        },
    );

    let extract_capture = std::rc::Rc::clone(&local);
    let actions = ActionPath::new(
        move |action: &AppAction| {
            std::hint::black_box(&extract_capture);
            match action {
                AppAction::OptionalChild(child) => Some(child),
                _ => None,
            }
        },
        AppAction::OptionalChild,
    );

    let registration = store.scope_some(path, actions);
    assert_eq!(std::rc::Rc::strong_count(&local), 1);
    assert_send(registration);
}

/// Blocks an actor at a channel-confirmed boundary, not for a guessed delay.
#[derive(Default)]
struct Gate {
    open: Mutex<bool>,
    changed: Condvar,
}

impl Gate {
    /// Waits through spurious notifications until the release flag is set.
    fn wait(&self) {
        let mut open = self.open.lock().expect("gate lock must not be poisoned");
        while !*open {
            open = self
                .changed
                .wait(open)
                .expect("gate lock must not be poisoned");
        }
    }

    /// Releases every waiter, including callers cleaning up a failed assertion.
    fn open(&self) {
        *self.open.lock().expect("gate lock must not be poisoned") = true;
        self.changed.notify_all();
    }
}

/// Opens the gate before the fixture joins its actor, including assertion unwinding.
struct ReleaseGate(Arc<Gate>);

impl Drop for ReleaseGate {
    fn drop(&mut self) {
        self.0.open();
    }
}

#[test]
fn assertion_unwind_releases_a_blocked_actor_before_shutdown() {
    let runtime = ManualRuntime::new();
    let store = Store::new(AppState::default(), AppReducer, runtime.runtime());
    let mut actor = runtime.take_only_queued_task();
    let gate = Arc::new(Gate::default());
    let blocked_gate = Arc::clone(&gate);
    let (started, entered) = mpsc::sync_channel(1);
    let mut read = Box::pin(store.read(move |state| {
        started.send(()).unwrap();
        blocked_gate.wait();
        state.child.count
    }));
    let mut context = Context::from_waker(std::task::Waker::noop());
    assert!(read.as_mut().poll(&mut context).is_pending());
    let (finished, completed) = mpsc::sync_channel(1);
    let worker = std::thread::spawn(move || {
        let result = actor
            .as_mut()
            .poll(&mut Context::from_waker(std::task::Waker::noop()));
        finished.send(result.is_pending()).unwrap();
        actor
    });
    let entry = entered.recv_timeout(Duration::from_secs(5));
    if entry.is_err() {
        gate.open();
    }
    let panic = std::panic::catch_unwind(|| {
        let _release = ReleaseGate(Arc::clone(&gate));
        panic!("intentional assertion unwind while the actor waits");
    });
    let completion = completed.recv_timeout(Duration::from_secs(5));
    // Keep a broken guard from trapping the worker after the bounded assertion.
    gate.open();
    let mut actor = worker.join().expect("the released actor must return");

    entry.expect("actor must enter the gated read");
    let panic = panic.expect_err("the control must unwind intentionally");
    assert_eq!(
        panic.downcast_ref::<&str>().copied(),
        Some("intentional assertion unwind while the actor waits")
    );
    assert!(completion.expect("unwinding must release the actor"));
    assert!(matches!(
        read.as_mut().poll(&mut context),
        Poll::Ready(Ok(0))
    ));
    drop(read);
    drop(store);
    assert!(actor.as_mut().poll(&mut context).is_ready());
}

/// Polls once, then cancels the pending take without consuming the observation.
fn assert_next_pending<Value>(values: &mut rca_core::Observation<Value>) {
    let mut next = Box::pin(values.next());
    let waker = futures_util::task::noop_waker();
    let mut context = Context::from_waker(&waker);
    assert!(Pin::as_mut(&mut next).poll(&mut context).is_pending());
}

#[test]
fn distinct_scoped_observation_ignores_unrelated_commits() {
    let fixture = scoped_fixture();
    fixture.block_on(async {
        let store = fixture.store.clone();
        let child = store.scope(child_state_path(), child_action_path());
        let mut values = child.observe_distinct(|state| state.count).await.unwrap();
        assert_eq!(values.next().await, Some(0));
        let _ = store.send(AppAction::ChangeParentOnly).await.unwrap();
        assert_next_pending(&mut values);
        let _ = store
            .send(AppAction::Child(ChildAction::Increment))
            .await
            .unwrap();
        assert_eq!(values.next().await, Some(1));
    });
}

#[test]
fn scoped_observation_policies_follow_projected_values() {
    let fixture = scoped_fixture();
    fixture.block_on(async {
        let store = fixture.store.clone();
        let child = child_handle(&store);
        let mut every = child.observe(|state| state.count).await.unwrap();
        let mut distinct = child.observe_distinct(|state| state.count).await.unwrap();
        assert_eq!(every.next().await, Some(0));
        assert_eq!(distinct.next().await, Some(0));

        let _ = store.send(AppAction::ChangeParentOnly).await.unwrap();
        assert_eq!(every.next().await, Some(0));
        assert_next_pending(&mut distinct);

        let _ = store.send(AppAction::ChangeChildFromParent).await.unwrap();
        assert_eq!(every.next().await, Some(1));
        assert_eq!(distinct.next().await, Some(1));
    });
}

#[test]
fn old_scoped_observation_closes_without_new_generation_values() {
    let fixture = optional_fixture();
    fixture.block_on(async {
        let store = fixture.store.clone();
        let child = store
            .scope_some(optional_child_state_path(), optional_child_action_path())
            .await
            .unwrap();
        let mut old = child.observe(|state| state.count).await.unwrap();
        assert_eq!(old.next().await, Some(0));

        let _ = store.send(AppAction::RemoveChild).await.unwrap();
        let _ = store.send(AppAction::InsertChild).await.unwrap();
        let replacement = store
            .scope_some(optional_child_state_path(), optional_child_action_path())
            .await
            .unwrap();
        let mut current = replacement.observe(|state| state.count).await.unwrap();
        assert_eq!(current.next().await, Some(0));
        let _ = replacement.send(ChildAction::Increment).await.unwrap();

        assert_eq!(old.next().await, None);
        assert_eq!(current.next().await, Some(1));
    });
}

#[test]
fn scoped_take_before_removal_receives_last_value() {
    let fixture = optional_fixture();
    fixture.block_on(async {
        let store = fixture.store.clone();
        let child = store
            .scope_some(optional_child_state_path(), optional_child_action_path())
            .await
            .unwrap();
        let mut values = child.observe(|state| state.count).await.unwrap();
        assert_eq!(values.next().await, Some(0));
        let _ = child.send(ChildAction::Increment).await.unwrap();

        let gate = Arc::new(Gate::default());
        let release = ReleaseGate(Arc::clone(&gate));
        let blocked_gate = Arc::clone(&gate);
        let (started, started_receive) = std::sync::mpsc::sync_channel(1);
        let mut blocked = Box::pin(store.read(move |_| {
            started.send(()).expect("actor blocking read starts");
            blocked_gate.wait();
        }));
        let waker = futures_util::task::noop_waker();
        let mut context = Context::from_waker(&waker);
        assert!(Pin::as_mut(&mut blocked).poll(&mut context).is_pending());
        started_receive
            .recv_timeout(Duration::from_secs(5))
            .expect("actor reaches blocking read");

        let mut take = Box::pin(values.next());
        assert!(Pin::as_mut(&mut take).poll(&mut context).is_pending());
        let mut removal = Box::pin(store.send(AppAction::RemoveChild));
        assert!(Pin::as_mut(&mut removal).poll(&mut context).is_pending());
        drop(release);
        blocked.await.unwrap();
        assert_eq!(take.await, Some(1));
        let _ = removal.await.unwrap();
        assert_eq!(values.next().await, None);
    });
}

#[test]
fn scoped_removal_before_take_observes_closure() {
    let fixture = optional_fixture();
    fixture.block_on(async {
        let store = fixture.store.clone();
        let child = store
            .scope_some(optional_child_state_path(), optional_child_action_path())
            .await
            .unwrap();
        let mut values = child.observe(|state| state.count).await.unwrap();
        assert_eq!(values.next().await, Some(0));
        let _ = child.send(ChildAction::Increment).await.unwrap();

        let gate = Arc::new(Gate::default());
        let release = ReleaseGate(Arc::clone(&gate));
        let blocked_gate = Arc::clone(&gate);
        let (started, started_receive) = std::sync::mpsc::sync_channel(1);
        let mut blocked = Box::pin(store.read(move |_| {
            started.send(()).expect("actor blocking read starts");
            blocked_gate.wait();
        }));
        let waker = futures_util::task::noop_waker();
        let mut context = Context::from_waker(&waker);
        assert!(Pin::as_mut(&mut blocked).poll(&mut context).is_pending());
        started_receive
            .recv_timeout(Duration::from_secs(5))
            .expect("actor reaches blocking read");

        let mut removal = Box::pin(store.send(AppAction::RemoveChild));
        assert!(Pin::as_mut(&mut removal).poll(&mut context).is_pending());
        let mut take = Box::pin(values.next());
        assert!(Pin::as_mut(&mut take).poll(&mut context).is_pending());
        drop(release);
        blocked.await.unwrap();
        let _ = removal.await.unwrap();
        assert_eq!(take.await, None);
    });
}

#[test]
fn cancelled_scoped_take_resumes_without_losing_value() {
    let fixture = scoped_fixture();
    fixture.block_on(async {
        let child = child_handle(&fixture.store);
        let mut values = child.observe(|state| state.count).await.unwrap();
        assert_eq!(values.next().await, Some(0));
        let mut cancelled = Box::pin(values.next());
        let waker = futures_util::task::noop_waker();
        let mut context = Context::from_waker(&waker);
        assert!(Pin::as_mut(&mut cancelled).poll(&mut context).is_pending());
        drop(cancelled);

        let _ = child.send(ChildAction::Increment).await.unwrap();
        assert_eq!(values.next().await, Some(1));
    });
}

#[cfg(feature = "test-util")]
#[test]
fn cancelled_scoped_registration_is_cleaned_after_acceptance() {
    let fixture = scoped_fixture();
    let child = child_handle(&fixture.store);
    let gate = Arc::new(Gate::default());
    let release = ReleaseGate(Arc::clone(&gate));
    let selector_gate = Arc::clone(&gate);
    let selections = Arc::new(AtomicUsize::new(0));
    let selector_selections = Arc::clone(&selections);
    let (started, started_receive) = std::sync::mpsc::sync_channel(1);

    let mut registration = Box::pin(child.observe(move |state| {
        selector_selections.fetch_add(1, Ordering::AcqRel);
        started.send(()).expect("selector start must be observed");
        selector_gate.wait();
        state.count
    }));
    let waker = futures_util::task::noop_waker();
    let mut context = Context::from_waker(&waker);
    assert!(
        Pin::as_mut(&mut registration)
            .poll(&mut context)
            .is_pending()
    );
    started_receive
        .recv_timeout(Duration::from_secs(5))
        .expect("selector must start after actor acceptance");
    drop(registration);
    drop(release);

    fixture.block_on(async {
        fixture
            .store
            .read(|_| ())
            .await
            .expect("cancelled registration must drain");
        assert_eq!(selections.load(Ordering::Acquire), 1);
        assert_eq!(
            fixture
                .store
                .runtime_snapshot()
                .await
                .expect("runtime snapshot")
                .observation_count,
            0
        );
    });
}

#[cfg(feature = "test-util")]
#[test]
fn dropped_child_closes_scoped_observation_and_cleans_registries() {
    #[cfg(not(miri))]
    if subprocess::run_in_child("dropped_child_closes_scoped_observation_and_cleans_registries") {
        return;
    }

    let fixture = optional_fixture();
    fixture.block_on(async {
        let child = fixture
            .store
            .scope_some(optional_child_state_path(), optional_child_action_path())
            .await
            .expect("optional child");
        let mut values = child.observe(|state| state.count).await.unwrap();
        assert_eq!(values.next().await, Some(0));
        let active = fixture.store.runtime_diagnostics().await.unwrap();
        assert_eq!(active.handle_registration_count(), 1);
        assert_eq!(active.runtime().observation_count, 1);

        drop(child);
        assert_eq!(values.next().await, None);
        let cleaned = fixture.store.runtime_diagnostics().await.unwrap();
        assert_eq!(cleaned.handle_registration_count(), 0);
        assert_eq!(cleaned.runtime().observation_count, 0);
    });
}

#[test]
fn dropping_final_registered_child_closes_observation_while_root_is_idle() {
    #[cfg(not(miri))]
    if subprocess::run_in_child(
        "dropping_final_registered_child_closes_observation_while_root_is_idle",
    ) {
        return;
    }

    let fixture = optional_fixture();
    let child = fixture.block_on(async {
        fixture
            .store
            .scope_some(optional_child_state_path(), optional_child_action_path())
            .await
            .expect("optional child")
    });
    let mut values = fixture
        .block_on(child.observe(|state| state.count))
        .unwrap();
    assert_eq!(fixture.block_on(values.next()), Some(0));

    drop(child);
    assert_eq!(fixture.block_on(values.next()), None);
    std::hint::black_box(&fixture.store);
}

#[test]
fn scoped_observation_does_not_keep_store_alive() {
    let runtime = ManualRuntime::new();
    let store = Store::new(AppState::default(), AppReducer, runtime.runtime());
    let closed = store.closed();
    let child = child_handle(&store);
    runtime.start_all();
    let mut values = futures_executor::block_on(child.observe(|state| state.count)).unwrap();
    assert_eq!(futures_executor::block_on(values.next()), Some(0));

    drop(child);
    drop(store);
    futures_executor::block_on(closed);
    assert_eq!(futures_executor::block_on(values.next()), None);
}

#[cfg(feature = "test-util")]
#[test]
fn ten_thousand_scoped_observations_do_not_block_commits() {
    let fixture = scoped_fixture();
    fixture.block_on(async {
        let child = child_handle(&fixture.store);
        let mut observations = Vec::with_capacity(10_000);
        for _ in 0..10_000 {
            observations.push(child.observe_distinct(|state| state.count).await.unwrap());
        }
        assert_eq!(
            fixture
                .store
                .runtime_snapshot()
                .await
                .expect("active snapshot")
                .observation_count,
            10_000
        );

        let _ = child.send(ChildAction::Increment).await.unwrap();
        let first = observations.first_mut().expect("first observation");
        assert_eq!(first.next().await, Some(0));
        assert_eq!(first.next().await, Some(1));
        assert_next_pending(first);

        drop(observations);
        fixture.store.read(|_| ()).await.expect("cleanup marker");
        assert_eq!(
            fixture
                .store
                .runtime_snapshot()
                .await
                .expect("cleaned snapshot")
                .observation_count,
            0
        );
    });
}

#[test]
fn old_optional_handle_cannot_address_reinserted_child() {
    let fixture = optional_fixture();
    fixture.block_on(async {
        let store = fixture.store.clone();
        let child = store
            .scope_some(optional_child_state_path(), optional_child_action_path())
            .await
            .expect("present child registers");
        let _ = store.send(AppAction::RemoveChild).await.unwrap();
        let _ = store.send(AppAction::InsertChild).await.unwrap();
        let error = child
            .send(ChildAction::Increment)
            .await
            .expect_err("old handle ended");
        assert!(error.is_scope_ended());
        assert_eq!(
            error.to_string().lines().next(),
            Some("scoped store ended before work completed")
        );
        let display = error.to_string();
        assert!(display.ends_with(&error.backtrace().to_string()));
        assert!(format!("{error:?}").contains("kind: ScopeEnded"));
        assert!(!error.is_store_stopped());
        assert!(std::error::Error::source(&error).is_none());
        assert_eq!(
            store
                .read(|state| state.optional_child.as_ref().unwrap().count)
                .await
                .unwrap(),
            0,
        );
    });
}

#[cfg(feature = "test-util")]
#[test]
fn first_gate_decision_cannot_be_replaced_before_actor_resumes() {
    use futures_util::FutureExt;

    for (release_first, child_remains, commit_version) in [(true, false, 2), (false, true, 1)] {
        let runtime = ManualRuntime::new();
        let gate = rca_core::testing::AssertionGate::new();
        let store = Store::builder(
            AppState {
                optional_child: Some(ChildState::default()),
                ..AppState::default()
            },
            AppReducer,
            runtime.runtime(),
        )
        .assertion_gate(gate.clone())
        .build()
        .unwrap();
        let mut actor = runtime.take_only_queued_task();
        let mut context = Context::from_waker(std::task::Waker::noop());
        let start = store.try_send(AppAction::StartRemoval).unwrap();
        assert!(actor.as_mut().poll(&mut context).is_pending());
        let started = start
            .finish()
            .now_or_never()
            .expect("starting action committed")
            .unwrap();
        let mut effect = runtime.take_only_queued_task();
        assert!(effect.as_mut().poll(&mut context).is_pending());
        assert!(actor.as_mut().poll(&mut context).is_pending());
        assert_eq!(
            gate.inspect_pending(|action| matches!(action, AppAction::RemoveChild)),
            Some(true)
        );

        let release = if release_first {
            Some(
                gate.release_pending_with_receipt()
                    .expect("first release accepted"),
            )
        } else {
            assert!(gate.skip_pending());
            None
        };
        // The actor is not polled between competing decisions.
        assert!(!gate.skip_pending());
        assert!(!gate.release_pending());
        assert!(gate.release_pending_with_receipt().is_none());
        assert!(gate.has_pending());
        assert!(!started.is_finished());

        assert!(actor.as_mut().poll(&mut context).is_pending());
        let released = release.map(|release| {
            let task = release
                .finish()
                .now_or_never()
                .expect("original release receipt retained")
                .unwrap();
            assert!(
                !task.is_finished(),
                "the sending effect still owns causal work"
            );
            task
        });
        assert!(effect.as_mut().poll(&mut context).is_ready());
        assert!(actor.as_mut().poll(&mut context).is_pending());
        assert!(started.is_finished());
        if let Some(released) = released {
            assert!(released.is_finished());
        }
        assert!(!gate.has_pending());
        assert_eq!(
            gate.latest_snapshot().unwrap().commit_version,
            commit_version
        );
        assert_eq!(
            drive_scoped_actor(
                &mut actor,
                store.read(|state| state.optional_child.is_some())
            )
            .unwrap(),
            child_remains
        );
        drop(store);
        assert!(actor.as_mut().poll(&mut context).is_ready());
    }
}

#[cfg(feature = "test-util")]
#[test]
fn queued_stale_send_and_read_do_not_execute_or_commit() {
    let runtime = ManualRuntime::new();
    let gate = rca_core::testing::AssertionGate::new();
    let state = AppState {
        optional_child: Some(ChildState::default()),
        ..AppState::default()
    };
    let store = Store::builder(state, AppReducer, runtime.runtime())
        .assertion_gate(gate.clone())
        .build()
        .unwrap();
    runtime.start_all();

    futures_executor::block_on(async {
        let embeddings = Arc::new(AtomicUsize::new(0));
        let counted_embeddings = Arc::clone(&embeddings);
        let action_path = ActionPath::new(
            |action: &AppAction| match action {
                AppAction::OptionalChild(child) => Some(child),
                _ => None,
            },
            move |action| {
                counted_embeddings.fetch_add(1, Ordering::AcqRel);
                AppAction::OptionalChild(action)
            },
        );
        let child = store
            .scope_some(optional_child_state_path(), action_path)
            .await
            .unwrap();
        let _ = store.send(AppAction::StartRemoval).await.unwrap();
        // The action receipt can precede acceptance of the removal effect.
        runtime.wait_for_queued_task();
        runtime.start_all();
        gate.wait_for_pending().await;

        let mut send = Box::pin(child.send(ChildAction::Increment));
        let selections = Arc::new(AtomicUsize::new(0));
        let counted_selections = Arc::clone(&selections);
        let mut read = Box::pin(child.read(move |state| {
            counted_selections.fetch_add(1, Ordering::AcqRel);
            state.count
        }));
        let waker = futures_util::task::noop_waker();
        let mut context = Context::from_waker(&waker);
        assert!(Pin::as_mut(&mut send).poll(&mut context).is_pending());
        assert!(Pin::as_mut(&mut read).poll(&mut context).is_pending());

        let removal = gate
            .release_pending_with_receipt()
            .expect("removal effect is gated");
        let _ = removal.finish().await.expect("removal commits");
        assert!(send.await.unwrap_err().is_scope_ended());
        assert!(read.await.unwrap_err().is_scope_ended());

        let snapshot = store.runtime_snapshot().await.unwrap();
        assert_eq!(snapshot.commit_version, 2);
        assert_eq!(embeddings.load(Ordering::Acquire), 0);
        assert_eq!(selections.load(Ordering::Acquire), 0);
        assert_eq!(
            store.read(|state| state.optional_reductions).await.unwrap(),
            0
        );
        store.shutdown().await.unwrap();
    });
}

#[cfg(feature = "test-util")]
#[test]
fn admitted_try_send_is_coarsely_rejected_after_prior_removal() {
    let runtime = ManualRuntime::new();
    let gate = rca_core::testing::AssertionGate::new();
    let state = AppState {
        optional_child: Some(ChildState::default()),
        ..AppState::default()
    };
    let store = Store::builder(state, AppReducer, runtime.runtime())
        .assertion_gate(gate.clone())
        .build()
        .unwrap();
    runtime.start_all();

    futures_executor::block_on(async {
        let embeddings = Arc::new(AtomicUsize::new(0));
        let counted_embeddings = Arc::clone(&embeddings);
        let action_path = ActionPath::new(
            |action: &AppAction| match action {
                AppAction::OptionalChild(child) => Some(child),
                _ => None,
            },
            move |action| {
                counted_embeddings.fetch_add(1, Ordering::AcqRel);
                AppAction::OptionalChild(action)
            },
        );
        let child = store
            .scope_some(optional_child_state_path(), action_path)
            .await
            .unwrap();
        let _ = store.send(AppAction::StartRemoval).await.unwrap();
        // The action receipt can precede acceptance of the removal effect.
        runtime.wait_for_queued_task();
        runtime.start_all();
        gate.wait_for_pending().await;
        assert_eq!(
            gate.inspect_pending(|action| matches!(action, AppAction::RemoveChild)),
            Some(true),
        );
        assert_eq!(
            gate.pending_snapshot()
                .expect("removal has a snapshot")
                .commit_version,
            1,
        );

        let receipt = child
            .try_send(ChildAction::Increment)
            .expect("scope signal still admits before removal commits");
        let removal = gate
            .release_pending_with_receipt()
            .expect("removal effect is gated");
        let _ = removal.finish().await.expect("removal commits");
        let coarse_error = receipt
            .finish()
            .await
            .expect_err("actor rejects the admitted stale action");
        assert_eq!(
            coarse_error.to_string().lines().next(),
            Some("store operation ended before successful completion"),
        );

        let snapshot = store.runtime_snapshot().await.unwrap();
        assert_eq!(snapshot.commit_version, 2);
        assert_eq!(embeddings.load(Ordering::Acquire), 0);
        assert_eq!(
            store.read(|state| state.optional_reductions).await.unwrap(),
            0,
        );

        let root_task = store
            .send(AppAction::InsertChild)
            .await
            .expect("coarse receipt error does not stop root authority");
        root_task.finish().await;
        assert_eq!(store.runtime_snapshot().await.unwrap().commit_version, 3,);
        assert_eq!(
            store
                .read(|state| state.optional_child.as_ref().unwrap().count)
                .await
                .unwrap(),
            0,
        );
        store.shutdown().await.unwrap();
    });
}

#[cfg(feature = "test-util")]
#[test]
fn cancelled_and_dropped_registrations_are_cleaned_lazily() {
    let runtime = ManualRuntime::new();
    let state = AppState {
        optional_child: Some(ChildState::default()),
        ..AppState::default()
    };
    let store = Store::new(state, AppReducer, runtime.runtime());

    let mut registration =
        Box::pin(store.scope_some(optional_child_state_path(), optional_child_action_path()));
    let waker = futures_util::task::noop_waker();
    let mut context = Context::from_waker(&waker);
    assert!(
        Pin::as_mut(&mut registration)
            .poll(&mut context)
            .is_pending()
    );
    runtime.start_all();
    let accepted = futures_executor::block_on(store.runtime_diagnostics()).unwrap();
    assert_eq!(accepted.handle_registration_count(), 1);
    drop(registration);
    futures_executor::block_on(store.read(|_state| ())).unwrap();
    let cancelled = futures_executor::block_on(store.runtime_diagnostics()).unwrap();
    assert_eq!(cancelled.handle_registration_count(), 0);

    let child = futures_executor::block_on(
        store.scope_some(optional_child_state_path(), optional_child_action_path()),
    )
    .unwrap();
    assert_eq!(
        futures_executor::block_on(store.runtime_diagnostics())
            .unwrap()
            .handle_registration_count(),
        1
    );
    drop(child);
    futures_executor::block_on(store.read(|_state| ())).unwrap();
    assert_eq!(
        futures_executor::block_on(store.runtime_diagnostics())
            .unwrap()
            .handle_registration_count(),
        0
    );
    futures_executor::block_on(store.shutdown()).unwrap();
}

#[cfg(feature = "test-util")]
#[test]
fn registration_cancelled_before_mailbox_acceptance_is_never_installed() {
    let runtime = ManualRuntime::new();
    let state = AppState {
        optional_child: Some(ChildState::default()),
        ..AppState::default()
    };
    let store = Store::builder(state, AppReducer, runtime.runtime())
        .mailbox_capacity(1)
        .build()
        .unwrap();
    let action = store.try_send(AppAction::InsertChild).unwrap();
    let mut registration =
        Box::pin(store.scope_some(optional_child_state_path(), optional_child_action_path()));
    let waker = futures_util::task::noop_waker();
    let mut context = Context::from_waker(&waker);
    assert!(
        Pin::as_mut(&mut registration)
            .poll(&mut context)
            .is_pending()
    );
    drop(registration);

    runtime.start_all();
    let _ = futures_executor::block_on(action.finish()).unwrap();
    assert_eq!(
        futures_executor::block_on(store.runtime_diagnostics())
            .unwrap()
            .handle_registration_count(),
        0
    );
    futures_executor::block_on(store.shutdown()).unwrap();
}

#[cfg(feature = "test-util")]
#[test]
fn admitted_registration_cancelled_before_actor_start_skips_selector() {
    let runtime = ManualRuntime::new();
    let state = AppState {
        optional_child: Some(ChildState::default()),
        ..AppState::default()
    };
    let store = Store::builder(state, AppReducer, runtime.runtime())
        .mailbox_capacity(1)
        .build()
        .unwrap();
    let capture = Arc::new(());
    let held = Arc::clone(&capture);
    let selections = Arc::new(AtomicUsize::new(0));
    let selected = Arc::clone(&selections);
    let path = OptionalStatePath::new(
        move |state: &AppState| {
            std::hint::black_box(&held);
            selected.fetch_add(1, Ordering::SeqCst);
            state.optional_child.as_ref()
        },
        |state: &mut AppState| state.optional_child.as_mut(),
    );
    let mut registration = Box::pin(store.scope_some(path, optional_child_action_path()));
    let mut context = Context::from_waker(std::task::Waker::noop());
    assert!(registration.as_mut().poll(&mut context).is_pending());
    assert!(
        store
            .try_send(AppAction::ChangeParentOnly)
            .unwrap_err()
            .is_full()
    );
    drop(registration);
    assert_eq!(Arc::strong_count(&capture), 2);

    runtime.start_all();
    let diagnostics = futures_executor::block_on(store.runtime_diagnostics()).unwrap();
    assert_eq!(diagnostics.handle_registration_count(), 0);
    assert_eq!(selections.load(Ordering::SeqCst), 0);
    assert_eq!(Arc::strong_count(&capture), 1);

    let child = futures_executor::block_on(
        store.scope_some(optional_child_state_path(), optional_child_action_path()),
    )
    .unwrap();
    let task = futures_executor::block_on(child.send(ChildAction::Increment)).unwrap();
    futures_executor::block_on(task.finish());
    assert_eq!(
        futures_executor::block_on(
            store.read(|state| state.optional_child.as_ref().unwrap().count)
        )
        .unwrap(),
        1
    );
    drop(child);
    futures_executor::block_on(store.shutdown()).unwrap();
}

#[test]
fn queued_shutdown_discards_later_registration_as_store_stopped() {
    let runtime = ManualRuntime::new();
    let state = AppState {
        optional_child: Some(ChildState::default()),
        ..AppState::default()
    };
    let store = Store::builder(state, AppReducer, runtime.runtime())
        .mailbox_capacity(2)
        .build()
        .unwrap();
    let mut context = Context::from_waker(std::task::Waker::noop());
    let mut shutdown = Box::pin(store.shutdown());
    assert!(shutdown.as_mut().poll(&mut context).is_pending());

    let capture = Arc::new(());
    let held = Arc::clone(&capture);
    let selections = Arc::new(AtomicUsize::new(0));
    let selected = Arc::clone(&selections);
    let path = OptionalStatePath::new(
        move |state: &AppState| {
            std::hint::black_box(&held);
            selected.fetch_add(1, Ordering::SeqCst);
            state.optional_child.as_ref()
        },
        |state: &mut AppState| state.optional_child.as_mut(),
    );
    let mut registration = Box::pin(store.scope_some(path, optional_child_action_path()));
    assert!(registration.as_mut().poll(&mut context).is_pending());
    assert!(
        store
            .try_send(AppAction::ChangeParentOnly)
            .unwrap_err()
            .is_full()
    );

    runtime.start_all();
    futures_executor::block_on(shutdown).unwrap();
    let error = futures_executor::block_on(registration).unwrap_err();
    assert!(error.is_store_stopped());
    assert!(!error.is_scope_ended());
    assert_eq!(selections.load(Ordering::SeqCst), 0);
    assert_eq!(Arc::strong_count(&capture), 1);
    assert!(runtime.join_all().into_iter().all(|result| result.is_ok()));
}

/// Drives synchronous client work for at most sixteen actor polls.
///
/// This does not start queued effects or advance timers. A stalled request fails
/// rather than relying on system load to decide when it should have completed.
fn drive_scoped_actor<F: Future>(actor: &mut rca_core::RuntimeFuture, future: F) -> F::Output {
    let mut future = std::pin::pin!(future);
    let mut context = Context::from_waker(std::task::Waker::noop());
    // These fixtures have no timers or spawned effects; stalled work must fail boundedly.
    for _ in 0..16 {
        if let Poll::Ready(output) = future.as_mut().poll(&mut context) {
            return output;
        }
        assert!(actor.as_mut().poll(&mut context).is_pending());
    }
    panic!("scoped operation remained pending after 16 actor polls");
}

mod selection_authority {
    //! Changes lawful external selection between validation and projection.

    use std::sync::atomic::AtomicBool;

    use rca_core::{KeyedAction, KeyedState, KeyedStatePath, StackAction, StackState};

    use super::*;

    /// Switches a getter's next selection without committing a reducer action.
    #[derive(Default)]
    struct Selection {
        // External changes are legal: StatePath does not require pure getters.
        current: AtomicUsize,
        // Return the old child once, then make the next selection choose another.
        switch_after_selection: AtomicBool,
    }

    impl Selection {
        /// Returns this selection and optionally changes the next one.
        fn index(&self) -> usize {
            let selected = self.current.load(Ordering::SeqCst);
            if self.switch_after_selection.swap(false, Ordering::SeqCst) {
                self.current.store(1, Ordering::SeqCst);
            }
            selected
        }
    }

    /// Names the callback boundary where the path changes its selected child.
    #[derive(Clone, Copy, Debug)]
    enum ProjectionPhase {
        Read,
        Registration,
        Publication,
    }

    /// Gives the original and replacement children distinct identities and values.
    fn state(replacement: bool) -> AppState {
        AppState {
            child: ChildState {
                id: 7,
                count: 3,
                ..ChildState::default()
            },
            optional_child: replacement.then_some(ChildState {
                id: 8,
                count: 40,
                ..ChildState::default()
            }),
            ..AppState::default()
        }
    }

    #[test]
    fn actor_owned_selection_accepts_send_only_captures() {
        let runtime = ManualRuntime::new();
        let store = Store::new(state(true), AppReducer, runtime.runtime());
        let mut actor = runtime.take_only_queued_task();
        let get_calls = Arc::new(AtomicUsize::new(0));
        let selected = Arc::clone(&get_calls);
        let get_count = std::cell::Cell::new(0);
        let identity_calls = Arc::new(AtomicUsize::new(0));
        let identified = Arc::clone(&identity_calls);
        let identity_count = std::cell::Cell::new(0);
        let child = drive_scoped_actor(
            &mut actor,
            store.scope_identified(
                StatePath::new(
                    move |state: &AppState| {
                        get_count.set(get_count.get() + 1);
                        selected.store(get_count.get(), Ordering::SeqCst);
                        &state.child
                    },
                    |state: &mut AppState| &mut state.child,
                ),
                child_action_path(),
                move |child: &ChildState| {
                    identity_count.set(identity_count.get() + 1);
                    identified.store(identity_count.get(), Ordering::SeqCst);
                    &child.id
                },
            ),
        )
        .unwrap();

        // Cell is Send but not Sync; only the actor owns these callbacks.
        assert_send_type::<ScopedStore<ChildState, ChildAction>>();
        assert_sync_type::<ScopedStore<ChildState, ChildAction>>();
        assert_eq!(get_calls.load(Ordering::SeqCst), 1);
        assert_eq!(identity_calls.load(Ordering::SeqCst), 1);

        assert_eq!(
            drive_scoped_actor(&mut actor, child.read(|child| child.id)).unwrap(),
            7
        );
        assert_eq!(get_calls.load(Ordering::SeqCst), 2);
        assert_eq!(identity_calls.load(Ordering::SeqCst), 2);
        assert!(!store.is_closed());
    }

    /// Selects the externally chosen child while retaining no state borrow.
    #[expect(
        clippy::type_complexity,
        reason = "exercises public optional paths with inferred higher-ranked closures"
    )]
    fn path(
        selection: &Arc<Selection>,
    ) -> OptionalStatePath<
        AppState,
        ChildState,
        impl for<'a> Fn(&'a AppState) -> Option<&'a ChildState> + Send + Sync + use<>,
        impl for<'a> Fn(&'a mut AppState) -> Option<&'a mut ChildState> + Send + Sync + use<>,
    > {
        let selected = Arc::clone(selection);
        let mutable = Arc::clone(selection);
        OptionalStatePath::new(
            move |state: &AppState| match selected.index() {
                0 => Some(&state.child),
                _ => state.optional_child.as_ref(),
            },
            move |state: &mut AppState| match mutable.current.load(Ordering::SeqCst) {
                0 => Some(&mut state.child),
                _ => state.optional_child.as_mut(),
            },
        )
    }

    /// Requires the old observation to close before projecting a changed selection.
    fn assert_publication_rejects_changed_selection(replacement: bool) {
        let runtime = ManualRuntime::new();
        let store = Store::new(state(replacement), AppReducer, runtime.runtime());
        let mut actor = runtime.take_only_queued_task();
        let selection = Arc::new(Selection::default());
        let child = drive_scoped_actor(
            &mut actor,
            store.scope_some_identified(path(&selection), child_action_path(), |child| &child.id),
        )
        .unwrap();
        let projected = Arc::new(Mutex::new(Vec::new()));
        let calls = Arc::clone(&projected);
        let mut values = drive_scoped_actor(
            &mut actor,
            child.observe(move |child| {
                calls.lock().unwrap().push(child.id);
                child.id
            }),
        )
        .unwrap();
        assert_eq!(drive_scoped_actor(&mut actor, values.next()), Some(7));

        // Refresh selects child 7, then the publication sees a different selection.
        selection
            .switch_after_selection
            .store(true, Ordering::SeqCst);
        let task = drive_scoped_actor(&mut actor, store.send(AppAction::ChangeParentOnly)).unwrap();
        assert!(task.is_finished());

        assert_eq!(
            *projected.lock().unwrap(),
            [7],
            "a refreshed handle cannot authorise projection of a different child"
        );
        assert_eq!(drive_scoped_actor(&mut actor, values.next()), None);
        let error = drive_scoped_actor(&mut actor, child.read(|child| child.id)).unwrap_err();
        assert!(error.is_scope_ended());
        assert!(!error.is_store_stopped());
        assert_eq!(
            drive_scoped_actor(&mut actor, store.read(|state| state.child.id)).unwrap(),
            7
        );
        assert!(!store.is_closed());
    }

    #[test]
    fn later_publication_cannot_project_a_different_identified_child() {
        assert_publication_rejects_changed_selection(true);
    }

    #[test]
    fn later_publication_closes_when_selection_becomes_absent() {
        assert_publication_rejects_changed_selection(false);
    }

    #[test]
    fn buffered_take_rechecks_external_identity_before_delivering_value() {
        let runtime = ManualRuntime::new();
        let store = Store::new(state(true), AppReducer, runtime.runtime());
        let mut actor = runtime.take_only_queued_task();
        let selection = Arc::new(Selection::default());
        let child = drive_scoped_actor(
            &mut actor,
            store.scope_some_identified(path(&selection), child_action_path(), |child| &child.id),
        )
        .unwrap();
        let owner = Arc::new(());
        let selected_owner = Arc::clone(&owner);
        let calls = Arc::new(AtomicUsize::new(0));
        let projected = Arc::clone(&calls);
        let mut values = drive_scoped_actor(
            &mut actor,
            child.observe(move |child| {
                projected.fetch_add(1, Ordering::SeqCst);
                (child.id, child.count, Arc::clone(&selected_owner))
            }),
        )
        .unwrap();
        let initial = drive_scoped_actor(&mut actor, values.next()).unwrap();
        assert_eq!((initial.0, initial.1), (7, 3));
        drop(initial);

        let task =
            drive_scoped_actor(&mut actor, store.send(AppAction::ChangeChildFromParent)).unwrap();
        assert!(task.is_finished());
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(
            Arc::strong_count(&owner),
            3,
            "selector and buffered value remain live"
        );

        // No reducer or refresh occurs between external expiry and the buffered take.
        selection.current.store(1, Ordering::SeqCst);
        assert!(drive_scoped_actor(&mut actor, values.next()).is_none());
        let error = drive_scoped_actor(&mut actor, child.read(|child| child.id)).unwrap_err();
        assert!(error.is_scope_ended());
        assert!(!error.is_store_stopped());
        assert_eq!(
            drive_scoped_actor(&mut actor, store.read(|state| state.child.count)).unwrap(),
            4
        );
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(Arc::strong_count(&owner), 1);
    }

    /// Checks expiry synchronously while the caller still owns the route.
    fn assert_route_ended(handle: &rca_core::WeakScopedStore<ChildState, ChildAction>) {
        let handle = handle.upgrade().expect("the caller still owns the route");
        let mut read = std::pin::pin!(handle.read(|_child| -> usize {
            panic!("an expired route must not enter a re-entrant query");
        }));
        let Poll::Ready(Err(error)) = read
            .as_mut()
            .poll(&mut Context::from_waker(std::task::Waker::noop()))
        else {
            panic!("expiry must be visible before rejection cleanup");
        };
        assert!(error.is_scope_ended());
        assert!(!error.is_store_stopped());
    }

    /// Re-enters the rejected route when its query capture is destroyed.
    struct RejectedCapture {
        handle: rca_core::WeakScopedStore<ChildState, ChildAction>,
        destroyed: Arc<AtomicBool>,
    }

    impl Drop for RejectedCapture {
        fn drop(&mut self) {
            assert_route_ended(&self.handle);
            self.destroyed.store(true, Ordering::SeqCst);
        }
    }

    /// Re-enters the rejected route from the actual reply notification.
    struct RejectionWake {
        handle: rca_core::WeakScopedStore<ChildState, ChildAction>,
        called: AtomicBool,
    }

    impl std::task::Wake for RejectionWake {
        fn wake(self: Arc<Self>) {
            assert_route_ended(&self.handle);
            self.called.store(true, Ordering::SeqCst);
        }
    }

    #[test]
    fn rejection_ends_authority_before_query_capture_destruction() {
        for observe in [false, true] {
            let runtime = ManualRuntime::new();
            let store = Store::new(state(true), AppReducer, runtime.runtime());
            let mut actor = runtime.take_only_queued_task();
            let selection = Arc::new(Selection::default());
            let child = drive_scoped_actor(
                &mut actor,
                store.scope_some_identified(path(&selection), child_action_path(), |child| {
                    &child.id
                }),
            )
            .unwrap();
            let destroyed = Arc::new(AtomicBool::new(false));
            let capture = RejectedCapture {
                handle: child.downgrade(),
                destroyed: Arc::clone(&destroyed),
            };
            let calls = Arc::new(AtomicUsize::new(0));
            let queried = Arc::clone(&calls);
            selection.current.store(1, Ordering::SeqCst);

            let query = async {
                if observe {
                    child
                        .observe(move |child| {
                            std::hint::black_box(&capture);
                            queried.fetch_add(1, Ordering::SeqCst);
                            child.id
                        })
                        .await
                        .map(drop)
                } else {
                    child
                        .read(move |child| {
                            std::hint::black_box(&capture);
                            queried.fetch_add(1, Ordering::SeqCst);
                            child.id
                        })
                        .await
                        .map(drop)
                }
            };
            let mut query = std::pin::pin!(query);
            let wake = Arc::new(RejectionWake {
                handle: child.downgrade(),
                called: AtomicBool::new(false),
            });
            let waker = std::task::Waker::from(Arc::clone(&wake));
            assert!(
                query
                    .as_mut()
                    .poll(&mut Context::from_waker(&waker))
                    .is_pending()
            );
            let mut context = Context::from_waker(std::task::Waker::noop());
            assert!(actor.as_mut().poll(&mut context).is_pending());
            assert!(wake.called.load(Ordering::SeqCst));
            let Poll::Ready(result) = query.as_mut().poll(&mut context) else {
                panic!("rejection must complete the admitted request");
            };
            let error = result.unwrap_err();
            assert!(error.is_scope_ended());
            assert!(!error.is_store_stopped());
            assert!(destroyed.load(Ordering::SeqCst));
            assert_eq!(calls.load(Ordering::SeqCst), 0);
            assert!(!store.is_closed());
        }
    }

    /// Shares only the publication oracle between keyed and stack route APIs.
    fn assert_collection_publication<State: Send + 'static, Action: Send + 'static>(
        actor: &mut rca_core::RuntimeFuture,
        store: &Store<State, Action>,
        child: &ScopedStore<usize, ()>,
        selection: &Selection,
        action: Action,
    ) {
        let projected = Arc::new(Mutex::new(Vec::new()));
        let calls = Arc::clone(&projected);
        let mut values = drive_scoped_actor(
            actor,
            child.observe(move |child| {
                calls.lock().unwrap().push(*child);
                *child
            }),
        )
        .unwrap();
        assert_eq!(drive_scoped_actor(actor, values.next()), Some(7));

        selection
            .switch_after_selection
            .store(true, Ordering::SeqCst);
        assert!(
            drive_scoped_actor(actor, store.send(action))
                .unwrap()
                .is_finished()
        );

        assert_eq!(*projected.lock().unwrap(), [7]);
        assert_eq!(drive_scoped_actor(actor, values.next()), None);
    }

    #[test]
    fn keyed_selection_cannot_change_collections_between_validation_and_projection() {
        for phase in [
            ProjectionPhase::Read,
            ProjectionPhase::Registration,
            ProjectionPhase::Publication,
        ] {
            let runtime = ManualRuntime::new();
            let mut first = KeyedState::new();
            first.insert(1_u8, 7_usize);
            let mut second = KeyedState::new();
            second.insert(1_u8, 8_usize);
            let reducer = rca_core::Reduce::new(
                |_state: &mut [KeyedState<u8, usize>; 2], _action: &KeyedAction<u8, ()>, _| {
                    Effect::none()
                },
            );
            let store = Store::new([first, second], reducer, runtime.runtime());
            let mut actor = runtime.take_only_queued_task();
            let selection = Arc::new(Selection::default());
            let selected = Arc::clone(&selection);
            let mutable = Arc::clone(&selection);
            let child = drive_scoped_actor(
                &mut actor,
                store.scope_keyed(
                    KeyedStatePath::new(
                        move |state: &[KeyedState<u8, usize>; 2]| &state[selected.index()],
                        move |state: &mut [KeyedState<u8, usize>; 2]| {
                            &mut state[mutable.current.load(Ordering::SeqCst)]
                        },
                    ),
                    rca_core::identity_keyed_action_path(),
                    1,
                ),
            )
            .unwrap();
            if matches!(phase, ProjectionPhase::Publication) {
                assert_collection_publication(
                    &mut actor,
                    &store,
                    &child,
                    &selection,
                    KeyedAction::new(1, ()),
                );
            } else {
                selection
                    .switch_after_selection
                    .store(true, Ordering::SeqCst);
                let result = if matches!(phase, ProjectionPhase::Registration) {
                    drive_scoped_actor(&mut actor, child.observe(|child| *child))
                        .map(|mut values| drive_scoped_actor(&mut actor, values.next()).unwrap())
                } else {
                    drive_scoped_actor(&mut actor, child.read(|child| *child))
                };
                assert!(
                    matches!(result, Ok(7))
                        || result
                            .as_ref()
                            .is_err_and(rca_core::ScopedStoreEnded::is_scope_ended),
                    "a keyed handle must not project another collection in {phase:?}: {result:?}"
                );
            }
            let error = drive_scoped_actor(&mut actor, child.read(|child| *child)).unwrap_err();
            assert!(error.is_scope_ended());
            assert!(!error.is_store_stopped());
            assert!(!store.is_closed());
        }
    }

    #[test]
    fn stack_selection_cannot_change_collections_between_validation_and_projection() {
        for phase in [
            ProjectionPhase::Read,
            ProjectionPhase::Registration,
            ProjectionPhase::Publication,
        ] {
            let runtime = ManualRuntime::new();
            let mut first = StackState::new();
            let id = first.push(7_usize);
            let mut second = StackState::new();
            second.try_push(id.clone(), 8_usize).unwrap();
            let reducer = rca_core::Reduce::new(
                |_state: &mut [StackState<usize>; 2], _action: &StackAction<(), ()>, _| {
                    Effect::none()
                },
            );
            let store = Store::new([first, second], reducer, runtime.runtime());
            let mut actor = runtime.take_only_queued_task();
            let selection = Arc::new(Selection::default());
            let selected = Arc::clone(&selection);
            let mutable = Arc::clone(&selection);
            let child = drive_scoped_actor(
                &mut actor,
                store.scope_stack(
                    StatePath::new(
                        move |state: &[StackState<usize>; 2]| &state[selected.index()],
                        move |state: &mut [StackState<usize>; 2]| {
                            &mut state[mutable.current.load(Ordering::SeqCst)]
                        },
                    ),
                    rca_core::identity_action_path(),
                    id.clone(),
                ),
            )
            .unwrap();
            if matches!(phase, ProjectionPhase::Publication) {
                assert_collection_publication(
                    &mut actor,
                    &store,
                    &child,
                    &selection,
                    StackAction::Element { id, action: () },
                );
            } else {
                selection
                    .switch_after_selection
                    .store(true, Ordering::SeqCst);
                let result = if matches!(phase, ProjectionPhase::Registration) {
                    drive_scoped_actor(&mut actor, child.observe(|child| *child))
                        .map(|mut values| drive_scoped_actor(&mut actor, values.next()).unwrap())
                } else {
                    drive_scoped_actor(&mut actor, child.read(|child| *child))
                };
                assert!(
                    matches!(result, Ok(7))
                        || result
                            .as_ref()
                            .is_err_and(rca_core::ScopedStoreEnded::is_scope_ended),
                    "a stack handle must not project another collection in {phase:?}: {result:?}"
                );
            }
            let error = drive_scoped_actor(&mut actor, child.read(|child| *child)).unwrap_err();
            assert!(error.is_scope_ended());
            assert!(!error.is_store_stopped());
            assert!(!store.is_closed());
        }
    }
}

#[test]
fn backpressured_scoped_take_closes_and_releases_values_when_actor_is_dropped() {
    use futures_util::FutureExt as _;

    for registered in [false, true] {
        let runtime = ManualRuntime::new();
        let store = Store::builder(
            AppState {
                optional_child: Some(ChildState::default()),
                ..AppState::default()
            },
            AppReducer,
            runtime.runtime(),
        )
        .mailbox_capacity(1)
        .build()
        .unwrap();
        let mut actor = runtime.take_only_queued_task();
        let child = if registered {
            drive_scoped_actor(
                &mut actor,
                store.scope_some(optional_child_state_path(), optional_child_action_path()),
            )
            .unwrap()
        } else {
            child_handle(&store)
        };
        let owner = Arc::new(());
        let selected_owner = Arc::clone(&owner);
        let mut values = drive_scoped_actor(
            &mut actor,
            child.observe(move |state| (state.count, Arc::clone(&selected_owner))),
        )
        .unwrap();
        let initial = values.next().now_or_never().unwrap().unwrap();
        assert_eq!(initial.0, 0);
        drop(initial);

        let update = drive_scoped_actor(&mut actor, child.send(ChildAction::Increment)).unwrap();
        update.finish().now_or_never().expect("no effects remain");
        assert_eq!(Arc::strong_count(&owner), 3, "selector and buffered value");
        let filler = store.try_send(AppAction::ChangeParentOnly).unwrap();
        assert!(
            store
                .try_send(AppAction::ChangeParentOnly)
                .unwrap_err()
                .is_full()
        );
        let mut next = Box::pin(values.next());
        let mut context = Context::from_waker(std::task::Waker::noop());
        assert!(next.as_mut().poll(&mut context).is_pending());

        drop(actor);
        assert!(
            next.as_mut()
                .now_or_never()
                .expect("actor loss wakes take")
                .is_none()
        );
        drop(next);
        let _: rca_core::StoreStopped = filler
            .finish()
            .now_or_never()
            .expect("actor loss rejects queued work")
            .unwrap_err();
        assert!(
            values
                .next()
                .now_or_never()
                .expect("closure is terminal")
                .is_none()
        );
        assert_eq!(Arc::strong_count(&owner), 1);
    }
}

#[test]
fn final_registered_handle_drop_closes_buffered_observation_before_actor_cleanup() {
    use futures_util::FutureExt as _;

    let runtime = ManualRuntime::new();
    let store = Store::new(
        AppState {
            optional_child: Some(ChildState::default()),
            ..AppState::default()
        },
        AppReducer,
        runtime.runtime(),
    );
    let mut actor = runtime.take_only_queued_task();
    let child = drive_scoped_actor(
        &mut actor,
        store.scope_some(optional_child_state_path(), optional_child_action_path()),
    )
    .unwrap();
    let weak = child.downgrade();
    let owner = Arc::new(());
    let selected_owner = Arc::clone(&owner);
    let selections = Arc::new(AtomicUsize::new(0));
    let selected_count = Arc::clone(&selections);
    let mut values = drive_scoped_actor(
        &mut actor,
        child.observe(move |state| {
            selected_count.fetch_add(1, Ordering::SeqCst);
            (state.count, Arc::clone(&selected_owner))
        }),
    )
    .unwrap();
    let initial = values.next().now_or_never().unwrap().unwrap();
    assert_eq!(initial.0, 0);
    assert_eq!(selections.load(Ordering::SeqCst), 1);
    drop(initial);
    let update = drive_scoped_actor(&mut actor, child.send(ChildAction::Increment)).unwrap();
    update.finish().now_or_never().expect("no effects remain");
    assert_eq!(selections.load(Ordering::SeqCst), 2);
    assert_eq!(Arc::strong_count(&owner), 3, "selector and buffered value");

    drop(child);
    assert!(weak.upgrade().is_none());
    assert!(
        values
            .next()
            .now_or_never()
            .expect("ended handle closes take without actor progress")
            .is_none()
    );
    assert_eq!(selections.load(Ordering::SeqCst), 2);
    assert_eq!(
        Arc::strong_count(&owner),
        3,
        "actor still owns the unconsumed value"
    );
    assert!(!store.is_closed());

    let mut context = Context::from_waker(std::task::Waker::noop());
    assert!(actor.as_mut().poll(&mut context).is_pending());
    assert_eq!(selections.load(Ordering::SeqCst), 2);
    assert_eq!(
        Arc::strong_count(&owner),
        1,
        "actor cleanup releases value and selector"
    );
    assert!(
        values
            .next()
            .now_or_never()
            .expect("closure is terminal")
            .is_none()
    );
    assert_eq!(
        drive_scoped_actor(
            &mut actor,
            store.read(|state| state.optional_child.as_ref().unwrap().count)
        )
        .unwrap(),
        1,
        "ending the handle does not remove domain state"
    );
    assert_eq!(selections.load(Ordering::SeqCst), 2);
}

#[test]
fn final_registered_handle_drop_during_reduction_releases_identity_without_reselection() {
    let runtime = ManualRuntime::new();
    let owned_child = Arc::new(Mutex::new(None::<ScopedStore<ChildState, ChildAction>>));
    let reducer_child = Arc::downgrade(&owned_child);
    let reducer = rca_core::Reduce::new(
        move |state: &mut AppState, action: &AppAction, context: &mut ReduceContext<'_>| {
            if matches!(action, AppAction::ChangeChildFromParent) {
                let child = reducer_child.upgrade().unwrap().lock().unwrap().take();
                drop(child);
            }
            AppReducer.reduce(state, action, context)
        },
    );
    let store = Store::new(AppState::default(), reducer, runtime.runtime());
    let mut actor = runtime.take_only_queued_task();
    let identity_calls = Arc::new(AtomicUsize::new(0));
    let select_identity = Arc::clone(&identity_calls);
    let child = drive_scoped_actor(
        &mut actor,
        store.scope_identified(child_state_path(), child_action_path(), move |state| {
            select_identity.fetch_add(1, Ordering::SeqCst);
            &state.id
        }),
    )
    .unwrap();
    let weak = child.downgrade();
    *owned_child.lock().unwrap() = Some(child);
    assert_eq!(identity_calls.load(Ordering::SeqCst), 1);
    assert_eq!(Arc::strong_count(&identity_calls), 2);

    // Release after control cleanup, but before post-reduction handle refresh.
    let task =
        drive_scoped_actor(&mut actor, store.send(AppAction::ChangeChildFromParent)).unwrap();
    assert!(task.is_finished());
    assert!(owned_child.lock().unwrap().is_none());
    assert!(weak.upgrade().is_none());
    assert_eq!(identity_calls.load(Ordering::SeqCst), 1);
    assert_eq!(Arc::strong_count(&identity_calls), 1);
    assert!(!store.is_closed());
    assert_eq!(
        drive_scoped_actor(
            &mut actor,
            store.read(|state| (state.child.id, state.child.count))
        )
        .unwrap(),
        (0, 1)
    );

    let task =
        drive_scoped_actor(&mut actor, store.send(AppAction::ChangeChildFromParent)).unwrap();
    assert!(task.is_finished());
    assert_eq!(
        drive_scoped_actor(&mut actor, store.read(|state| state.child.count)).unwrap(),
        2
    );
    assert_eq!(identity_calls.load(Ordering::SeqCst), 1);
    drop(store);
    assert!(
        actor
            .as_mut()
            .poll(&mut Context::from_waker(std::task::Waker::noop()))
            .is_ready()
    );
    assert_eq!(runtime.spawn_count(), 1);
    assert_eq!(Arc::weak_count(&owned_child), 0);
}

#[test]
fn admitted_scoped_take_closes_and_releases_values_when_actor_loses_reply() {
    use futures_util::FutureExt as _;

    for registered in [false, true] {
        let runtime = ManualRuntime::new();
        let store = Store::builder(
            AppState {
                optional_child: Some(ChildState::default()),
                ..AppState::default()
            },
            AppReducer,
            runtime.runtime(),
        )
        .mailbox_capacity(1)
        .build()
        .unwrap();
        let mut actor = runtime.take_only_queued_task();
        let child = if registered {
            drive_scoped_actor(
                &mut actor,
                store.scope_some(optional_child_state_path(), optional_child_action_path()),
            )
            .unwrap()
        } else {
            child_handle(&store)
        };
        let owner = Arc::new(());
        let selected_owner = Arc::clone(&owner);
        let selections = Arc::new(AtomicUsize::new(0));
        let selected_count = Arc::clone(&selections);
        let mut values = drive_scoped_actor(
            &mut actor,
            child.observe(move |state| {
                selected_count.fetch_add(1, Ordering::SeqCst);
                (state.count, Arc::clone(&selected_owner))
            }),
        )
        .unwrap();
        let initial = values.next().now_or_never().unwrap().unwrap();
        assert_eq!(initial.0, 0);
        assert_eq!(selections.load(Ordering::SeqCst), 1);
        drop(initial);
        let update = drive_scoped_actor(&mut actor, child.send(ChildAction::Increment)).unwrap();
        update.finish().now_or_never().expect("no effects remain");
        assert_eq!(selections.load(Ordering::SeqCst), 2);
        assert_eq!(Arc::strong_count(&owner), 3);

        // The actor drained the update. Only this take can now occupy the mailbox.
        let mut next = Box::pin(values.next());
        let mut context = Context::from_waker(std::task::Waker::noop());
        assert!(next.as_mut().poll(&mut context).is_pending());
        assert!(
            store
                .try_send(AppAction::ChangeParentOnly)
                .unwrap_err()
                .is_full()
        );
        drop(actor);
        assert!(
            next.as_mut()
                .now_or_never()
                .expect("lost reply closes admitted take")
                .is_none()
        );
        assert_eq!(selections.load(Ordering::SeqCst), 2);
        drop(next);
        assert_eq!(Arc::strong_count(&owner), 1);
        assert!(
            values
                .next()
                .now_or_never()
                .expect("closure is terminal")
                .is_none()
        );
        assert_eq!(selections.load(Ordering::SeqCst), 2);
        assert!(store.is_closed());
    }
}

#[test]
fn actor_loss_rejects_capacity_waiters_without_running_scoped_callbacks() {
    let runtime = ManualRuntime::new();
    let store = Store::builder(AppState::default(), AppReducer, runtime.runtime())
        .mailbox_capacity(1)
        .build()
        .unwrap();
    let mut actor = runtime.take_only_queued_task();
    let embeddings = Arc::new(AtomicUsize::new(0));
    let embedded = Arc::clone(&embeddings);
    let action_path = ActionPath::new(
        |action: &AppAction| match action {
            AppAction::Child(child) => Some(child),
            _ => None,
        },
        move |action| {
            embedded.fetch_add(1, Ordering::SeqCst);
            AppAction::Child(action)
        },
    );
    let child = drive_scoped_actor(
        &mut actor,
        store.scope_identified(child_state_path(), action_path, |state| &state.id),
    )
    .unwrap();
    let queued = store.try_send(AppAction::ChangeParentOnly).unwrap();
    let mut context = Context::from_waker(std::task::Waker::noop());
    let mut send = Box::pin(child.send(ChildAction::Increment));
    assert!(send.as_mut().poll(&mut context).is_pending());

    let calls = Arc::new(AtomicUsize::new(0));
    let capture = Arc::new(());
    let read_calls = Arc::clone(&calls);
    let read_capture = Arc::clone(&capture);
    let mut read = Box::pin(child.read(move |state| {
        std::hint::black_box(&read_capture);
        read_calls.fetch_add(1, Ordering::SeqCst);
        state.count
    }));
    assert!(read.as_mut().poll(&mut context).is_pending());
    let mut shutdown = Box::pin(store.clone().shutdown_into_state());
    assert!(shutdown.as_mut().poll(&mut context).is_pending());
    assert_eq!(Arc::strong_count(&capture), 2);

    // The unpolled actor cannot admit any waiter before its mailbox closes.
    drop(actor);
    let Poll::Ready(Err(send_error)) = send.as_mut().poll(&mut context) else {
        panic!("closed ingress must reject the waiting scoped send");
    };
    let Poll::Ready(Err(read_error)) = read.as_mut().poll(&mut context) else {
        panic!("closed ingress must reject the waiting scoped read");
    };
    assert!(matches!(
        shutdown.as_mut().poll(&mut context),
        Poll::Ready(Err(_))
    ));
    for error in [send_error, read_error] {
        assert!(error.is_store_stopped());
        assert!(!error.is_scope_ended());
    }
    assert!(store.is_closed());
    assert_eq!(embeddings.load(Ordering::SeqCst), 0);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(Arc::strong_count(&capture), 1);
    let mut abandoned = std::pin::pin!(queued.finish());
    assert!(matches!(
        abandoned.as_mut().poll(&mut context),
        Poll::Ready(Err(_))
    ));
}

#[test]
fn queued_root_shutdown_rejects_admitted_scoped_operations_without_callbacks() {
    let runtime = ManualRuntime::new();
    let store = Store::builder(
        AppState {
            child: ChildState {
                id: 7,
                count: 3,
                ..ChildState::default()
            },
            ..AppState::default()
        },
        AppReducer,
        runtime.runtime(),
    )
    .mailbox_capacity(4)
    .build()
    .unwrap();
    let mut actor = runtime.take_only_queued_task();
    let embeddings = Arc::new(AtomicUsize::new(0));
    let embedded = Arc::clone(&embeddings);
    let action_path = ActionPath::new(
        |action: &AppAction| match action {
            AppAction::Child(child) => Some(child),
            _ => None,
        },
        move |action| {
            embedded.fetch_add(1, Ordering::SeqCst);
            AppAction::Child(action)
        },
    );
    let child = drive_scoped_actor(
        &mut actor,
        store.scope_identified(child_state_path(), action_path, |state| &state.id),
    )
    .unwrap();
    assert_eq!(
        drive_scoped_actor(&mut actor, child.read(|state| state.count)).unwrap(),
        3
    );

    let mut context = Context::from_waker(std::task::Waker::noop());
    let mut shutdown = Box::pin(store.clone().shutdown_into_state());
    assert!(shutdown.as_mut().poll(&mut context).is_pending());
    let mut send = Box::pin(child.send(ChildAction::Add(9)));
    assert!(send.as_mut().poll(&mut context).is_pending());

    let calls = Arc::new(AtomicUsize::new(0));
    let capture = Arc::new(());
    let read_calls = Arc::clone(&calls);
    let read_capture = Arc::clone(&capture);
    let mut read = Box::pin(child.read(move |state| {
        std::hint::black_box(&read_capture);
        read_calls.fetch_add(1, Ordering::SeqCst);
        state.count
    }));
    assert!(read.as_mut().poll(&mut context).is_pending());
    let observation_calls = Arc::clone(&calls);
    let observation_capture = Arc::clone(&capture);
    let mut observation = Box::pin(child.observe(move |state| {
        std::hint::black_box(&observation_capture);
        observation_calls.fetch_add(1, Ordering::SeqCst);
        state.count
    }));
    assert!(observation.as_mut().poll(&mut context).is_pending());
    assert!(
        store
            .try_send(AppAction::ChangeParentOnly)
            .unwrap_err()
            .is_full()
    );
    assert_eq!(Arc::strong_count(&capture), 3);

    assert!(actor.as_mut().poll(&mut context).is_ready());
    let Poll::Ready(Ok(state)) = shutdown.as_mut().poll(&mut context) else {
        panic!("the terminal command must return its unchanged owned state");
    };
    let Poll::Ready(Err(send_error)) = send.as_mut().poll(&mut context) else {
        panic!("admitted send must receive root-closure rejection");
    };
    let Poll::Ready(Err(read_error)) = read.as_mut().poll(&mut context) else {
        panic!("admitted read must receive root-closure rejection");
    };
    let Poll::Ready(Err(observation_error)) = observation.as_mut().poll(&mut context) else {
        panic!("admitted observation must receive root-closure rejection");
    };
    for error in [send_error, read_error, observation_error] {
        assert!(error.is_store_stopped());
        assert!(!error.is_scope_ended());
        assert!(
            std::error::Error::source(&error)
                .is_some_and(<dyn std::error::Error>::is::<rca_core::StoreStopped>)
        );
    }
    assert!(store.is_closed());
    assert_eq!((state.child.id, state.child.count), (7, 3));
    assert_eq!(embeddings.load(Ordering::SeqCst), 0);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(Arc::strong_count(&capture), 1);
}

#[test]
fn queued_root_shutdown_discards_owned_scoped_send_before_embedding() {
    let runtime = ManualRuntime::new();
    let store = Store::builder(
        AppState {
            child: ChildState {
                id: 7,
                count: 3,
                ..ChildState::default()
            },
            ..AppState::default()
        },
        AppReducer,
        runtime.runtime(),
    )
    .mailbox_capacity(2)
    .build()
    .unwrap();
    let mut actor = runtime.take_only_queued_task();
    let embeddings = Arc::new(AtomicUsize::new(0));
    let embedded = Arc::clone(&embeddings);
    let child = store.scope(
        child_state_path(),
        ActionPath::new(
            |action: &AppAction| match action {
                AppAction::Child(child) => Some(child),
                _ => None,
            },
            move |action| {
                embedded.fetch_add(1, Ordering::SeqCst);
                AppAction::Child(action)
            },
        ),
    );
    let calls = Arc::new(AtomicUsize::new(0));
    let predicate_calls = Arc::clone(&calls);
    let capture = Arc::new(());
    let predicate_capture = Arc::clone(&capture);
    let mut wait = child.send_and_wait_while(ChildAction::Add(9), move |state| {
        std::hint::black_box(&predicate_capture);
        predicate_calls.fetch_add(1, Ordering::SeqCst);
        state.count < 12
    });
    let mut context = Context::from_waker(std::task::Waker::noop());
    let mut shutdown = Box::pin(store.clone().shutdown_into_state());
    assert!(shutdown.as_mut().poll(&mut context).is_pending());
    assert!(Pin::new(&mut wait).poll(&mut context).is_pending());
    // Both slots are occupied before the actor runs: shutdown, then owned send.
    assert!(
        store
            .try_send(AppAction::ChangeParentOnly)
            .unwrap_err()
            .is_full()
    );
    assert!(!store.is_closed());
    assert_eq!(embeddings.load(Ordering::SeqCst), 0);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(Arc::strong_count(&capture), 2);

    assert!(actor.as_mut().poll(&mut context).is_ready());
    let Poll::Ready(Ok(state)) = shutdown.as_mut().poll(&mut context) else {
        panic!("shutdown must return state without committing the later owned send");
    };
    assert_eq!(Arc::strong_count(&capture), 2);
    assert_eq!(
        Pin::new(&mut wait).poll(&mut context),
        Poll::Ready(rca_core::PredicateWaitOutcome::StoreStopped)
    );
    assert!(store.is_closed());
    assert_eq!((state.child.id, state.child.count), (7, 3));
    assert_eq!(state.child.commits, [] as [usize; 0]);
    assert!(state.optional_child.is_none());
    assert_eq!(state.optional_reductions, 0);
    assert_eq!(embeddings.load(Ordering::SeqCst), 0);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(Arc::strong_count(&capture), 1);
}

#[test]
fn required_identified_scope_projects_only_its_registered_lifetime() {
    let runtime = ManualRuntime::new();
    let store = Store::new(
        AppState {
            child: ChildState {
                id: 7,
                count: 3,
                ..ChildState::default()
            },
            ..AppState::default()
        },
        AppReducer,
        runtime.runtime(),
    );
    let mut actor = runtime.take_only_queued_task();
    let child = drive_scoped_actor(
        &mut actor,
        store.scope_identified(child_state_path(), child_action_path(), |child| &child.id),
    )
    .unwrap();
    assert_eq!(
        drive_scoped_actor(&mut actor, child.read(|state| (state.id, state.count))).unwrap(),
        (7, 3)
    );
    let task = drive_scoped_actor(&mut actor, child.send(ChildAction::Add(2))).unwrap();
    assert!(task.is_finished());
    assert_eq!(
        drive_scoped_actor(&mut actor, child.read(|state| (state.id, state.count))).unwrap(),
        (7, 5)
    );

    let task = drive_scoped_actor(
        &mut actor,
        store.send(AppAction::ReplaceRequiredChild { id: 8, count: 40 }),
    )
    .unwrap();
    assert!(task.is_finished());
    let reads = Arc::new(AtomicUsize::new(0));
    let read_calls = Arc::clone(&reads);
    let error = drive_scoped_actor(
        &mut actor,
        child.read(move |state| {
            read_calls.fetch_add(1, Ordering::SeqCst);
            state.count
        }),
    )
    .unwrap_err();
    assert!(error.is_scope_ended());
    assert!(!error.is_store_stopped());
    let error = drive_scoped_actor(&mut actor, child.send(ChildAction::Add(100))).unwrap_err();
    assert!(error.is_scope_ended());
    assert!(!error.is_store_stopped());
    assert_eq!(reads.load(Ordering::SeqCst), 0);
    assert_eq!(
        drive_scoped_actor(&mut actor, store.read(|state| state.child.count)).unwrap(),
        40
    );

    let replacement = drive_scoped_actor(
        &mut actor,
        store.scope_identified(child_state_path(), child_action_path(), |child| &child.id),
    )
    .unwrap();
    assert_eq!(
        drive_scoped_actor(
            &mut actor,
            replacement.read(|state| (state.id, state.count))
        )
        .unwrap(),
        (8, 40)
    );
    let task = drive_scoped_actor(&mut actor, replacement.send(ChildAction::Add(2))).unwrap();
    assert!(task.is_finished());
    assert_eq!(
        drive_scoped_actor(
            &mut actor,
            store.read(|state| { (state.child.id, state.child.count, state.optional_reductions) })
        )
        .unwrap(),
        (8, 42, 0)
    );
}

// Catches retargeting an identified handle when shared state changes its path
// before a reducer commit can refresh registered lifetimes.
#[test]
fn identified_scope_rejects_external_selection_change_before_commit() {
    struct SelectionState {
        selected: Arc<AtomicUsize>,
        children: [ChildState; 2],
    }

    let runtime = ManualRuntime::new();
    let selected = Arc::new(AtomicUsize::new(0));
    let state = SelectionState {
        selected: Arc::clone(&selected),
        children: [
            ChildState {
                id: 7,
                count: 3,
                ..ChildState::default()
            },
            ChildState {
                id: 8,
                count: 40,
                ..ChildState::default()
            },
        ],
    };
    let reducer = rca_core::Reduce::new(|state: &mut SelectionState, amount: &usize, _| {
        state.children[state.selected.load(Ordering::SeqCst)].count += amount;
        Effect::none()
    });
    let path = || {
        StatePath::new(
            |state: &SelectionState| &state.children[state.selected.load(Ordering::SeqCst)],
            |state: &mut SelectionState| {
                let selected = state.selected.load(Ordering::SeqCst);
                &mut state.children[selected]
            },
        )
    };
    let store = Store::new(state, reducer, runtime.runtime());
    let mut actor = runtime.take_only_queued_task();
    let child = drive_scoped_actor(
        &mut actor,
        store.scope_identified(path(), rca_core::identity_action_path::<usize>(), |child| {
            &child.id
        }),
    )
    .unwrap();
    assert_eq!(
        drive_scoped_actor(&mut actor, child.read(|state| (state.id, state.count))).unwrap(),
        (7, 3)
    );

    // No action or actor poll occurs between changing the path and the stale read.
    selected.store(1, Ordering::SeqCst);
    let calls = Arc::new(AtomicUsize::new(0));
    let read_calls = Arc::clone(&calls);
    let capture = Arc::new(());
    let released = Arc::downgrade(&capture);
    let read = child.read(move |state| {
        drop(capture);
        read_calls.fetch_add(1, Ordering::SeqCst);
        state.count
    });
    assert!(released.upgrade().is_some());
    let error = drive_scoped_actor(&mut actor, read).unwrap_err();
    assert!(error.is_scope_ended());
    assert!(!error.is_store_stopped());
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert!(released.upgrade().is_none());
    let error = drive_scoped_actor(&mut actor, child.send(100)).unwrap_err();
    assert!(error.is_scope_ended());
    assert!(!error.is_store_stopped());

    let replacement = drive_scoped_actor(
        &mut actor,
        store.scope_identified(path(), rca_core::identity_action_path::<usize>(), |child| {
            &child.id
        }),
    )
    .unwrap();
    assert_eq!(
        drive_scoped_actor(
            &mut actor,
            replacement.read(|state| (state.id, state.count))
        )
        .unwrap(),
        (8, 40)
    );
    let task = drive_scoped_actor(&mut actor, replacement.send(2)).unwrap();
    assert!(task.is_finished());
    assert_eq!(
        drive_scoped_actor(
            &mut actor,
            store.read(|state| (state.children[0].count, state.children[1].count))
        )
        .unwrap(),
        (3, 42)
    );

    drop((child, replacement, store));
    assert!(
        actor
            .as_mut()
            .poll(&mut Context::from_waker(std::task::Waker::noop()))
            .is_ready()
    );
    drop(actor);
    assert_eq!(Arc::strong_count(&selected), 1);
}

#[cfg(feature = "test-util")]
#[test]
fn registration_cancelled_during_selection_releases_its_capture() {
    struct CaptureDrop(Arc<AtomicUsize>);
    impl Drop for CaptureDrop {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    let runtime = ManualRuntime::new();
    let store = Store::new(
        AppState {
            optional_child: Some(ChildState::default()),
            ..AppState::default()
        },
        AppReducer,
        runtime.runtime(),
    );
    let mut actor = runtime.take_only_queued_task();
    let gate = Arc::new(Gate::default());
    let release = ReleaseGate(Arc::clone(&gate));
    let selections = Arc::new(AtomicUsize::new(0));
    let selected = Arc::clone(&selections);
    let drops = Arc::new(AtomicUsize::new(0));
    let capture = CaptureDrop(Arc::clone(&drops));
    let (started, selection_started) = mpsc::sync_channel(1);
    let path = OptionalStatePath::new(
        move |state: &AppState| {
            std::hint::black_box(&capture);
            selected.fetch_add(1, Ordering::SeqCst);
            started
                .try_send(())
                .expect("selector entry must be observed");
            gate.wait();
            state.optional_child.as_ref()
        },
        |state: &mut AppState| state.optional_child.as_mut(),
    );
    let mut registration = Box::pin(store.scope_some(path, optional_child_action_path()));
    let mut context = Context::from_waker(std::task::Waker::noop());
    assert!(registration.as_mut().poll(&mut context).is_pending());
    let (completed, actor_completed) = mpsc::sync_channel(1);
    let worker = std::thread::spawn(move || {
        let mut context = Context::from_waker(std::task::Waker::noop());
        assert!(actor.as_mut().poll(&mut context).is_pending());
        completed
            .send(())
            .expect("actor completion must be observed");
        actor
    });
    let entered = selection_started.recv_timeout(Duration::from_secs(5));
    let selections_before_release = selections.load(Ordering::SeqCst);
    drop(registration);
    let drops_before_release = drops.load(Ordering::SeqCst);
    // Release and join before propagating a timeout or a phase assertion.
    drop(release);
    let completed = actor_completed.recv_timeout(Duration::from_secs(5));
    let mut actor = worker.join().expect("registration actor poll must finish");
    entered.expect("actor must enter registration selection");
    assert_eq!(selections_before_release, 1);
    assert_eq!(drops_before_release, 0);
    completed.expect("cancelled registration must let the actor return");
    assert_eq!(selections.load(Ordering::SeqCst), 1);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    let diagnostics = drive_scoped_actor(&mut actor, store.runtime_diagnostics()).unwrap();
    assert_eq!(diagnostics.handle_registration_count(), 0);

    let child = drive_scoped_actor(
        &mut actor,
        store.scope_some(optional_child_state_path(), optional_child_action_path()),
    )
    .unwrap();
    let diagnostics = drive_scoped_actor(&mut actor, store.runtime_diagnostics()).unwrap();
    assert_eq!(diagnostics.handle_registration_count(), 1);
    let task = drive_scoped_actor(&mut actor, child.send(ChildAction::Increment)).unwrap();
    assert!(task.is_finished());
    assert_eq!(
        drive_scoped_actor(
            &mut actor,
            store.read(|state| {
                (
                    state.optional_child.as_ref().unwrap().count,
                    state.optional_reductions,
                )
            })
        )
        .unwrap(),
        (1, 1)
    );
    drop(child);
    let diagnostics = drive_scoped_actor(&mut actor, store.runtime_diagnostics()).unwrap();
    assert_eq!(diagnostics.handle_registration_count(), 0);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
}

#[test]
fn required_scoped_store_hides_parent_and_routes_to_root() {
    let fixture = scoped_fixture();
    fixture.block_on(async {
        let store = fixture.store.clone();
        let child = child_handle(&store);
        child
            .send(ChildAction::Increment)
            .await
            .expect("child action commits")
            .finish()
            .await;
        assert_eq!(
            child.read(|state| state.count).await.expect("child read"),
            1
        );
        assert_eq!(
            store
                .read(|state| state.child.count)
                .await
                .expect("root read"),
            1
        );
    });
}

#[test]
fn scoped_store_handles_and_futures_cross_threads_safely() {
    assert_send_type::<ScopedStore<ChildState, ChildAction>>();
    assert_sync_type::<ScopedStore<ChildState, ChildAction>>();

    let fixture = scoped_fixture();
    let child = child_handle(&fixture.store);
    assert_send(child.send(ChildAction::Increment));
    assert_send(child.read(|state| state.count));
    assert_send(child.observe(|state| state.count));
    assert_send(child.observe_distinct(|state| state.count));
}

#[test]
fn scoped_try_send_recovers_action_when_root_mailbox_is_full() {
    let runtime = ManualRuntime::new();
    let store = Store::builder(AppState::default(), AppReducer, runtime.runtime())
        .mailbox_capacity(1)
        .build()
        .expect("valid capacity");
    let child = child_handle(&store);

    let first = child
        .try_send(ChildAction::Increment)
        .expect("first action fits");
    let error = child
        .try_send(ChildAction::Add(9))
        .expect_err("second action must be recovered when full");
    assert!(error.is_full());
    assert!(matches!(error.into_action(), ChildAction::Add(9)));

    runtime.start_all();
    futures_executor::block_on(async {
        first
            .finish()
            .await
            .expect("first action commits")
            .finish()
            .await;
    });
    drop(child);
    drop(store);
}

#[test]
fn scoped_read_cancellation_does_not_enqueue_a_query() {
    let runtime = ManualRuntime::new();
    let store = Store::builder(AppState::default(), AppReducer, runtime.runtime())
        .mailbox_capacity(1)
        .build()
        .expect("valid capacity");
    let child = child_handle(&store);
    let action = store
        .try_send(AppAction::Child(ChildAction::Increment))
        .expect("fills queue");

    let selections = Arc::new(AtomicUsize::new(0));
    let counted_selections = Arc::clone(&selections);
    let mut read = Box::pin(child.read(move |state| {
        counted_selections.fetch_add(1, Ordering::AcqRel);
        state.count
    }));
    let waker = futures_util::task::noop_waker();
    let mut context = Context::from_waker(&waker);
    assert!(matches!(
        Pin::as_mut(&mut read).poll(&mut context),
        Poll::Pending
    ));
    drop(read);

    runtime.start_all();
    futures_executor::block_on(async {
        action.await.expect("queued action commits").finish().await;
        assert_eq!(
            store
                .read(|state| state.child.count)
                .await
                .expect("root read"),
            1
        );
    });
    assert_eq!(
        selections.load(Ordering::Acquire),
        0,
        "cancelled pre-admission selector must not run"
    );
    drop(child);
    drop(store);
}

#[test]
fn scoped_handle_keeps_root_authority_after_original_store_drop() {
    let runtime = ManualRuntime::new();
    let store = Store::new(AppState::default(), AppReducer, runtime.runtime());
    let closed = store.closed();
    let child = child_handle(&store);

    runtime.start_all();
    drop(store);
    futures_executor::block_on(async {
        child
            .send(ChildAction::Increment)
            .await
            .expect("scoped handle retains root authority")
            .finish()
            .await;
        assert_eq!(
            child.read(|state| state.count).await.expect("scoped read"),
            1
        );
    });
    drop(child);
    futures_executor::block_on(closed);
}

#[test]
fn root_shutdown_reports_scoped_store_stopped() {
    let fixture = scoped_fixture();
    fixture.block_on(async {
        let child = child_handle(&fixture.store);
        fixture.store.shutdown().await.expect("clean shutdown");

        let error = child
            .send(ChildAction::Increment)
            .await
            .expect_err("closed root rejects scoped action");
        assert!(error.is_store_stopped());
        assert!(!error.is_scope_ended());

        assert_eq!(
            error.to_string().lines().next(),
            Some("root store stopped before scoped work completed")
        );
        let display = error.to_string();
        assert!(display.contains(&format!("\n{}\n", error.backtrace())));
        assert!(format!("{error:?}").contains("kind: StoreStopped("));
        let cause = std::error::Error::source(&error)
            .unwrap()
            .downcast_ref::<rca_core::StoreStopped>()
            .expect("root closure retains its StoreStopped cause");
        assert_eq!(
            cause.to_string().lines().next(),
            Some("store operation ended before successful completion")
        );
        assert!(display.ends_with(&cause.to_string()));
        assert!(std::error::Error::source(cause).is_none());

        let error = child
            .try_send(ChildAction::Add(9))
            .expect_err("closed root rejects scoped immediate admission");
        assert!(error.is_store_stopped());
        assert!(matches!(error.into_action(), ChildAction::Add(9)));
    });
}

#[test]
#[cfg_attr(
    miri,
    ignore = "panic verification launches a subprocess, which Miri isolation forbids"
)]
fn scoped_selector_panic_tears_down_root_authority() {
    #[cfg(not(miri))]
    if subprocess::run_in_child("scoped_selector_panic_tears_down_root_authority") {
        return;
    }

    let runtime = ManualRuntime::new();
    let store = Store::new(AppState::default(), AppReducer, runtime.runtime());
    let child = child_handle(&store);
    let closed = store.closed();

    let mut selector =
        Box::pin(child.observe(|_state| -> usize { panic!("intentional scoped selector panic") }));
    let mut root_read = Box::pin(store.read(|state| state.child.count));
    let waker = futures_util::task::noop_waker();
    let mut context = Context::from_waker(&waker);
    assert!(matches!(
        Pin::as_mut(&mut selector).poll(&mut context),
        Poll::Pending
    ));
    assert!(matches!(
        Pin::as_mut(&mut root_read).poll(&mut context),
        Poll::Pending
    ));

    runtime.start_all();
    assert!(
        futures_executor::block_on(selector).is_err(),
        "selector panic closes scoped observation registration"
    );
    futures_executor::block_on(root_read).expect_err("selector panic closes queued root read");
    futures_executor::block_on(closed);
    assert!(store.is_closed());

    let scoped_error = child
        .try_send(ChildAction::Increment)
        .expect_err("closed root rejects later scoped action");
    assert!(scoped_error.is_store_stopped());
    assert!(matches!(scoped_error.into_action(), ChildAction::Increment));
    futures_executor::block_on(store.send(AppAction::Child(ChildAction::Increment)))
        .expect_err("closed root rejects later root action");

    let mut joined = runtime.join_all();
    assert_eq!(joined.len(), 1, "only the actor was started");
    let panic = joined
        .pop()
        .expect("actor thread must exist")
        .expect_err("actor thread must propagate selector panic");
    assert_eq!(
        panic.downcast_ref::<&str>().copied(),
        Some("intentional scoped selector panic")
    );
}

#[test]
fn cloned_scoped_handles_commit_unique_actions_in_admission_order() {
    let runtime = thread_runtime::ThreadRuntime::new();
    let store = Store::new(AppState::default(), AppReducer, runtime.runtime());
    let child = child_handle(&store);

    let accepted = Arc::new(Mutex::new(Vec::new()));
    std::thread::scope(|scope| {
        for id in 0..32 {
            let child = child.clone();
            let accepted = Arc::clone(&accepted);
            scope.spawn(move || {
                let mut accepted = accepted.lock().expect("accepted receipts lock");
                let receipt = child
                    .try_send(ChildAction::Record(id))
                    .expect("concurrent child action fits");
                accepted.push((id, receipt));
            });
        }
    });
    let accepted = Arc::try_unwrap(accepted)
        .expect("workers released accepted receipts")
        .into_inner()
        .expect("accepted receipts lock");
    let accepted_ids: Vec<_> = accepted.iter().map(|(id, _)| *id).collect();
    futures_executor::block_on(async {
        for (_, receipt) in accepted {
            receipt
                .finish()
                .await
                .expect("accepted child action commits")
                .finish()
                .await;
        }
    });
    assert_eq!(
        futures_executor::block_on(store.read(|state| state.child.commits.clone()))
            .expect("root commit order"),
        accepted_ids
    );
    futures_executor::block_on(store.shutdown()).expect("clean shutdown");
    drop(child);
    drop(store);
    runtime.join_all();
}

#[test]
fn ended_optional_child_observation_rejects_without_actor_or_selector() {
    /// Counts release of the rejected selector's owned capture.
    struct CaptureDrop(Arc<AtomicUsize>);

    impl Drop for CaptureDrop {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    let runtime = ManualRuntime::new();
    let store = Store::new(
        AppState {
            optional_child: Some(ChildState::default()),
            ..AppState::default()
        },
        AppReducer,
        runtime.runtime(),
    );
    let mut actor = runtime.take_only_queued_task();
    let child = drive_scoped_actor(
        &mut actor,
        store.scope_some(optional_child_state_path(), optional_child_action_path()),
    )
    .unwrap();
    assert!(
        drive_scoped_actor(&mut actor, store.send(AppAction::RemoveChild))
            .unwrap()
            .is_finished()
    );
    assert!(
        drive_scoped_actor(&mut actor, store.send(AppAction::InsertChild))
            .unwrap()
            .is_finished()
    );

    let drops = Arc::new(AtomicUsize::new(0));
    let capture = CaptureDrop(Arc::clone(&drops));
    let calls = Arc::new(AtomicUsize::new(0));
    let selected = Arc::clone(&calls);
    let mut registration = Box::pin(child.observe(move |state| {
        std::hint::black_box(&capture);
        selected.fetch_add(1, Ordering::SeqCst);
        state.count
    }));
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    let mut context = Context::from_waker(std::task::Waker::noop());
    // The retained handle is already ended. No actor poll admits this selector.
    let Poll::Ready(Err(error)) = registration.as_mut().poll(&mut context) else {
        panic!("an ended child must reject observation immediately");
    };
    assert!(error.is_scope_ended());
    assert!(!error.is_store_stopped());
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    drop(registration);

    let replacement = drive_scoped_actor(
        &mut actor,
        store.scope_some(optional_child_state_path(), optional_child_action_path()),
    )
    .unwrap();
    let mut values =
        drive_scoped_actor(&mut actor, replacement.observe(|state| state.count)).unwrap();
    assert_eq!(drive_scoped_actor(&mut actor, values.next()), Some(0));
    assert!(
        drive_scoped_actor(&mut actor, replacement.send(ChildAction::Increment))
            .unwrap()
            .is_finished()
    );
    assert_eq!(drive_scoped_actor(&mut actor, values.next()), Some(1));
    assert_eq!(
        drive_scoped_actor(
            &mut actor,
            store.read(|state| { state.optional_child.as_ref().map(|child| child.count) })
        )
        .unwrap(),
        Some(1)
    );
    assert!(!store.is_closed());
    drop((values, replacement, child));
    drop(store);
    assert!(actor.as_mut().poll(&mut context).is_ready());
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
}

#[test]
fn ended_optional_child_try_send_recovers_payload_before_embedding() {
    let runtime = ManualRuntime::new();
    let reducer = rca_core::Reduce::new(
        |state: &mut Option<usize>, action: &Option<Arc<usize>>, _| {
            if let Some(amount) = action {
                *state.as_mut().expect("only live handles may reduce") += **amount;
            } else {
                *state = None;
            }
            Effect::none()
        },
    );
    let store = Store::new(Some(0), reducer, runtime.runtime());
    let mut actor = runtime.take_only_queued_task();
    let embeddings = Arc::new(AtomicUsize::new(0));
    let embedded = Arc::clone(&embeddings);
    let child = drive_scoped_actor(
        &mut actor,
        store.scope_some(
            OptionalStatePath::new(
                |state: &Option<usize>| state.as_ref(),
                |state: &mut Option<usize>| state.as_mut(),
            ),
            ActionPath::new(
                |action: &Option<Arc<usize>>| action.as_ref(),
                move |action| {
                    embedded.fetch_add(1, Ordering::SeqCst);
                    Some(action)
                },
            ),
        ),
    )
    .unwrap();
    assert!(
        drive_scoped_actor(&mut actor, store.send(None))
            .unwrap()
            .is_finished()
    );

    let payload = Arc::new(11);
    let error = child
        .try_send(Arc::clone(&payload))
        .expect_err("an ended child must reject before mailbox admission");
    assert!(error.is_scope_ended());
    assert!(!error.is_full());
    assert!(!error.is_store_stopped());
    let recovered = error.into_action();
    assert!(Arc::ptr_eq(&recovered, &payload));
    assert_eq!(Arc::strong_count(&payload), 2);
    drop(recovered);
    assert_eq!(Arc::strong_count(&payload), 1);
    assert_eq!(embeddings.load(Ordering::SeqCst), 0);
    assert_eq!(
        drive_scoped_actor(&mut actor, store.read(|state| *state)).unwrap(),
        None
    );

    drop(child);
    drop(store);
    let mut context = Context::from_waker(std::task::Waker::noop());
    assert!(actor.as_mut().poll(&mut context).is_ready());
    assert_eq!(Arc::strong_count(&payload), 1);
}

#[test]
fn queued_optional_removal_rejects_admitted_handle_before_embedding() {
    let runtime = ManualRuntime::new();
    let reducer = rca_core::Reduce::new(
        |state: &mut Option<usize>, action: &Option<Arc<usize>>, _| {
            if let Some(amount) = action {
                *state.as_mut().expect("only live handles may reduce") += **amount;
            } else {
                *state = None;
            }
            Effect::none()
        },
    );
    let store = Store::new(Some(0), reducer, runtime.runtime());
    let mut actor = runtime.take_only_queued_task();
    let embeddings = Arc::new(AtomicUsize::new(0));
    let embedded = Arc::clone(&embeddings);
    let child = drive_scoped_actor(
        &mut actor,
        store.scope_some(
            OptionalStatePath::new(
                |state: &Option<usize>| state.as_ref(),
                |state: &mut Option<usize>| state.as_mut(),
            ),
            ActionPath::new(
                |action: &Option<Arc<usize>>| action.as_ref(),
                move |action| {
                    embedded.fetch_add(1, Ordering::SeqCst);
                    Some(action)
                },
            ),
        ),
    )
    .unwrap();
    assert!(
        drive_scoped_actor(&mut actor, child.send(Arc::new(7)))
            .unwrap()
            .is_finished()
    );
    assert_eq!(
        drive_scoped_actor(&mut actor, store.read(|s| *s)).unwrap(),
        Some(7)
    );
    assert_eq!(embeddings.load(Ordering::SeqCst), 1);

    let removal = store.try_send(None).unwrap();
    let payload = Arc::new(11);
    let mut send = Box::pin(child.send(Arc::clone(&payload)));
    let mut context = Context::from_waker(std::task::Waker::noop());
    assert!(send.as_mut().poll(&mut context).is_pending());
    assert_eq!(Arc::strong_count(&payload), 2);
    assert_eq!(embeddings.load(Ordering::SeqCst), 1);
    assert!(actor.as_mut().poll(&mut context).is_pending());
    let mut removal = std::pin::pin!(removal.finish());
    assert!(matches!(
        removal.as_mut().poll(&mut context),
        Poll::Ready(Ok(_))
    ));
    let Poll::Ready(Err(error)) = send.as_mut().poll(&mut context) else {
        panic!("already-admitted action must reject after the earlier removal");
    };
    assert!(error.is_scope_ended());
    assert!(!error.is_store_stopped());
    drop(send);
    assert_eq!(Arc::strong_count(&payload), 1);
    assert_eq!(embeddings.load(Ordering::SeqCst), 1);
    assert_eq!(
        drive_scoped_actor(&mut actor, store.read(|s| *s)).unwrap(),
        None
    );
    drop(child);
    drop(store);
    assert!(actor.as_mut().poll(&mut context).is_ready());
    assert_eq!(Arc::strong_count(&payload), 1);
}

#[test]
fn queued_optional_removal_rejects_effect_output_before_physical_exit() {
    use rca_core::ReducerExt as _;

    let runtime = ManualRuntime::new();
    let (export, receive) = mpsc::channel();
    let mut export = Some(export);
    let child = rca_core::Reduce::new(move |state: &mut usize, amount: &Arc<usize>, _| {
        if **amount == 0 {
            let export = export.take().unwrap();
            Effect::run(move |sender| async move {
                export.send(sender).unwrap();
                std::future::pending::<()>().await;
            })
        } else {
            *state += **amount;
            Effect::none()
        }
    });
    let reductions = Arc::new(AtomicUsize::new(0));
    let parent_calls = Arc::clone(&reductions);
    let parent = rca_core::Reduce::new(
        move |state: &mut Option<usize>, action: &Option<Arc<usize>>, _| {
            parent_calls.fetch_add(1, Ordering::SeqCst);
            if action.is_none() {
                *state = None;
            }
            Effect::none()
        },
    );
    let reducer = parent.if_some(
        OptionalStatePath::new(
            |state: &Option<usize>| state.as_ref(),
            |state: &mut Option<usize>| state.as_mut(),
        ),
        ActionPath::new(|action: &Option<Arc<usize>>| action.as_ref(), Some),
        child,
    );
    let store = Store::new(Some(0), reducer, runtime.runtime());
    let mut actor = runtime.take_only_queued_task();
    let mut context = Context::from_waker(std::task::Waker::noop());
    let start = store.try_send(Some(Arc::new(0))).unwrap();
    assert!(actor.as_mut().poll(&mut context).is_pending());
    let mut start = std::pin::pin!(start.finish());
    let Poll::Ready(Ok(task)) = start.as_mut().poll(&mut context) else {
        panic!("child start must commit before its effect is polled");
    };
    let mut run = runtime.take_only_queued_task();
    assert!(run.as_mut().poll(&mut context).is_pending());
    let sender = receive.try_recv().unwrap();
    let mut live = Box::pin(sender.send(Arc::new(7)));
    assert!(live.as_mut().poll(&mut context).is_pending());
    assert!(actor.as_mut().poll(&mut context).is_pending());
    assert!(matches!(
        live.as_mut().poll(&mut context),
        Poll::Ready(Ok(()))
    ));
    drop(live);
    assert_eq!(
        drive_scoped_actor(&mut actor, store.read(|s| *s)).unwrap(),
        Some(7)
    );
    assert_eq!(reductions.load(Ordering::SeqCst), 2);

    let removal = store.try_send(None).unwrap();
    let payload = Arc::new(11);
    let mut send = Box::pin(sender.send(Arc::clone(&payload)));
    assert!(send.as_mut().poll(&mut context).is_pending());
    assert_eq!(Arc::strong_count(&payload), 2);
    assert!(actor.as_mut().poll(&mut context).is_pending());
    let mut removal = std::pin::pin!(removal.finish());
    assert!(matches!(
        removal.as_mut().poll(&mut context),
        Poll::Ready(Ok(_))
    ));
    assert!(!task.is_cancelled());
    assert!(!task.is_finished());
    let Poll::Ready(Err(error)) = send.as_mut().poll(&mut context) else {
        panic!("stale scoped output must reject before its runtime task exits");
    };
    assert!(error.is_cancelled());
    assert!(!error.is_store_stopped());
    drop(send);
    assert_eq!(Arc::strong_count(&payload), 1);
    assert_eq!(reductions.load(Ordering::SeqCst), 3);
    assert_eq!(
        drive_scoped_actor(&mut actor, store.read(|s| *s)).unwrap(),
        None
    );
    assert!(run.as_mut().poll(&mut context).is_ready());
    assert!(actor.as_mut().poll(&mut context).is_pending());
    assert!(task.is_finished());
    let mut finished = std::pin::pin!(task.finish());
    assert!(finished.as_mut().poll(&mut context).is_ready());
    drop(store);
    assert!(actor.as_mut().poll(&mut context).is_ready());
    assert_eq!(Arc::strong_count(&payload), 1);
    assert_eq!(reductions.load(Ordering::SeqCst), 3);
}
