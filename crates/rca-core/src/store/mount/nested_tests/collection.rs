//! The actual full keyed collection authoring and actor path.
use super::*;
mod failure;
#[cfg(feature = "local")]
mod local;
mod refresh_churn;

/// Records callback phase and key so order and per-entry disposal can be checked.
type Trace = Arc<Mutex<Vec<(&'static str, u8)>>>;
/// Reports the named owner's disposal without keeping its mount alive.
struct Capture(&'static str, u8, Trace);
impl Drop for Capture {
    fn drop(&mut self) {
        self.2.lock().push((self.0, self.1));
    }
}
struct EntryState {
    value: usize,
    configuration: u64,
    detail: Option<Cell<usize>>,
}
struct CollectionState {
    rows: KeyedState<u8, EntryState>,
}
enum CollectionAction {
    Reorder,
    Insert,
    Remove,
    Replace,
    ReplaceTwo,
    Configure,
    Noop,
}
enum EntryAction {
    Touch,
    Leave,
}
enum DetailAction {
    Add(usize),
}
struct CollectionRoot;
impl Feature for CollectionRoot {
    type State = CollectionState;
    type Action = CollectionAction;
    fn update(
        &mut self,
        state: &mut CollectionState,
        action: &CollectionAction,
        _: &mut FeatureContext<'_, CollectionState, CollectionAction>,
    ) {
        match action {
            CollectionAction::Reorder => state.rows.reverse(),
            CollectionAction::Insert => {
                state.rows.insert(4, row(4));
            }
            CollectionAction::Remove => {
                state.rows.remove(&2);
            }
            CollectionAction::Replace => {
                state.rows.remove(&1);
                state.rows.insert(1, row(1));
            }
            CollectionAction::ReplaceTwo => {
                state.rows.remove(&1);
                state.rows.remove(&3);
                state.rows.insert(5, row(5));
                state.rows.insert(6, row(6));
            }
            CollectionAction::Configure => state.rows.get_mut(&1).unwrap().configuration += 1,
            CollectionAction::Noop => {}
        }
    }
}
fn row(id: u8) -> EntryState {
    EntryState {
        value: usize::from(id),
        configuration: 0,
        detail: Some(Cell::new(usize::from(id))),
    }
}
/// Keeps a reducer-local count that must survive reorder and configuration refresh.
struct EntryFeature {
    cache: Cell<usize>,
    _capture: Capture,
}
impl Feature for EntryFeature {
    type State = EntryState;
    type Action = EntryAction;
    fn update(
        &mut self,
        state: &mut EntryState,
        action: &EntryAction,
        context: &mut FeatureContext<'_, EntryState, EntryAction>,
    ) {
        match action {
            EntryAction::Touch => {
                self.cache.set(self.cache.get() + 1);
                state.value = self.cache.get();
            }
            EntryAction::Leave => {
                let context = context.reducer_context();
                context.leave(context.scope_token());
            }
        }
    }
}
/// Gives the nested child an independent cache and disposal capture.
struct DetailFeature {
    _capture: Capture,
    cache: Cell<usize>,
}
impl Feature for DetailFeature {
    type State = Cell<usize>;
    type Action = DetailAction;
    fn update(
        &mut self,
        state: &mut Self::State,
        action: &DetailAction,
        _: &mut FeatureContext<'_, Self::State, DetailAction>,
    ) {
        let DetailAction::Add(amount) = action;
        self.cache.set(self.cache.get() + 1);
        state.set(state.get() + amount);
    }
}
type EntrySite = MountSite<CollectionState, EntryState, EntryAction, CollectionAction>;
type DetailSite = MountSite<CollectionState, Cell<usize>, DetailAction, CollectionAction>;

/// Drives at most 32 actor turns; it does not test wake notification delivery.
fn run<T>(driver: &Driver, future: impl Future<Output = T>) -> T {
    let mut future = Box::pin(future);
    for _ in 0..32 {
        if let Poll::Ready(value) = poll(future.as_mut()) {
            return value;
        }
        driver.turn();
    }
    panic!("bounded actor operation did not complete")
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "keep reorder, replacement and explicit leave assertions in one collection lifetime"
)]
fn full_collection_tuple_authoring_reorder_replace_and_explicit_leave() {
    let driver = Driver::default();
    let trace = Trace::default();
    let sites = Arc::new(Mutex::new(Vec::<(u8, EntrySite, DetailSite)>::new()));
    let make_trace = std::sync::Arc::clone(&trace);
    let make_sites = std::sync::Arc::clone(&sites);
    let make_capture = Capture("make drop", 0, std::sync::Arc::clone(&trace));
    let make_cell = Cell::new(0);
    let store = RootMount::new(CollectionRoot)
        .on_initial(|_| (), {
            let trace = std::sync::Arc::clone(&trace);
            move |_, _| {
                trace.lock().push(("initial root", 0));
                Task::run(|_| async {})
            }
        })
        .builder(
            CollectionState {
                rows: [(1, row(1)), (2, row(2)), (3, row(3))]
                    .into_iter()
                    .collect(),
            },
            Runtime::new(driver.clone()),
        )
        .keyed_with_children(
            |state| &state.rows,
            |state| &mut state.rows,
            move |id, _row| {
                let _ = &make_capture;
                make_cell.set(make_cell.get() + 1);
                let id = *id;
                make_trace.lock().push(("make", id));
                let config_capture = Capture("config drop", id, std::sync::Arc::clone(&make_trace));
                let initial_capture =
                    Capture("initial drop", id, std::sync::Arc::clone(&make_trace));
                let initial_trace = std::sync::Arc::clone(&make_trace);
                let entry = EntryMount::new(EntryFeature {
                    cache: Cell::new(0),
                    _capture: Capture("entry drop", id, std::sync::Arc::clone(&make_trace)),
                })
                .configuration(move |row| {
                    let _ = &config_capture;
                    row.configuration
                })
                .on_initial(
                    move |row| {
                        let _ = &initial_capture;
                        Box::new(row.value)
                    },
                    move |_, _| {
                        initial_trace.lock().push(("initial entry", id));
                        Task::run(|_| async {})
                    },
                );
                let recipe_capture = Capture("recipe drop", id, std::sync::Arc::clone(&make_trace));
                let trace = std::sync::Arc::clone(&make_trace);
                let sites = std::sync::Arc::clone(&make_sites);
                (entry, move |_row, children| {
                    let _ = &recipe_capture;
                    trace.lock().push(("recipe", id));
                    let child_trace = std::sync::Arc::clone(&trace);
                    let initial_trace = std::sync::Arc::clone(&trace);
                    let detail = children.optional(
                        OptionalMount::new(
                            |row: &EntryState| row.detail.as_ref(),
                            |row: &mut EntryState| row.detail.as_mut(),
                            |row: &EntryState| {
                                row.detail.as_ref().map(|_| MountIdentity {
                                    key: (),
                                    replacement: 0,
                                    configuration: 0,
                                })
                            },
                            move |_| DetailFeature {
                                _capture: Capture(
                                    "detail drop",
                                    id,
                                    std::sync::Arc::clone(&child_trace),
                                ),
                                cache: Cell::new(0),
                            },
                        )
                        .on_initial(Cell::get, move |_, _| {
                            initial_trace.lock().push(("initial detail", id));
                            Task::run(|_| async {})
                        }),
                    );
                    sites.lock().push((id, children.site().clone(), detail));
                    let source = children.site().clone();
                    let output_capture = Capture("output drop", id, std::sync::Arc::clone(&trace));
                    let _output = children.output::<()>(&source, |handlers| {
                        handlers.at(&source, move |_, (), _| {
                            let _ = &output_capture;
                            crate::OutputFlow::Consumed
                        });
                    });
                })
            },
        )
        .build()
        .unwrap();
    driver.turn();
    assert_eq!(
        &trace.lock()[..6],
        &[
            ("make", 1),
            ("make", 2),
            ("make", 3),
            ("recipe", 1),
            ("recipe", 2),
            ("recipe", 3)
        ]
    );
    assert_eq!(
        trace
            .lock()
            .iter()
            .filter(|(event, _)| event.starts_with("initial "))
            .copied()
            .collect::<Vec<_>>(),
        [
            ("initial detail", 1),
            ("initial entry", 1),
            ("initial detail", 2),
            ("initial entry", 2),
            ("initial detail", 3),
            ("initial entry", 3),
            ("initial root", 0)
        ]
    );
    let (_, old_entry_site, old_detail_site) = sites.lock()[0].clone();
    let entry = run(&driver, store.acquire(&old_entry_site))
        .unwrap()
        .unwrap();
    let detail = run(&driver, store.acquire(&old_detail_site))
        .unwrap()
        .unwrap();
    drop(run(&driver, entry.send(EntryAction::Touch)).unwrap());
    drop(run(&driver, detail.send(DetailAction::Add(7))).unwrap());
    trace.lock().clear();
    drop(run(&driver, store.send(CollectionAction::Reorder)).unwrap());
    assert!(trace.lock().is_empty());
    drop(run(&driver, entry.send(EntryAction::Touch)).unwrap());
    assert_eq!(
        run(&driver, entry.read(|row| row.value)).unwrap(),
        2,
        "reorder retains Feature cache"
    );
    assert_eq!(run(&driver, detail.read(Cell::get)).unwrap(), 8);
    drop(run(&driver, store.send(CollectionAction::Configure)).unwrap());
    assert!(trace.lock().is_empty());
    assert_eq!(
        run(&driver, entry.read(|_| panic!("stale entry selector")))
            .unwrap_err()
            .reason(),
        TaskAccessReason::ScopeEnded
    );
    let entry = run(&driver, store.acquire(&old_entry_site))
        .unwrap()
        .unwrap();
    drop(run(&driver, entry.send(EntryAction::Touch)).unwrap());
    assert_eq!(run(&driver, entry.read(|row| row.value)).unwrap(), 3);
    trace.lock().clear();
    drop(run(&driver, entry.send(EntryAction::Leave)).unwrap());
    assert_eq!(
        trace
            .lock()
            .iter()
            .filter(|(event, _)| *event == "make")
            .copied()
            .collect::<Vec<_>>(),
        [("make", 1)]
    );
    assert_eq!(
        trace
            .lock()
            .iter()
            .filter(|(event, _)| *event == "recipe")
            .copied()
            .collect::<Vec<_>>(),
        [("recipe", 1)]
    );
    let events = trace.lock().clone();
    let make = events
        .iter()
        .position(|event| *event == ("make", 1))
        .unwrap();
    for event in [
        "detail drop",
        "entry drop",
        "initial drop",
        "config drop",
        "recipe drop",
        "output drop",
    ] {
        assert!(
            events.iter().position(|item| *item == (event, 1)).unwrap() < make,
            "{event} before successor"
        );
    }
    assert_eq!(sites.lock().len(), 4);
    run(&driver, store.acquire(&old_entry_site))
        .as_ref()
        .expect_err("retired entry acquisition must fail");
    trace.lock().clear();
    drop(run(&driver, store.send(CollectionAction::Insert)).unwrap());
    assert_eq!(
        trace
            .lock()
            .iter()
            .filter(|(event, _)| *event == "make")
            .copied()
            .collect::<Vec<_>>(),
        [("make", 4)]
    );
    assert_eq!(
        trace
            .lock()
            .iter()
            .filter(|(event, _)| event.starts_with("initial "))
            .copied()
            .collect::<Vec<_>>(),
        [("initial detail", 4), ("initial entry", 4)]
    );
    trace.lock().clear();
    drop(run(&driver, store.send(CollectionAction::Remove)).unwrap());
    assert!(!trace.lock().iter().any(|(event, _)| *event == "make"));
    trace.lock().clear();
    drop(run(&driver, store.send(CollectionAction::Replace)).unwrap());
    assert_eq!(
        trace
            .lock()
            .iter()
            .filter(|(event, _)| *event == "make")
            .copied()
            .collect::<Vec<_>>(),
        [("make", 1)]
    );
    trace.lock().clear();
    let current_site = sites
        .lock()
        .iter()
        .rev()
        .find(|(id, _, _)| *id == 1)
        .unwrap()
        .1
        .clone();
    let retained_context = run(&driver, store.acquire(&current_site)).unwrap().unwrap();
    let mut replace = Box::pin(store.send(CollectionAction::ReplaceTwo));
    assert!(poll(replace.as_mut()).is_pending());
    let mut queued = Box::pin(store.acquire(&current_site));
    assert!(poll(queued.as_mut()).is_pending());
    driver.turn();
    assert!(matches!(poll(replace.as_mut()), Poll::Ready(Ok(_))));
    assert!(matches!(poll(queued.as_mut()), Poll::Ready(Err(_))));
    assert!(
        run(
            &driver,
            retained_context.read(|_| panic!("retired queued context selector"))
        )
        .is_err()
    );
    let events = trace.lock().clone();
    let make = events
        .iter()
        .position(|(event, _)| *event == "make")
        .unwrap();
    for id in [1, 3] {
        for event in [
            "detail drop",
            "entry drop",
            "initial drop",
            "config drop",
            "recipe drop",
            "output drop",
        ] {
            assert!(
                events.iter().position(|item| *item == (event, id)).unwrap() < make,
                "{event} {id} before first successor"
            );
        }
    }
    assert_eq!(
        events
            .iter()
            .filter(|(event, _)| *event == "make" || *event == "recipe")
            .copied()
            .collect::<Vec<_>>(),
        [("make", 5), ("make", 6), ("recipe", 5), ("recipe", 6)]
    );
    run(&driver, store.shutdown()).unwrap();
    driver.turn();
    assert!(driver.0.lock().is_empty());
}

#[test]
fn collection_without_children_accepts_move_only_cell_factory() {
    let driver = Driver::default();
    let trace = Trace::default();
    let capture = Capture("make drop", 0, std::sync::Arc::clone(&trace));
    let count = Cell::new(0);
    let store = Store::builder(
        CollectionState {
            rows: [(1, row(1)), (2, row(2))].into_iter().collect(),
        },
        CollectionRoot,
        Runtime::new(driver.clone()),
    )
    .keyed(
        |state| &state.rows,
        |state| &mut state.rows,
        move |id, _| {
            let _ = &capture;
            count.set(count.get() + 1);
            EntryMount::new(EntryFeature {
                cache: Cell::new(0),
                _capture: Capture("entry drop", *id, std::sync::Arc::clone(&trace)),
            })
            .on_initial(
                |_| (),
                |_, _| {
                    Task::run(|context| async move {
                        drop(
                            context
                                .update(|state: &mut EntryState| state.value += 10)
                                .await
                                .unwrap(),
                        );
                    })
                },
            )
        },
    )
    .build()
    .unwrap();
    driver.turn();
    driver.turn();
    assert_eq!(
        run(
            &driver,
            store.read(|state| state
                .rows
                .iter()
                .map(|(key, row)| (*key, row.value))
                .collect::<Vec<_>>())
        )
        .unwrap(),
        [(1, 11), (2, 12)]
    );
    drop(run(&driver, store.send(CollectionAction::Reorder)).unwrap());
    assert_eq!(
        run(
            &driver,
            store.read(|state| state
                .rows
                .iter()
                .map(|(key, row)| (*key, row.value))
                .collect::<Vec<_>>())
        )
        .unwrap(),
        [(2, 12), (1, 11)]
    );
    drop(run(&driver, store.send(CollectionAction::Noop)).unwrap());
    run(&driver, store.shutdown()).unwrap();
    driver.turn();
    assert!(driver.0.lock().is_empty());
}
