//! Separates weak route ownership from the lifetime of the child it addresses.

#[path = "support/thread_runtime.rs"]
mod support;

use futures_executor::block_on;
use rca_core::{
    ActionPath, Effect, OptionalStatePath, ReduceContext, Reducer, StatePath, Store,
    WeakScopedStore,
};

/// Commits additions without effects so a receipt proves the final count.
struct Counter;

impl Reducer for Counter {
    type State = u32;
    type Action = u32;

    fn reduce(&mut self, state: &mut u32, action: &u32, _: &mut ReduceContext<'_>) -> Effect<u32> {
        *state += action;
        Effect::none()
    }
}

// Catches storing a strong driver in the supposed weak attachment.
#[test]
fn last_captured_owner_drop_ends_attachment_while_root_remains_alive() {
    let runtime = support::ThreadRuntime::new();
    let store = Store::new(7, Counter, runtime.runtime());
    let owner = store.scope(
        StatePath::new(|value: &u32| value, |value: &mut u32| value),
        ActionPath::new(|value: &u32| Some(value), |value| value),
    );
    let weak: WeakScopedStore<u32, u32> = owner.downgrade();
    let alias = owner.clone();
    drop(owner);
    let upgraded = weak.upgrade().unwrap();
    let _ = block_on(upgraded.send(2)).unwrap();
    drop(upgraded);
    drop(alias);
    assert!(weak.upgrade().is_none());
    assert!(weak.clone().upgrade().is_none());
    assert_eq!(block_on(store.read(|value| *value)).unwrap(), 9);
    block_on(store.shutdown()).unwrap();
}

/// Replaces optional state with a visibly different child value.
enum OptionalAction {
    Add(u32),
    Remove,
    Insert,
}

/// Leaves lifetime checks to the public scoped route before reducing input.
struct OptionalCounter;

impl Reducer for OptionalCounter {
    type State = Option<u32>;
    type Action = OptionalAction;

    fn reduce(
        &mut self,
        state: &mut Option<u32>,
        action: &OptionalAction,
        _: &mut ReduceContext<'_>,
    ) -> Effect<OptionalAction> {
        match action {
            OptionalAction::Add(value) => *state.as_mut().unwrap() += value,
            OptionalAction::Remove => *state = None,
            OptionalAction::Insert => *state = Some(40),
        }
        Effect::none()
    }
}

// Catches upgrade performing a fresh logical-child lookup or registration.
#[test]
fn upgrading_a_retained_old_driver_does_not_renew_removed_child_authority() {
    let runtime = support::ThreadRuntime::new();
    let store = Store::new(Some(7), OptionalCounter, runtime.runtime());
    let owner = block_on(store.scope_some(
        OptionalStatePath::new(Option::as_ref, Option::as_mut),
        ActionPath::new(
            |action: &OptionalAction| match action {
                OptionalAction::Add(value) => Some(value),
                OptionalAction::Remove | OptionalAction::Insert => None,
            },
            OptionalAction::Add,
        ),
    ))
    .unwrap();
    let weak = owner.downgrade();
    let _ = block_on(store.send(OptionalAction::Remove)).unwrap();
    let _ = block_on(store.send(OptionalAction::Insert)).unwrap();
    let old = weak.upgrade().unwrap();
    let error = block_on(old.send(100)).unwrap_err();
    assert!(error.is_scope_ended());
    assert!(!error.is_store_stopped());
    assert_eq!(block_on(store.read(|value| *value)).unwrap(), Some(40));
    drop(old);
    drop(owner);
    assert!(weak.upgrade().is_none());
    block_on(store.shutdown()).unwrap();
}

// Catches imposing State/Action Clone, Sync or Debug merely to downgrade a driver.
#[test]
fn weak_handles_keep_the_existing_transfer_bounds() {
    fn transferable<T: Send + Sync>() {}
    transferable::<WeakScopedStore<std::cell::Cell<u32>, std::cell::Cell<u32>>>();
}
