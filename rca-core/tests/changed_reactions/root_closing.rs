macro_rules! root_closing_cases {
    ($name:ident, $($api:ident)::+) => {
        mod $name {
            use super::Driver;
            use $($api)::+::{Feature, FeatureContext, OptionalMount, RootMount, Runtime, Store, Task};
            use rca_core::{InitialPolicy, MountIdentity, ReactionContext, TaskAccessReason, TaskProgress};
            use rca_core::ReactionChange;
            use std::sync::{Arc, Mutex, atomic::{AtomicUsize, Ordering}};

            struct Root;
            impl Feature for Root {
                type State = i32;
                type Action = ();
                fn update(&mut self, _: &mut i32, _: &(), _: &mut FeatureContext<'_, i32, ()>) {}
            }

            struct Child(Arc<AtomicUsize>);
            impl Drop for Child {
                fn drop(&mut self) { self.0.fetch_add(1, Ordering::SeqCst); }
            }
            impl Feature for Child {
                type State = i32;
                type Action = ();
                fn update(&mut self, _: &mut i32, _: &(), _: &mut FeatureContext<'_, i32, ()>) {}
            }

            #[test]
            fn absent_child_reinstalls_with_new_reaction_history_and_no_retained_owner() {
                let driver = Driver::default();
                let root_selections = Arc::new(AtomicUsize::new(0));
                let root_select = Arc::clone(&root_selections);
                let child_selections = Arc::new(AtomicUsize::new(0));
                let child_select = Arc::clone(&child_selections);
                let child_drops = Arc::new(AtomicUsize::new(0));
                let make_drops = Arc::clone(&child_drops);
                let changes = Arc::new(Mutex::new(Vec::new()));
                let record = Arc::clone(&changes);
                let child = OptionalMount::new(
                    |state: &i32| (*state > 0).then_some(state),
                    |state: &mut i32| (*state > 0).then_some(state),
                    |state| (*state > 0).then_some(MountIdentity { key: (), replacement: 0, configuration: 0 }),
                    move |_| Child(Arc::clone(&make_drops)),
                ).on_change(move |value| {
                    child_select.fetch_add(1, Ordering::SeqCst);
                    *value
                }, InitialPolicy::Run, move |change, _| {
                    record.lock().unwrap().push(match change {
                        ReactionChange::Initial { current } => (None, current),
                        ReactionChange::Changed { previous, current } => (Some(previous), current),
                    });
                    Task::run(|_| async {})
                });
                let (send, receive) = futures_channel::oneshot::channel();
                let mut send = Some(send);
                let (builder, site) = RootMount::new(Root)
                    .on_initial(|_| (), move |_, _| {
                        let send = send.take().unwrap();
                        Task::run(move |context| async move {
                            let _ = send.send(context);
                            std::future::pending::<()>().await;
                        })
                    })
                    .on_change(move |value| {
                        root_select.fetch_add(1, Ordering::SeqCst);
                        *value
                    }, InitialPolicy::Skip, |_, _| Task::run(|_| async {}))
                    .builder(0, Runtime::new(driver.clone()))
                    .mount_with_site(child);
                let store = builder.build().unwrap();
                driver.finish(store.ready()).unwrap();
                let root = driver.finish(receive).unwrap();
                assert!(driver.finish(store.acquire(&site)).unwrap().is_none());
                assert!(changes.lock().unwrap().is_empty());
                assert_eq!(child_selections.load(Ordering::SeqCst), 0);

                let before_absent = root_selections.load(Ordering::SeqCst);
                driver.finish(root.update(|state| *state = 0)).unwrap();
                assert!(root_selections.load(Ordering::SeqCst) > before_absent);
                assert_eq!(child_selections.load(Ordering::SeqCst), 0);
                assert!(changes.lock().unwrap().is_empty());

                driver.finish(root.update(|state| *state = 1)).unwrap();
                let stale = driver.finish(store.acquire(&site)).unwrap().unwrap();
                assert_eq!(driver.finish(stale.read(|value| *value)).unwrap(), 1);
                driver.finish(root.update(|state| *state = 2)).unwrap();
                assert_eq!(*changes.lock().unwrap(), [(None, 1), (Some(1), 2)]);

                driver.finish(root.update(|state| *state = 0)).unwrap();
                assert_eq!(child_drops.load(Ordering::SeqCst), 1);
                let error = driver.finish(stale.read(|_| panic!("retired child callback"))).unwrap_err();
                assert_eq!((error.reason(), error.progress()), (TaskAccessReason::ScopeEnded, TaskProgress::NotStarted));
                let absent_selections = child_selections.load(Ordering::SeqCst);
                driver.finish(root.update(|state| *state = 0)).unwrap();
                assert_eq!(child_selections.load(Ordering::SeqCst), absent_selections);

                driver.finish(root.update(|state| *state = 3)).unwrap();
                let current = driver.finish(store.acquire(&site)).unwrap().unwrap();
                assert_eq!(driver.finish(current.read(|value| *value)).unwrap(), 3);
                let error = driver.finish(stale.read(|_| panic!("old incarnation callback"))).unwrap_err();
                assert_eq!((error.reason(), error.progress()), (TaskAccessReason::ScopeEnded, TaskProgress::NotStarted));
                driver.finish(root.update(|state| *state = 4)).unwrap();
                assert_eq!(*changes.lock().unwrap(), [(None, 1), (Some(1), 2), (None, 3), (Some(3), 4)]);

                drop(store);
                driver.turn();
                assert_eq!(child_drops.load(Ordering::SeqCst), 2);
                let error = driver.finish(root.update(|_| panic!("closed root callback"))).unwrap_err();
                assert_eq!((error.reason(), error.progress()), (TaskAccessReason::StoreClosed, TaskProgress::NotStarted));
            }

            #[test]
            fn mixed_reaction_roster_preserves_changed_child_capture() {
                let driver = Driver::default();
                let trace = Arc::new(Mutex::new(Vec::new()));
                let select_trace = Arc::clone(&trace);
                let factory_trace = Arc::clone(&trace);
                let (send, receive) = futures_channel::oneshot::channel();
                let mut send = Some(send);
                let child = OptionalMount::new(
                    |state: &i32| Some(state),
                    |state: &mut i32| Some(state),
                    |_| Some(MountIdentity { key: (), replacement: 0, configuration: 0 }),
                    |_| Root,
                ).on_change(move |state| {
                    select_trace.lock().unwrap().push(*state);
                    *state
                }, InitialPolicy::Skip, move |change, _| {
                    assert!(matches!(change, ReactionChange::Changed { previous: 4, current: 5 }));
                    factory_trace.lock().unwrap().push(99);
                    Task::run(|_| async {})
                });
                let passive = OptionalMount::new(
                    |state: &i32| Some(state),
                    |state: &mut i32| Some(state),
                    |_| Some(MountIdentity { key: (), replacement: 0, configuration: 0 }),
                    |_| Root,
                );
                let (builder, passive_site) = RootMount::new(Root)
                    .on_initial(|_| (), move |_, _| {
                        let send = send.take().unwrap();
                        Task::run(move |context| async move {
                            let _ = send.send(context);
                            std::future::pending::<()>().await;
                        })
                    })
                    .builder(4, Runtime::new(driver.clone()))
                    .mount(child).mount_with_site(passive);
                let store = builder.build().unwrap();
                driver.finish(store.ready()).unwrap();
                let passive = driver.finish(store.acquire(&passive_site)).unwrap().unwrap();
                let context = driver.finish(receive).unwrap();
                driver.finish(context.update(|value| *value = 5)).unwrap();
                assert_eq!(*trace.lock().unwrap(), [4, 5, 99]);
                assert_eq!(driver.finish(passive.read(|value| *value)).unwrap(), 5);
                drop(store);
                driver.turn();
            }

            #[test]
            fn before_selector_shutdown_prevents_mutation_and_later_capture() {
                let driver = Driver::default();
                let owner = Arc::new(Mutex::new(None::<Store<i32, ()>>));
                let trace = Arc::new(Mutex::new(Vec::new()));
                let selector_owner = Arc::clone(&owner);
                let selector_trace = Arc::clone(&trace);
                let factory_trace = Arc::clone(&trace);
                let later_trace = Arc::clone(&trace);
                let (send, receive) = futures_channel::oneshot::channel();
                let mut send = Some(send);
                let store = RootMount::new(Root)
                    .on_initial(|_| (), move |_, _| {
                        let send = send.take().unwrap();
                        Task::run(move |context| async move {
                            let _ = send.send(context);
                            std::future::pending::<()>().await;
                        })
                    })
                    .on_change(move |state| {
                        selector_trace.lock().unwrap().push("before");
                        drop(selector_owner.lock().unwrap().take());
                        *state
                    }, InitialPolicy::Skip, move |_, _| {
                        factory_trace.lock().unwrap().push("factory");
                        Task::run(|_| async {})
                    })
                    .on_change(move |state| {
                        later_trace.lock().unwrap().push("later before");
                        *state
                    }, InitialPolicy::Skip, |_, _| Task::run(|_| async {}))
                    .builder(4, Runtime::new(driver.clone()))
                    .build().unwrap();
                driver.finish(store.ready()).unwrap();
                let context = driver.finish(receive).unwrap();
                *owner.lock().unwrap() = Some(store);
                let mutation_trace = Arc::clone(&trace);
                let error = driver.finish(context.update(move |value| {
                    mutation_trace.lock().unwrap().push("mutation");
                    *value = 5;
                })).unwrap_err();
                assert_eq!(error.reason(), TaskAccessReason::StoreClosed);
                assert_eq!(error.progress(), TaskProgress::MayHaveRun);
                assert!(owner.lock().unwrap().is_none());
                driver.turn();
                assert_eq!(*trace.lock().unwrap(), ["before"]);
                let error = driver.finish(context.update(|_| panic!("closed update"))).unwrap_err();
                assert_eq!(error.reason(), TaskAccessReason::StoreClosed);
                assert_eq!(error.progress(), TaskProgress::NotStarted);
            }

            #[test]
            #[expect(
                clippy::too_many_lines,
                reason = "One recipe lifetime compares live capture/map/reduce order with final-owner closure before mapping."
            )]
            fn mapped_command_captures_reactions_before_mapping_and_rejects_capture_shutdown() {
                struct Input {
                    value: Box<i32>,
                    trace: Arc<Mutex<Vec<&'static str>>>,
                    disposed: Arc<Mutex<Vec<(usize, i32)>>>,
                }
                impl Drop for Input {
                    fn drop(&mut self) {
                        self.trace.lock().unwrap().push("input drop");
                        self.disposed.lock().unwrap().push((
                            std::ptr::from_ref(self.value.as_ref()) as usize,
                            *self.value,
                        ));
                    }
                }
                struct CommandChild {
                    trace: Arc<Mutex<Vec<&'static str>>>,
                    drops: Arc<AtomicUsize>,
                }
                impl Drop for CommandChild {
                    fn drop(&mut self) {
                        self.drops.fetch_add(1, Ordering::SeqCst);
                    }
                }
                impl Feature for CommandChild {
                    type State = i32;
                    type Action = i32;
                    fn update(
                        &mut self,
                        state: &mut i32,
                        action: &i32,
                        _: &mut FeatureContext<'_, i32, i32>,
                    ) {
                        self.trace.lock().unwrap().push("reduce");
                        *state += action;
                    }
                }

                let driver = Driver::default();
                let owner = Arc::new(Mutex::new(None::<Store<i32, ()>>));
                let armed = Arc::new(std::sync::atomic::AtomicBool::new(false));
                let trace = Arc::new(Mutex::new(Vec::new()));
                let disposed = Arc::new(Mutex::new(Vec::new()));
                let child_drops = Arc::new(AtomicUsize::new(0));
                let site = Arc::new(Mutex::new(None));
                let declared = Arc::clone(&site);
                let select_owner = Arc::clone(&owner);
                let select_armed = Arc::clone(&armed);
                let select_trace = Arc::clone(&trace);
                let factory_trace = Arc::clone(&trace);
                let child_trace = Arc::clone(&trace);
                let make_drops = Arc::clone(&child_drops);
                let map_trace = Arc::clone(&trace);
                let (builder, child_site) = RootMount::new(Root)
                    .on_change(
                        move |state| {
                            select_trace.lock().unwrap().push("capture");
                            if select_armed.load(Ordering::SeqCst) {
                                drop(select_owner.lock().unwrap().take());
                            }
                            *state
                        },
                        InitialPolicy::Skip,
                        move |change, _| {
                            assert!(matches!(
                                change,
                                ReactionChange::Changed {
                                    previous: 4,
                                    current: 7
                                }
                            ));
                            factory_trace.lock().unwrap().push("reaction");
                            let work_trace = Arc::clone(&factory_trace);
                            Task::run(move |_| async move {
                                work_trace.lock().unwrap().push("work");
                            })
                        },
                    )
                    .builder(4, Runtime::new(driver.clone()))
                    .mount_with_children(
                        OptionalMount::new(
                            |state: &i32| Some(state),
                            |state: &mut i32| Some(state),
                            |_| {
                                Some(MountIdentity {
                                    key: (),
                                    replacement: 0,
                                    configuration: 0,
                                })
                            },
                            move |_| CommandChild {
                                trace: Arc::clone(&child_trace),
                                drops: Arc::clone(&make_drops),
                            },
                        ),
                        move |_, children| {
                            let map_trace = Arc::clone(&map_trace);
                            *declared.lock().unwrap() = Some(children.command(move |input: Input| {
                                map_trace.lock().unwrap().push("map");
                                let value = *input.value;
                                drop(input);
                                value
                            }));
                        },
                    );
                let store = builder.build().unwrap();
                driver.finish(store.ready()).unwrap();
                let child = driver.finish(store.acquire(&child_site)).unwrap().unwrap();
                let command_site = site.lock().unwrap().take().unwrap();
                let port = driver.finish(store.command_port(&command_site)).unwrap().unwrap();
                *owner.lock().unwrap() = Some(store);
                assert!(trace.lock().unwrap().is_empty());
                let first = Box::new(3);
                let first_address = std::ptr::from_ref(first.as_ref()) as usize;
                let task = driver
                    .finish(port.send(Input {
                        value: first,
                        trace: Arc::clone(&trace),
                        disposed: Arc::clone(&disposed),
                    }))
                    .unwrap();
                driver.finish(task.finish());
                assert_eq!(driver.finish(child.read(|value| *value)).unwrap(), 7);
                assert_eq!(
                    *trace.lock().unwrap(),
                    [
                        "capture",
                        "map",
                        "input drop",
                        "reduce",
                        "capture",
                        "reaction",
                        "work",
                    ]
                );
                assert_eq!(*disposed.lock().unwrap(), [(first_address, 3)]);

                trace.lock().unwrap().clear();
                armed.store(true, Ordering::SeqCst);
                let second = Box::new(5);
                let second_address = std::ptr::from_ref(second.as_ref()) as usize;
                let Err(rca_core::TaskSendError::Failed { error }) = driver.finish(port.send(Input {
                    value: second,
                    trace: Arc::clone(&trace),
                    disposed: Arc::clone(&disposed),
                })) else {
                    panic!("entered capture shutdown must fail without recovering or mapping the input");
                };
                assert_eq!(
                    (error.reason(), error.progress()),
                    (TaskAccessReason::StoreClosed, TaskProgress::MayHaveRun)
                );
                assert!(owner.lock().unwrap().is_none());
                driver.turn();
                assert_eq!(*trace.lock().unwrap(), ["capture", "input drop"]);
                assert_eq!(*disposed.lock().unwrap(), [(first_address, 3), (second_address, 5)]);
                assert_eq!(child_drops.load(Ordering::SeqCst), 1);
                assert_eq!(Arc::strong_count(&owner), 1);
                assert_eq!(Arc::strong_count(&trace), 1);
            }

            #[test]
            fn configuration_selector_shutdown_stops_initial_and_changed_reactions() {
                for close_during_initial in [true, false] {
                    let driver = Driver::default();
                    let owner = Arc::new(Mutex::new(None::<Store<i32, ()>>));
                    let trace = Arc::new(Mutex::new(Vec::new()));
                    let configuration_owner = Arc::clone(&owner);
                    let configuration_trace = Arc::clone(&trace);
                    let initial_trace = Arc::clone(&trace);
                    let select_trace = Arc::clone(&trace);
                    let factory_trace = Arc::clone(&trace);
                    let (send, receive) = futures_channel::oneshot::channel();
                    let mut send = Some(send);
                    let mut first = true;
                    let store = RootMount::new(Root)
                        .configuration(move |_| {
                            configuration_trace.lock().unwrap().push("configuration");
                            if first == close_during_initial {
                                drop(configuration_owner.lock().unwrap().take());
                            }
                            first = false;
                            1
                        })
                        .on_initial(|_| (), move |_, _| {
                            initial_trace.lock().unwrap().push("initial");
                            let send = send.take().unwrap();
                            Task::run(move |context| async move {
                                let _ = send.send(context);
                                std::future::pending::<()>().await;
                            })
                        })
                        .on_change(move |state| {
                            select_trace.lock().unwrap().push("selected");
                            *state
                        }, InitialPolicy::Skip, move |_, _| {
                            factory_trace.lock().unwrap().push("factory");
                            Task::run(|_| async {})
                        })
                        .builder(4, Runtime::new(driver.clone()))
                        .build().unwrap();
                    if close_during_initial {
                        *owner.lock().unwrap() = Some(store);
                        driver.turn();
                        driver.turn();
                        assert_eq!(*trace.lock().unwrap(), ["configuration"]);
                        driver.finish(receive).unwrap_err();
                    } else {
                        driver.finish(store.ready()).unwrap();
                        let context = driver.finish(receive).unwrap();
                        assert_eq!(*trace.lock().unwrap(), ["configuration", "initial"]);
                        trace.lock().unwrap().clear();
                        *owner.lock().unwrap() = Some(store);
                        let mutation_trace = Arc::clone(&trace);
                        let committed = driver.finish(context.update(move |value| {
                            mutation_trace.lock().unwrap().push("mutation");
                            *value = 5;
                        })).unwrap();
                        assert!(committed.revision() > 0);
                        driver.turn();
                        assert_eq!(*trace.lock().unwrap(), ["selected", "mutation", "configuration"]);
                    }
                    assert!(owner.lock().unwrap().is_none());
                }
            }

            #[test]
            fn initial_selector_shutdown_suppresses_its_factory_and_later_reactions() {
                for changed_initial in [false, true] {
                    let driver = Driver::default();
                    let owner = Arc::new(Mutex::new(None::<Store<i32, ()>>));
                    let selector_owner = std::sync::Arc::clone(&owner);
                    let trace = Arc::new(Mutex::new(Vec::new()));
                    let selector_trace = std::sync::Arc::clone(&trace);
                    let select = move |state: &i32| {
                        selector_trace.lock().unwrap().push("closing selector");
                        drop(selector_owner.lock().unwrap().take());
                        *state
                    };
                    let factory_trace = std::sync::Arc::clone(&trace);
                    let factory = move |_: ReactionChange<i32>, _: ReactionContext<'_, i32, ()>| {
                        factory_trace.lock().unwrap().push("closed factory");
                        let body_trace = std::sync::Arc::clone(&factory_trace);
                        Task::run(move |_| async move {
                            body_trace.lock().unwrap().push("closed work");
                        })
                    };
                    let root = RootMount::new(Root);
                    let root = if changed_initial {
                        root.on_change(select, InitialPolicy::Run, factory)
                    } else {
                        root.on_initial(select, factory)
                    };
                    let later_select = std::sync::Arc::clone(&trace);
                    let later_factory = std::sync::Arc::clone(&trace);
                    let store = root
                        .on_change(
                            move |state| {
                                later_select.lock().unwrap().push("later selector");
                                *state
                            },
                            InitialPolicy::Run,
                            move |_, _| {
                                later_factory.lock().unwrap().push("later factory");
                                Task::run(|_| async {})
                            },
                        )
                        .builder(4, Runtime::new(driver.clone()))
                        .build()
                        .unwrap();
                    *owner.lock().unwrap() = Some(store);
                    driver.turn();
                    assert!(owner.lock().unwrap().is_none());
                    driver.turn();
                    assert_eq!(*trace.lock().unwrap(), ["closing selector"]);
                }
            }
        }
    };
}
