use super::Driver;
use rca_core::ReactionChange;
use rca_core::{
    EntryMount, Feature, FeatureContext, InitialPolicy, KeyedState, MountChildren, MountIdentity,
    MountSite, OptionalMount, RootMount, Runtime, Task,
};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, Weak};

struct Row {
    child: Option<i32>,
    configuration: u64,
}
type State = KeyedState<u8, Row>;
enum Action {
    Reverse,
    Increment,
    ReplaceOne,
    RefreshTwo,
}
struct Root;
struct Parent;
struct Leaf;
impl Feature for Root {
    type State = State;
    type Action = Action;
    fn update(
        &mut self,
        state: &mut State,
        action: &Action,
        _: &mut FeatureContext<'_, State, Action>,
    ) {
        match action {
            Action::Reverse => state.reverse(),
            Action::Increment => {
                for row in state.values_mut() {
                    *row.child.as_mut().unwrap() += 1;
                }
            }
            Action::ReplaceOne => {
                state.remove(&1);
                state.insert(
                    1,
                    Row {
                        child: Some(9),
                        configuration: 0,
                    },
                );
            }
            Action::RefreshTwo => {
                let row = state.get_mut(&2).unwrap();
                row.configuration += 1;
                *row.child.as_mut().unwrap() += 1;
            }
        }
    }
}
impl Feature for Parent {
    type State = Row;
    type Action = ();
    fn update(&mut self, _: &mut Row, (): &(), _: &mut FeatureContext<'_, Row, ()>) {}
}
impl Feature for Leaf {
    type State = i32;
    type Action = ();
    fn update(&mut self, state: &mut i32, (): &(), _: &mut FeatureContext<'_, i32, ()>) {
        *state += 1;
    }
}
type Trace = Arc<Mutex<Vec<(&'static str, u8, Option<i32>, i32)>>>;
fn record(trace: &Trace, name: &'static str, key: u8, change: &ReactionChange<i32>) {
    let (previous, current) = match change {
        ReactionChange::Initial { current } => (None, *current),
        ReactionChange::Changed { previous, current } => (Some(*previous), *current),
    };
    trace.lock().unwrap().push((name, key, previous, current));
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "One keyed postorder sequence preserves roster changes and retired callback capture checks"
)]
fn keyed_postorder_follows_current_roster_and_old_captures_do_not_retain_retired_callbacks() {
    let driver = Driver::default();
    let trace = Trace::default();
    let root_trace = std::sync::Arc::clone(&trace);
    let callbacks = Arc::new(Mutex::new(BTreeMap::<u8, Vec<Weak<()>>>::new()));
    let captures = std::sync::Arc::clone(&callbacks);
    let sites = Arc::new(Mutex::new(
        BTreeMap::<u8, MountSite<State, i32, (), Action>>::new(),
    ));
    let declared_sites = std::sync::Arc::clone(&sites);
    let child_trace = std::sync::Arc::clone(&trace);
    let store = RootMount::new(Root)
        .on_change(
            |state| state.values().map(|row| row.child.unwrap()).sum::<i32>(),
            InitialPolicy::Skip,
            move |change, _| {
                record(&root_trace, "root", 0, &change);
                Task::run(|_| async {})
            },
        )
        .builder(
            [
                (
                    1,
                    Row {
                        child: Some(4),
                        configuration: 0,
                    },
                ),
                (
                    2,
                    Row {
                        child: Some(6),
                        configuration: 0,
                    },
                ),
            ]
            .into_iter()
            .collect(),
            Runtime::new(driver.clone()),
        )
        .keyed_with_children(
            |state| state,
            |state| state,
            move |key, _| {
                let key = *key;
                assert!(
                    captures
                        .lock()
                        .unwrap()
                        .get(&key)
                        .is_none_or(|old| old.iter().all(|weak| weak.upgrade().is_none())),
                    "retired Changed selectors/factories must drain before replacement construction"
                );
                let parent_capture = Arc::new(());
                captures
                    .lock()
                    .unwrap()
                    .insert(key, vec![Arc::downgrade(&parent_capture)]);
                let parent_trace = std::sync::Arc::clone(&child_trace);
                let leaf_trace = std::sync::Arc::clone(&child_trace);
                let captures = std::sync::Arc::clone(&captures);
                let sites = std::sync::Arc::clone(&declared_sites);
                (
                    EntryMount::new(Parent)
                        .configuration(|row| row.configuration)
                        .on_change(
                            move |row| {
                                std::hint::black_box(&parent_capture);
                                row.child.unwrap()
                            },
                            InitialPolicy::Run,
                            move |change, _| {
                                record(&parent_trace, "parent", key, &change);
                                Task::run(|_| async {})
                            },
                        ),
                    move |_: &Row, children: &mut MountChildren<'_, State, Action, Row, ()>| {
                        let leaf_capture = Arc::new(());
                        captures
                            .lock()
                            .unwrap()
                            .get_mut(&key)
                            .unwrap()
                            .push(Arc::downgrade(&leaf_capture));
                        let trace = std::sync::Arc::clone(&leaf_trace);
                        let leaf = OptionalMount::new(
                            |row: &Row| row.child.as_ref(),
                            |row: &mut Row| row.child.as_mut(),
                            |row| {
                                row.child.map(|_| MountIdentity {
                                    key: (),
                                    replacement: 0,
                                    configuration: 0,
                                })
                            },
                            |_| Leaf,
                        )
                        .on_change(
                            move |value| {
                                std::hint::black_box(&leaf_capture);
                                *value
                            },
                            InitialPolicy::Run,
                            move |change, _| {
                                record(&trace, "leaf", key, &change);
                                Task::run(|_| async {})
                            },
                        );
                        sites.lock().unwrap().insert(key, children.optional(leaf));
                    },
                )
            },
        )
        .build()
        .unwrap();
    driver.finish(store.ready()).unwrap();
    assert_eq!(
        *trace.lock().unwrap(),
        [
            ("leaf", 1, None, 4),
            ("parent", 1, None, 4),
            ("leaf", 2, None, 6),
            ("parent", 2, None, 6)
        ]
    );
    trace.lock().unwrap().clear();
    let _ = driver.finish(store.send(Action::Reverse)).unwrap();
    assert!(trace.lock().unwrap().is_empty());
    let _ = driver.finish(store.send(Action::Increment)).unwrap();
    assert_eq!(
        *trace.lock().unwrap(),
        [
            ("leaf", 2, Some(6), 7),
            ("parent", 2, Some(6), 7),
            ("leaf", 1, Some(4), 5),
            ("parent", 1, Some(4), 5),
            ("root", 0, Some(10), 12)
        ]
    );
    trace.lock().unwrap().clear();
    let _ = driver.finish(store.send(Action::ReplaceOne)).unwrap();
    assert_eq!(
        *trace.lock().unwrap(),
        [
            ("leaf", 1, None, 9),
            ("parent", 1, None, 9),
            ("root", 0, Some(12), 16)
        ]
    );
    trace.lock().unwrap().clear();
    let two = sites.lock().unwrap().get(&2).unwrap().clone();
    let old = driver.finish(store.acquire(&two)).unwrap().unwrap();
    let _ = driver.finish(store.send(Action::RefreshTwo)).unwrap();
    assert_eq!(
        *trace.lock().unwrap(),
        [("root", 0, Some(16), 17)],
        "ancestor configuration invalidates descendant old history"
    );
    driver
        .finish(old.update(|value| *value += 1))
        .expect_err("the old configuration rejects mutation");
    trace.lock().unwrap().clear();
    let fresh = driver.finish(store.acquire(&two)).unwrap().unwrap();
    let _ = driver.finish(fresh.send(())).unwrap();
    assert_eq!(
        *trace.lock().unwrap(),
        [
            ("leaf", 2, Some(8), 9),
            ("parent", 2, Some(8), 9),
            ("root", 0, Some(17), 18)
        ]
    );
    drop(store);
    driver.turn();
    assert!(
        callbacks
            .lock()
            .unwrap()
            .values()
            .flatten()
            .all(|weak| weak.upgrade().is_none())
    );
}
