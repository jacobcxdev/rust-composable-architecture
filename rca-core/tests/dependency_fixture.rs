//! Checks dependency isolation, captured ownership, and output authority after child replacement.
//!
//! Each test creates its own dependency values. Actor tests explicitly start queued work and stop their Stores.

mod support;

use std::any::{Any, TypeId};
use std::future::pending;
use std::pin::pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll, Waker};

use rca_core::{
    DependencyKey, DependencyValues, Effect, ReduceContext, Reducer, Store, try_dependency_mock,
    with_dependency,
};

struct Number;
impl DependencyKey for Number {
    type Value = usize;
}

/// Creates an independent environment with one known value.
fn values(number: usize) -> DependencyValues {
    let mut values = DependencyValues::new();
    values.set::<Number>(number);
    values
}

// Mutating the parent map instead of an owned child leaks nested overrides.
#[test]
fn nested_sync_scopes_restore_parent_and_sibling_values() {
    let mut root = values(1);
    let result = root.scope(
        |outer| outer.set::<Number>(2),
        |outer| {
            let inner = outer.scope(
                |inner| inner.set::<Number>(3),
                |inner| *inner.get_or_insert::<Number>(),
            );
            (inner, *outer.get_or_insert::<Number>())
        },
    );
    assert_eq!(result, (3, 2));
    assert_eq!(*root.get_or_insert::<Number>(), 1);
    assert_eq!(
        root.scope(|_| {}, |sibling| *sibling.get_or_insert::<Number>()),
        1
    );
}

// Capturing the parent borrow or reading it on first poll breaks owned escape.
#[test]
fn nested_async_scopes_escape_and_retain_construction_time_values() {
    fn require_send<T: Send>(_: &T) {}

    let future = {
        let mut root = values(1);
        let future = root.scope_async(
            |outer| outer.set::<Number>(2),
            |mut outer| async move {
                let result = outer
                    .scope_async(
                        |inner| inner.set::<Number>(3),
                        |mut inner| async move { *inner.get_or_insert::<Number>() },
                    )
                    .await;
                (result, *outer.get_or_insert::<Number>())
            },
        );
        root.set::<Number>(4);
        future
    };
    require_send(&future);
    assert_eq!(futures_executor::block_on(future), (3, 2));
}

// Ambient inheritance would leak sibling overrides across an actual suspension.
#[test]
fn suspended_local_future_keeps_its_explicit_environment() {
    let mut root = values(1);
    let (release, released) = futures_channel::oneshot::channel::<()>();
    let local = std::rc::Rc::new(5);
    let first = root.scope_async(
        |scope| scope.set::<Number>(2),
        |mut scope| async move {
            released.await.unwrap();
            (*scope.get_or_insert::<Number>(), *local)
        },
    );
    let mut first = pin!(first);
    assert_eq!(
        first.as_mut().poll(&mut Context::from_waker(Waker::noop())),
        Poll::Pending
    );
    let sibling = root.scope_async(
        |scope| scope.set::<Number>(3),
        |mut scope| async move { *scope.get_or_insert::<Number>() },
    );
    assert_eq!(futures_executor::block_on(sibling), 3);
    assert_eq!(*root.get_or_insert::<Number>(), 1);
    release.send(()).unwrap();
    assert_eq!(futures_executor::block_on(first), (2, 5));
}

// Restoring only on the success path would leave an override after callback panic.
#[test]
fn synchronous_unwind_preserves_the_parent_environment() {
    let mut root = values(1);
    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        root.scope(
            |scope| scope.set::<Number>(2),
            |scope| {
                assert_eq!(*scope.get_or_insert::<Number>(), 2);
                panic!("fixture callback failed");
            },
        );
    }))
    .expect_err("the fixture callback must propagate its deliberate panic");
    assert_eq!(
        panic.downcast_ref::<&str>(),
        Some(&"fixture callback failed")
    );
    assert_eq!(*root.get_or_insert::<Number>(), 1);
}

/// Counts releases of an owned override without retaining that override.
struct Resource(Arc<AtomicUsize>);
impl Drop for Resource {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}
struct ResourceKey;
impl DependencyKey for ResourceKey {
    type Value = Resource;
}

#[test]
fn async_scope_construction_panics_release_the_child_without_mutating_the_parent() {
    for panic_in_update in [false, true] {
        let drops = Arc::new(AtomicUsize::new(0));
        let mut root = values(1);
        let mut operation_calls = 0;
        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            root.scope_async(
                |child| {
                    child.set::<ResourceKey>(Resource(Arc::clone(&drops)));
                    child.set::<Number>(2);
                    assert!(!panic_in_update, "update failed");
                },
                |mut child| -> std::future::Pending<()> {
                    operation_calls += 1;
                    assert_eq!(*child.get_or_insert::<Number>(), 2);
                    panic!("operation failed");
                },
            )
        }))
        .expect_err("scope construction must propagate either callback's panic");
        assert_eq!(
            panic.downcast_ref::<&str>(),
            Some(&if panic_in_update {
                "update failed"
            } else {
                "operation failed"
            }),
        );
        assert_eq!(operation_calls, usize::from(!panic_in_update));
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert_eq!(*root.get_or_insert::<Number>(), 1);
    }
}

// A hidden spawn or retained fixture owner would delay release after cancellation.
#[test]
fn dropping_unpolled_and_suspended_scopes_releases_owned_overrides() {
    for poll in [false, true] {
        let drops = Arc::new(AtomicUsize::new(0));
        let root = values(1);
        let future = root.scope_async(
            |scope| scope.set::<ResourceKey>(Resource(Arc::clone(&drops))),
            |mut scope| async move {
                pending::<()>().await;
                std::hint::black_box(scope.get_or_insert::<ResourceKey>());
            },
        );
        {
            let mut future = pin!(future);
            if poll {
                assert_eq!(
                    future
                        .as_mut()
                        .poll(&mut Context::from_waker(Waker::noop())),
                    Poll::Pending
                );
            }
            assert_eq!(drops.load(Ordering::SeqCst), 0);
        };
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }
}

// Dropping the returned future cannot release a child transferred elsewhere.
#[test]
fn transferred_async_scope_outlives_its_unpolled_future() {
    let drops = Arc::new(AtomicUsize::new(0));
    let root = values(1);
    let mut transferred = None;
    let future = root.scope_async(
        |child| child.set::<ResourceKey>(Resource(Arc::clone(&drops))),
        |child| {
            transferred = Some(child);
            pending::<()>()
        },
    );

    assert!(transferred.is_some());
    drop(future);
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    drop(transferred);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
}

struct Counter;
impl DependencyKey for Counter {
    type Value = AtomicUsize;
}

// Reusing a previous case's environment shares its cached mutable provider.
#[test]
fn parameter_and_repetition_factories_create_independent_provider_state() {
    let make = |parameter| {
        let mut fixture = values(parameter);
        fixture.set::<Counter>(AtomicUsize::new(0));
        fixture
    };
    for parameter in [7, 11] {
        for _ in 0..2 {
            let mut fixture = make(parameter);
            assert_eq!(
                fixture
                    .get_or_insert::<Counter>()
                    .fetch_add(1, Ordering::SeqCst),
                0
            );
            let result = fixture.scope_async(
                |_| {},
                |mut inherited| async move {
                    (
                        *inherited.get_or_insert::<Number>(),
                        inherited
                            .get_or_insert::<Counter>()
                            .fetch_add(1, Ordering::SeqCst),
                    )
                },
            );
            assert_eq!(futures_executor::block_on(result), (parameter, 1));
            assert_eq!(fixture.get_or_insert::<Counter>().load(Ordering::SeqCst), 2);
        }
    }
}

trait Greeter: Any {
    fn greeting(&self) -> &'static str;
}
struct FixtureGreeter;
impl Greeter for FixtureGreeter {
    fn greeting(&self) -> &'static str {
        "fixture"
    }
}
struct OtherGreeter;

// An unchecked cast, wrong target check or consuming mismatch breaks safe recovery.
#[test]
fn checked_mock_interface_mismatch_preserves_usable_interface() {
    let interface: &dyn Greeter = &FixtureGreeter;
    let matched = try_dependency_mock::<FixtureGreeter>(interface).unwrap();
    assert_eq!(matched.greeting(), "fixture");
    let mismatch = try_dependency_mock::<OtherGreeter>(interface)
        .err()
        .unwrap();
    assert_eq!(mismatch.expected_type_id(), TypeId::of::<OtherGreeter>());
    assert_eq!(mismatch.actual_type_id(), TypeId::of::<FixtureGreeter>());
    assert_eq!(interface.greeting(), "fixture");
    assert_eq!(
        try_dependency_mock::<FixtureGreeter>(interface)
            .unwrap()
            .greeting(),
        "fixture"
    );
}

// Failure diagnostics must identify the requested type without exposing the erased value.
#[test]
fn checked_mock_failure_diagnostics_preserve_metadata_without_private_values() {
    let private = String::from("private-interface-value-7319");
    let interface: &dyn Any = &private;
    let mismatch = try_dependency_mock::<OtherGreeter>(interface)
        .err()
        .unwrap();
    let expected = std::any::type_name::<OtherGreeter>();
    assert_eq!(mismatch.expected_type_name(), expected);
    assert_eq!(mismatch.expected_type_id(), TypeId::of::<OtherGreeter>());
    assert_eq!(mismatch.actual_type_id(), TypeId::of::<String>());
    let display = mismatch.to_string();
    assert!(display.contains(expected));
    assert!(display.ends_with(&mismatch.backtrace().to_string()));
    for diagnostic in [display, format!("{mismatch:?}")] {
        assert!(!diagnostic.contains(private.as_str()));
    }
    assert!(std::error::Error::source(&mismatch).is_none());
    assert!(std::ptr::eq(
        try_dependency_mock::<String>(interface).unwrap(),
        std::ptr::from_ref(&private),
    ));
    assert_eq!(private, "private-interface-value-7319");
}

struct Selected;
impl DependencyKey for Selected {
    type Value = Arc<Resource>;
}

/// Sends the effective dependency outside the reducer so the test can observe its lifetime.
struct Capture(async_channel::Sender<Arc<Resource>>);
impl Reducer for Capture {
    type State = ();
    type Action = ();

    fn reduce(&mut self, (): &mut (), (): &(), context: &mut ReduceContext<'_>) -> Effect<()> {
        let selected = context.capture_dependency::<Selected>();
        let transport = self.0.clone();
        Effect::run(move |_| async move {
            transport.send(selected).await.unwrap();
        })
    }
}

// Capturing store defaults instead of the effective lexical layer returns the wrong resource.
#[test]
fn capture_retains_selected_feature_value_after_context_and_store_are_gone() {
    let runtime = support::ManualRuntime::new();
    let root_drops = Arc::new(AtomicUsize::new(0));
    let selected_drops = Arc::new(AtomicUsize::new(0));
    let (transport, received) = async_channel::bounded(1);
    let selected = Arc::new(Resource(Arc::clone(&selected_drops)));
    let witness = Arc::downgrade(&selected);
    let store = Store::builder(
        (),
        with_dependency::<Selected, _>(selected, Capture(transport)),
        runtime.runtime(),
    )
    .with_dependency::<Selected>(Arc::new(Resource(Arc::clone(&root_drops))))
    .build()
    .unwrap();
    runtime.start_all();
    let task = futures_executor::block_on(store.send(())).unwrap();
    runtime.wait_for_queued_task();
    runtime.start_all();
    let captured = futures_executor::block_on(received.recv()).unwrap();
    assert!(Arc::ptr_eq(&captured, &witness.upgrade().unwrap()));
    futures_executor::block_on(task.finish());
    futures_executor::block_on(store.shutdown()).unwrap();
    drop(store);
    for result in runtime.join_all() {
        result.unwrap();
    }
    assert_eq!(root_drops.load(Ordering::SeqCst), 1);
    assert_eq!(selected_drops.load(Ordering::SeqCst), 0);
    drop(captured);
    assert_eq!(selected_drops.load(Ordering::SeqCst), 1);
}

/// Checks captured senders against optional, enum-case, and keyed child replacement.
mod authority {
    use super::*;
    use rca_core::{
        ActionPath, CaseStatePath, EffectSender, KeyedAction, KeyedActionPath, KeyedState,
        KeyedStatePath, OptionalStatePath, for_each, if_case, if_some,
    };

    struct SenderKey;
    impl DependencyKey for SenderKey {
        type Value = EffectSender<ChildAction>;
    }

    enum Case {
        Absent,
        Present(usize),
    }
    struct State {
        optional: Option<usize>,
        case: Case,
        keyed: KeyedState<u8, usize>,
    }
    enum Action {
        Optional(ChildAction),
        Case(ChildAction),
        Keyed(KeyedAction<u8, ChildAction>),
        Remove,
        Insert,
    }
    enum ChildAction {
        Start,
        Output,
        External,
    }
    struct Parent;
    impl Reducer for Parent {
        type State = State;
        type Action = Action;

        fn reduce(
            &mut self,
            state: &mut State,
            action: &Action,
            _: &mut ReduceContext<'_>,
        ) -> Effect<Action> {
            match action {
                Action::Remove => {
                    state.optional = None;
                    state.case = Case::Absent;
                    state.keyed.remove(&7);
                }
                Action::Insert => {
                    state.optional = Some(0);
                    state.case = Case::Present(0);
                    state.keyed.insert(7, 0);
                }
                Action::Optional(_) | Action::Case(_) | Action::Keyed(_) => {}
            }
            Effect::none()
        }
    }

    struct Child(async_channel::Sender<DependencyValues>);
    impl Reducer for Child {
        type State = usize;
        type Action = ChildAction;

        fn reduce(
            &mut self,
            state: &mut usize,
            action: &ChildAction,
            context: &mut ReduceContext<'_>,
        ) -> Effect<ChildAction> {
            match action {
                ChildAction::Start => {
                    let selected = context.with_dependency_override::<Number, _>(3, |context| {
                        context.capture_dependency::<Number>()
                    });
                    let fixture = values(selected);
                    let transport = self.0.clone();
                    Effect::run(move |sender| {
                        fixture.scope_async(
                            |child| child.set::<SenderKey>(sender),
                            |child| async move {
                                // Explicit transport exports granted output authority; the Store owns shutdown.
                                transport.send(child).await.unwrap();
                                pending::<()>().await;
                            },
                        )
                    })
                }
                ChildAction::Output => {
                    *state += 100;
                    Effect::none()
                }
                ChildAction::External => {
                    *state += 1;
                    Effect::none()
                }
            }
        }
    }

    // Renewing captured authority, or confusing root input with old output, breaks these controls.
    #[test]
    fn owned_fixtures_preserve_revocation_for_optional_case_and_keyed_outputs() {
        let runtime = support::ManualRuntime::new();
        let (transport, received) = async_channel::bounded(3);
        let optional = OptionalStatePath::new(
            |state: &State| state.optional.as_ref(),
            |state: &mut State| state.optional.as_mut(),
        );
        let optional_action = ActionPath::new(
            |action: &Action| {
                if let Action::Optional(child) = action {
                    Some(child)
                } else {
                    None
                }
            },
            Action::Optional,
        );
        let case = CaseStatePath::new(
            |state: &State| {
                if let Case::Present(child) = &state.case {
                    Some(child)
                } else {
                    None
                }
            },
            |state: &mut State| {
                if let Case::Present(child) = &mut state.case {
                    Some(child)
                } else {
                    None
                }
            },
        );
        let case_action = ActionPath::new(
            |action: &Action| {
                if let Action::Case(child) = action {
                    Some(child)
                } else {
                    None
                }
            },
            Action::Case,
        );
        let keyed = KeyedStatePath::new(|s: &State| &s.keyed, |s: &mut State| &mut s.keyed);
        let keyed_action = KeyedActionPath::new(
            |action: &Action| {
                if let Action::Keyed(child) = action {
                    Some(child)
                } else {
                    None
                }
            },
            Action::Keyed,
        );
        let reducer = for_each(
            if_case(
                if_some(Parent, optional, optional_action, Child(transport.clone())),
                case,
                case_action,
                Child(transport.clone()),
            ),
            keyed,
            keyed_action,
            Child(transport),
        );
        let store = Store::builder(
            State {
                optional: Some(0),
                case: Case::Present(0),
                keyed: [(7, 0)].into_iter().collect(),
            },
            with_dependency::<Number, _>(2, reducer),
            runtime.runtime(),
        )
        .with_dependency::<Number>(1)
        .build()
        .unwrap();
        runtime.start_all();
        futures_executor::block_on(async {
            let old = [
                store.scope_some(optional, optional_action).await.unwrap(),
                store.scope_case(case, case_action).await.unwrap(),
                store.scope_keyed(keyed, keyed_action, 7).await.unwrap(),
            ];
            check_authority(&store, &runtime, old, received).await;
            store.shutdown().await.unwrap();
        });
        for result in runtime.join_all() {
            result.unwrap();
        }
    }

    /// Checks active output, revoked output, and fresh input for the same three child routes.
    async fn check_authority(
        store: &Store<State, Action>,
        runtime: &support::ManualRuntime,
        old: [rca_core::ScopedStore<usize, ChildAction>; 3],
        received: async_channel::Receiver<DependencyValues>,
    ) {
        let mut captures = Vec::new();
        let mut tasks = Vec::new();
        for handle in &old {
            tasks.push(handle.send(ChildAction::Start).await.unwrap());
            runtime.wait_for_queued_task();
            runtime.start_all();
            let mut fixture = received.recv().await.unwrap();
            assert_eq!(*fixture.get_or_insert::<Number>(), 3);
            fixture
                .get_or_insert::<SenderKey>()
                .send(ChildAction::Output)
                .await
                .unwrap();
            assert_eq!(handle.read(|value| *value).await.unwrap(), 100);
            captures.push(fixture);
        }
        store.send(Action::Remove).await.unwrap().finish().await;
        store.send(Action::Insert).await.unwrap().finish().await;
        for (handle, mut fixture) in old.into_iter().zip(captures) {
            let error = fixture
                .get_or_insert::<SenderKey>()
                .send(ChildAction::Output)
                .await
                .unwrap_err();
            assert!(error.is_cancelled());
            let error = handle.send(ChildAction::Output).await.unwrap_err();
            assert!(error.is_scope_ended());
        }
        for task in tasks {
            task.finish().await;
        }
        assert_eq!(
            store
                .read(|s| (
                    s.optional,
                    matches!(s.case, Case::Present(0)),
                    s.keyed.get(&7).copied()
                ))
                .await
                .unwrap(),
            (Some(0), true, Some(0))
        );
        // Same public routing is valid as fresh root input, not inherited output authority.
        for action in [
            Action::Optional(ChildAction::External),
            Action::Case(ChildAction::External),
            Action::Keyed(KeyedAction::new(7, ChildAction::External)),
        ] {
            store.send(action).await.unwrap().finish().await;
        }
        assert_eq!(
            store
                .read(|s| (
                    s.optional,
                    matches!(s.case, Case::Present(1)),
                    s.keyed.get(&7).copied()
                ))
                .await
                .unwrap(),
            (Some(1), true, Some(1))
        );
    }
}
