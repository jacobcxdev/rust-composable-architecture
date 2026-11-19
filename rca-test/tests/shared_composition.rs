//! Qualifies disjoint private and parent-owned shared reducer inputs.

use std::cell::Cell;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use futures_executor::block_on;
use rca_core::{
    ActionPath, Effect, Issue, ReduceContext, Reducer, SharedReducer, StatePath, Store, combine,
    scope, scope_shared,
};
use rca_test::{DeterministicRuntime, TestStore};

/// Holds move-only, non-`Debug`, non-`Sync` state for one child.
struct Private(Cell<u64>);

/// Supplies one parent-owned value without cloning it into each child.
struct Settings(Cell<u64>);

/// Keeps both private inputs, shared settings and an unrelated value observable.
struct State {
    left: Private,
    right: Private,
    settings: Settings,
    unrelated: u64,
}

impl Default for State {
    fn default() -> Self {
        Self {
            left: Private(Cell::new(0)),
            right: Private(Cell::new(0)),
            settings: Settings(Cell::new(0)),
            unrelated: 0,
        }
    }
}

/// Exercises mutation, effect ownership, issue reporting and a deliberate reducer panic.
enum ChildAction {
    Mutate(u64),
    Start(u64),
    Cancel,
    Panic,
    Report,
    PrivateOnly,
}

/// Routes to exactly one child or bypasses both shared projections.
enum Action {
    Left(ChildAction),
    Right(ChildAction),
    Unrelated,
}

/// Mutates disjoint inputs and captures only owned values in effects.
struct Child;
impl SharedReducer for Child {
    type State = Private;
    type Shared = Settings;
    type Action = ChildAction;

    fn reduce(
        &mut self,
        state: &mut Private,
        shared: &mut Settings,
        action: &ChildAction,
        context: &mut ReduceContext<'_>,
    ) -> Effect<ChildAction> {
        match action {
            ChildAction::PrivateOnly => state.0.set(state.0.get() + 1),
            ChildAction::Mutate(digit) => {
                state.0.set(shared.0.get() + digit);
                shared.0.set(shared.0.get() * 10 + digit);
            }
            ChildAction::Start(digit) => {
                return Effect::send_after(Duration::from_secs(1), ChildAction::Mutate(*digit))
                    .cancellable("pending");
            }
            ChildAction::Cancel => return Effect::cancel("pending"),
            ChildAction::Panic => {
                state.0.set(99);
                shared.0.set(99);
                panic!("shared child panic");
            }
            ChildAction::Report => {
                context
                    .report_issue(Issue::new("shared-sync", "borrowed inputs"))
                    .unwrap();
                let reporter = context.issue_reporter();
                return Effect::run(move |_| async move {
                    reporter
                        .report(Issue::new("shared-async", "owned capture"))
                        .unwrap();
                });
            }
        }
        Effect::none()
    }
}

/// Changes only unrelated state so distinct observations can reject false notifications.
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
        if matches!(action, Action::Unrelated) {
            state.unrelated += 1;
        }
        Effect::none()
    }
}

/// Selects left-child actions without granting access on unrelated actions.
fn left_path() -> ActionPath<
    Action,
    ChildAction,
    impl Fn(&Action) -> Option<&ChildAction>,
    impl Fn(ChildAction) -> Action + Clone,
> {
    ActionPath::new(
        |action: &Action| match action {
            Action::Left(child) => Some(child),
            _ => None,
        },
        Action::Left,
    )
}

/// Preserves right-child identity when embedding delayed output.
fn right_path() -> ActionPath<
    Action,
    ChildAction,
    impl Fn(&Action) -> Option<&ChildAction>,
    impl Fn(ChildAction) -> Action + Clone,
> {
    ActionPath::new(
        |action: &Action| match action {
            Action::Right(child) => Some(child),
            _ => None,
        },
        Action::Right,
    )
}

/// Borrows the same settings through two distinct required child scopes.
fn reducer() -> impl Reducer<State = State, Action = Action> {
    combine((
        scope_shared(
            |state: &mut State| (&mut state.left, &mut state.settings),
            left_path(),
            Child,
        ),
        scope_shared(
            |state: &mut State| (&mut state.right, &mut state.settings),
            right_path(),
            Child,
        ),
        Parent,
    ))
}

/// Selects both private inputs, shared settings and unrelated state independently.
fn snapshot(state: &State) -> (u64, u64, u64, u64) {
    (
        state.left.0.get(),
        state.right.0.get(),
        state.settings.0.get(),
        state.unrelated,
    )
}

// Catches copied settings, wrong private routing and delayed cross-child visibility.
#[test]
fn children_mutate_private_state_and_immediately_share_one_settings_value() {
    block_on(async {
        let mut store = TestStore::projected(State::default(), reducer(), snapshot);
        let _ = store
            .send_with(Action::Left(ChildAction::Mutate(1)), |s| *s = (1, 0, 1, 0))
            .await
            .unwrap();
        let _ = store
            .send_with(Action::Right(ChildAction::Mutate(2)), |s| {
                *s = (1, 3, 12, 0);
            })
            .await
            .unwrap();
        let _ = store
            .send_with(Action::Left(ChildAction::Mutate(3)), |s| {
                *s = (15, 3, 123, 0);
            })
            .await
            .unwrap();
        store.finish().await.unwrap();
    });
}

// Catches omission of either input from ordinary full-root snapshot comparison.
#[test]
fn full_root_snapshots_cover_private_and_shared_inputs() {
    /// Mutates both inputs so ordinary tuple equality must account for each.
    struct FullChild;
    impl SharedReducer for FullChild {
        type State = u64;
        type Shared = u64;
        type Action = ();
        fn reduce(
            &mut self,
            state: &mut u64,
            shared: &mut u64,
            (): &(),
            _: &mut ReduceContext<'_>,
        ) -> Effect<()> {
            *state += *shared;
            *shared += 1;
            Effect::none()
        }
    }
    let reducer = scope_shared(
        |s: &mut (u64, u64)| (&mut s.0, &mut s.1),
        ActionPath::new(|a: &()| Some(a), |a| a),
        FullChild,
    );
    block_on(async {
        let mut store = TestStore::new((0, 2), reducer);
        let _ = store.send_with((), |s| *s = (2, 3)).await.unwrap();
        let _ = store.send_with((), |s| *s = (5, 4)).await.unwrap();
        store.finish().await.unwrap();
    });
}

// Catches a projected assertion that silently excludes either reducer input.
#[test]
#[should_panic(expected = "state mismatch")]
fn send_assertion_cannot_omit_private_mutation() {
    block_on(async {
        let mut store = TestStore::projected(State::default(), reducer(), snapshot);
        let _ = store
            .send_with(Action::Left(ChildAction::Mutate(1)), |s| s.2 = 1)
            .await
            .unwrap();
    });
}
#[test]
#[should_panic(expected = "state mismatch")]
fn send_assertion_cannot_omit_shared_mutation() {
    block_on(async {
        let mut store = TestStore::projected(State::default(), reducer(), snapshot);
        let _ = store
            .send_with(Action::Left(ChildAction::Mutate(1)), |s| s.0 = 1)
            .await
            .unwrap();
    });
}
#[test]
#[should_panic(expected = "state mismatch")]
fn receive_assertion_cannot_omit_private_mutation() {
    block_on(async {
        let mut store = TestStore::projected(State::default(), reducer(), snapshot);
        let _ = store
            .send(Action::Left(ChildAction::Start(1)))
            .await
            .unwrap();
        store.advance(Duration::from_secs(1)).await.unwrap();
        let _ = store
            .receive_with(
                |a| matches!(a, Action::Left(ChildAction::Mutate(1))),
                |s| s.2 = 1,
            )
            .await
            .unwrap();
    });
}
#[test]
#[should_panic(expected = "state mismatch")]
fn receive_assertion_cannot_omit_shared_mutation() {
    block_on(async {
        let mut store = TestStore::projected(State::default(), reducer(), snapshot);
        let _ = store
            .send(Action::Left(ChildAction::Start(1)))
            .await
            .unwrap();
        store.advance(Duration::from_secs(1)).await.unwrap();
        let _ = store
            .receive_with(
                |a| matches!(a, Action::Left(ChildAction::Mutate(1))),
                |s| s.0 = 1,
            )
            .await
            .unwrap();
    });
}

// Catches partial root commits and shared notifications caused by private/unrelated work.
#[test]
fn root_observation_is_coherent_and_settings_observation_is_distinct() {
    let host = DeterministicRuntime::new();
    let store = Store::new(State::default(), reducer(), host.runtime());
    block_on(host.run(async {
        let mut root = store.observe(snapshot).await.unwrap();
        let mut settings = store
            .observe_distinct(|s| s.settings.0.get())
            .await
            .unwrap();
        assert_eq!(root.next().await, Some((0, 0, 0, 0)));
        assert_eq!(settings.next().await, Some(0));
        let _ = store
            .send(Action::Left(ChildAction::Mutate(1)))
            .await
            .unwrap();
        assert_eq!(root.next().await, Some((1, 0, 1, 0)));
        assert_eq!(settings.next().await, Some(1));
        let _ = store
            .send(Action::Left(ChildAction::PrivateOnly))
            .await
            .unwrap();
        assert_eq!(root.next().await, Some((2, 0, 1, 0)));
        assert!(matches!(
            std::pin::pin!(settings.next())
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop())),
            Poll::Pending
        ));
        let _ = store.send(Action::Unrelated).await.unwrap();
        assert_eq!(root.next().await, Some((2, 0, 1, 1)));
        let mut next = std::pin::pin!(settings.next());
        assert!(matches!(
            next.as_mut().poll(&mut Context::from_waker(Waker::noop())),
            Poll::Pending
        ));
        store.shutdown().await.unwrap();
    }))
    .unwrap();
}

// Catches shared cancellation identities and incorrect effect action embedding.
#[test]
fn equal_user_cancellation_ids_remain_isolated_between_children() {
    block_on(async {
        let mut store = TestStore::projected(State::default(), reducer(), snapshot);
        let _ = store
            .send(Action::Left(ChildAction::Start(1)))
            .await
            .unwrap();
        let _ = store
            .send(Action::Right(ChildAction::Start(2)))
            .await
            .unwrap();
        let _ = store.send(Action::Left(ChildAction::Cancel)).await.unwrap();
        store.advance(Duration::from_secs(1)).await.unwrap();
        let _ = store
            .receive_with(
                |a| matches!(a, Action::Right(ChildAction::Mutate(2))),
                |s| *s = (0, 2, 2, 0),
            )
            .await
            .unwrap();
        store.finish().await.unwrap();
    });
}

// A nonmatching action must not grant any mutable projection access.
#[test]
fn unmatched_action_does_not_invoke_split() {
    let reducer = scope_shared(
        |_: &mut State| -> (&mut Private, &mut Settings) { panic!("unmatched split invoked") },
        left_path(),
        Child,
    );
    assert_eq!(format!("{reducer:?}"), "SharedScope { .. }");
    block_on(async {
        let mut store = TestStore::projected(State::default(), reducer, snapshot);
        let _ = store.send(Action::Unrelated).await.unwrap();
        store.finish().await.unwrap();
    });
}

// Catches loss of ordinary outer composition, effect embedding or scoped issue endpoints.
#[test]
fn ordinary_outer_scope_preserves_shared_effects_and_issue_endpoint() {
    let nested = scope(
        StatePath::new(|s: &(State, u64)| &s.0, |s: &mut (State, u64)| &mut s.0),
        ActionPath::new(|a: &Action| Some(a), |a| a),
        reducer(),
    );
    block_on(async {
        let mut store =
            TestStore::projected((State::default(), 7), nested, |s| (snapshot(&s.0), s.1));
        let _ = store.send(Action::Left(ChildAction::Report)).await.unwrap();
        assert_eq!(
            store
                .expect_issue(|i| i.code == "shared-sync")
                .await
                .unwrap()
                .message,
            "borrowed inputs"
        );
        assert_eq!(
            store
                .expect_issue(|i| i.code == "shared-async")
                .await
                .unwrap()
                .message,
            "owned capture"
        );
        let _ = store
            .send(Action::Right(ChildAction::Start(2)))
            .await
            .unwrap();
        store.advance(Duration::from_secs(1)).await.unwrap();
        let _ = store
            .receive_with(
                |a| matches!(a, Action::Right(ChildAction::Mutate(2))),
                |s| s.0 = (0, 2, 2, 0),
            )
            .await
            .unwrap();
        store.finish().await.unwrap();
    });
}

// A child panic aborts the actor; no rollback or borrowed-state recovery is promised.
#[test]
fn shared_child_panic_closes_root_authority() {
    let host = DeterministicRuntime::new();
    let store = Store::new(State::default(), reducer(), host.runtime());
    let receipt = store.try_send(Action::Left(ChildAction::Panic)).unwrap();
    let panic = catch_unwind(AssertUnwindSafe(|| host.run_until_stalled())).unwrap_err();
    assert_eq!(panic.downcast_ref::<&str>(), Some(&"shared child panic"));
    assert!(store.is_closed());
    block_on(async {
        receipt
            .await
            .expect_err("panicking reduction cannot commit");
        store
            .read(snapshot)
            .await
            .expect_err("root authority closed");
        store
            .send(Action::Unrelated)
            .await
            .expect_err("root rejects later actions");
        store
            .shutdown_into_state()
            .await
            .err()
            .expect("partial state is not recoverable");
    });
}
