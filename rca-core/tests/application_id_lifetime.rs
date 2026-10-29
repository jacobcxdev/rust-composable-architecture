//! Keeps repeated application IDs separate from keyed child lifetimes.

mod support;

/// Bounds the native lifecycle scenario with the shared 60-second child watchdog.
#[cfg(not(miri))]
mod subprocess {
    include!("../src/allocation_test/child.rs");
}

use rca_core::{
    ApplicationIds, Effect, EffectSender, KeyedAction, KeyedActionPath, KeyedState, KeyedStatePath,
    ReduceContext, Reducer, Store, for_each,
};

/// Owns a repeatable ID source and the current child entries.
struct State {
    /// Reuses public ID 7 to challenge private lifetime identity.
    ids: ApplicationIds<u8>,
    /// Stores the live child independently of its previous public ID owner.
    rows: KeyedState<u8, usize>,
}

/// Routes child work and replaces the entry with the same public ID.
enum Action {
    /// Carries a keyed action through the composed reducer.
    Child(KeyedAction<u8, ChildAction>),
    /// Ends the current entry's lifetime.
    Remove,
    /// Starts a fresh entry with the repeated public ID.
    Insert,
}

/// Separates effect creation from a visible state mutation.
enum ChildAction {
    /// Exposes the effect sender, then waits for cancellation.
    Start,
    /// Increments the currently addressed child.
    Add,
}

/// Replaces child entries without controlling their private lifetime identities.
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
                state.rows.remove(&7);
            }
            Action::Insert => {
                state.rows.insert(state.ids.next().unwrap(), 0);
            }
            Action::Child(_) => {}
        }
        Effect::none()
    }
}

/// Exposes the original effect sender for a post-removal admission attempt.
struct Child(async_channel::Sender<EffectSender<ChildAction>>);

impl Reducer for Child {
    type State = usize;
    type Action = ChildAction;

    fn reduce(
        &mut self,
        state: &mut usize,
        action: &ChildAction,
        _: &mut ReduceContext<'_>,
    ) -> Effect<ChildAction> {
        match action {
            ChildAction::Add => {
                *state += 1;
                Effect::none()
            }
            ChildAction::Start => {
                let capture = self.0.clone();
                Effect::run(move |sender| async move {
                    capture.send(sender).await.unwrap();
                    std::future::pending::<()>().await;
                })
            }
        }
    }
}

/// Rejects old handles and effects after a replacement reuses the same public ID.
#[test]
fn constant_application_ids_do_not_revive_old_keyed_work() {
    #[cfg(not(miri))]
    if subprocess::run_in_child("constant_application_ids_do_not_revive_old_keyed_work") {
        return;
    }
    // Miri runs directly under its external job deadline because it cannot spawn.
    run_repeated_id_scenario();
}

/// Retains both stale authorities while a replacement child handles fresh work.
fn run_repeated_id_scenario() {
    let runtime = support::ManualRuntime::new();
    let (capture, captured) = async_channel::bounded(1);
    let ids = ApplicationIds::constant(7);
    let state_path = KeyedStatePath::new(|s: &State| &s.rows, |s: &mut State| &mut s.rows);
    let action_path = KeyedActionPath::new(
        |a: &Action| match a {
            Action::Child(a) => Some(a),
            _ => None,
        },
        Action::Child,
    );
    let reducer = for_each(Parent, state_path, action_path, Child(capture));
    let store = Store::new(
        State {
            rows: [(ids.next().unwrap(), 0)].into_iter().collect(),
            ids,
        },
        reducer,
        runtime.runtime(),
    );
    runtime.start_all();
    futures_executor::block_on(async {
        let old = store.scope_keyed(state_path, action_path, 7).await.unwrap();
        let started = old.send(ChildAction::Start).await.unwrap();
        runtime.start_all();
        let old_sender = captured.recv().await.unwrap();
        store.send(Action::Remove).await.unwrap().finish().await;
        store.send(Action::Insert).await.unwrap().finish().await;
        assert!(
            old.send(ChildAction::Add)
                .await
                .err()
                .unwrap()
                .is_scope_ended()
        );
        assert!(
            old_sender
                .send(ChildAction::Add)
                .await
                .unwrap_err()
                .is_cancelled()
        );
        started.finish().await;
        let current = store.scope_keyed(state_path, action_path, 7).await.unwrap();
        assert_eq!(current.read(|value| *value).await.unwrap(), 0);
        current.send(ChildAction::Add).await.unwrap().finish().await;
        assert_eq!(current.read(|value| *value).await.unwrap(), 1);
        store.shutdown().await.unwrap();
    });
    for result in runtime.join_all() {
        result.unwrap();
    }
}
