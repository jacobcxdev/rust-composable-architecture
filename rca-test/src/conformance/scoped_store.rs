//! Checks scoped-handle routing, replacement identity and observation lifetimes.

use std::pin::Pin;
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use rca_core::{
    ActionPath, CaseStatePath, Effect, KeyedAction, KeyedActionPath, KeyedState, KeyedStatePath,
    OptionalStatePath, Reduce, ReduceContext, Reducer, ReducerExt, StatePath, Store, combine,
    scope,
};

use super::RuntimeFactory;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
/// Gives each child an explicit identity and independently checked update count.
struct Child {
    /// Identifies replacements when a scope uses an identity projection.
    id: u64,
    /// Records accepted child increments.
    count: usize,
}

#[derive(Clone, Copy, Debug, PartialEq)]
/// Supplies present and absent variants for case-scoped handles.
enum Case {
    /// Variant selected by the case path.
    Child(Child),
    /// Unselected variant that ends the previous case lifetime.
    Other,
}

/// Holds each supported scope shape beside an independent routing counter.
struct State {
    /// Always-present child, optionally tracked by its explicit identity.
    required: Child,
    /// Child whose absence ends its scope lifetime.
    optional: Option<Child>,
    /// Child selected only while its enum case is active.
    case: Case,
    /// Two entries used to distinguish identity, order and collection replacement.
    keyed: KeyedState<u64, Child>,
    /// Counts actual routed child actions, including any incorrectly accepted send.
    child_reductions: usize,
}

/// Changes a child without replacing its identity.
enum ChildAction {
    /// Adds one observable update to the selected child.
    Increment,
}

/// Routes child input and performs explicit parent lifetime transitions.
enum Action {
    /// Input through the required-child action path.
    Required(ChildAction),
    /// Input through the optional-child action path.
    Optional(ChildAction),
    /// Input through the enum-case action path.
    Case(ChildAction),
    /// Input addressed to one keyed entry.
    Keyed(KeyedAction<u64, ChildAction>),
    /// Replaces a required child's explicit identity.
    ReplaceRequired(u64),
    /// Ends the optional-child lifetime.
    RemoveOptional,
    /// Creates an optional child after absence.
    InsertOptional(u64),
    /// Replaces an optional child without an intervening absent state.
    ReplaceOptional(u64),
    /// Switches away from the selected enum case.
    HideCase,
    /// Selects a new case child after the other variant.
    ShowCase(u64),
    /// Replaces the selected case child's explicit identity.
    ReplaceCase(u64),
    /// Ends one keyed entry lifetime.
    RemoveKey(u64),
    /// Reinserts a key with a new entry lifetime.
    InsertKey(u64),
    /// Changes display order while preserving entry lifetimes.
    MoveKey,
    /// Replaces the entire keyed container with equal public keys.
    ReplaceCollection,
}

/// Applies routed input directly so stale admission causes a visible failure.
struct TestReducer;

impl Reducer for TestReducer {
    type State = State;
    type Action = Action;

    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &Self::Action,
        _context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        match action {
            Action::Required(ChildAction::Increment) => {
                state.child_reductions += 1;
                state.required.count += 1;
            }
            Action::Optional(ChildAction::Increment) => {
                state.child_reductions += 1;
                state
                    .optional
                    .as_mut()
                    .expect("optional child exists")
                    .count += 1;
            }
            Action::Case(ChildAction::Increment) => {
                state.child_reductions += 1;
                let Case::Child(child) = &mut state.case else {
                    panic!("case child exists")
                };
                child.count += 1;
            }
            Action::Keyed(action) => {
                state.child_reductions += 1;
                state
                    .keyed
                    .get_mut(action.id())
                    .expect("keyed child exists")
                    .count += 1;
            }
            Action::ReplaceRequired(id) => state.required = Child { id: *id, count: 0 },
            Action::RemoveOptional => state.optional = None,
            Action::InsertOptional(id) | Action::ReplaceOptional(id) => {
                state.optional = Some(Child { id: *id, count: 0 });
            }
            Action::HideCase => state.case = Case::Other,
            Action::ShowCase(id) | Action::ReplaceCase(id) => {
                state.case = Case::Child(Child { id: *id, count: 0 });
            }
            Action::RemoveKey(id) => {
                let _ = state.keyed.remove(id);
            }
            Action::InsertKey(id) => state.keyed.insert(*id, Child::default()),
            Action::MoveKey => state.keyed.move_index(0, 1),
            Action::ReplaceCollection => {
                state.keyed = [(1, Child::default()), (2, Child::default())]
                    .into_iter()
                    .collect();
            }
        }
        Effect::none()
    }
}

/// Creates zero-count children with two independently addressable keyed entries.
fn state() -> State {
    State {
        required: Child::default(),
        optional: Some(Child::default()),
        case: Case::Child(Child::default()),
        keyed: [(1, Child::default()), (2, Child::default())]
            .into_iter()
            .collect(),
        child_reductions: 0,
    }
}

/// Projects the required child through the ordinary public path constructor.
fn required_state() -> StatePath<
    State,
    Child,
    impl for<'a> Fn(&'a State) -> &'a Child + Clone + Send + Sync,
    impl for<'a> Fn(&'a mut State) -> &'a mut Child + Clone + Send + Sync,
> {
    StatePath::new(|state: &State| &state.required, |state| &mut state.required)
}

#[expect(
    clippy::type_complexity,
    reason = "opaque path closures exercise the normal public construction surface"
)]
/// Projects optional presence without inventing an identity policy.
fn optional_state() -> OptionalStatePath<
    State,
    Child,
    impl for<'a> Fn(&'a State) -> Option<&'a Child> + Clone + Send + Sync,
    impl for<'a> Fn(&'a mut State) -> Option<&'a mut Child> + Clone + Send + Sync,
> {
    OptionalStatePath::new(
        |state: &State| state.optional.as_ref(),
        |state| state.optional.as_mut(),
    )
}

#[expect(
    clippy::type_complexity,
    reason = "opaque path closures exercise the normal public construction surface"
)]
/// Selects only the child enum variant for shared and mutable access.
fn case_state() -> CaseStatePath<
    State,
    Child,
    impl for<'a> Fn(&'a State) -> Option<&'a Child> + Clone + Send + Sync,
    impl for<'a> Fn(&'a mut State) -> Option<&'a mut Child> + Clone + Send + Sync,
> {
    CaseStatePath::new(
        |state: &State| match &state.case {
            Case::Child(child) => Some(child),
            Case::Other => None,
        },
        |state| match &mut state.case {
            Case::Child(child) => Some(child),
            Case::Other => None,
        },
    )
}

#[expect(
    clippy::type_complexity,
    reason = "opaque path closures exercise the normal public construction surface"
)]
/// Projects the keyed container while leaving entry identity to its public API.
fn keyed_state() -> KeyedStatePath<
    State,
    u64,
    Child,
    impl for<'a> Fn(&'a State) -> &'a KeyedState<u64, Child> + Clone + Send + Sync,
    impl for<'a> Fn(&'a mut State) -> &'a mut KeyedState<u64, Child> + Clone + Send + Sync,
> {
    KeyedStatePath::new(|state: &State| &state.keyed, |state| &mut state.keyed)
}

/// Selects and embeds required-child input without matching other action families.
fn required_action() -> ActionPath<
    Action,
    ChildAction,
    impl for<'a> Fn(&'a Action) -> Option<&'a ChildAction> + Clone + Send + Sync,
    impl Fn(ChildAction) -> Action + Clone + Send + Sync,
> {
    ActionPath::new(
        |action| match action {
            Action::Required(child) => Some(child),
            _ => None,
        },
        Action::Required,
    )
}

/// Selects and embeds optional-child input independently of current state presence.
fn optional_action() -> ActionPath<
    Action,
    ChildAction,
    impl for<'a> Fn(&'a Action) -> Option<&'a ChildAction> + Clone + Send + Sync,
    impl Fn(ChildAction) -> Action + Clone + Send + Sync,
> {
    ActionPath::new(
        |action| match action {
            Action::Optional(child) => Some(child),
            _ => None,
        },
        Action::Optional,
    )
}

/// Selects and embeds case-child input independently of the current enum variant.
fn case_action() -> ActionPath<
    Action,
    ChildAction,
    impl for<'a> Fn(&'a Action) -> Option<&'a ChildAction> + Clone + Send + Sync,
    impl Fn(ChildAction) -> Action + Clone + Send + Sync,
> {
    ActionPath::new(
        |action| match action {
            Action::Case(child) => Some(child),
            _ => None,
        },
        Action::Case,
    )
}

#[expect(
    clippy::type_complexity,
    reason = "opaque path closures exercise the normal public construction surface"
)]
/// Preserves the key while selecting and embedding keyed-child input.
fn keyed_action() -> KeyedActionPath<
    Action,
    u64,
    ChildAction,
    impl for<'a> Fn(&'a Action) -> Option<&'a KeyedAction<u64, ChildAction>> + Clone + Send + Sync,
    impl Fn(KeyedAction<u64, ChildAction>) -> Action + Clone + Send + Sync,
> {
    KeyedActionPath::new(
        |action| match action {
            Action::Keyed(child) => Some(child),
            _ => None,
        },
        Action::Keyed,
    )
}

/// Waits for a root action commit; this fixture's root reducer returns no effects.
async fn commit(store: &Store<State, Action>, action: Action) {
    let _ = store.send(action).await.expect("root action commits");
}

/// Runs routing, identity, observation and root-shutdown checks on one adapter.
pub(super) async fn run(factory: impl RuntimeFactory) {
    composed_paths_and_scoped_handles_interoperate(factory.clone()).await;
    send_before_removal(factory.clone()).await;
    replacement_lifetimes(factory.clone()).await;
    keyed_lifetimes(factory.clone()).await;
    observation_lifetimes(factory.clone()).await;
    idle_root_observation_closure(factory.clone()).await;
    root_shutdown(factory).await;
}

/// Distinguishes every-commit delivery, distinct values and terminal old observations.
async fn observation_lifetimes(factory: impl RuntimeFactory) {
    let store = Store::new(state(), TestReducer, factory.runtime());
    let required = store.scope(required_state(), required_action());
    let mut every = required.observe(|child| child.count).await.unwrap();
    let mut distinct = required
        .observe_distinct(|child| child.count)
        .await
        .unwrap();
    assert_eq!(every.next().await, Some(0));
    assert_eq!(distinct.next().await, Some(0));

    commit(&store, Action::RemoveOptional).await;
    assert_eq!(every.next().await, Some(0));
    assert_next_pending(&mut distinct);
    commit(&store, Action::Required(ChildAction::Increment)).await;
    assert_eq!(every.next().await, Some(1));
    assert_eq!(distinct.next().await, Some(1));

    commit(&store, Action::InsertOptional(7)).await;
    let optional = store
        .scope_some(optional_state(), optional_action())
        .await
        .unwrap();
    let mut old = optional.observe(|child| child.count).await.unwrap();
    assert_eq!(old.next().await, Some(0));
    commit(&store, Action::RemoveOptional).await;
    assert_eq!(old.next().await, None);
    commit(&store, Action::InsertOptional(8)).await;
    let replacement = store
        .scope_some(optional_state(), optional_action())
        .await
        .unwrap();
    let mut current = replacement.observe(|child| child.count).await.unwrap();
    assert_eq!(current.next().await, Some(0));
    let _ = replacement.send(ChildAction::Increment).await.unwrap();
    assert_eq!(old.next().await, None);
    assert_eq!(current.next().await, Some(1));
    store.shutdown().await.unwrap();
}

/// Requires dropped scope ownership to close its observation without stopping the root.
///
/// The finite retry budget bounds a failed adapter check; it is not a latency guarantee.
async fn idle_root_observation_closure(factory: impl RuntimeFactory) {
    let store = Store::new(state(), TestReducer, factory.runtime());
    let optional = store
        .scope_some(optional_state(), optional_action())
        .await
        .expect("optional child registers");
    let mut values = optional.observe(|child| child.count).await.unwrap();
    assert_eq!(values.next().await, Some(0));
    let mut closure = Box::pin(values.next());
    assert!(matches!(poll_once(closure.as_mut()), Poll::Pending));

    drop(optional);
    let mut result = None;
    for _ in 0..100 {
        factory
            .advance(Duration::from_millis(10))
            .await
            .expect("runtime advances while awaiting idle-root cleanup");
        if let Poll::Ready(value) = poll_once(closure.as_mut()) {
            result = Some(value);
            break;
        }
    }

    assert_eq!(result, Some(None));
    assert!(!store.is_closed());
    store.shutdown().await.unwrap();
}

/// Samples readiness once without claiming that an executor received a wake.
fn poll_once<FutureType: Future>(future: Pin<&mut FutureType>) -> Poll<FutureType::Output> {
    let mut context = Context::from_waker(Waker::noop());
    future.poll(&mut context)
}

/// Rejects both an available value and terminal closure when quiescence is expected.
fn assert_next_pending<Value>(values: &mut rca_core::Observation<Value>) {
    let mut next = Box::pin(values.next());
    let mut context = Context::from_waker(Waker::noop());
    assert!(matches!(
        Pin::as_mut(&mut next).poll(&mut context),
        Poll::Pending
    ));
}

/// Checks committed input before removal and stale-handle rejection after reinsertion.
async fn send_before_removal(factory: impl RuntimeFactory) {
    let store = Store::new(state(), TestReducer, factory.runtime());
    let child = store
        .scope_some(optional_state(), optional_action())
        .await
        .expect("optional child registers");
    let _ = child
        .send(ChildAction::Increment)
        .await
        .expect("current send");
    commit(&store, Action::RemoveOptional).await;
    let absent = store
        .scope_some(optional_state(), optional_action())
        .await
        .expect_err("absent optional child rejects registration");
    assert!(absent.is_scope_ended());
    assert!(!absent.is_store_stopped());
    commit(&store, Action::InsertOptional(1)).await;
    child
        .send(ChildAction::Increment)
        .await
        .expect_err("removed child rejects later send");
    assert_eq!(store.read(|state| state.child_reductions).await.unwrap(), 1);
    store.shutdown().await.unwrap();
}

/// Requires identity changes to invalidate old handles and admit fresh replacements.
async fn replacement_lifetimes(factory: impl RuntimeFactory) {
    let store = Store::new(state(), TestReducer, factory.runtime());
    let required = store
        .scope_identified(required_state(), required_action(), |child| &child.id)
        .await
        .unwrap();
    let optional = store
        .scope_some_identified(optional_state(), optional_action(), |child| &child.id)
        .await
        .unwrap();
    let case = store
        .scope_case_identified(case_state(), case_action(), |child| &child.id)
        .await
        .unwrap();
    required
        .send(ChildAction::Increment)
        .await
        .expect("identified required child commits before replacement")
        .finish()
        .await;
    optional
        .send(ChildAction::Increment)
        .await
        .expect("identified optional child commits before replacement")
        .finish()
        .await;
    case.send(ChildAction::Increment)
        .await
        .expect("identified case child commits before replacement")
        .finish()
        .await;
    assert_eq!(required.read(|child| child.count).await.unwrap(), 1);
    assert_eq!(optional.read(|child| child.count).await.unwrap(), 1);
    assert_eq!(case.read(|child| child.count).await.unwrap(), 1);
    assert_eq!(store.read(|state| state.child_reductions).await.unwrap(), 3);
    commit(&store, Action::ReplaceRequired(1)).await;
    commit(&store, Action::ReplaceOptional(1)).await;
    commit(&store, Action::ReplaceCase(1)).await;
    assert!(
        required
            .send(ChildAction::Increment)
            .await
            .unwrap_err()
            .is_scope_ended()
    );
    assert!(
        optional
            .send(ChildAction::Increment)
            .await
            .unwrap_err()
            .is_scope_ended()
    );
    assert!(
        case.send(ChildAction::Increment)
            .await
            .unwrap_err()
            .is_scope_ended()
    );

    let present = store.scope_case(case_state(), case_action()).await.unwrap();
    let _ = present
        .send(ChildAction::Increment)
        .await
        .expect("present case child commits before removal");
    assert_eq!(
        present.read(|child| (child.id, child.count)).await.unwrap(),
        (1, 1)
    );
    commit(&store, Action::HideCase).await;
    commit(&store, Action::ShowCase(2)).await;
    assert!(
        present
            .send(ChildAction::Increment)
            .await
            .unwrap_err()
            .is_scope_ended()
    );
    assert_eq!(store.read(|state| state.child_reductions).await.unwrap(), 4);

    let replacement = store.scope_case(case_state(), case_action()).await.unwrap();
    let _ = replacement
        .send(ChildAction::Increment)
        .await
        .expect("fresh case child commits after replacement");
    assert_eq!(
        replacement
            .read(|child| (child.id, child.count))
            .await
            .unwrap(),
        (2, 1)
    );
    assert_eq!(store.read(|state| state.child_reductions).await.unwrap(), 5);
    store.shutdown().await.unwrap();
}

/// Distinguishes a reorder from entry reinsertion and whole-container replacement.
async fn keyed_lifetimes(factory: impl RuntimeFactory) {
    let store = Store::new(state(), TestReducer, factory.runtime());
    let moved = store
        .scope_keyed(keyed_state(), keyed_action(), 1)
        .await
        .unwrap();
    commit(&store, Action::MoveKey).await;
    assert_eq!(
        store
            .read(|state| state.keyed.ids().copied().collect::<Vec<_>>())
            .await
            .unwrap(),
        [2, 1]
    );
    let _ = moved.send(ChildAction::Increment).await.unwrap();

    let reinserted = store
        .scope_keyed(keyed_state(), keyed_action(), 1)
        .await
        .unwrap();
    commit(&store, Action::RemoveKey(1)).await;
    commit(&store, Action::InsertKey(1)).await;
    assert!(
        reinserted
            .send(ChildAction::Increment)
            .await
            .unwrap_err()
            .is_scope_ended()
    );

    let replaced = store
        .scope_keyed(keyed_state(), keyed_action(), 2)
        .await
        .unwrap();
    commit(&store, Action::ReplaceCollection).await;
    assert!(
        replaced
            .send(ChildAction::Increment)
            .await
            .unwrap_err()
            .is_scope_ended()
    );
    assert_eq!(store.read(|state| state.child_reductions).await.unwrap(), 1);
    store.shutdown().await.unwrap();
}

/// Requires stopped-root errors rather than child-lifetime errors after shutdown.
async fn root_shutdown(factory: impl RuntimeFactory) {
    let store = Store::new(state(), TestReducer, factory.runtime());
    let child = store
        .scope_some(optional_state(), optional_action())
        .await
        .unwrap();
    store.shutdown().await.unwrap();
    let error = child.send(ChildAction::Increment).await.unwrap_err();
    assert!(error.is_store_stopped());
    assert!(!error.is_scope_ended());
    let registration = store
        .scope_some(optional_state(), optional_action())
        .await
        .expect_err("stopped root rejects registration");
    assert!(registration.is_store_stopped());
    assert!(!registration.is_scope_ended());
}

/// Compares all routed child counts with explicit scenario expectations.
async fn assert_composed_state(
    store: &Store<State, Action>,
    expected: (usize, usize, usize, [usize; 2], usize),
) {
    let (required, optional, case, keyed, child_reductions) = expected;
    assert_eq!(
        store
            .read(|state| {
                (
                    state.required.count,
                    state.optional.as_ref().map(|child| child.count),
                    state.case,
                    [
                        state.keyed.get(&1).map(|child| child.count),
                        state.keyed.get(&2).map(|child| child.count),
                    ],
                    state.child_reductions,
                )
            })
            .await
            .expect("direct root state reflects both routed actions"),
        (
            required,
            Some(optional),
            Case::Child(Child { id: 0, count: case }),
            [Some(keyed[0]), Some(keyed[1])],
            child_reductions,
        ),
        "composed root state"
    );
}

/// Routes input and effect output through every scope shape on the same root.
async fn composed_paths_and_scoped_handles_interoperate(factory: impl RuntimeFactory) {
    let (required_state, required_action) = (required_state(), required_action());
    let (optional_state, optional_action) = (optional_state(), optional_action());
    let (case_state, case_action) = (case_state(), case_action());
    let (keyed_state, keyed_action) = (keyed_state(), keyed_action());
    let reduce_child = |state: &mut Child, _: &ChildAction, _: &mut ReduceContext<'_>| {
        state.count += 1;
        if state.count == 1 {
            Effect::send(ChildAction::Increment)
        } else {
            Effect::none()
        }
    };
    let reducer = combine((
        scope(
            required_state.clone(),
            required_action.clone(),
            Reduce::new(reduce_child),
        ),
        Reduce::new(|state: &mut State, _: &Action, _| {
            state.child_reductions += 1;
            Effect::none()
        }),
    ))
    .if_some(
        optional_state.clone(),
        optional_action.clone(),
        Reduce::new(reduce_child),
    )
    .if_case(
        case_state.clone(),
        case_action.clone(),
        Reduce::new(reduce_child),
    )
    .for_each(
        keyed_state.clone(),
        keyed_action.clone(),
        Reduce::new(reduce_child),
    );
    let store = Store::new(state(), reducer, factory.runtime());
    let required = store.scope(required_state, required_action);
    let optional = store
        .scope_some(optional_state, optional_action)
        .await
        .expect("optional child registers");
    let case = store
        .scope_case(case_state, case_action)
        .await
        .expect("case child registers");
    let keyed = store
        .scope_keyed(keyed_state, keyed_action, 2)
        .await
        .expect("second keyed child registers");

    for (child, expected) in [
        (required, (2, 0, 0, [0, 0], 2)),
        (optional, (2, 2, 0, [0, 0], 4)),
        (case, (2, 2, 2, [0, 0], 6)),
        (keyed, (2, 2, 2, [0, 2], 8)),
    ] {
        child
            .send(ChildAction::Increment)
            .await
            .expect("scoped action commits")
            .finish()
            .await;
        assert_eq!(child.read(|state| state.count).await.unwrap(), 2);
        assert_composed_state(&store, expected).await;
    }
    let snapshot = store
        .runtime_snapshot()
        .await
        .expect("completed scopes snapshot");
    assert_eq!(snapshot.commit_version, 8);
    assert!(snapshot.live_task_ids.is_empty());
    store.shutdown().await.expect("composed Store shuts down");
}

#[cfg(test)]
mod tests {
    use std::panic::{AssertUnwindSafe, catch_unwind};

    use super::{Action, Case, ChildAction, KeyedAction, State, Store, TestReducer, state};
    use crate::DeterministicRuntime;

    use std::pin::Pin;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::time::Duration;

    use futures_util::task::AtomicWaker;
    use rca_core::{MonotonicInstant, Runtime, RuntimeDriver, RuntimeFuture};

    use crate::conformance::{
        ConformanceError, ConformanceFuture, DeterministicFactory, RuntimeFactory,
    };

    #[derive(Clone, Debug)]
    struct ExternallyHosted {
        driver: crate::runtime::DeterministicDriver,
        host: std::sync::Weak<DeterministicRuntime>,
    }

    impl RuntimeFactory for ExternallyHosted {
        fn runtime(&self) -> Runtime {
            Runtime::new(self.driver.clone())
        }

        fn spawn_probe(&self, future: RuntimeFuture) {
            self.driver.spawn(future);
        }

        fn now_probe(&self) -> MonotonicInstant {
            self.driver.now()
        }

        fn sleep_probe(&self, deadline: MonotonicInstant) -> RuntimeFuture {
            self.driver.sleep_until(deadline)
        }

        fn yield_probe(&self) -> RuntimeFuture {
            self.driver.yield_now()
        }

        fn advance(&self, duration: Duration) -> ConformanceFuture<Result<(), ConformanceError>> {
            let host = self.host.upgrade();
            Box::pin(async move {
                host.ok_or_else(|| ConformanceError::new("external host has stopped"))?
                    .advance(duration)
                    .map(|_| ())
                    .map_err(|error| ConformanceError::new(error.to_string()))
            })
        }

        fn teardown_probe(&self) -> ConformanceFuture<()> {
            DeterministicFactory::new().teardown_probe()
        }
    }

    #[test]
    fn idle_observation_checker_rejects_host_teardown_before_initial_pending() {
        let host = Arc::new(DeterministicRuntime::new());
        let factory = ExternallyHosted {
            driver: host.driver.clone(),
            host: Arc::downgrade(&host),
        };
        let mut check = Box::pin(super::idle_root_observation_closure(factory.clone()));

        // Complete scope registration, then observation registration, without
        // polling the checker between the observation ACK and host teardown.
        assert!(super::poll_once(check.as_mut()).is_pending());
        host.run_until_stalled()
            .expect("scope registration is acknowledged");
        assert!(super::poll_once(check.as_mut()).is_pending());
        host.run_until_stalled()
            .expect("observation registration is acknowledged");

        // This owned diagnostic retains scheduler data, not host authority.
        let closed_scheduler = host.drain_deadlines(0);
        drop(host);
        assert!(factory.host.upgrade().is_none());
        let panic = catch_unwind(AssertUnwindSafe(|| super::poll_once(check.as_mut())))
            .expect_err("a stopped host must fail the initial pending guard");
        let message = panic
            .downcast_ref::<&str>()
            .copied()
            .or_else(|| panic.downcast_ref::<String>().map(String::as_str));
        assert_eq!(
            message,
            Some("assertion failed: matches!(poll_once(closure.as_mut()), Poll::Pending)")
        );
        drop(check);

        let stopped = futures_executor::block_on(closed_scheduler)
            .expect_err("the real host destructor closed the scheduler");
        assert!(stopped.is_host_closed());
        assert_eq!(stopped.polls(), 0);
        assert!(stopped.live_task_ids().is_empty());
        assert!(stopped.ready_task_ids().is_empty());
        assert!(stopped.pending_sleeps().is_empty());
        assert_eq!(
            futures_executor::block_on(factory.advance(Duration::ZERO)),
            Err(ConformanceError::new("external host has stopped"))
        );
    }

    #[derive(Debug, Default)]
    struct CleanupGate {
        closed: AtomicBool,
        advances: AtomicUsize,
        blocked_polls: AtomicUsize,
        submitted: AtomicUsize,
        waiter: AtomicWaker,
        task_capture: Arc<()>,
    }

    impl CleanupGate {
        fn release(&self) {
            self.closed.store(false, Ordering::Release);
            self.waiter.wake();
        }
    }

    #[derive(Clone, Debug)]
    struct DelayedCleanup {
        inner: DeterministicFactory,
        gate: Arc<CleanupGate>,
        release_at: usize,
    }

    impl DelayedCleanup {
        fn new(release_at: usize) -> Self {
            Self {
                inner: DeterministicFactory::new(),
                gate: Arc::new(CleanupGate::default()),
                release_at,
            }
        }

        fn drive_check(&self, mut check: Pin<&mut impl Future<Output = ()>>) {
            let scheduler = self.inner.scheduler();
            // Registration, observation and shutdown are real actor handshakes.
            // Keep a broken handshake finite instead of waiting for an external wake.
            for _ in 0..8 {
                if super::poll_once(check.as_mut()).is_ready() {
                    return;
                }
                scheduler.run_until_stalled().expect("scenario tasks stall");
            }
            panic!("idle observation scenario exceeded eight drive turns");
        }

        fn assert_drained(&self) {
            let scheduler = self.inner.scheduler();
            scheduler
                .run_until_stalled()
                .expect("released actor drains");
            let report = futures_executor::block_on(scheduler.drain_deadlines(0))
                .expect("no actor or timer remains after scenario teardown");
            assert_eq!(report.polls(), 0);
            assert_eq!(self.gate.submitted.load(Ordering::Acquire), 1);
            assert_eq!(Arc::strong_count(&self.gate.task_capture), 1);
        }
    }

    #[derive(Debug)]
    struct CleanupDriver {
        inner: crate::runtime::DeterministicDriver,
        gate: Arc<CleanupGate>,
    }

    impl RuntimeDriver for CleanupDriver {
        fn spawn(&self, mut task: RuntimeFuture) {
            self.gate.submitted.fetch_add(1, Ordering::AcqRel);
            let gate = Arc::clone(&self.gate);
            let capture = Arc::clone(&gate.task_capture);
            self.inner.spawn(Box::pin(async move {
                futures_util::future::poll_fn(move |context| {
                    if gate.closed.load(Ordering::Acquire) {
                        gate.waiter.register(context.waker());
                        if gate.closed.load(Ordering::Acquire) {
                            gate.blocked_polls.fetch_add(1, Ordering::AcqRel);
                            return std::task::Poll::Pending;
                        }
                    }
                    task.as_mut().poll(context)
                })
                .await;
                drop(capture);
            }));
        }

        fn now(&self) -> MonotonicInstant {
            self.inner.now()
        }

        fn sleep_until(&self, deadline: MonotonicInstant) -> RuntimeFuture {
            self.inner.sleep_until(deadline)
        }

        fn yield_now(&self) -> RuntimeFuture {
            self.inner.yield_now()
        }
    }

    impl RuntimeFactory for DelayedCleanup {
        fn runtime(&self) -> Runtime {
            Runtime::new(CleanupDriver {
                inner: self.inner.scheduler().driver.clone(),
                gate: Arc::clone(&self.gate),
            })
        }

        fn spawn_probe(&self, future: RuntimeFuture) {
            self.inner.spawn_probe(future);
        }

        fn now_probe(&self) -> MonotonicInstant {
            self.inner.now_probe()
        }

        fn sleep_probe(&self, deadline: MonotonicInstant) -> RuntimeFuture {
            self.inner.sleep_probe(deadline)
        }

        fn yield_probe(&self) -> RuntimeFuture {
            self.inner.yield_probe()
        }

        fn advance(&self, duration: Duration) -> ConformanceFuture<Result<(), ConformanceError>> {
            let factory = self.clone();
            Box::pin(async move {
                let advance = factory.gate.advances.fetch_add(1, Ordering::AcqRel) + 1;
                if advance == 1 {
                    factory.gate.closed.store(true, Ordering::Release);
                }
                if advance == factory.release_at {
                    factory.gate.release();
                }
                // Advance the real shared clock even while actor polling is withheld.
                factory.inner.advance(duration).await
            })
        }

        fn teardown_probe(&self) -> ConformanceFuture<()> {
            self.inner.teardown_probe()
        }
    }

    #[test]
    fn idle_observation_checker_retries_delayed_actor_cleanup() {
        let factory = DelayedCleanup::new(2);
        let mut check = Box::pin(super::idle_root_observation_closure(factory.clone()));

        factory.drive_check(check.as_mut());
        drop(check);

        assert_eq!(factory.gate.advances.load(Ordering::Acquire), 2);
        assert_eq!(factory.gate.blocked_polls.load(Ordering::Acquire), 1);
        assert_eq!(
            factory.inner.now_probe(),
            MonotonicInstant::from_elapsed(Duration::from_millis(20))
        );
        factory.assert_drained();
    }

    #[test]
    fn case_capture_skips_an_unselected_case_without_running_callbacks() {
        for (case, expected, modified, callback_count) in [
            (Case::Other, Case::Other, false, 0),
            (
                Case::Child(super::Child { id: 7, count: 3 }),
                Case::Child(super::Child { id: 7, count: 4 }),
                true,
                1,
            ),
        ] {
            let mut parent = state();
            parent.case = case;
            let captures = std::cell::Cell::new(0);
            let transforms = std::cell::Cell::new(0);
            let result = super::case_state().try_modify_captured(
                &mut parent,
                |child| {
                    captures.set(captures.get() + 1);
                    *child
                },
                |mut child| {
                    transforms.set(transforms.get() + 1);
                    child.count += 1;
                    Ok::<_, ()>(child)
                },
            );
            assert_eq!(result, Ok(modified));
            assert_eq!(parent.case, expected);
            assert_eq!(captures.get(), callback_count);
            assert_eq!(transforms.get(), callback_count);
            assert_eq!(parent.required, super::Child::default());
            assert_eq!(parent.child_reductions, 0);
        }
    }

    #[test]
    fn pending_observation_checker_rejects_an_available_value() {
        let runtime = DeterministicRuntime::with_step_budget(64);
        futures_executor::block_on(runtime.run(async {
            let store = Store::new(state(), TestReducer, runtime.runtime());
            let child = store.scope(super::required_state(), super::required_action());
            let mut values = child.observe(|child| child.count).await.unwrap();

            // The public observation still holds its ordinary initial snapshot.
            let panic = catch_unwind(AssertUnwindSafe(|| super::assert_next_pending(&mut values)))
                .expect_err("an available value must not satisfy the pending validator");
            let message = panic
                .downcast_ref::<&str>()
                .copied()
                .or_else(|| panic.downcast_ref::<String>().map(String::as_str));
            assert!(message.is_some_and(|message| message.contains("Poll::Pending")));
            assert!(!store.is_closed(), "the validator panics outside the actor");

            // The rejected poll consumed the snapshot, leaving a live, empty observation.
            super::assert_next_pending(&mut values);
            super::commit(&store, Action::Required(ChildAction::Increment)).await;
            assert_eq!(values.next().await, Some(1));
            store.shutdown().await.unwrap();
            assert_eq!(values.next().await, None);
        }))
        .unwrap();
        futures_executor::block_on(runtime.drain_deadlines(0)).unwrap();
    }

    #[test]
    fn pending_observation_checker_rejects_a_closed_observation() {
        let runtime = DeterministicRuntime::with_step_budget(64);
        futures_executor::block_on(runtime.run(async {
            let store = Store::new(state(), TestReducer, runtime.runtime());
            let child = store.scope(super::required_state(), super::required_action());
            let mut values = child.observe(|child| child.count).await.unwrap();
            assert_eq!(values.next().await, Some(0));
            super::assert_next_pending(&mut values);
            store.shutdown().await.unwrap();

            let panic = catch_unwind(AssertUnwindSafe(|| super::assert_next_pending(&mut values)))
                .expect_err("a closed observation must not satisfy the pending validator");
            let message = panic
                .downcast_ref::<&str>()
                .copied()
                .or_else(|| panic.downcast_ref::<String>().map(String::as_str));
            assert!(message.is_some_and(|message| message.contains("Poll::Pending")));
            assert!(store.is_closed());
            assert_eq!(values.next().await, None, "closure remains terminal");
        }))
        .unwrap();
        futures_executor::block_on(runtime.drain_deadlines(0)).unwrap();
    }

    #[test]
    fn idle_observation_checker_rejects_cleanup_past_its_finite_bound() {
        // No release occurs during the scenario's 100 advances. This tests its
        // finite timeout policy, not a universal runtime scheduling-latency bound.
        let factory = DelayedCleanup::new(101);
        let mut check = Box::pin(super::idle_root_observation_closure(factory.clone()));

        let panic = catch_unwind(AssertUnwindSafe(|| factory.drive_check(check.as_mut())))
            .expect_err("the checker must reject an observation that has not closed");
        let message = panic
            .downcast_ref::<&str>()
            .copied()
            .or_else(|| panic.downcast_ref::<String>().map(String::as_str));
        assert_eq!(
            message,
            Some("assertion `left == right` failed\n  left: None\n right: Some(None)")
        );
        assert_eq!(factory.gate.advances.load(Ordering::Acquire), 100);
        assert_eq!(factory.gate.blocked_polls.load(Ordering::Acquire), 1);
        assert_eq!(
            factory.inner.now_probe(),
            MonotonicInstant::from_elapsed(Duration::from_secs(1))
        );

        // Never repoll the panicked future. Dropping it releases the root Store;
        // reopening the task gate then lets the actual actor finish teardown.
        drop(check);
        factory.gate.release();
        factory.assert_drained();
    }

    #[test]
    fn composed_state_oracle_rejects_real_fixture_transitions() {
        for (action, expected) in [
            (Action::HideCase, (0, 0, 0, [0, 0], 0)),
            (Action::ReplaceCase(1), (0, 0, 0, [0, 0], 0)),
            (Action::Case(ChildAction::Increment), (0, 0, 0, [0, 0], 1)),
            (
                Action::Required(ChildAction::Increment),
                (0, 0, 0, [0, 0], 1),
            ),
            (
                Action::Optional(ChildAction::Increment),
                (0, 0, 0, [0, 0], 1),
            ),
            (
                Action::Keyed(KeyedAction::new(1, ChildAction::Increment)),
                (0, 0, 0, [0, 0], 1),
            ),
            (
                Action::Keyed(KeyedAction::new(2, ChildAction::Increment)),
                (0, 0, 0, [0, 0], 1),
            ),
            (Action::RemoveOptional, (0, 0, 0, [0, 0], 0)),
            (Action::RemoveKey(1), (0, 0, 0, [0, 0], 0)),
            (Action::RemoveKey(2), (0, 0, 0, [0, 0], 0)),
            (
                Action::Required(ChildAction::Increment),
                (1, 0, 0, [0, 0], 0),
            ),
        ] {
            let runtime = DeterministicRuntime::with_step_budget(64);
            let store = Store::new(state(), TestReducer, runtime.runtime());
            let mut polls = 0;
            runtime
                .run_ready(
                    super::assert_composed_state(&store, (0, 0, 0, [0, 0], 0)),
                    &mut polls,
                )
                .expect("initial state read drives")
                .expect("initial state matches the oracle");

            let receipt = store.try_send(action).expect("fixture transition admitted");
            let task = runtime
                .run_ready(async { receipt.await }, &mut polls)
                .expect("fixture commit drives")
                .expect("fixture commit is ready")
                .expect("fixture transition commits");
            runtime
                .run_ready(task.finish(), &mut polls)
                .expect("fixture completion drives")
                .expect("fixture action completes");
            assert!(task.is_finished());

            let panic = catch_unwind(AssertUnwindSafe(|| {
                runtime.run_ready(super::assert_composed_state(&store, expected), &mut polls)
            }))
            .expect_err("the oracle must reject the changed real state");
            let message = panic
                .downcast_ref::<&str>()
                .copied()
                .or_else(|| panic.downcast_ref::<String>().map(String::as_str));
            assert!(
                message.is_some_and(|message| message.contains("composed root state")),
                "failure must come from the shared state oracle"
            );
            assert!(
                !store.is_closed(),
                "oracle assertion stays outside the actor"
            );
            runtime
                .run_ready(store.shutdown(), &mut polls)
                .expect("shutdown drives")
                .expect("shutdown completes")
                .expect("Store shuts down");
            futures_executor::block_on(runtime.drain_deadlines(0))
                .expect("each fixture releases its actor and timers");
        }
    }

    // Root submissions deliberately bypass scoped admission to test the harness's
    // own fault detection, not to claim these states can pass a scoped handle.
    fn assert_missing_child_panics(state: State, action: Action, expected: &str) {
        let runtime = DeterministicRuntime::with_step_budget(64);
        let store = Store::new(state, TestReducer, runtime.runtime());
        let _receipt = store.try_send(action).expect("fault action is admitted");

        let panic = catch_unwind(AssertUnwindSafe(|| runtime.run_until_stalled()))
            .expect_err("the conformance reducer must detect an absent child");
        let message = panic
            .downcast_ref::<&str>()
            .copied()
            .or_else(|| panic.downcast_ref::<String>().map(String::as_str));
        assert_eq!(message, Some(expected));
    }

    #[test]
    fn validator_rejects_an_absent_optional_child() {
        let mut state = state();
        state.optional = None;
        assert_missing_child_panics(
            state,
            Action::Optional(ChildAction::Increment),
            "optional child exists",
        );
    }

    #[test]
    fn validator_rejects_an_absent_case_child() {
        let mut state = state();
        state.case = Case::Other;
        assert_missing_child_panics(
            state,
            Action::Case(ChildAction::Increment),
            "case child exists",
        );
    }

    #[test]
    fn validator_rejects_an_absent_keyed_child() {
        let mut state = state();
        let _ = state.keyed.remove(&1);
        assert_missing_child_panics(
            state,
            Action::Keyed(KeyedAction::new(1, ChildAction::Increment)),
            "keyed child exists",
        );
    }
}
