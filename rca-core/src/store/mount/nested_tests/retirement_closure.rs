//! Real actor retirement with reentrant closure, acquisition and destructor panic.

use super::*;

/// Closes the last Store owner during child disposal, before any successor work.
struct ClosingChild {
    owner: Arc<Mutex<Option<Store<State, RootAction>>>>,
    trace: Arc<Mutex<Vec<&'static str>>>,
}
impl Drop for ClosingChild {
    fn drop(&mut self) {
        self.trace.lock().push("child drop");
        drop(self.owner.lock().take());
    }
}
impl Feature for ClosingChild {
    type State = Cell<usize>;
    type Action = ChildAction;
    fn update(
        &mut self,
        _: &mut Self::State,
        _: &ChildAction,
        _: &mut FeatureContext<'_, Self::State, ChildAction>,
    ) {
    }
}

#[test]
fn retired_descendant_drop_closes_root_before_successor_constructor_or_recipe() {
    let driver = Driver::default();
    let owner = Arc::new(Mutex::new(None));
    let trace = Arc::new(Mutex::new(Vec::new()));
    let make_trace = std::sync::Arc::clone(&trace);
    let recipe_trace = std::sync::Arc::clone(&trace);
    let child_owner = std::sync::Arc::clone(&owner);
    let (declaration, parent_site) =
        super::super::nested::attach_selected_key::<State, RootAction, RowFeature, _, _>(
            |state| &state.rows,
            |state| &mut state.rows,
            "one".to_owned(),
            move |_| {
                make_trace.lock().push("row make");
                RowFeature(std::sync::Arc::clone(&make_trace))
            },
            move |_, children| {
                recipe_trace.lock().push("recipe");
                let trace = std::sync::Arc::clone(&recipe_trace);
                let owner = std::sync::Arc::clone(&child_owner);
                children.optional(OptionalMount::new(
                    |row: &Row| row.child.as_ref(),
                    |row: &mut Row| row.child.as_mut(),
                    |row: &Row| {
                        row.child.as_ref().map(|_| MountIdentity {
                            key: (),
                            replacement: 0,
                            configuration: 0,
                        })
                    },
                    move |_| {
                        trace.lock().push("child make");
                        ClosingChild {
                            owner: std::sync::Arc::clone(&owner),
                            trace: std::sync::Arc::clone(&trace),
                        }
                    },
                ));
            },
        );
    let store = Store::builder(
        State {
            rows: [(
                "one".to_owned(),
                Row {
                    child: Some(Cell::new(1)),
                },
            )]
            .into_iter()
            .collect(),
            configuration: 0,
        },
        RootFeature,
        Runtime::new(driver.clone()),
    )
    .with_mount(declaration)
    .build()
    .unwrap();
    driver.turn();
    assert_eq!(*trace.lock(), ["row make", "recipe", "child make"]);
    let mut acquire = Box::pin(store.acquire(&parent_site));
    assert!(poll(acquire.as_mut()).is_pending());
    driver.turn();
    let Poll::Ready(Ok(Some(parent))) = poll(acquire.as_mut()) else {
        panic!("parent context")
    };
    *owner.lock() = Some(store);
    trace.lock().clear();
    let mut leave = Box::pin(parent.send(RowAction::Leave));
    assert!(poll(leave.as_mut()).is_pending());
    driver.turn();
    assert!(poll(leave.as_mut()).is_ready());
    assert!(owner.lock().is_none());
    assert_eq!(*trace.lock(), ["child drop", "row drop"]);
}

/// Runs the acquisition probe before its wrapped parent feature is dropped.
struct AcquiringParent {
    feature: RowFeature,
    /// Polls public acquisition reentrantly while the parent destructor is running.
    inspect: Box<dyn FnMut() + Send>,
}
impl Drop for AcquiringParent {
    fn drop(&mut self) {
        (self.inspect)();
    }
}
impl Feature for AcquiringParent {
    type State = Row;
    type Action = RowAction;
    fn update(
        &mut self,
        state: &mut Row,
        action: &RowAction,
        context: &mut FeatureContext<'_, Row, RowAction>,
    ) {
        self.feature.update(state, action, context);
    }
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "One actor retirement observes the weak child site inside its parent's destructor and proves mailbox admission remains available"
)]
fn retired_child_site_rejects_in_parent_destructor_without_queuing_acquisition() {
    let driver = Driver::default();
    let trace = Arc::new(Mutex::new(Vec::new()));
    let owner = Arc::new(Mutex::new(None::<Store<State, RootAction>>));
    let child_site = Arc::new(Mutex::new(None));
    let observed = Arc::new(Mutex::new(Vec::new()));
    let make_trace = Arc::clone(&trace);
    let make_owner = Arc::clone(&owner);
    let make_site = Arc::clone(&child_site);
    let make_observed = Arc::clone(&observed);
    let recipe_trace = Arc::clone(&trace);
    let recipe_site = Arc::clone(&child_site);
    let (declaration, _) =
        super::super::nested::attach_selected_key::<State, RootAction, AcquiringParent, _, _>(
            |state| &state.rows,
            |state| &mut state.rows,
            "one".to_owned(),
            move |_| {
                make_trace.lock().push("row make");
                let owner = Arc::clone(&make_owner);
                let site = Arc::clone(&make_site);
                let observed = Arc::clone(&make_observed);
                AcquiringParent {
                    feature: RowFeature(Arc::clone(&make_trace)),
                    inspect: Box::new(move || {
                        let Some(store) = owner.lock().clone() else {
                            return;
                        };
                        let site = site.lock().clone().expect("installed child site");
                        let mut acquisition = Box::pin(store.acquire(&site));
                        let rejected = matches!(
                            poll(acquisition.as_mut()),
                            Poll::Ready(Err(error))
                                if error.reason() == TaskAccessReason::StoreClosed
                                    && error.progress() == TaskProgress::NotStarted
                        );
                        drop(acquisition);
                        // Capacity one distinguishes immediate rejection from a
                        // pending metadata request whose receiver was abandoned.
                        let admitted = store.try_send(RootAction::Noop).is_ok();
                        observed.lock().push((rejected, admitted));
                    }),
                }
            },
            move |_, children| {
                recipe_trace.lock().push("recipe");
                let trace = Arc::clone(&recipe_trace);
                *recipe_site.lock() = Some(children.optional(OptionalMount::new(
                    |row: &Row| row.child.as_ref(),
                    |row: &mut Row| row.child.as_mut(),
                    |row: &Row| {
                        row.child.as_ref().map(|_| MountIdentity {
                            key: (),
                            replacement: 0,
                            configuration: 0,
                        })
                    },
                    move |_| {
                        trace.lock().push("child make");
                        ChildFeature {
                            cache: Cell::new(0),
                            trace: Arc::clone(&trace),
                        }
                    },
                )));
            },
        );
    let store = Store::builder(
        State {
            rows: [(
                "one".to_owned(),
                Row {
                    child: Some(Cell::new(1)),
                },
            )]
            .into_iter()
            .collect(),
            configuration: 0,
        },
        RootFeature,
        Runtime::new(driver.clone()),
    )
    .mailbox_capacity(1)
    .with_mount(declaration)
    .build()
    .unwrap();
    driver.turn();
    assert_eq!(*trace.lock(), ["row make", "recipe", "child make"]);
    *owner.lock() = Some(store.clone());
    trace.lock().clear();
    let mut replacement = Box::pin(store.send(RootAction::Replace));
    assert!(poll(replacement.as_mut()).is_pending());
    driver.turn();
    assert!(matches!(poll(replacement.as_mut()), Poll::Ready(Ok(_))));
    drop(replacement);
    assert_eq!(
        *observed.lock(),
        [(true, true)],
        "retired weak site must reject before admission and leave the mailbox slot free"
    );
    assert_eq!(
        *trace.lock(),
        ["child drop", "row drop", "row make", "recipe", "child make"]
    );
    driver.turn();
    assert_eq!(observed.lock().len(), 1, "successor remained installed");
    owner.lock().take();
    drop(store);
    driver.turn();
    assert!(driver.0.lock().is_empty());
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one actor mutation keeps mixed retirement, Changed factory and delayed task entry in a single trace"
)]
fn mixed_retirement_precedes_changed_factory_and_tasks() {
    /// Projects an active structural branch without inventing a new branch API.
    enum Branch {
        Active(usize),
        Inactive,
    }

    /// Keeps optional-state removal independent of the structural branch switch.
    struct MixedState {
        optional: Option<usize>,
        branch: Branch,
    }

    /// Removes both forms in the same exclusive actor mutation.
    struct MixedRoot;

    impl Feature for MixedRoot {
        type State = MixedState;
        type Action = ();

        fn update(
            &mut self,
            state: &mut MixedState,
            (): &(),
            _: &mut FeatureContext<'_, MixedState, ()>,
        ) {
            state.optional = None;
            state.branch = Branch::Inactive;
        }
    }

    /// Records actual Feature disposal, not state projection or declaration removal.
    struct RetiringFeature {
        trace: Arc<Mutex<Vec<&'static str>>>,
        event: &'static str,
    }

    impl Feature for RetiringFeature {
        type State = usize;
        type Action = ();

        fn update(&mut self, _: &mut usize, (): &(), _: &mut FeatureContext<'_, usize, ()>) {}
    }

    impl Drop for RetiringFeature {
        fn drop(&mut self) {
            self.trace.lock().push(self.event);
        }
    }

    let driver = Driver::default();
    let trace = Arc::new(Mutex::new(Vec::new()));
    let optional_trace = Arc::clone(&trace);
    let optional = OptionalMount::new(
        |state: &MixedState| state.optional.as_ref(),
        |state: &mut MixedState| state.optional.as_mut(),
        |state: &MixedState| {
            state.optional.map(|_| MountIdentity {
                key: (),
                replacement: 0,
                configuration: 0,
            })
        },
        move |_| RetiringFeature {
            trace: Arc::clone(&optional_trace),
            event: "optional drop",
        },
    );
    let branch_trace = Arc::clone(&trace);
    let branch = OptionalMount::new(
        |state: &MixedState| match &state.branch {
            Branch::Active(value) => Some(value),
            Branch::Inactive => None,
        },
        |state: &mut MixedState| match &mut state.branch {
            Branch::Active(value) => Some(value),
            Branch::Inactive => None,
        },
        |state: &MixedState| {
            matches!(state.branch, Branch::Active(_)).then_some(MountIdentity {
                key: (),
                replacement: 0,
                configuration: 0,
            })
        },
        move |_| RetiringFeature {
            trace: Arc::clone(&branch_trace),
            event: "branch drop",
        },
    );
    let changed_trace = Arc::clone(&trace);
    let child_trace = Arc::clone(&trace);

    // Declaration topology: root -> [optional, branch -> [first, second]].
    // Root declarations reconcile forwards. Retiring the branch drops its
    // children in reverse roster order, before their parent Feature.
    let (builder, _) = crate::RootMount::new(MixedRoot)
        .on_change(
            |state| {
                (
                    state.optional.is_some(),
                    matches!(state.branch, Branch::Active(_)),
                )
            },
            crate::InitialPolicy::Skip,
            move |change, _| {
                assert!(matches!(
                    change,
                    ReactionChange::Changed {
                        previous: (true, true),
                        current: (false, false),
                    }
                ));
                changed_trace.lock().push("changed factory");
                let trace = Arc::clone(&changed_trace);
                crate::Task::run(move |_| async move {
                    trace.lock().push("changed task");
                })
            },
        )
        .builder(
            MixedState {
                optional: Some(1),
                branch: Branch::Active(2),
            },
            Runtime::new(driver.clone()),
        )
        .mount(optional)
        .mount_with_children(branch, move |_, children| {
            for event in ["branch first drop", "branch second drop"] {
                let trace = Arc::clone(&child_trace);
                children.optional(OptionalMount::new(
                    |state: &usize| Some(state),
                    |state: &mut usize| Some(state),
                    |_| {
                        Some(MountIdentity {
                            key: (),
                            replacement: 0,
                            configuration: 0,
                        })
                    },
                    move |_| RetiringFeature {
                        trace: Arc::clone(&trace),
                        event,
                    },
                ));
            }
        });
    let store = builder.build().unwrap();
    driver.turn();
    assert!(
        trace.lock().is_empty(),
        "InitialPolicy::Skip creates no work"
    );

    let mut mutation = Box::pin(store.send(()));
    assert!(poll(mutation.as_mut()).is_pending());
    driver.turn();
    let Poll::Ready(Ok(task)) = poll(mutation.as_mut()) else {
        panic!("the combined retirement mutation must commit");
    };
    drop(mutation);

    assert_eq!(
        *trace.lock(),
        [
            "optional drop",
            "branch second drop",
            "branch first drop",
            "branch drop",
            "changed factory",
        ]
    );
    assert!(!task.is_finished(), "the Changed task has not entered");
    let mut finished = Box::pin(task.finish());
    assert!(poll(finished.as_mut()).is_pending());

    driver.turn();
    assert!(poll(finished.as_mut()).is_ready());
    assert_eq!(
        *trace.lock(),
        [
            "optional drop",
            "branch second drop",
            "branch first drop",
            "branch drop",
            "changed factory",
            "changed task",
        ]
    );

    let mut shutdown = Box::pin(store.shutdown_into_state());
    assert!(poll(shutdown.as_mut()).is_pending());
    driver.turn();
    let Poll::Ready(Ok(state)) = poll(shutdown.as_mut()) else {
        panic!("the actor must finish after the Changed task");
    };
    assert!(state.optional.is_none());
    assert!(matches!(state.branch, Branch::Inactive));
    assert!(driver.0.lock().is_empty());
}

/// Optionally panics on disposal; the owned capture proves unwind cleanup.
struct UnwindingChild {
    trace: Arc<Mutex<Vec<&'static str>>>,
    panics: bool,
    _capture: Arc<()>,
}
impl Drop for UnwindingChild {
    fn drop(&mut self) {
        self.trace.lock().push(if self.panics {
            "panicking child drop"
        } else {
            "remaining child drop"
        });
        assert!(!self.panics, "injected descendant destructor panic");
    }
}
impl Feature for UnwindingChild {
    type State = Cell<usize>;
    type Action = ChildAction;
    fn update(
        &mut self,
        _: &mut Self::State,
        _: &ChildAction,
        _: &mut FeatureContext<'_, Self::State, ChildAction>,
    ) {
    }
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "One actor retirement retains the real descendant iterator across a destructor panic"
)]
fn retired_descendant_panic_disposes_remaining_prefix_without_successors() {
    let driver = Driver::default();
    let trace = Arc::new(Mutex::new(Vec::new()));
    let captures = Arc::new(Mutex::new(Vec::new()));
    let make_trace = Arc::clone(&trace);
    let recipe_trace = Arc::clone(&trace);
    let recipe_captures = Arc::clone(&captures);
    let (declaration, parent_site) =
        super::super::nested::attach_selected_key::<State, RootAction, RowFeature, _, _>(
            |state| &state.rows,
            |state| &mut state.rows,
            "one".to_owned(),
            move |_| {
                make_trace.lock().push("row make");
                RowFeature(Arc::clone(&make_trace))
            },
            move |_, children| {
                recipe_trace.lock().push("recipe");
                for panics in [false, true] {
                    let trace = Arc::clone(&recipe_trace);
                    let capture = Arc::new(());
                    recipe_captures.lock().push(Arc::downgrade(&capture));
                    children.optional(OptionalMount::new(
                        |row: &Row| row.child.as_ref(),
                        |row: &mut Row| row.child.as_mut(),
                        |row: &Row| {
                            row.child.as_ref().map(|_| MountIdentity {
                                key: (),
                                replacement: 0,
                                configuration: 0,
                            })
                        },
                        move |_| {
                            trace.lock().push("child make");
                            UnwindingChild {
                                trace: Arc::clone(&trace),
                                panics,
                                _capture: Arc::clone(&capture),
                            }
                        },
                    ));
                }
            },
        );
    let store = Store::builder(
        State {
            rows: [(
                "one".to_owned(),
                Row {
                    child: Some(Cell::new(1)),
                },
            )]
            .into_iter()
            .collect(),
            configuration: 0,
        },
        RootFeature,
        Runtime::new(driver.clone()),
    )
    .with_mount(declaration)
    .build()
    .unwrap();
    driver.turn();
    assert_eq!(
        *trace.lock(),
        ["row make", "recipe", "child make", "child make"]
    );
    assert_eq!(captures.lock().len(), 2);
    let mut acquire = Box::pin(store.acquire(&parent_site));
    assert!(poll(acquire.as_mut()).is_pending());
    driver.turn();
    let Poll::Ready(Ok(Some(parent))) = poll(acquire.as_mut()) else {
        panic!("parent context");
    };
    trace.lock().clear();
    let mut leave = Box::pin(parent.send(RowAction::Leave));
    assert!(poll(leave.as_mut()).is_pending());
    assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| driver.turn())).is_err());
    assert!(matches!(
        poll(leave.as_mut()),
        Poll::Ready(Err(crate::TaskSendError::Failed { error }))
            if error.reason() == TaskAccessReason::ActorFailed
                && error.progress() == TaskProgress::MayHaveRun
    ));
    let events = trace.lock().clone();
    assert_eq!(events.first(), Some(&"panicking child drop"));
    assert!(
        events.iter().all(|event| event.ends_with("drop")),
        "no successor constructor or recipe after the panic: {events:?}"
    );
    for event in ["panicking child drop", "remaining child drop", "row drop"] {
        assert_eq!(events.iter().filter(|seen| **seen == event).count(), 1);
    }
    assert!(
        captures
            .lock()
            .iter()
            .all(|capture| capture.upgrade().is_none()),
        "both installed payloads and declaration captures were disposed"
    );
    assert!(matches!(
        poll(Box::pin(store.ready()).as_mut()),
        Poll::Ready(Err(error)) if error.reason() == TaskAccessReason::ActorFailed
    ));
    assert!(driver.0.lock().is_empty());
}
