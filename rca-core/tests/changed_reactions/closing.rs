macro_rules! closing_cases {
    ($name:ident, $($api:ident)::+) => {
        mod $name {
            use super::Driver;
            use $($api)::+::{Feature, FeatureContext, OptionalMount, Runtime, Store, Task};
            use rca_core::ReactionChange;
            use rca_core::{InitialPolicy, MountIdentity, ReactionContext, TaskAccessReason, TaskProgress};
            use std::sync::{Arc, Mutex, atomic::{AtomicUsize, Ordering}};

            struct Root;
            struct Child;
            impl Feature for Root {
                type State = Option<i32>;
                type Action = ();
                fn update(&mut self, _: &mut Self::State, _: &(), _: &mut FeatureContext<'_, Self::State, ()>) {}
            }
            impl Feature for Child {
                type State = i32;
                type Action = ();
                fn update(&mut self, _: &mut i32, _: &(), _: &mut FeatureContext<'_, i32, ()>) {}
            }
            type Owner = Arc<Mutex<Option<Store<Option<i32>, ()>>>>;
            type Trace = Arc<Mutex<Vec<&'static str>>>;
            struct Value {
                current: i32,
                index: usize,
                phase: usize,
                owner: Owner,
                trace: Trace,
            }
            impl PartialEq for Value {
                fn eq(&self, other: &Self) -> bool {
                    self.trace.lock().unwrap().push("equality");
                    if self.phase == 1 { drop(self.owner.lock().unwrap().take()); }
                    self.current == other.current
                }
            }
            impl Drop for Value {
                fn drop(&mut self) {
                    self.trace.lock().unwrap().push(if self.index == 0 { "old drop" } else { "current drop" });
                    if (self.phase == 3 && self.index == 0) || (self.phase == 4 && self.index == 1) {
                        drop(self.owner.lock().unwrap().take());
                    }
                }
            }
            struct WorkDrop(Arc<AtomicUsize>);
            impl Drop for WorkDrop {
                fn drop(&mut self) { self.0.fetch_add(1, Ordering::SeqCst); }
            }

            struct InitialState {
                child: Option<i32>,
                owner: Arc<Mutex<Option<Store<InitialState, ()>>>>,
                constructed: Arc<AtomicUsize>,
                close_structural: bool,
                trace: Trace,
            }
            struct InitialRoot;
            impl Feature for InitialRoot {
                type State = InitialState;
                type Action = ();
                fn update(&mut self, _: &mut InitialState, _: &(), _: &mut FeatureContext<'_, InitialState, ()>) {}
            }

            fn initial_projection_closure(close_structural: bool, close_factory: bool) {
                for changed_initial in [false, true] {
                    let driver = Driver::default();
                    let owner = Arc::new(Mutex::new(None));
                    let trace = Trace::default();
                    let constructed = Arc::new(AtomicUsize::new(0));
                    let make_constructed = Arc::clone(&constructed);
                    let make_trace = Arc::clone(&trace);
                    let selected_owner = Arc::clone(&owner);
                    let selected_trace = Arc::clone(&trace);
                    let factory_owner = Arc::clone(&owner);
                    let factories = Arc::new(AtomicUsize::new(0));
                    let factory_count = Arc::clone(&factories);
                    let factory_trace = Arc::clone(&trace);
                    let bodies = Arc::new(AtomicUsize::new(0));
                    let factory_bodies = Arc::clone(&bodies);
                    let work_drops = Arc::new(AtomicUsize::new(0));
                    let factory_drops = Arc::clone(&work_drops);
                    let mount = OptionalMount::new(
                        |state: &InitialState| {
                            let constructed = state.constructed.load(Ordering::SeqCst) != 0;
                            state.trace.lock().unwrap().push(if constructed { "reaction read" } else { "installation read" });
                            if constructed && state.close_structural {
                                drop(state.owner.lock().unwrap().take());
                            }
                            state.child.as_ref()
                        },
                        |state: &mut InitialState| state.child.as_mut(),
                        |state| {
                            state.trace.lock().unwrap().push("identity");
                            state.child.map(|_| MountIdentity { key: (), replacement: 0, configuration: 0 })
                        },
                        move |_| {
                            make_trace.lock().unwrap().push("constructed");
                            // Installation has selected the child; the next read belongs to reactions.
                            make_constructed.store(1, Ordering::SeqCst);
                            Child
                        },
                    );
                    let select = move |value: &i32| {
                        selected_trace.lock().unwrap().push("selected");
                        if !close_structural && !close_factory { drop(selected_owner.lock().unwrap().take()); }
                        *value
                    };
                    let factory = move |_: ReactionChange<i32>, _: ReactionContext<'_, i32, ()>| {
                        factory_trace.lock().unwrap().push("factory");
                        factory_count.fetch_add(1, Ordering::SeqCst);
                        let bodies = Arc::clone(&factory_bodies);
                        let guard = WorkDrop(Arc::clone(&factory_drops));
                        let task = Task::run(move |_| async move {
                            bodies.fetch_add(1, Ordering::SeqCst);
                            drop(guard);
                        });
                        if close_factory { drop(factory_owner.lock().unwrap().take()); }
                        task
                    };
                    let mount = if changed_initial {
                        mount.on_change(select, InitialPolicy::Run, factory)
                    } else {
                        mount.on_initial(select, factory)
                    };
                    let later_select_trace = Arc::clone(&trace);
                    let later_factory_trace = Arc::clone(&trace);
                    let later_factory_count = Arc::clone(&factories);
                    let later_bodies = Arc::clone(&bodies);
                    let mount = mount.on_change(move |value| {
                        later_select_trace.lock().unwrap().push("later selected");
                        *value
                    }, InitialPolicy::Run, move |_, _| {
                        later_factory_trace.lock().unwrap().push("later factory");
                        later_factory_count.fetch_add(1, Ordering::SeqCst);
                        let bodies = Arc::clone(&later_bodies);
                        Task::run(move |_| async move { bodies.fetch_add(1, Ordering::SeqCst); })
                    });
                    let store = Store::builder(InitialState {
                        child: Some(4),
                        owner: Arc::clone(&owner),
                        constructed,
                        close_structural,
                        trace: Arc::clone(&trace),
                    }, InitialRoot, Runtime::new(driver.clone())).mount(mount).build().unwrap();
                    let ready = store.ready();
                    *owner.lock().unwrap() = Some(store);
                    driver.turn();
                    assert!(owner.lock().unwrap().is_none(), "change initial: {changed_initial}");
                    let expected: &[&str] = if close_structural {
                        &["identity", "installation read", "constructed", "reaction read"]
                    } else if close_factory {
                        &["identity", "installation read", "constructed", "reaction read", "selected", "factory"]
                    } else {
                        &["identity", "installation read", "constructed", "reaction read", "selected"]
                    };
                    assert_eq!(*trace.lock().unwrap(), expected, "change initial: {changed_initial}");
                    assert_eq!(factories.load(Ordering::SeqCst), usize::from(close_factory));
                    driver.turn();
                    assert_eq!(bodies.load(Ordering::SeqCst), 0);
                    assert_eq!(work_drops.load(Ordering::SeqCst), usize::from(close_factory));
                    let error = driver.finish(ready).expect_err("last owner closed before readiness");
                    assert_eq!(error.reason(), TaskAccessReason::StoreClosed);
                    assert_eq!(error.progress(), TaskProgress::NotStarted);
                    assert_eq!(*trace.lock().unwrap(), expected, "shutdown cannot resume initial reactions");
                }
            }

            #[test]
            fn closing_initial_structural_read_suppresses_projection_factory_and_work() {
                initial_projection_closure(true, false);
            }

            #[test]
            fn closing_initial_selected_value_suppresses_factory_and_work() {
                initial_projection_closure(false, false);
            }

            #[test]
            fn closing_initial_factory_discards_unstarted_work_before_readiness() {
                initial_projection_closure(false, true);
            }

            #[test]
            #[expect(
                clippy::too_many_lines,
                reason = "Keeps each closure checkpoint, retained child and stale-input assertions in one ownership timeline"
            )]
            fn closing_each_reaction_checkpoint_prevents_later_entry_and_stale_capture() {
                for phase in 0..5 {
                    let driver = Driver::default();
                    let owner = Owner::default();
                    let trace = Trace::default();
                    let select_owner = Arc::clone(&owner);
                    let select_trace = Arc::clone(&trace);
                    let mut selected = 0;
                    let factory_owner = Arc::clone(&owner);
                    let factory_trace = Arc::clone(&trace);
                    let work_drops = Arc::new(AtomicUsize::new(0));
                    let factory_drops = Arc::clone(&work_drops);
                    let bodies = Arc::new(AtomicUsize::new(0));
                    let factory_bodies = Arc::clone(&bodies);
                    let later_trace = Arc::clone(&trace);
                    let mut later_selected = 0;
                    let later_factories = Arc::new(AtomicUsize::new(0));
                    let later_factory_count = Arc::clone(&later_factories);
                    let mount = OptionalMount::new(
                        |state: &Option<i32>| state.as_ref(),
                        |state: &mut Option<i32>| state.as_mut(),
                        |state| {
                            state.map(|_| MountIdentity {
                                key: (),
                                replacement: 0,
                                configuration: 0,
                            })
                        },
                        |_| Child,
                    )
                    .on_change(
                        move |value| {
                            let index = selected;
                            selected += 1;
                            select_trace.lock().unwrap().push(if index == 0 {
                                "before"
                            } else {
                                "after"
                            });
                            if phase == 0 && index == 1 {
                                drop(select_owner.lock().unwrap().take());
                            }
                            Value {
                                current: *value,
                                index,
                                phase,
                                owner: Arc::clone(&select_owner),
                                trace: Arc::clone(&select_trace),
                            }
                        },
                        InitialPolicy::Skip,
                        move |change, _| {
                            factory_trace.lock().unwrap().push("factory entered");
                            drop(factory_owner.lock().unwrap().take());
                            drop(change);
                            factory_trace.lock().unwrap().push("factory completed");
                            let guard = WorkDrop(Arc::clone(&factory_drops));
                            let bodies = Arc::clone(&factory_bodies);
                            Task::run(move |_| async move {
                                bodies.fetch_add(1, Ordering::SeqCst);
                                drop(guard);
                            })
                        },
                    )
                    .on_change(
                        move |value| {
                            later_trace.lock().unwrap().push(if later_selected == 0 {
                                "later before"
                            } else {
                                "later after"
                            });
                            later_selected += 1;
                            *value
                        },
                        InitialPolicy::Skip,
                        move |_, _| {
                            later_factory_count.fetch_add(1, Ordering::SeqCst);
                            Task::run(|_| async {})
                        },
                    );
                    let (builder, site) =
                        Store::builder(Some(4), Root, Runtime::new(driver.clone()))
                            .mount_with_site(mount);
                    let store = builder.build().unwrap();
                    driver.finish(store.ready()).unwrap();
                    let child = driver.finish(store.acquire(&site)).unwrap().unwrap();
                    *owner.lock().unwrap() = Some(store);
                    let mutation_trace = Arc::clone(&trace);
                    let committed = driver
                        .finish(child.update(move |value| {
                            mutation_trace.lock().unwrap().push("mutation");
                            *value = if phase < 3 { 5 } else { 4 };
                        }))
                        .unwrap();
                    assert!(committed.revision() > 0);
                    assert!(owner.lock().unwrap().is_none(), "phase {phase}");
                    let observed = trace.lock().unwrap().clone();
                    assert_eq!(
                        &observed[..4],
                        ["before", "later before", "mutation", "after"]
                    );
                    assert!(
                        !observed.contains(&"later after"),
                        "phase {phase}: {observed:?}"
                    );
                    assert_eq!(observed.contains(&"equality"), phase != 0);
                    assert_eq!(observed.contains(&"factory entered"), phase == 2);
                    assert_eq!(observed.contains(&"factory completed"), phase == 2);
                    assert_eq!(
                        observed.iter().filter(|event| **event == "old drop").count(),
                        1
                    );
                    assert_eq!(
                        observed.iter().filter(|event| **event == "current drop").count(),
                        1
                    );
                    assert_eq!(later_factories.load(Ordering::SeqCst), 0);
                    assert_eq!(work_drops.load(Ordering::SeqCst), usize::from(phase == 2));
                    driver.turn();
                    assert_eq!(bodies.load(Ordering::SeqCst), 0);
                    let mutations = Arc::new(AtomicUsize::new(0));
                    let stale_mutations = Arc::clone(&mutations);
                    let error = driver
                        .finish(child.update(move |_| {
                            stale_mutations.fetch_add(1, Ordering::SeqCst);
                        }))
                        .unwrap_err();
                    assert_eq!(error.reason(), TaskAccessReason::StoreClosed);
                    assert_eq!(error.progress(), TaskProgress::NotStarted);
                    assert_eq!(mutations.load(Ordering::SeqCst), 0);
                    assert_eq!(
                        *trace.lock().unwrap(),
                        observed,
                        "stale input cannot capture reactions"
                    );
                }
            }

            struct CaptureState {
                child: Option<i32>,
                owner: Arc<Mutex<Option<Store<CaptureState, ()>>>>,
                trace: Trace,
                armed: Arc<AtomicUsize>,
                close_at: usize,
            }
            struct CaptureRoot;
            impl Feature for CaptureRoot {
                type State = CaptureState;
                type Action = ();
                fn update(&mut self, _: &mut CaptureState, _: &(), _: &mut FeatureContext<'_, CaptureState, ()>) {}
            }
            fn projection(state: &CaptureState) -> Option<&i32> {
                let phase = state.armed.load(Ordering::SeqCst);
                if phase != 0 {
                    state.trace.lock().unwrap().push("reaction read");
                    if (state.close_at == 0 && phase == 1)
                        || (state.close_at == 2 && phase == 2)
                    {
                        drop(state.owner.lock().unwrap().take());
                    }
                }
                state.child.as_ref()
            }

            #[test]
            fn closing_child_reaction_capture_or_saved_projection_stops_later_work() {
                for close_at in 0..3 {
                    let driver = Driver::default();
                    let owner = Arc::new(Mutex::new(None));
                    let trace = Trace::default();
                    let armed = Arc::new(AtomicUsize::new(0));
                    let mutations = Arc::new(AtomicUsize::new(0));
                    let drops = Arc::new(AtomicUsize::new(0));
                    let factories = Arc::new(AtomicUsize::new(0));
                    let later_selected = Arc::new(AtomicUsize::new(0));
                    let later = Arc::new(AtomicUsize::new(0));
                    let selected_owner = Arc::clone(&owner);
                    let selected_trace = Arc::clone(&trace);
                    let selected_armed = Arc::clone(&armed);
                    let factory_count = Arc::clone(&factories);
                    let later_selection = Arc::clone(&later_selected);
                    let later_count = Arc::clone(&later);
                    let mount = OptionalMount::new(
                        projection,
                        |state: &mut CaptureState| state.child.as_mut(),
                        |state: &CaptureState| state.child.map(|_| MountIdentity { key: (), replacement: 0, configuration: 0 }),
                        |_| Child,
                    ).on_change(move |value| {
                        selected_trace.lock().unwrap().push("selected");
                        if close_at == 1 && selected_armed.load(Ordering::SeqCst) == 1 {
                            drop(selected_owner.lock().unwrap().take());
                        }
                        *value
                    }, InitialPolicy::Skip, move |_, _| {
                        factory_count.fetch_add(1, Ordering::SeqCst);
                        Task::run(|_| async {})
                    }).on_change(move |value| {
                        later_selection.fetch_add(1, Ordering::SeqCst);
                        *value
                    }, InitialPolicy::Skip, move |_, _| {
                        later_count.fetch_add(1, Ordering::SeqCst);
                        Task::run(|_| async {})
                    });
                    let (builder, site) = Store::builder(CaptureState {
                        child: Some(4),
                        owner: Arc::clone(&owner),
                        trace: Arc::clone(&trace),
                        armed: Arc::clone(&armed),
                        close_at,
                    }, CaptureRoot, Runtime::new(driver.clone()))
                        .mount_with_site(mount);
                    let store = builder.build().unwrap();
                    driver.finish(store.ready()).unwrap();
                    let child = driver.finish(store.acquire(&site)).unwrap().unwrap();
                    *owner.lock().unwrap() = Some(store);
                    armed.store(1, Ordering::SeqCst);
                    let changed = Arc::clone(&mutations);
                    let after = Arc::clone(&armed);
                    let guard = WorkDrop(Arc::clone(&drops));
                    let result = driver.finish(child.update(move |state| {
                        let _guard = guard;
                        changed.fetch_add(1, Ordering::SeqCst);
                        *state = 5;
                        after.store(2, Ordering::SeqCst);
                    }));
                    if close_at == 2 {
                        let committed = result.expect("closure follows mutation");
                        assert!(committed.revision() > 0);
                    } else {
                        let error = result.unwrap_err();
                        assert_eq!(error.reason(), TaskAccessReason::StoreClosed);
                        assert_eq!(error.progress(), TaskProgress::MayHaveRun);
                    }
                    assert!(owner.lock().unwrap().is_none());
                    assert_eq!(mutations.load(Ordering::SeqCst), usize::from(close_at == 2));
                    assert_eq!(drops.load(Ordering::SeqCst), 1);
                    assert_eq!(factories.load(Ordering::SeqCst), 0);
                    assert_eq!(later_selected.load(Ordering::SeqCst), usize::from(close_at == 2));
                    assert_eq!(later.load(Ordering::SeqCst), 0);
                    let expected: &[&str] = match close_at {
                        0 => &["reaction read"],
                        1 => &["reaction read", "selected"],
                        // Both hooks read before mutation; the third read is
                        // the first hook's saved post-mutation continuation.
                        _ => &["reaction read", "selected", "reaction read", "reaction read"],
                    };
                    assert_eq!(*trace.lock().unwrap(), expected, "closure point {close_at}");
                    driver.turn();
                    assert_eq!(factories.load(Ordering::SeqCst), 0);
                    assert_eq!(later.load(Ordering::SeqCst), 0);
                    let stale_mutations = Arc::clone(&mutations);
                    let error = driver.finish(child.update(move |_| {
                        stale_mutations.fetch_add(1, Ordering::SeqCst);
                    })).unwrap_err();
                    assert_eq!(error.reason(), TaskAccessReason::StoreClosed);
                    assert_eq!(error.progress(), TaskProgress::NotStarted);
                    assert_eq!(mutations.load(Ordering::SeqCst), usize::from(close_at == 2));
                }
            }
        }
    };
}
