//! Checks cancellation hand-off without removing structural ownership or explicit keys.

use std::time::Duration;

use futures_executor::block_on;
use rca_core::{ActionPath, Effect, OptionalStatePath, ReduceContext, Reducer, ReducerExt};
use rca_test::TestStore;

/// Records the causal output sequence and controls its cancellation or child removal.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Action {
    Start,
    Child,
    First,
    Second,
    Cancel,
    CancelLocal,
    Remove,
    Nested,
}

/// Selects where inherited cancellation is removed relative to sequence boundaries.
#[derive(Clone, Copy)]
enum Mode {
    WholeSequence,
    Element,
    Merge,
    Nested,
    Explicit,
    Deferred,
}

/// Creates a child output trace while keeping its explicit cancellation keys separate.
struct Feature(Mode);

impl Reducer for Feature {
    type State = Vec<Action>;
    type Action = Action;
    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &Action,
        _: &mut ReduceContext<'_>,
    ) -> Effect<Action> {
        state.push(*action);
        match action {
            Action::Start => Effect::send(Action::Child).cancellable("source"),
            Action::Child => {
                let first = Effect::send_after(Duration::from_secs(1), Action::First);
                let second = Effect::send_after(Duration::from_secs(1), Action::Second);
                match self.0 {
                    Mode::WholeSequence => Effect::concatenate([first, second])
                        .without_inherited_cancellation()
                        .map(|a| a),
                    Mode::Element => {
                        Effect::concatenate([first.without_inherited_cancellation(), second])
                    }
                    Mode::Merge => {
                        Effect::merge([first.without_inherited_cancellation().map(|a| a), second])
                    }
                    Mode::Nested => Effect::concatenate([
                        Effect::none(),
                        Effect::concatenate([
                            first
                                .without_inherited_cancellation()
                                .without_inherited_cancellation(),
                            second,
                        ])
                        .without_inherited_cancellation(),
                    ])
                    .without_inherited_cancellation(),
                    Mode::Explicit => Effect::concatenate([first, second])
                        .without_inherited_cancellation()
                        .cancellable("source"),
                    Mode::Deferred => {
                        Effect::concatenate([first, second.without_inherited_cancellation()])
                            .cancellable("local")
                    }
                }
            }
            Action::First => Effect::send(Action::Nested),
            Action::Cancel => Effect::cancel("source"),
            Action::CancelLocal => Effect::cancel("local"),
            _ => Effect::none(),
        }
    }
}

// Missing prefix retention/filtering breaks timer delivery, continuation or merge isolation.
#[test]
fn sequence_and_merge_boundaries_keep_exact_ownership() {
    for (mode, first, second) in [
        (Mode::WholeSequence, true, true),
        (Mode::Element, true, false),
        (Mode::Merge, true, false),
        (Mode::Nested, true, true),
        (Mode::Explicit, false, false),
    ] {
        block_on(async {
            let mut store = TestStore::new(Vec::new(), Feature(mode));
            let root = store
                .send_with(Action::Start, |s| s.push(Action::Start))
                .await
                .unwrap();
            let _ = store
                .receive_with(|a| *a == Action::Child, |s| s.push(Action::Child))
                .await
                .unwrap();
            let _ = store
                .send_with(Action::Cancel, |s| s.push(Action::Cancel))
                .await
                .unwrap();
            store.advance(Duration::from_secs(1)).await.unwrap();
            if first {
                let _ = store
                    .receive_with(|a| *a == Action::First, |s| s.push(Action::First))
                    .await
                    .unwrap();
                let _ = store
                    .receive_with(|a| *a == Action::Nested, |s| s.push(Action::Nested))
                    .await
                    .unwrap();
            }
            store.advance(Duration::from_secs(1)).await.unwrap();
            if second {
                let _ = store
                    .receive_with(|a| *a == Action::Second, |s| s.push(Action::Second))
                    .await
                    .unwrap();
            }
            store.finish().await.unwrap();
            assert!(root.is_finished());
        });
    }
}

// A later sequence element must remember which tokens were inherited at admission.
#[test]
fn deferred_boundary_preserves_explicit_outer_generation() {
    for cancel_local in [false, true] {
        block_on(async {
            let mut store = TestStore::new(Vec::new(), Feature(Mode::Deferred));
            let _ = store
                .send_with(Action::Start, |s| s.push(Action::Start))
                .await
                .unwrap();
            let _ = store
                .receive_with(|a| *a == Action::Child, |s| s.push(Action::Child))
                .await
                .unwrap();
            store.advance(Duration::from_secs(1)).await.unwrap();
            let _ = store
                .receive_with(|a| *a == Action::First, |s| s.push(Action::First))
                .await
                .unwrap();
            let _ = store
                .receive_with(|a| *a == Action::Nested, |s| s.push(Action::Nested))
                .await
                .unwrap();
            let _ = store
                .send_with(Action::Cancel, |s| s.push(Action::Cancel))
                .await
                .unwrap();
            if cancel_local {
                let _ = store
                    .send_with(Action::CancelLocal, |s| s.push(Action::CancelLocal))
                    .await
                    .unwrap();
            }
            store.advance(Duration::from_secs(1)).await.unwrap();
            if !cancel_local {
                let _ = store
                    .receive_with(|a| *a == Action::Second, |s| s.push(Action::Second))
                    .await
                    .unwrap();
            }
            store.finish().await.unwrap();
        });
    }
}

/// Removes the optional child without routing removal through that child.
struct Parent;

impl Reducer for Parent {
    type State = Option<Vec<Action>>;
    type Action = Action;
    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &Action,
        _: &mut ReduceContext<'_>,
    ) -> Effect<Action> {
        if *action == Action::Remove {
            *state = None;
        }
        Effect::none()
    }
}

// Boundary must not turn child-owned timers into Store-global tasks.
#[test]
fn structural_removal_still_owns_transferred_sequence() {
    block_on(async {
        let reducer = Parent.if_some(
            OptionalStatePath::new(
                |s: &Option<Vec<Action>>| s.as_ref(),
                |s: &mut Option<Vec<Action>>| s.as_mut(),
            ),
            ActionPath::new(
                |a: &Action| if *a == Action::Remove { None } else { Some(a) },
                |a| a,
            ),
            Feature(Mode::WholeSequence),
        );
        let mut store = TestStore::new(Some(Vec::new()), reducer);
        let root = store
            .send_with(Action::Start, |s| s.as_mut().unwrap().push(Action::Start))
            .await
            .unwrap();
        let _ = store
            .receive_with(
                |a| *a == Action::Child,
                |s| s.as_mut().unwrap().push(Action::Child),
            )
            .await
            .unwrap();
        let _ = store
            .send_with(Action::Remove, |s| *s = None)
            .await
            .unwrap();
        store.advance(Duration::from_secs(3)).await.unwrap();
        store.finish().await.unwrap();
        assert!(root.is_finished());
    });
}

/// Removes one keyed lifetime without changing the surviving sibling.
struct Membership;

/// Routes child work by key and keeps membership changes at the parent boundary.
#[derive(Debug, Eq, PartialEq)]
enum KeyedAction {
    Row(rca_core::KeyedAction<u8, Action>),
    Remove(u8),
}

impl Reducer for Membership {
    type State = rca_core::KeyedState<u8, Vec<Action>>;
    type Action = KeyedAction;
    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &KeyedAction,
        _: &mut ReduceContext<'_>,
    ) -> Effect<KeyedAction> {
        if let KeyedAction::Remove(id) = action {
            state.remove(id);
        }
        Effect::none()
    }
}

/// Pairs a literal row identity with an owned child action.
fn row(id: u8, action: Action) -> KeyedAction {
    KeyedAction::Row(rca_core::KeyedAction::new(id, action))
}

// Transferred work remains keyed: removing one lifetime cannot cancel its sibling.
#[test]
fn keyed_removal_preserves_transferred_sibling() {
    block_on(async {
        let reducer = rca_core::for_each(
            Membership,
            rca_core::KeyedStatePath::new(
                |s: &rca_core::KeyedState<u8, Vec<Action>>| s,
                |s: &mut rca_core::KeyedState<u8, Vec<Action>>| s,
            ),
            rca_core::KeyedActionPath::new(
                |a: &KeyedAction| match a {
                    KeyedAction::Row(a) => Some(a),
                    KeyedAction::Remove(_) => None,
                },
                KeyedAction::Row,
            ),
            Feature(Mode::WholeSequence),
        );
        let mut initial = rca_core::KeyedState::new();
        initial.insert(1, Vec::new());
        initial.insert(2, Vec::new());
        let mut store = TestStore::new(initial, reducer);
        for id in [1, 2] {
            let _ = store
                .send_with(row(id, Action::Start), move |s| {
                    s.get_mut(&id).unwrap().push(Action::Start);
                })
                .await
                .unwrap();
            let _ = store
                .receive_with(
                    move |a| *a == row(id, Action::Child),
                    move |s| s.get_mut(&id).unwrap().push(Action::Child),
                )
                .await
                .unwrap();
        }
        let _ = store
            .send_with(KeyedAction::Remove(1), |s| {
                s.remove(&1);
            })
            .await
            .unwrap();
        store.advance(Duration::from_secs(1)).await.unwrap();
        let _ = store
            .receive_with(
                |a| *a == row(2, Action::First),
                |s| s.get_mut(&2).unwrap().push(Action::First),
            )
            .await
            .unwrap();
        let _ = store
            .receive_with(
                |a| *a == row(2, Action::Nested),
                |s| s.get_mut(&2).unwrap().push(Action::Nested),
            )
            .await
            .unwrap();
        store.advance(Duration::from_secs(1)).await.unwrap();
        let _ = store
            .receive_with(
                |a| *a == row(2, Action::Second),
                |s| s.get_mut(&2).unwrap().push(Action::Second),
            )
            .await
            .unwrap();
        store.finish().await.unwrap();
    });
}
