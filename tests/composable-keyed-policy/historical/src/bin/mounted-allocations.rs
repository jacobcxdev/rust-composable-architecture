//! Records mounted allocation boundaries through historical RCA's native keyed routing.
//!
//! Each process owns one native Store and worker. The output does not qualify a performance comparison.

#[path = "../../../mounted_common.rs"]
mod common;

use common::{ChildAction, ChildData, Ledger, Snapshot};
use composable::{Effects, Keyed, KeyedState, RecursiveReducer, Reducer, Store};

/// Instruments allocations across the complete owned driver lifecycle.
#[global_allocator]
static ALLOCATOR: dhat::Alloc = dhat::Alloc;

/// Owns scalar child data because historical reducer objects are state.
struct Child {
    data: ChildData,
}

impl Reducer for Child {
    type Action = ChildAction;
    type Output = Self;

    fn reduce(&mut self, action: ChildAction, effects: impl Effects<ChildAction>) {
        if let Some(output) = self.data.reduce(action) {
            effects.action(output);
        }
    }
}

/// Separates root lifecycle commands from keyed child work.
#[derive(Clone)]
enum Action {
    Mount,
    Child(Keyed<usize, ChildAction>),
    Replace,
    Remove,
    Reinsert,
    Clear,
}

impl From<Keyed<usize, ChildAction>> for Action {
    fn from(value: Keyed<usize, ChildAction>) -> Self {
        Self::Child(value)
    }
}

impl TryFrom<Action> for Keyed<usize, ChildAction> {
    type Error = ();

    fn try_from(value: Action) -> Result<Self, Self::Error> {
        match value {
            Action::Child(child) => Ok(child),
            _ => Err(()),
        }
    }
}

/// Owns native children and records payloads independently of child reduction order.
#[derive(RecursiveReducer)]
struct Parent {
    children: KeyedState<usize, Child>,
    #[reducer(skip)]
    ledger: Ledger,
    #[reducer(skip)]
    count: usize,
}

impl RecursiveReducer for Parent {
    type Action = Action;

    fn reduce(&mut self, action: Action, _: impl Effects<Action>) {
        self.ledger.reductions += 1;
        match action {
            Action::Child(Keyed {
                key,
                action: ChildAction::Completed(value),
            }) => self.ledger.output(key, value),
            Action::Replace => {
                let old = self.children.remove(&1).unwrap();
                assert_eq!(old.data.row(), (18, 4, 2, Some(14)));
                drop(old);
                self.children.insert(
                    1,
                    Child {
                        data: ChildData::new(7, 3),
                    },
                );
            }
            Action::Remove => {
                common::verify_rows(
                    self.children.iter().map(|(id, child)| (id, &child.data)),
                    self.count,
                );
                self.ledger.rows_verified += 1;
                drop(self.children.remove(&1).unwrap());
            }
            Action::Reinsert => {
                assert!(self
                    .children
                    .insert(
                        1,
                        Child {
                            data: ChildData::new(7, 3)
                        }
                    )
                    .is_none());
            }
            Action::Clear => {
                if self.count != 0 {
                    common::verify_rows(
                        self.children.iter().map(|(id, child)| (id, &child.data)),
                        self.count,
                    );
                    self.ledger.rows_verified += 1;
                }
                self.children.clear();
            }
            _ => {}
        }
    }
}

/// Embeds one action through the historical keyed action type.
fn child(id: usize, action: ChildAction) -> Action {
    Action::Child(Keyed::new(id, action))
}

/// Runs one bounded workload and reports after all owned domain state drops.
fn main() {
    let count = common::cardinality();
    let profiler = dhat::Profiler::builder().testing().build();
    let baseline = Snapshot::take("baseline");
    let mut children: KeyedState<usize, Child> = KeyedState::default();
    children.reserve(count);
    for id in 1..=count {
        children.insert(
            id,
            Child {
                data: ChildData::initial(id),
            },
        );
    }
    let parent = Parent {
        children,
        ledger: Ledger::new(),
        count,
    };
    let populated = Snapshot::take("populated_unmounted");
    let store = Store::with_initial(parent);
    let constructed = Snapshot::take("store_constructed");
    macro_rules! send {
        ($action:expr, $outputs:expr) => {{
            // Native completion precedes evidence, without an extra state read.
            store.sync($action);
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

    // Native extraction joins the owned worker before the snapshot.
    let parent = store.into_inner();
    let extracted = Snapshot::take("extracted_host_stopped");
    assert!(parent.children.is_empty());
    common::verify(&parent.ledger, count);
    let reductions = parent.ledger.reductions;
    let mut trace = [(0, 0); 6];
    let trace_len = parent.ledger.trace.len();
    trace[..trace_len].copy_from_slice(&parent.ledger.trace);
    drop(parent);
    let released = Snapshot::take("released");
    drop(profiler);
    common::report(
        "rca-0.7",
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
