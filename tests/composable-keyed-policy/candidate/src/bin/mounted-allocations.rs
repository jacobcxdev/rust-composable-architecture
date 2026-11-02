//! Records mounted allocation boundaries through current RCA's native keyed routing.
//!
//! Each process owns one runtime and Store. The output does not qualify a performance comparison.

#[path = "../../../mounted_common.rs"]
mod common;

use common::{ChildAction, ChildData, Ledger, Snapshot};
use rca_core::{
    for_each, Effect, KeyedAction, KeyedActionPath, KeyedState, KeyedStatePath, ReduceContext,
    Reducer, Store,
};
use rca_tokio::TokioRuntime;

/// Instruments allocations across the complete owned driver lifecycle.
#[global_allocator]
static ALLOCATOR: dhat::Alloc = dhat::Alloc;

/// Separates root lifecycle commands from keyed child work.
enum Action {
    Mount,
    Child(KeyedAction<usize, ChildAction>),
    Replace,
    Remove,
    Reinsert,
    Clear,
}

/// Owns children and completion evidence until native state extraction.
struct State {
    children: KeyedState<usize, ChildData>,
    ledger: Ledger,
    count: usize,
}

/// Records output payloads without depending on parent-before-child reduction order.
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
        state.ledger.reductions += 1;
        match action {
            Action::Child(child) => {
                if let ChildAction::Completed(value) = child.action() {
                    state.ledger.output(*child.id(), *value);
                }
            }
            Action::Replace => {
                let old = state.children.remove(&1).unwrap();
                assert_eq!(old.row(), (18, 4, 2, Some(14)));
                drop(old);
                state.children.insert(1, ChildData::new(7, 3));
            }
            Action::Remove => {
                common::verify_rows(state.children.iter(), state.count);
                state.ledger.rows_verified += 1;
                drop(state.children.remove(&1).unwrap());
            }
            Action::Reinsert => {
                assert!(state.children.get(&1).is_none());
                state.children.insert(1, ChildData::new(7, 3));
            }
            Action::Clear => {
                if state.count != 0 {
                    common::verify_rows(state.children.iter(), state.count);
                    state.ledger.rows_verified += 1;
                }
                state.children.clear();
            }
            Action::Mount => {}
        }
        Effect::none()
    }
}

/// Adapts scalar child transitions without retaining per-child reducer state.
struct Child;

impl Reducer for Child {
    type State = ChildData;
    type Action = ChildAction;

    fn reduce(
        &mut self,
        state: &mut ChildData,
        action: &ChildAction,
        _: &mut ReduceContext<'_>,
    ) -> Effect<ChildAction> {
        state
            .reduce(*action)
            .map_or_else(Effect::none, Effect::send)
    }
}

/// Embeds one action through the native keyed action type.
fn child(id: usize, action: ChildAction) -> Action {
    Action::Child(KeyedAction::new(id, action))
}

/// Runs one bounded workload and reports after all owned domain state drops.
fn main() {
    let count = common::cardinality();
    let profiler = dhat::Profiler::builder().testing().build();
    let baseline = Snapshot::take("baseline");
    let mut children = KeyedState::with_capacity(count);
    for id in 1..=count {
        children.insert(id, ChildData::initial(id));
    }
    let state = State {
        children,
        ledger: Ledger::new(),
        count,
    };
    let populated = Snapshot::take("populated_unmounted");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let (state, phases) = runtime.block_on(async {
        let reducer = for_each(
            Parent,
            KeyedStatePath::new(|s: &State| &s.children, |s: &mut State| &mut s.children),
            KeyedActionPath::new(
                |a: &Action| match a {
                    Action::Child(c) => Some(c),
                    _ => None,
                },
                Action::Child,
            ),
            Child,
        );
        let store = Store::new(state, reducer, TokioRuntime::current().unwrap());
        let constructed = Snapshot::take("store_constructed");
        macro_rules! send {
            ($action:expr, $outputs:expr) => {{
                // Native completion precedes evidence, without an extra state read.
                store.send($action).await.unwrap().finish().await;
                common::boundary($outputs);
            }};
        }
        send!(Action::Mount, 0);
        let mounted = Snapshot::take("mounted");
        if count != 0 {
            for (action, outputs) in [
                (child(1, ChildAction::SetStep(4)), 0),
                (child(1, ChildAction::Step), 1),
                (child(2, ChildAction::Step), 2),
                (child(1, ChildAction::Step), 3),
                (Action::Replace, 3),
                (child(1, ChildAction::Step), 4),
                (child(2, ChildAction::Step), 5),
            ] {
                send!(action, outputs);
            }
        }
        let work = Snapshot::take("work_completed");
        if count != 0 {
            send!(Action::Remove, 5);
        }
        let removed = Snapshot::take("removed");
        if count != 0 {
            send!(Action::Reinsert, 5);
            send!(child(1, ChildAction::Step), 6);
        }
        let reinserted = Snapshot::take("reinserted");
        send!(Action::Clear, if count == 0 { 0 } else { 6 });
        let cleared = Snapshot::take("cleared");
        let state = store
            .shutdown_into_state()
            .await
            .expect("state extraction failed");
        (
            state,
            [constructed, mounted, work, removed, reinserted, cleared],
        )
    });

    // Host teardown belongs to the extraction boundary, before domain-state release.
    drop(runtime);
    let extracted = Snapshot::take("extracted_host_stopped");
    assert!(state.children.is_empty());
    common::verify(&state.ledger, count);
    let reductions = state.ledger.reductions;
    let mut trace = [(0, 0); 6];
    let trace_len = state.ledger.trace.len();
    trace[..trace_len].copy_from_slice(&state.ledger.trace);
    drop(state);
    let released = Snapshot::take("released");
    drop(profiler);
    let [constructed, mounted, work, removed, reinserted, cleared] = phases;
    common::report(
        "rca",
        count,
        reductions,
        &trace[..trace_len],
        [
            baseline,
            populated,
            constructed,
            mounted,
            work,
            removed,
            reinserted,
            cleared,
            extracted,
            released,
        ],
    );
}
