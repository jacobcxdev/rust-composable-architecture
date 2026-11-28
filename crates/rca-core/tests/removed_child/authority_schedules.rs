//! Removal authority across parent reconfiguration, producer retirement and store closure.

use super::*;
use std::sync::atomic::AtomicBool;

#[cfg(feature = "test-util")]
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "One waiting-transfer sequence preserves parent reconfiguration and original owned-value recovery"
)]
fn parent_reconfiguration_returns_waiting_owned_transfer() {
    /// Keeps the same parent identity while changing its configuration revision.
    struct ConfiguredParent {
        child: Option<Child>,
        configuration: u64,
    }
    /// Leaves reconfiguration to the explicitly scheduled parent update.
    struct ConfiguredRootFeature;
    impl Feature for ConfiguredRootFeature {
        type State = Option<ConfiguredParent>;
        type Action = ();
        fn update(
            &mut self,
            _: &mut Self::State,
            (): &(),
            _: &mut FeatureContext<'_, Self::State, ()>,
        ) {
        }
    }
    /// Holds the removable child without authoring unrelated work.
    struct ConfiguredParentFeature;
    impl Feature for ConfiguredParentFeature {
        type State = ConfiguredParent;
        type Action = ();
        fn update(
            &mut self,
            _: &mut Self::State,
            (): &(),
            _: &mut FeatureContext<'_, Self::State, ()>,
        ) {
        }
    }

    let driver = Driver::default();
    let gate = rca_core::testing::TransactionGate::new();
    let old = Box::new(Cell::new(7));
    let identity = (&raw const *old) as usize;
    let sites = Arc::new(Mutex::new(None));
    let export = Arc::clone(&sites);
    let (builder, parent_site) = RootMount::new(ConfiguredRootFeature)
        .builder(
            Some(ConfiguredParent {
                child: Some(Child { value: old }),
                configuration: 0,
            }),
            Runtime::new(driver.clone()),
        )
        .mailbox_capacity(1)
        .mount_with_children(
            OptionalMount::new(
                |root: &Option<ConfiguredParent>| root.as_ref(),
                |root: &mut Option<ConfiguredParent>| root.as_mut(),
                |root: &Option<ConfiguredParent>| {
                    root.as_ref().map(|parent| MountIdentity {
                        key: (),
                        replacement: 0,
                        configuration: parent.configuration,
                    })
                },
                |_| ConfiguredParentFeature,
            ),
            move |_, children| {
                *export.lock().unwrap() = Some(children.optional_removable(
                    |parent: &ConfiguredParent| &parent.child,
                    |parent: &mut ConfiguredParent| &mut parent.child,
                    |parent: &ConfiguredParent| {
                        parent.child.as_ref().map(|_| MountIdentity {
                            key: (),
                            replacement: 0,
                            configuration: 0,
                        })
                    },
                    |_| ChildFeature,
                ));
            },
        );
    let handlers = Arc::new(AtomicUsize::new(0));
    let entered = Arc::clone(&handlers);
    let (builder, output) = builder.output::<Box<Cell<usize>>>(&parent_site, |handlers| {
        handlers.at_root(move |_, value, _| {
            entered.fetch_add(1, Ordering::SeqCst);
            drop(value);
            OutputFlow::Consumed
        });
    });
    let store = builder.transaction_gate(gate.clone()).build().unwrap();
    driver.turn();
    assert_eq!(
        gate.pending_kind(),
        Some(rca_core::testing::TransactionKind::Structural)
    );
    assert!(gate.release_pending());
    driver.turn();
    assert!(gate.acknowledge(gate.boundary().unwrap()));
    driver.finish(store.ready()).unwrap();
    let (child_site, removal) = sites.lock().unwrap().take().unwrap();
    let parent = driver.finish(store.acquire(&parent_site)).unwrap().unwrap();
    let child = driver.finish(store.acquire(&child_site)).unwrap().unwrap();

    let captures = Arc::new(AtomicUsize::new(0));
    let capture = DropWatch(Arc::clone(&captures));
    let receives = Arc::new(AtomicUsize::new(0));
    let delivery_count = Arc::clone(&receives);
    let mut remove = Box::pin(parent.remove_child(
        &removal,
        &child,
        |_| {},
        move |_, removed, context| {
            removed
                .into_output(
                    context,
                    &output,
                    |child| child.value,
                    move |result| {
                        let Err(rca_core::OutputError::Returned { value, error }) = result else {
                            panic!("reconfigured parent returns the original owned output");
                        };
                        assert_eq!(error.reason(), rca_core::TaskAccessReason::ScopeEnded);
                        assert_eq!(error.progress(), rca_core::TaskProgress::NotStarted);
                        assert_eq!((&raw const *value) as usize, identity);
                        assert_eq!(value.get(), 7);
                        delivery_count.fetch_add(1, Ordering::SeqCst);
                        drop((value, capture));
                    },
                )
                .unwrap();
        },
    ));
    assert!(poll(remove.as_mut()).is_pending());
    driver.turn();
    assert_eq!(
        gate.pending_kind(),
        Some(rca_core::testing::TransactionKind::Update)
    );
    let mut refresh = Box::pin(parent.update(|parent| parent.configuration = 1));
    assert!(poll(refresh.as_mut()).is_pending());
    assert!(store.try_send(()).unwrap_err().is_full());
    assert!(gate.release_pending());
    driver.turn();
    let Poll::Ready(Ok(task)) = poll(remove.as_mut()) else {
        panic!("removal committed");
    };
    for _ in 0..3 {
        driver.turn();
    }
    assert_eq!(
        receives.load(Ordering::SeqCst),
        0,
        "transfer waits behind full ingress"
    );
    assert_eq!(captures.load(Ordering::SeqCst), 0);
    assert_eq!(handlers.load(Ordering::SeqCst), 0);
    assert!(gate.acknowledge(gate.boundary().unwrap()));
    driver.turn();
    assert_eq!(
        gate.pending_kind(),
        Some(rca_core::testing::TransactionKind::Update)
    );
    assert!(gate.release_pending());
    driver.turn();
    assert!(gate.acknowledge(gate.boundary().unwrap()));
    for _ in 0..6 {
        driver.turn();
    }
    assert!(matches!(poll(refresh.as_mut()), Poll::Ready(Ok(_))));
    driver.finish(task.finish());
    assert_eq!(receives.load(Ordering::SeqCst), 1);
    assert_eq!(captures.load(Ordering::SeqCst), 1);
    assert_eq!(handlers.load(Ordering::SeqCst), 0);
    assert_eq!(
        driver.finish(parent.read(|_| ())).unwrap_err().reason(),
        rca_core::TaskAccessReason::ScopeEnded
    );
    let current_parent = driver.finish(store.acquire(&parent_site)).unwrap().unwrap();
    assert_eq!(
        driver
            .finish(current_parent.read(|parent| (parent.configuration, parent.child.is_none())))
            .unwrap(),
        (1, true)
    );
    assert!(
        gate.pending_kind().is_none(),
        "rejected admission creates no phantom boundary"
    );
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "One cancelled-producer sequence preserves live parent removal and output delivery"
)]
fn cancelled_child_producer_does_not_cancel_live_parent_removal_output() {
    /// Owns removal independently from the child's cancelled producer.
    struct Parent;
    impl Feature for Parent {
        type State = Option<Child>;
        type Action = ();
        fn update(
            &mut self,
            _: &mut Self::State,
            (): &(),
            _: &mut FeatureContext<'_, Self::State, ()>,
        ) {
        }
    }
    /// Exports a child producer's context and counts its physical disposal.
    struct Worker {
        export: Arc<Mutex<Option<rca_core::TaskContext<Child, ()>>>>,
        drops: Arc<AtomicUsize>,
    }
    impl Feature for Worker {
        type State = Child;
        type Action = ();
        fn update(&mut self, _: &mut Child, (): &(), context: &mut FeatureContext<'_, Child, ()>) {
            let export = Arc::clone(&self.export);
            let capture = DropWatch(Arc::clone(&self.drops));
            context.task(rca_core::Task::run(move |task| async move {
                let _capture = capture;
                *export.lock().unwrap() = Some(task);
                std::future::pending::<()>().await;
            }));
        }
    }

    let driver = Driver::default();
    let old = Box::new(Cell::new(7));
    let identity = (&raw const *old) as usize;
    let parents = Arc::new(Mutex::new(None));
    let parent_export = Arc::clone(&parents);
    let child_export = Arc::new(Mutex::new(None));
    let children = Arc::clone(&child_export);
    let child_drops = Arc::new(AtomicUsize::new(0));
    let dropped = Arc::clone(&child_drops);
    let release = Arc::new(AtomicBool::new(false));
    let release_task = Arc::clone(&release);
    let (builder, site, removal) = RootMount::new(Parent)
        .on_initial(
            |_| (),
            move |_, _| {
                let export = Arc::clone(&parent_export);
                let release = Arc::clone(&release_task);
                rca_core::Task::run(move |task| async move {
                    *export.lock().unwrap() = Some(task);
                    // The manual driver polls every queued future on each turn.
                    std::future::poll_fn(move |_| {
                        if release.load(Ordering::SeqCst) {
                            Poll::Ready(())
                        } else {
                            Poll::Pending
                        }
                    })
                    .await;
                })
            },
        )
        .builder(Some(Child { value: old }), Runtime::new(driver.clone()))
        .mount_removable(
            |state| state,
            |state| state,
            |state| {
                state.as_ref().map(|_| MountIdentity {
                    key: (),
                    replacement: 0,
                    configuration: 0,
                })
            },
            move |_| Worker {
                export: Arc::clone(&children),
                drops: Arc::clone(&dropped),
            },
        );
    let (builder, output) = builder.root_output::<Box<Cell<usize>>>(|_| {});
    let store = builder.build().unwrap();
    driver.finish(store.ready()).unwrap();
    let mounted = driver.finish(store.acquire(&site)).unwrap().unwrap();
    let child_task = driver.finish(mounted.send(())).unwrap();
    driver.turn();
    let child = child_export.lock().unwrap().take().unwrap();
    child_task.cancel();
    assert_eq!(
        driver.finish(child.read(|_| ())).unwrap_err().reason(),
        rca_core::TaskAccessReason::Cancelled
    );
    driver.finish(child_task.finish());
    assert_eq!(child_drops.load(Ordering::SeqCst), 1);
    let installed = driver.finish(store.acquire(&site)).unwrap().unwrap();
    assert_eq!(
        driver
            .finish(installed.read(|child| child.value.get()))
            .unwrap(),
        7
    );
    let parent = parents.lock().unwrap().take().unwrap();
    assert_eq!(
        driver
            .finish(parent.read(|state| state.as_ref().map(|child| child.value.get())))
            .unwrap(),
        Some(7)
    );

    let before = Arc::new(AtomicUsize::new(0));
    let entered = Arc::clone(&before);
    let after = Arc::new(AtomicUsize::new(0));
    let extracted = Arc::clone(&after);
    let receives = Arc::new(AtomicUsize::new(0));
    let delivery_count = Arc::clone(&receives);
    let task = driver
        .finish(parent.remove_child(
            &removal,
            &child,
            move |child| {
                entered.fetch_add(1, Ordering::SeqCst);
                child.value.set(9);
            },
            move |_, removed, context| {
                extracted.fetch_add(1, Ordering::SeqCst);
                removed
                    .into_output(
                        context,
                        &output,
                        |child| child.value,
                        move |result| {
                            let OutputDelivery::Unhandled { value, .. } = result.unwrap() else {
                                panic!("root output");
                            };
                            assert_eq!((&raw const *value) as usize, identity);
                            assert_eq!(value.get(), 9);
                            delivery_count.fetch_add(1, Ordering::SeqCst);
                        },
                    )
                    .unwrap();
            },
        ))
        .unwrap();
    release.store(true, Ordering::SeqCst);
    driver.finish(task.finish());
    assert_eq!(before.load(Ordering::SeqCst), 1);
    assert_eq!(after.load(Ordering::SeqCst), 1);
    assert_eq!(receives.load(Ordering::SeqCst), 1);
    assert_eq!(child_drops.load(Ordering::SeqCst), 1);
    assert!(driver.finish(store.read(Option::is_none)).unwrap());
    driver.finish(store.shutdown()).unwrap();
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one real named parent distinguishes completed ancestry, cancellation and full-mailbox closure without changing child authority"
)]
fn named_parent_removal_rejects_terminal_ownership_and_closed_full_mailbox() {
    /// Exports a named producer while counting any unexpected queued reduction.
    struct Producer {
        export: Arc<Mutex<Option<rca_core::TaskContext<Parent, u8>>>>,
        complete: Arc<AtomicBool>,
        drops: Arc<AtomicUsize>,
        reductions: Arc<AtomicUsize>,
    }
    impl Feature for Producer {
        type State = Parent;
        type Action = u8;

        fn update(
            &mut self,
            _: &mut Parent,
            action: &u8,
            context: &mut FeatureContext<'_, Parent, u8>,
        ) {
            self.reductions.fetch_add(1, Ordering::SeqCst);
            assert_eq!(*action, 0, "queued filler must never enter reduction");
            let export = Arc::clone(&self.export);
            let complete = Arc::clone(&self.complete);
            let capture = DropWatch(Arc::clone(&self.drops));
            context.task(rca_core::Task::run_named(
                "parent",
                rca_core::OperationPolicy::Coexist,
                move |task| async move {
                    let _capture = capture;
                    *export.lock().unwrap() = Some(task);
                    std::future::poll_fn(move |_| {
                        if complete.load(Ordering::SeqCst) {
                            Poll::Ready(())
                        } else {
                            Poll::Pending
                        }
                    })
                    .await;
                },
            ));
        }
    }
    /// Observes completion before the actor harvests the producer's terminal state.
    struct ObserverWake {
        callback: Mutex<Option<Box<dyn FnOnce() + Send>>>,
        calls: AtomicUsize,
    }
    impl std::task::Wake for ObserverWake {
        fn wake(self: Arc<Self>) {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let callback = self.callback.lock().unwrap().take();
            if let Some(callback) = callback {
                callback();
            }
        }
    }
    /// Distinguishes ended ownership, cancellation and closure while admission waits.
    enum Rejection {
        CompletedAncestry,
        CancelledProducer,
        ClosedMailbox,
    }

    for rejection in [
        Rejection::CompletedAncestry,
        Rejection::CancelledProducer,
        Rejection::ClosedMailbox,
    ] {
        let driver = Driver::default();
        let export = Arc::new(Mutex::new(None));
        let complete = Arc::new(AtomicBool::new(false));
        let producer_drops = Arc::new(AtomicUsize::new(0));
        let reductions = Arc::new(AtomicUsize::new(0));
        let old = Box::new(Cell::new(7));
        let identity = (&raw const *old) as usize;
        let (builder, site, removal) = RootMount::new(Producer {
            export: Arc::clone(&export),
            complete: Arc::clone(&complete),
            drops: Arc::clone(&producer_drops),
            reductions: Arc::clone(&reductions),
        })
        .builder(
            Parent {
                child: Some(Child { value: old }),
            },
            Runtime::new(driver.clone()),
        )
        .mailbox_capacity(1)
        .mount_removable(
            |parent| &parent.child,
            |parent| &mut parent.child,
            |parent| {
                parent.child.as_ref().map(|_| MountIdentity {
                    key: (),
                    replacement: 0,
                    configuration: 0,
                })
            },
            |_| ChildFeature,
        );
        let store = builder.build().unwrap();
        driver.finish(store.ready()).unwrap();
        let child = driver.finish(store.acquire(&site)).unwrap().unwrap();
        let mut actor = {
            let mut queued = driver.0.lock().unwrap();
            assert_eq!(queued.len(), 1);
            queued.pop().unwrap()
        };
        let mut start = Box::pin(store.send(0));
        assert!(poll(start.as_mut()).is_pending());
        assert!(poll(actor.as_mut()).is_pending());
        let Poll::Ready(Ok(task)) = poll(start.as_mut()) else {
            panic!("named parent producer must be registered");
        };
        drop(start);
        let mut capture = Box::pin(store.operation("parent").capture());
        assert!(poll(capture.as_mut()).is_pending());
        assert!(poll(actor.as_mut()).is_pending());
        let Poll::Ready(Ok(wait)) = poll(capture.as_mut()) else {
            panic!("named parent operation must be captured while live");
        };
        drop(capture);
        let mut wait = Box::pin(wait);
        let mut run = {
            let mut queued = driver.0.lock().unwrap();
            assert_eq!(queued.len(), 1);
            queued.pop().unwrap()
        };
        assert!(poll(run.as_mut()).is_pending());
        let parent = export.lock().unwrap().take().unwrap();
        assert!(!task.is_finished());
        assert!(!task.is_cancelled());
        assert_eq!(producer_drops.load(Ordering::SeqCst), 0);
        assert_eq!(reductions.load(Ordering::SeqCst), 1);
        let calls = Arc::new(AtomicUsize::new(0));
        let callback_drops = Arc::new(AtomicUsize::new(0));
        let before = DropWatch(Arc::clone(&callback_drops));
        let after = DropWatch(Arc::clone(&callback_drops));
        let before_calls = Arc::clone(&calls);
        let after_calls = Arc::clone(&calls);
        let mut request = Box::pin(parent.remove_child(
            &removal,
            &child,
            move |child| {
                let _ = &before;
                before_calls.fetch_add(1, Ordering::SeqCst);
                child.value.set(100);
            },
            move |_, _, _| {
                let _ = &after;
                after_calls.fetch_add(1, Ordering::SeqCst);
            },
        ));

        match rejection {
            Rejection::CompletedAncestry => {
                let observed = Arc::new(AtomicBool::new(false));
                let observed_in_wake = Arc::clone(&observed);
                let task_in_wake = task.clone();
                let drops_in_wake = Arc::clone(&callback_drops);
                let wake = Arc::new(ObserverWake {
                    calls: AtomicUsize::new(0),
                    callback: Mutex::new(Some(Box::new(move || {
                        // The observer is passive: neither actor nor completing
                        // task is polled reentrantly from this completion wake.
                        assert!(!task_in_wake.is_finished());
                        assert!(!task_in_wake.is_cancelled());
                        let Poll::Ready(Err(error)) = poll(request.as_mut()) else {
                            panic!("finished named ownership must not be resurrected by removal");
                        };
                        assert_eq!(error.reason(), rca_core::TaskAccessReason::Cancelled);
                        assert_eq!(error.progress(), rca_core::TaskProgress::NotStarted);
                        assert_eq!(drops_in_wake.load(Ordering::SeqCst), 2);
                        observed_in_wake.store(true, Ordering::SeqCst);
                    }))),
                });
                let waker = Waker::from(Arc::clone(&wake));
                assert!(
                    wait.as_mut()
                        .poll(&mut Context::from_waker(&waker))
                        .is_pending()
                );
                complete.store(true, Ordering::SeqCst);
                assert!(poll(run.as_mut()).is_ready());
                assert!(observed.load(Ordering::SeqCst));
                assert_eq!(wake.calls.load(Ordering::SeqCst), 1);
                assert!(wake.callback.lock().unwrap().is_none());
                assert!(
                    !task.is_finished(),
                    "terminal publication awaits actor harvest"
                );
                assert!(poll(actor.as_mut()).is_pending());
                assert!(task.is_finished());
                assert!(!task.is_cancelled());
            }
            Rejection::CancelledProducer => {
                assert!(poll(wait.as_mut()).is_pending());
                task.cancel();
                assert!(poll(actor.as_mut()).is_pending());
                let Poll::Ready(Err(error)) = poll(request.as_mut()) else {
                    panic!("cancelled producing task must reject removal before entry");
                };
                assert_eq!(error.reason(), rca_core::TaskAccessReason::Cancelled);
                assert_eq!(error.progress(), rca_core::TaskProgress::NotStarted);
                assert_eq!(callback_drops.load(Ordering::SeqCst), 2);
                assert!(poll(run.as_mut()).is_ready());
                assert!(poll(actor.as_mut()).is_pending());
                assert!(task.is_finished());
                assert!(task.is_cancelled());
            }
            Rejection::ClosedMailbox => {
                let mut filler = Box::pin(store.try_send(1).unwrap().finish());
                assert!(store.try_send(1).unwrap_err().is_full());
                assert!(poll(filler.as_mut()).is_pending());
                assert!(poll(request.as_mut()).is_pending());
                assert_eq!(callback_drops.load(Ordering::SeqCst), 0);
                assert!(poll(wait.as_mut()).is_pending());
                let mut closed = Box::pin(store.closed());
                drop(store);
                let Poll::Ready(Err(error)) = poll(request.as_mut()) else {
                    panic!("closed full mailbox returns removal without another actor turn");
                };
                assert_eq!(error.reason(), rca_core::TaskAccessReason::StoreClosed);
                assert_eq!(error.progress(), rca_core::TaskProgress::NotStarted);
                assert_eq!(callback_drops.load(Ordering::SeqCst), 2);
                assert_eq!(calls.load(Ordering::SeqCst), 0);
                assert_eq!(reductions.load(Ordering::SeqCst), 1);
                assert!(poll(actor.as_mut()).is_ready());
                assert!(poll(run.as_mut()).is_ready());
                assert_eq!(poll(closed.as_mut()), Poll::Ready(()));
                assert!(matches!(poll(filler.as_mut()), Poll::Ready(Err(_))));
                assert!(task.is_finished());
                assert!(task.is_cancelled());
                assert_eq!(poll(wait.as_mut()), Poll::Ready(Ok(())));
                drop((
                    request, parent, child, removal, site, actor, run, wait, task,
                ));
                assert_eq!(producer_drops.load(Ordering::SeqCst), 1);
                assert!(driver.0.lock().unwrap().is_empty());
                continue;
            }
        }
        assert_eq!(poll(wait.as_mut()), Poll::Ready(Ok(())));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(callback_drops.load(Ordering::SeqCst), 2);
        assert_eq!(producer_drops.load(Ordering::SeqCst), 1);
        assert_eq!(reductions.load(Ordering::SeqCst), 1);
        let mut shutdown = Box::pin(store.shutdown_into_state());
        assert!(poll(shutdown.as_mut()).is_pending());
        assert!(poll(actor.as_mut()).is_ready());
        let Poll::Ready(Ok(state)) = poll(shutdown.as_mut()) else {
            panic!("rejected removal leaves its installed child available to shutdown");
        };
        let value = state.child.unwrap().value;
        assert_eq!((&raw const *value) as usize, identity);
        assert_eq!(value.get(), 7);
        assert!(driver.0.lock().unwrap().is_empty());
    }
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one entered Shared removal fixture distinguishes reaction-capture shutdown from mutable-projection shutdown"
)]
fn shared_removal_entry_rechecks_capture_and_mutable_parent_projection() {
    /// Records retained child state during disposal without asserting while unwinding.
    struct SelectingRoot {
        parent: Option<Parent>,
        capture_hook: DropHook,
        write_hook: DropHook,
        selections: Arc<AtomicUsize>,
        retained: Arc<Mutex<Option<(usize, usize)>>>,
        drops: Arc<AtomicUsize>,
    }
    impl Drop for SelectingRoot {
        fn drop(&mut self) {
            let retained = self
                .parent
                .as_ref()
                .and_then(|parent| parent.child.as_ref())
                .map(|child| ((&raw const *child.value).addr(), child.value.get()));
            *self.retained.lock().unwrap() = retained;
            self.drops.fetch_add(1, Ordering::SeqCst);
        }
    }
    /// Rejects any root reduction after the entered removal closes the store.
    struct SelectingFeature;
    impl Feature for SelectingFeature {
        type State = SelectingRoot;
        type Action = ();

        fn update(
            &mut self,
            _: &mut SelectingRoot,
            (): &(),
            _: &mut FeatureContext<'_, SelectingRoot, ()>,
        ) {
            panic!("rejected removal must not enter the root reducer");
        }
    }
    /// Rejects child reduction and separately records feature disposal.
    struct TrackedChildFeature {
        _capture: DropWatch,
    }
    impl Feature for TrackedChildFeature {
        type State = Child;
        type Action = ();

        fn update(&mut self, _: &mut Child, (): &(), _: &mut FeatureContext<'_, Child, ()>) {
            panic!("rejected removal must not enter the child reducer");
        }
    }

    for close_during_capture in [true, false] {
        let driver = Driver::default();
        let owner = Arc::new(Mutex::new(None));
        let capture_hook: DropHook = Arc::new(Mutex::new(None));
        let write_hook: DropHook = Arc::new(Mutex::new(None));
        let selections = Arc::new(AtomicUsize::new(0));
        let state_drops = Arc::new(AtomicUsize::new(0));
        let retained = Arc::new(Mutex::new(None));
        let feature_drops = Arc::new(AtomicUsize::new(0));
        let feature_capture = Arc::clone(&feature_drops);
        let value = Box::new(Cell::new(7));
        let identity = (&raw const *value) as usize;
        let sites = Arc::new(Mutex::new(None));
        let export = Arc::clone(&sites);
        let (builder, parent_site) = RootMount::new(SelectingFeature)
            .on_change(
                |root: &SelectingRoot| {
                    root.selections.fetch_add(1, Ordering::SeqCst);
                    let hook = root.capture_hook.lock().unwrap().take();
                    if let Some(hook) = hook {
                        hook();
                    }
                },
                rca_core::InitialPolicy::Skip,
                |_, _| panic!("failed removal must not launch a reaction"),
            )
            .builder(
                SelectingRoot {
                    parent: Some(Parent {
                        child: Some(Child { value }),
                    }),
                    capture_hook: Arc::clone(&capture_hook),
                    write_hook: Arc::clone(&write_hook),
                    selections: Arc::clone(&selections),
                    retained: Arc::clone(&retained),
                    drops: Arc::clone(&state_drops),
                },
                Runtime::new(driver.clone()),
            )
            .mount_with_children(
                OptionalMount::new(
                    |root: &SelectingRoot| root.parent.as_ref(),
                    |root: &mut SelectingRoot| {
                        let hook = root.write_hook.lock().unwrap().take();
                        if let Some(hook) = hook {
                            hook();
                        }
                        root.parent.as_mut()
                    },
                    |root: &SelectingRoot| {
                        root.parent.as_ref().map(|_| MountIdentity {
                            key: (),
                            replacement: 0,
                            configuration: 0,
                        })
                    },
                    |_| ParentFeature,
                ),
                move |_, children| {
                    let feature_capture = Arc::clone(&feature_capture);
                    *export.lock().unwrap() = Some(children.optional_removable(
                        |parent: &Parent| &parent.child,
                        |parent: &mut Parent| &mut parent.child,
                        |parent: &Parent| {
                            parent.child.as_ref().map(|_| MountIdentity {
                                key: (),
                                replacement: 0,
                                configuration: 0,
                            })
                        },
                        move |_| TrackedChildFeature {
                            _capture: DropWatch(Arc::clone(&feature_capture)),
                        },
                    ));
                },
            );
        let store = builder.build().unwrap();
        driver.finish(store.ready()).unwrap();
        let parent = driver.finish(store.acquire(&parent_site)).unwrap().unwrap();
        let (child_site, removal) = sites.lock().unwrap().take().unwrap();
        let child = driver.finish(store.acquire(&child_site)).unwrap().unwrap();
        let selected = selections.load(Ordering::SeqCst);
        let closed = store.closed();
        *owner.lock().unwrap() = Some(store);
        let closing_owner = Arc::clone(&owner);
        let hook = Box::new(move || drop(closing_owner.lock().unwrap().take()));
        if close_during_capture {
            *capture_hook.lock().unwrap() = Some(hook);
        } else {
            *write_hook.lock().unwrap() = Some(hook);
        }
        let captures = Arc::new(AtomicUsize::new(0));
        let before = DropWatch(Arc::clone(&captures));
        let after = DropWatch(Arc::clone(&captures));
        let error = driver
            .finish(parent.remove_child(
                &removal,
                &child,
                move |_| {
                    drop(before);
                    panic!("entered rejection must suppress the before callback");
                },
                move |_, _, _| {
                    drop(after);
                    panic!("entered rejection must suppress the after callback");
                },
            ))
            .unwrap_err();
        assert_eq!(error.reason(), rca_core::TaskAccessReason::StoreClosed);
        assert_eq!(error.progress(), rca_core::TaskProgress::MayHaveRun);
        assert_eq!(captures.load(Ordering::SeqCst), 2);
        assert_eq!(selections.load(Ordering::SeqCst), selected + 1);
        assert!(owner.lock().unwrap().is_none());
        driver.finish(closed);
        // Closure notification precedes physical actor-owned state disposal.
        driver.turn();
        assert_eq!(state_drops.load(Ordering::SeqCst), 1);
        assert_eq!(*retained.lock().unwrap(), Some((identity, 7)));
        assert_eq!(feature_drops.load(Ordering::SeqCst), 1);
        assert!(driver.0.lock().unwrap().is_empty());
    }
}
