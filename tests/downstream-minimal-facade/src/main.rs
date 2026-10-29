//! Checks registry-only authoring without optional features or a chosen executor.
//!
//! Main executes dependency access. The mounted helpers are type-checked only.

#![deny(warnings)]

use std::ops::ControlFlow;

use architecture::{
    DependencyKey, DependencyValues, Effect, EntryMount, Feature, FeatureContext, Fold, FoldKey,
    FoldView, InitialPolicy, InputGate, KeyedState, MountChildren, MountSite, Output,
    ReactionChange, RemovalOrigin, RemovalSite, RemovedChild, RemovedOutputError, RootMount,
    Runtime, Store, StoreBuildError, Task, TaskAccessError,
};

/// Supplies a literal dependency default and a typed override.
struct Value;
impl DependencyKey for Value {
    type Value = u8;
    fn live_value() -> u8 {
        7
    }
}

/// Holds child state for the executor-independent authoring checks.
type State = KeyedState<u8, u64>;

/// Keeps the root unchanged while its children provide behaviour.
struct Root;
impl Feature for Root {
    type State = State;
    type Action = ();
    fn reduce(&mut self, _: &mut State, _: &(), _: &mut FeatureContext<'_, State, ()>) {}
}

/// Supplies a child reducer without optional runtime dependencies.
struct Row;
impl Feature for Row {
    type State = u64;
    type Action = ();
    fn reduce(&mut self, state: &mut u64, _: &(), _: &mut FeatureContext<'_, u64, ()>) {
        *state += 1;
    }
}

/// Combines child values through the public fold interface.
struct Total;
impl FoldKey for Total {
    type Value = u64;
    fn identity() -> u64 {
        0
    }
    fn reduce(accumulator: u64, next: u64) -> ControlFlow<u64, u64> {
        ControlFlow::Continue(accumulator + next)
    }
}

/// Type-checks keyed mounts, reactions and folds without choosing an executor.
///
/// # Errors
/// Returns the error from store construction.
fn build_headless(runtime: Runtime) -> Result<Store<State, ()>, StoreBuildError> {
    let fold = Fold::<State, Total>::new();
    RootMount::new(Root)
        .on_change(
            State::len,
            InitialPolicy::Run,
            |_: ReactionChange<usize>, _| Task::run(|_| async {}),
        )
        .builder(State::default(), runtime)
        .keyed_removable(
            |state| state,
            |state| state,
            move |_, _| {
                let fold = fold.clone();
                (
                    EntryMount::new(Row),
                    move |_: &u64,
                          children: &mut MountChildren<'_, State, (), u64, ()>,
                          _: RemovalSite<State, u64, ()>| {
                        children.contribute(&fold, |value| *value);
                    },
                )
            },
        )
        .build()
}

/// Type-checks typed child access and an optional fold read.
///
/// # Errors
/// Returns an acquisition or read error.
async fn inspect_child(
    store: &Store<State, ()>,
    site: &MountSite<State, u64, (), ()>,
    fold: &Fold<State, Total>,
) -> Result<(Option<InputGate>, Option<u64>), TaskAccessError> {
    let gate = store.input_gate(site).await?;
    let view: Option<FoldView<Total>> = store.acquire_fold(fold, site).await?;
    let total = match view {
        Some(view) => Some(view.read().await?),
        None => None,
    };
    Ok((gate, total))
}

/// Type-checks forwarding a removed child through its current turn.
///
/// # Errors
/// Returns the rejected removal if output forwarding fails.
fn forward_removed<'turn>(
    removed: RemovedChild<'turn, u64>,
    context: &mut FeatureContext<'turn, State, ()>,
    output: &Output<u64>,
) -> Result<(), RemovedOutputError<'turn, u64>> {
    let _: &RemovalOrigin = removed.origin();
    removed.into_output(context, output, |value| value, |_| {})
}

/// Checks dependency defaults and overrides without starting a runtime.
fn main() {
    let mut dependencies = DependencyValues::new();
    assert_eq!(*dependencies.get_or_insert::<Value>(), 7);
    dependencies.set::<Value>(11);
    assert_eq!(*dependencies.get_or_insert::<Value>(), 11);
    let _effect = Effect::<()>::none();
    let _authoring = (build_headless, inspect_child, forward_removed);
}
