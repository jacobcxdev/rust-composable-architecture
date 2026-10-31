use super::Driver;
use rca_core::ReactionChange;
use rca_core::{
    Feature, FeatureContext, InitialPolicy, MountIdentity, MountSite, OptionalMount, Output,
    OutputFlow, RootMount, Runtime, Store, Task, TaskContext,
};
use std::sync::{Arc, Mutex};

struct State {
    child: Option<i32>,
    sibling: i32,
    configuration: u64,
    replacement: u64,
}
enum Action {
    Set(i32),
    Transient,
    TaskSet(i32),
}
struct Root;
struct Child;
impl Feature for Root {
    type State = State;
    type Action = Action;
    fn update(
        &mut self,
        state: &mut State,
        action: &Action,
        context: &mut FeatureContext<'_, State, Action>,
    ) {
        match action {
            Action::Set(value) => state.child = Some(*value),
            Action::Transient => {
                let old = state.child;
                state.child = Some(999);
                state.child = old;
            }
            Action::TaskSet(value) => {
                let value = *value;
                context.task(Task::<State, Action>::run(move |context| async move {
                    context
                        .update(move |state| state.child = Some(value))
                        .await
                        .unwrap();
                }));
            }
        }
    }
}
impl Feature for Child {
    type State = i32;
    type Action = i32;
    fn update(&mut self, state: &mut i32, action: &i32, _: &mut FeatureContext<'_, i32, i32>) {
        *state += action;
    }
}
type Trace = Arc<Mutex<Vec<(&'static str, Option<i32>, i32)>>>;
fn record(trace: &Trace, name: &'static str, change: &ReactionChange<i32>) {
    trace.lock().unwrap().push(match change {
        ReactionChange::Initial { current } => (name, None, *current),
        ReactionChange::Changed { previous, current } => (name, Some(*previous), *current),
    });
}
struct Harness {
    driver: Driver,
    store: Store<State, Action>,
    root: TaskContext<State, Action>,
    site: MountSite<State, i32, i32, Action>,
    output: Output<i32>,
    trace: Trace,
}
impl Harness {
    #[expect(
        clippy::too_many_lines,
        reason = "One fixture constructor preserves overlapping hook and output registration order"
    )]
    fn new(overlap: bool) -> Self {
        let driver = Driver::default();
        let trace = Trace::default();
        let child_trace = std::sync::Arc::clone(&trace);
        let child = OptionalMount::new(
            |state: &State| state.child.as_ref(),
            |state: &mut State| state.child.as_mut(),
            |state| {
                state.child.map(|_| MountIdentity {
                    key: (),
                    replacement: state.replacement,
                    configuration: state.configuration,
                })
            },
            |_| Child,
        )
        .on_change(
            |value| *value,
            InitialPolicy::Run,
            move |change, _| {
                record(&child_trace, "child", &change);
                Task::run(|_| async {})
            },
        );
        let root_trace = std::sync::Arc::clone(&trace);
        let (send, receive) = futures_channel::oneshot::channel();
        let mut send = Some(send);
        let (builder, site) = RootMount::new(Root)
            .on_initial(
                |_| (),
                move |_, _| {
                    let send = send.take().unwrap();
                    Task::run(move |context| async move {
                        let _ = send.send(context);
                        std::future::pending::<()>().await;
                    })
                },
            )
            .on_change(
                |state| state.child.unwrap_or_default() + state.sibling,
                InitialPolicy::Skip,
                move |change, _| {
                    record(&root_trace, "root", &change);
                    Task::run(|_| async {})
                },
            )
            .builder(
                State {
                    child: Some(4),
                    sibling: 0,
                    configuration: 0,
                    replacement: 0,
                },
                Runtime::new(driver.clone()),
            )
            .mount_with_site(child);
        let read: fn(&State) -> Option<&i32> = if overlap {
            |state| state.child.as_ref()
        } else {
            |state| Some(&state.sibling)
        };
        let write: fn(&mut State) -> Option<&mut i32> = if overlap {
            |state| state.child.as_mut()
        } else {
            |state| Some(&mut state.sibling)
        };
        let sibling_trace = std::sync::Arc::clone(&trace);
        let sibling = OptionalMount::new(
            read,
            write,
            move |state| {
                read(state).map(|_| MountIdentity {
                    key: (),
                    replacement: 0,
                    configuration: 0,
                })
            },
            |_| Child,
        )
        .on_change(
            |value| *value,
            InitialPolicy::Skip,
            move |change, _| {
                record(
                    &sibling_trace,
                    if overlap { "overlap" } else { "sibling" },
                    &change,
                );
                Task::run(|_| async {})
            },
        );
        let (builder, output) = builder.mount(sibling).output(&site, |handlers| {
            handlers.at(&site, |state, value, _| {
                *state += 1;
                OutputFlow::Continue(value)
            });
            handlers.at_root(|state, value, _| {
                state.child = Some(value);
                state.sibling += 2;
                OutputFlow::Consumed
            });
        });
        let store = builder.build().unwrap();
        driver.finish(store.ready()).unwrap();
        let root = driver.finish(receive).unwrap();
        trace.lock().unwrap().clear();
        Self {
            driver,
            store,
            root,
            site,
            output,
            trace,
        }
    }
    fn clear(&self) {
        self.trace.lock().unwrap().clear();
    }
    fn events(&self) -> Vec<(&'static str, Option<i32>, i32)> {
        self.trace.lock().unwrap().clone()
    }
}

#[test]
fn separate_commits_react_twice_but_transient_reversal_commits_without_factory() {
    let h = Harness::new(false);
    let baseline = h.driver.finish(h.root.update(|_| ())).unwrap().revision();
    let _ = h.driver.finish(h.root.send(Action::Set(5))).unwrap();
    let _ = h.driver.finish(h.root.send(Action::Set(4))).unwrap();
    assert_eq!(
        h.events(),
        [
            ("child", Some(4), 5),
            ("root", Some(4), 5),
            ("child", Some(5), 4),
            ("root", Some(5), 4)
        ]
    );
    h.clear();
    let _ = h.driver.finish(h.store.send(Action::Transient)).unwrap();
    let end = h
        .driver
        .finish(h.root.update(|state| {
            state.child = Some(999);
            state.child = Some(4);
        }))
        .unwrap();
    assert_eq!(h.events(), [] as [(&str, Option<i32>, i32); 0]);
    assert_eq!(
        end.revision(),
        baseline + 4,
        "both transient/no-op mutation boundaries still stamp"
    );
    let task = h.driver.finish(h.store.send(Action::TaskSet(8))).unwrap();
    h.driver.finish(task.finish());
    assert_eq!(h.events(), [("child", Some(4), 8), ("root", Some(4), 8)]);
    drop(h.store);
    h.driver.turn();
}

#[test]
fn one_output_transaction_observes_final_child_sibling_and_root_once() {
    let h = Harness::new(false);
    let child = h.driver.finish(h.store.acquire(&h.site)).unwrap().unwrap();
    let _ = h
        .driver
        .finish(child.output(&h.output, 10))
        .expect("the child output commits");
    assert_eq!(
        h.events(),
        [
            ("child", Some(4), 10),
            ("sibling", Some(0), 2),
            ("root", Some(4), 12)
        ]
    );
    drop(h.store);
    h.driver.turn();
}

#[test]
fn coherent_overlapping_sibling_paths_both_observe_one_private_input() {
    let h = Harness::new(true);
    let child = h.driver.finish(h.store.acquire(&h.site)).unwrap().unwrap();
    let _ = h.driver.finish(child.send(3)).unwrap();
    assert_eq!(
        h.events(),
        [
            ("child", Some(4), 7),
            ("overlap", Some(4), 7),
            ("root", Some(4), 7)
        ]
    );
    drop(h.store);
    h.driver.turn();
}

#[test]
fn replacement_refresh_removal_and_reinsertion_never_pair_foreign_history() {
    let h = Harness::new(false);
    h.driver
        .finish(h.root.update(|state| {
            state.child = Some(7);
            state.replacement += 1;
        }))
        .unwrap();
    assert_eq!(h.events(), [("child", None, 7), ("root", Some(4), 7)]);
    h.clear();
    h.driver
        .finish(h.root.update(|state| {
            state.child = Some(8);
            state.configuration += 1;
        }))
        .unwrap();
    assert_eq!(h.events(), [("root", Some(7), 8)]);
    h.clear();
    let fresh = h.driver.finish(h.store.acquire(&h.site)).unwrap().unwrap();
    h.driver.finish(fresh.update(|value| *value = 9)).unwrap();
    assert_eq!(h.events(), [("child", Some(8), 9), ("root", Some(8), 9)]);
    h.clear();
    h.driver
        .finish(h.root.update(|state| state.child = None))
        .unwrap();
    assert_eq!(h.events(), [("root", Some(9), 0)]);
    h.clear();
    h.driver
        .finish(h.root.update(|state| state.child = Some(2)))
        .unwrap();
    assert_eq!(h.events(), [("child", None, 2), ("root", Some(0), 2)]);
    drop(h.store);
    h.driver.turn();
}
