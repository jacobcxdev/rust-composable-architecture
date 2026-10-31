//! Checks generated child bindings after removal, reinsertion and keyed reordering.
//!
//! The `d54-authority-runtime` consumer runs these tests against actual Stores.

#![cfg(test)]

#[path = "../../../../rca-core/tests/support/thread_runtime.rs"]
mod thread_runtime;

use rca_core::{Effect, EffectSender, KeyedAction, KeyedState, ReduceContext, Reducer, Store};
use std::sync::mpsc;
use thread_runtime::ThreadRuntime;

/// Captures an old effect sender or changes the active child state.
pub enum ChildAction {
    Capture(mpsc::Sender<EffectSender<ChildAction>>),
    Increment,
}
/// Keeps captured work pending until its child installation ends.
pub struct Counter;
impl Reducer for Counter {
    type State = usize;
    type Action = ChildAction;
    fn reduce(
        &mut self,
        state: &mut usize,
        action: &ChildAction,
        _: &mut ReduceContext<'_>,
    ) -> Effect<ChildAction> {
        match action {
            ChildAction::Capture(captured) => {
                let captured = captured.clone();
                Effect::run(move |sender| async move {
                    captured.send(sender).unwrap();
                    core::future::pending::<()>().await;
                })
            }
            ChildAction::Increment => {
                *state += 1;
                Effect::none()
            }
        }
    }
}

mod optional {
    use super::*;

    #[rca_macros::reducer(
        state(generated(pub struct State {
            #[rca(optional)] #[reducer(child = counter)] pub counter: Option<_>,
        })),
        action(generated(pub enum Action {
            #[reducer(child = counter)] Counter(_), Remove, Insert,
        })),
        body(supplied(body)),
        child(counter, reducer = Counter, kind = optional,
            state = StatePaths::counter(), action = ActionPaths::counter(),
            bind(pub fn bind_counter(parent: Parent, child: Counter)),
        ),
    )]
    pub struct Feature<R> {
        pub body: R,
    }

    pub struct Parent;
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
                Action::Remove => state.counter = None,
                Action::Insert => state.counter = Some(0),
                Action::Counter(_) => (),
            }
            Effect::none()
        }
    }
}

mod case {
    use super::*;

    #[rca_macros::reducer(
        state(generated(pub enum State {
            #[reducer(child = counter)] Active { value: _ }, Idle,
        })),
        action(generated(pub enum Action {
            #[reducer(child = counter)] Counter(_), Remove, Insert,
        })),
        body(supplied(body)),
        child(counter, reducer = Counter, kind = case,
            state = StatePaths::value(), action = ActionPaths::counter(),
            bind(pub fn bind_counter(parent: Parent, child: Counter)),
        ),
    )]
    pub struct Feature<R> {
        pub body: R,
    }

    pub struct Parent;
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
                Action::Remove => *state = State::Idle,
                Action::Insert => *state = State::Active { value: 0 },
                Action::Counter(_) => (),
            }
            Effect::none()
        }
    }
}

mod keyed {
    use super::*;

    #[rca_macros::reducer(
        state(generated(pub struct State {
            #[rca(keyed)] #[reducer(child = counter)] pub rows: KeyedState<u8, _>,
        })),
        action(generated(pub enum Action {
            #[rca(keyed)] #[reducer(child = counter)] Row(KeyedAction<u8, _>),
            Remove, Insert, Reorder,
        })),
        body(supplied(body)),
        child(counter, reducer = Counter, kind = keyed,
            state = StatePaths::rows(), action = ActionPaths::row(),
            bind(pub fn bind_counter(parent: Parent, child: Counter)),
        ),
    )]
    pub struct Feature<R> {
        pub body: R,
    }

    pub struct Parent;
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
                    state.rows.remove(&1);
                }
                Action::Insert => state.rows.insert(1, 0),
                Action::Reorder => state.rows.move_index(1, 0),
                Action::Row(_) => (),
            }
            Effect::none()
        }
    }
}

#[test]
fn macro_optional_binding_fences_effects_callbacks_and_subscription_after_equal_reinsert() {
    let runtime = ThreadRuntime::new();
    let store = Store::new(
        optional::State { counter: Some(0) },
        optional::Feature {
            body: optional::bind_counter(optional::Parent, Counter),
        },
        runtime.runtime(),
    );
    let (send, receive) = mpsc::channel();
    futures_executor::block_on(async {
        let old_handle = store
            .scope_some(
                optional::StatePaths::counter(),
                optional::ActionPaths::counter(),
            )
            .await
            .unwrap();
        let mut values = old_handle.observe(|state| *state).await.unwrap();
        assert_eq!(values.next().await, Some(0));
        let work = store
            .send(optional::Action::Counter(ChildAction::Capture(send)))
            .await
            .unwrap();
        let output = receive
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        store
            .send(optional::Action::Remove)
            .await
            .unwrap()
            .finish()
            .await;
        work.finish().await;
        store
            .send(optional::Action::Insert)
            .await
            .unwrap()
            .finish()
            .await;
        assert!(
            output
                .send(ChildAction::Increment)
                .await
                .unwrap_err()
                .is_cancelled()
        );
        assert!(
            old_handle
                .send(ChildAction::Increment)
                .await
                .unwrap_err()
                .is_scope_ended()
        );
        assert_eq!(values.next().await, None);
        assert_eq!(store.read(|state| state.counter).await.unwrap(), Some(0));
        let fresh = store
            .scope_some(
                optional::StatePaths::counter(),
                optional::ActionPaths::counter(),
            )
            .await
            .unwrap();
        fresh
            .send(ChildAction::Increment)
            .await
            .unwrap()
            .finish()
            .await;
        assert_eq!(fresh.read(|state| *state).await.unwrap(), 1);
        store.shutdown().await.unwrap();
    });
}

#[test]
fn macro_case_binding_fences_effects_callbacks_and_subscription_after_equal_reinsert() {
    let runtime = ThreadRuntime::new();
    let store = Store::new(
        case::State::Active { value: 0 },
        case::Feature {
            body: case::bind_counter(case::Parent, Counter),
        },
        runtime.runtime(),
    );
    let (send, receive) = mpsc::channel();
    futures_executor::block_on(async {
        let old_handle = store
            .scope_case(case::StatePaths::value(), case::ActionPaths::counter())
            .await
            .unwrap();
        let mut values = old_handle.observe(|state| *state).await.unwrap();
        assert_eq!(values.next().await, Some(0));
        let work = store
            .send(case::Action::Counter(ChildAction::Capture(send)))
            .await
            .unwrap();
        let output = receive
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        store
            .send(case::Action::Remove)
            .await
            .unwrap()
            .finish()
            .await;
        work.finish().await;
        store
            .send(case::Action::Insert)
            .await
            .unwrap()
            .finish()
            .await;
        assert!(
            output
                .send(ChildAction::Increment)
                .await
                .unwrap_err()
                .is_cancelled()
        );
        assert!(
            old_handle
                .send(ChildAction::Increment)
                .await
                .unwrap_err()
                .is_scope_ended()
        );
        assert_eq!(values.next().await, None);
        let fresh = store
            .scope_case(case::StatePaths::value(), case::ActionPaths::counter())
            .await
            .unwrap();
        assert_eq!(fresh.read(|state| *state).await.unwrap(), 0);
        fresh
            .send(ChildAction::Increment)
            .await
            .unwrap()
            .finish()
            .await;
        assert_eq!(fresh.read(|state| *state).await.unwrap(), 1);
        store.shutdown().await.unwrap();
    });
}

#[test]
fn macro_keyed_binding_fences_removed_row_but_preserves_reordered_survivor() {
    let runtime = ThreadRuntime::new();
    let mut rows = KeyedState::new();
    rows.insert(1, 0);
    rows.insert(2, 10);
    let store = Store::new(
        keyed::State { rows },
        keyed::Feature {
            body: keyed::bind_counter(keyed::Parent, Counter),
        },
        runtime.runtime(),
    );
    let (send, receive) = mpsc::channel();
    futures_executor::block_on(async {
        let old_handle = store
            .scope_keyed(keyed::StatePaths::rows(), keyed::ActionPaths::row(), 1)
            .await
            .unwrap();
        let survivor = store
            .scope_keyed(keyed::StatePaths::rows(), keyed::ActionPaths::row(), 2)
            .await
            .unwrap();
        let mut old_values = old_handle.observe(|state| *state).await.unwrap();
        let mut survivor_values = survivor.observe_distinct(|state| *state).await.unwrap();
        assert_eq!(old_values.next().await, Some(0));
        assert_eq!(survivor_values.next().await, Some(10));
        let work = store
            .send(keyed::Action::Row(KeyedAction::new(
                1,
                ChildAction::Capture(send),
            )))
            .await
            .unwrap();
        let output = receive
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        store
            .send(keyed::Action::Remove)
            .await
            .unwrap()
            .finish()
            .await;
        work.finish().await;
        store
            .send(keyed::Action::Insert)
            .await
            .unwrap()
            .finish()
            .await;
        store
            .send(keyed::Action::Reorder)
            .await
            .unwrap()
            .finish()
            .await;
        assert!(
            output
                .send(ChildAction::Increment)
                .await
                .unwrap_err()
                .is_cancelled()
        );
        assert!(
            old_handle
                .send(ChildAction::Increment)
                .await
                .unwrap_err()
                .is_scope_ended()
        );
        assert_eq!(old_values.next().await, None);
        survivor
            .send(ChildAction::Increment)
            .await
            .unwrap()
            .finish()
            .await;
        assert_eq!(survivor_values.next().await, Some(11));
        assert_eq!(
            store
                .read(|state| (state.rows[&1], state.rows[&2]))
                .await
                .unwrap(),
            (0, 11)
        );
        store.shutdown().await.unwrap();
    });
}
