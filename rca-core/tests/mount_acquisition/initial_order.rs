//! Factories use the final finite roster in child-before-parent declaration order.
use super::*;
use rca_core::{RootMount, Task};

/// Holds two independent children so factory order crosses sibling declarations.
struct State {
    first: Option<Cell<usize>>,
    second: Option<Cell<usize>>,
}
struct Root;
impl Feature for Root {
    type State = State;
    type Action = ();
    fn reduce(&mut self, _: &mut State, (): &(), _: &mut FeatureContext<'_, State, ()>) {}
}

#[test]
fn initial_factories_follow_children_then_root_without_polling_work() {
    let driver = Driver::default();
    let trace = Arc::new(Mutex::new(Vec::new()));
    let constructions = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let descriptor =
        |name,
         read: for<'a> fn(&'a State) -> Option<&'a Cell<usize>>,
         write: for<'a> fn(&'a mut State) -> Option<&'a mut Cell<usize>>| {
            let made = std::sync::Arc::clone(&constructions);
            let selected = std::sync::Arc::clone(&constructions);
            let trace = std::sync::Arc::clone(&trace);
            OptionalMount::new(
                read,
                write,
                move |state| {
                    read(state).map(|_| MountIdentity {
                        key: (),
                        replacement: 0,
                        configuration: 0,
                    })
                },
                move |_| {
                    made.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    ChildFeature(Cell::new(0))
                },
            )
            .on_initial(
                move |state| {
                    assert_eq!(
                        selected.load(std::sync::atomic::Ordering::SeqCst),
                        2,
                        "all installation precedes first reaction selector"
                    );
                    Box::new(state.get())
                },
                move |_, _| {
                    trace.lock().unwrap().push(name);
                    let trace = std::sync::Arc::clone(&trace);
                    Task::run(move |_| async move {
                        trace.lock().unwrap().push("child body");
                    })
                },
            )
        };
    let root_trace = std::sync::Arc::clone(&trace);
    let store = RootMount::new(Root)
        .on_initial(
            |_| Box::new(0),
            move |_, _| {
                root_trace.lock().unwrap().push("root factory");
                let trace = std::sync::Arc::clone(&root_trace);
                Task::run(move |_| async move {
                    trace.lock().unwrap().push("root body");
                })
            },
        )
        .builder(
            State {
                first: Some(Cell::new(1)),
                second: Some(Cell::new(2)),
            },
            Runtime::new(driver.clone()),
        )
        .mount(descriptor(
            "first factory",
            |state| state.first.as_ref(),
            |state| state.first.as_mut(),
        ))
        .mount(descriptor(
            "second factory",
            |state| state.second.as_ref(),
            |state| state.second.as_mut(),
        ))
        .build()
        .unwrap();
    let mut ready = Box::pin(store.ready());
    assert!(poll(ready.as_mut()).is_pending());
    driver.turn();
    assert!(matches!(poll(ready.as_mut()), Poll::Ready(Ok(()))));
    assert_eq!(
        *trace.lock().unwrap(),
        ["first factory", "second factory", "root factory"]
    );
    driver.turn();
    assert_eq!(trace.lock().unwrap().len(), 6);
    drop(store);
    driver.turn();
}

struct NestedState {
    parent: Option<ParentState>,
}
struct ParentState {
    first: Option<Cell<usize>>,
    second: Option<Cell<usize>>,
}
struct NestedRoot;
impl Feature for NestedRoot {
    type State = NestedState;
    type Action = ();
    fn reduce(
        &mut self,
        _: &mut NestedState,
        (): &(),
        _: &mut FeatureContext<'_, NestedState, ()>,
    ) {
    }
}
struct Parent(Cell<usize>);
impl Feature for Parent {
    type State = ParentState;
    type Action = bool;
    fn reduce(
        &mut self,
        _: &mut ParentState,
        _: &bool,
        _: &mut FeatureContext<'_, ParentState, bool>,
    ) {
        self.0.set(self.0.get() + 1);
    }
}

#[test]
fn nested_initial_factories_preserve_siblings_then_parent_then_root() {
    let driver = Driver::default();
    let trace = Arc::new(Mutex::new(Vec::new()));
    let parent_trace = std::sync::Arc::clone(&trace);
    let parent = OptionalMount::new(
        |state: &NestedState| state.parent.as_ref(),
        |state: &mut NestedState| state.parent.as_mut(),
        |state| {
            state.parent.as_ref().map(|_| MountIdentity {
                key: (),
                replacement: 0,
                configuration: 0,
            })
        },
        |_| Parent(Cell::new(0)),
    )
    .on_initial(
        |_| Box::new(0),
        move |_, _| {
            parent_trace.lock().unwrap().push("parent factory");
            Task::run(|_| async {})
        },
    );
    let root_trace = std::sync::Arc::clone(&trace);
    let builder = RootMount::new(NestedRoot)
        .on_initial(
            |_| Box::new(0),
            move |_, _| {
                root_trace.lock().unwrap().push("root factory");
                Task::run(|_| async {})
            },
        )
        .builder(
            NestedState {
                parent: Some(ParentState {
                    first: Some(Cell::new(1)),
                    second: Some(Cell::new(2)),
                }),
            },
            Runtime::new(driver.clone()),
        );
    let children_trace = std::sync::Arc::clone(&trace);
    let (builder, _parent_site) = builder.mount_with_children(parent, move |_, children| {
        for (name, read, write) in [
            (
                "first factory",
                (|state: &ParentState| state.first.as_ref())
                    as for<'a> fn(&'a ParentState) -> Option<&'a Cell<usize>>,
                (|state: &mut ParentState| state.first.as_mut())
                    as for<'a> fn(&'a mut ParentState) -> Option<&'a mut Cell<usize>>,
            ),
            (
                "second factory",
                (|state: &ParentState| state.second.as_ref())
                    as for<'a> fn(&'a ParentState) -> Option<&'a Cell<usize>>,
                (|state: &mut ParentState| state.second.as_mut())
                    as for<'a> fn(&'a mut ParentState) -> Option<&'a mut Cell<usize>>,
            ),
        ] {
            let trace = std::sync::Arc::clone(&children_trace);
            children.optional(
                OptionalMount::new(
                    read,
                    write,
                    move |state| {
                        read(state).map(|_| MountIdentity {
                            key: (),
                            replacement: 0,
                            configuration: 0,
                        })
                    },
                    |_| ChildFeature(Cell::new(0)),
                )
                .on_initial(
                    |state| Box::new(state.get()),
                    move |_, _| {
                        trace.lock().unwrap().push(name);
                        Task::run(|_| async {})
                    },
                ),
            );
        }
    });
    let store = builder.build().unwrap();
    let mut ready = Box::pin(store.ready());
    assert!(poll(ready.as_mut()).is_pending());
    driver.turn();
    assert!(matches!(poll(ready.as_mut()), Poll::Ready(Ok(()))));
    assert_eq!(
        *trace.lock().unwrap(),
        [
            "first factory",
            "second factory",
            "parent factory",
            "root factory"
        ]
    );
    drop(store);
    driver.turn();
}
