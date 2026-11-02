//! Ordered diagnostic folds use the held frontier without releasing mutations.

/// Checks read-only fold and acquisition behaviour in both execution modes.
macro_rules! fold_frontier {
    () => {
        use futures_executor::block_on;
        use rca_core::{FoldKey, MountIdentity, ReduceContext, TaskAccessReason, TaskProgress};
        use std::ops::ControlFlow;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::{Arc, Mutex, Weak};
        use std::task::Poll;
        /// Adds owned values without requiring a cloneable or thread-safe payload.
        struct Sum;
        impl FoldKey for Sum {
            type Value = Owned;
            fn identity() -> Owned {
                Owned::new(0)
            }
            fn reduce(acc: Owned, next: Owned) -> ControlFlow<Owned, Owned> {
                acc.0.set(acc.0.get() + next.0.get());
                ControlFlow::Continue(acc)
            }
        }
        /// Leaves root state unchanged while child work waits at the assertion boundary.
        struct Root;
        impl Feature for Root {
            type State = Option<usize>;
            type Action = ();
            fn update(
                &mut self,
                _: &mut Self::State,
                _: &(),
                _: &mut FeatureContext<'_, Self::State, ()>,
            ) {
            }
        }
        /// Adds child actions and keeps a separately observed lifetime marker alive.
        struct Child {
            _lifetime: Arc<()>,
        }
        impl Feature for Child {
            type State = usize;
            type Action = usize;
            fn update(
                &mut self,
                state: &mut usize,
                action: &usize,
                _: &mut FeatureContext<'_, usize, usize>,
            ) {
                *state += action;
            }
        }

        #[test]
        #[expect(
            clippy::too_many_lines,
            reason = "One finite-work schedule checks every diagnostic entry point's budget, retry and command usability."
        )]
        fn fold_propagates_settle_budget_and_retains_finite_work_for_retry() {
            /// Names the five diagnostic entry points that must share the same budget.
            #[derive(Clone, Copy)]
            enum Access {
                Fold,
                Acquire,
                AcquireFold,
                FoldView,
                CommandPort,
            }
            /// Starts one eight-poll effect for each explicitly released access attempt.
            struct GatedWork {
                gates: std::vec::IntoIter<rca_test::ControlledGate>,
                polls: Arc<AtomicUsize>,
            }
            impl Reducer for GatedWork {
                type State = Option<usize>;
                type Action = ();

                fn reduce(
                    &mut self,
                    _: &mut Option<usize>,
                    (): &(),
                    _: &mut ReduceContext<'_>,
                ) -> Effect<()> {
                    let gate = self.gates.next().expect("one finite effect per access");
                    let polls = Arc::clone(&self.polls);
                    Effect::run(move |_| async move {
                        gate.await.expect("test releases effect");
                        let mut effect_polls = 0;
                        std::future::poll_fn(move |context| {
                            polls.fetch_add(1, Ordering::SeqCst);
                            effect_polls += 1;
                            if effect_polls == 8 {
                                Poll::Ready(())
                            } else {
                                context.waker().wake_by_ref();
                                Poll::Pending
                            }
                        })
                        .await;
                    })
                }
            }

            block_on(async {
                let accesses = [
                    Access::Fold,
                    Access::Acquire,
                    Access::AcquireFold,
                    Access::FoldView,
                    Access::CommandPort,
                ];
                let (controls, gates): (Vec<_>, Vec<_>) = accesses
                    .iter()
                    .map(|_| rca_test::controlled_gate())
                    .unzip();
                let polls = Arc::new(AtomicUsize::new(0));
                let declared = Arc::new(Mutex::new(None));
                let captured = Arc::clone(&declared);
                let mapped = Arc::new(AtomicUsize::new(0));
                let mapped_calls = Arc::clone(&mapped);
                let mount = OptionalMount::new(
                    Option::as_ref,
                    Option::as_mut,
                    |state: &Option<usize>| {
                        state.map(|_| MountIdentity {
                            key: (),
                            replacement: 0,
                            configuration: 0,
                        })
                    },
                    |_| Child {
                        _lifetime: Arc::new(()),
                    },
                );
                let (builder, site) = TestStore::builder(
                    Some(2),
                    GatedWork {
                        gates: gates.into_iter(),
                        polls: Arc::clone(&polls),
                    },
                )
                .with_step_budget(7)
                .mount_with_children(mount, move |_, children| {
                    let mapped_calls = Arc::clone(&mapped_calls);
                    *captured.lock().unwrap() = Some(children.command(move |value: usize| {
                        mapped_calls.fetch_add(1, Ordering::SeqCst);
                        value
                    }));
                });
                let mut store = builder.build();
                store
                    .receive_structural(|_| {})
                    .await
                    .unwrap()
                    .finish()
                    .await
                    .unwrap();
                let fold = Fold::<Option<usize>, Sum>::new();
                let view = store.acquire_fold(&fold, &site).await.unwrap().unwrap();
                let command = declared.lock().unwrap().take();
                let command = command.expect("the child declares one command");
                for (index, (access, control)) in accesses.into_iter().zip(controls).enumerate() {
                    let action = store.send(()).await.unwrap();
                    control.wait_until_waiting().await.unwrap();
                    control.release().unwrap();
                    let result = match access {
                        Access::Fold => store.fold(&fold).await.map(|_| ()),
                        Access::Acquire => store.acquire(&site).await.map(|_| ()),
                        Access::AcquireFold => store.acquire_fold(&fold, &site).await.map(|_| ()),
                        Access::FoldView => store.fold_view(&view).await.map(|_| ()),
                        Access::CommandPort => store.command_port(&command).await.map(|_| ()),
                    };
                    let error = result.expect_err("finite ready work must exhaust the settle budget");
                    let drive = error.drive_error().expect("settle retains the drive cause");
                    assert!(drive.is_step_budget_exhausted());
                    assert_eq!((drive.polls(), drive.step_budget()), (7, 7));
                    assert!(error.task_access_error().is_none());
                    assert_eq!(polls.load(Ordering::SeqCst), index * 8 + 7);
                    assert_eq!(mapped.load(Ordering::SeqCst), 0);

                    match access {
                        Access::Fold => assert_eq!(store.fold(&fold).await.unwrap().0.get(), 0),
                        Access::Acquire => assert!(store.acquire(&site).await.unwrap().is_some()),
                        Access::AcquireFold => {
                            assert!(store.acquire_fold(&fold, &site).await.unwrap().is_some());
                        }
                        Access::FoldView => assert_eq!(store.fold_view(&view).await.unwrap().0.get(), 0),
                        Access::CommandPort => {
                            assert!(store.command_port(&command).await.unwrap().is_some());
                        }
                    }
                    assert_eq!(polls.load(Ordering::SeqCst), (index + 1) * 8);
                    action.finish().await.unwrap();
                }
                let port = store.command_port(&command).await.unwrap().unwrap();
                let committed = port.try_send(3).unwrap().committed();
                store
                    .receive_command(&port, |value| *value == 3, |state| *state = Some(5))
                    .await
                    .unwrap()
                    .finish()
                    .await
                    .unwrap();
                let _ = committed.await.unwrap();
                assert_eq!(mapped.load(Ordering::SeqCst), 1);
                store.finish().await.unwrap();
            });
        }

        #[test]
        #[expect(
            clippy::too_many_lines,
            reason = "One ordered schedule checks the held frontier, acknowledgement debt and capture release"
        )]
        fn fold_observes_held_state_without_releasing_or_acknowledging_pending_action() {
            block_on(async {
                let fold = Fold::<Option<usize>, Sum>::new();
                let declared = fold.clone();
                let calls = Arc::new(AtomicUsize::new(0));
                let declared_calls = Arc::clone(&calls);
                let captures = Arc::new(Mutex::new(Vec::<Weak<()>>::new()));
                let declared_captures = Arc::clone(&captures);
                let feature_captures = Arc::clone(&captures);
                let mount = OptionalMount::new(
                    Option::as_ref,
                    Option::as_mut,
                    |state: &Option<usize>| {
                        state.map(|_| MountIdentity {
                            key: (),
                            replacement: 0,
                            configuration: 0,
                        })
                    },
                    move |_| {
                        let lifetime = Arc::new(());
                        feature_captures
                            .lock()
                            .unwrap()
                            .push(Arc::downgrade(&lifetime));
                        Child {
                            _lifetime: lifetime,
                        }
                    },
                )
                .on_initial(
                    |_| (),
                    |_, _| {
                        Task::run(|context| async move {
                            let _ = context.send(7).await.unwrap();
                        })
                    },
                );
                let (builder, site) = TestStore::builder(Some(2), Root)
                    .mount_with_children(mount, move |_, children| {
                        let calls = Arc::clone(&declared_calls);
                        let capture = Arc::new(());
                        declared_captures
                            .lock()
                            .unwrap()
                            .push(Arc::downgrade(&capture));
                        children.contribute(&declared, move |state| {
                            let _keep = &capture;
                            calls.fetch_add(1, Ordering::SeqCst);
                            Owned::new(*state)
                        });
                    });
                let mut store = builder.build();
                let Err(error) = store.fold(&fold).await else {
                    panic!("not ready")
                };
                assert_eq!(
                    error.task_access_error().unwrap().reason(),
                    TaskAccessReason::NotReady
                );
                assert_eq!(
                    error.task_access_error().unwrap().progress(),
                    TaskProgress::NotStarted
                );
                let initial = store.receive_structural(|_| {}).await.unwrap();
                let context = store.acquire(&site).await.unwrap().unwrap();
                let view = store.acquire_fold(&fold, &site).await.unwrap().unwrap();
                assert_eq!(
                    calls.load(Ordering::SeqCst),
                    0,
                    "acquisition invokes no selectors"
                );
                assert_eq!(store.fold(&fold).await.unwrap().0.get(), 2);
                assert_eq!(store.fold_view(&view).await.unwrap().0.get(), 2);
                let calls_before = calls.load(Ordering::SeqCst);
                drop(store.fold(&fold));
                drop(store.fold_view(&view));
                assert_eq!(
                    calls.load(Ordering::SeqCst),
                    calls_before,
                    "unpolled diagnostics are inert"
                );
                let mut foreign = TestStore::builder(Some(17), Root)
                    .mount(OptionalMount::new(
                        Option::as_ref,
                        Option::as_mut,
                        |state: &Option<usize>| state.map(|_| MountIdentity {
                            key: (),
                            replacement: 0,
                            configuration: 0,
                        }),
                        |_| Child { _lifetime: Arc::new(()) },
                    ))
                    .build();
                let origin_line = line!();
                let Err(error) = foreign.fold_view(&view).await else {
                    panic!("unready store must reject before selecting a foreign view")
                };
                assert_eq!(error.origin().line(), origin_line);
                let access = error.task_access_error().unwrap();
                assert_eq!(access.reason(), TaskAccessReason::NotReady);
                assert_eq!(access.progress(), TaskProgress::NotStarted);
                assert_eq!(calls.load(Ordering::SeqCst), calls_before);
                foreign.receive_structural(|_| {}).await.unwrap().finish().await.unwrap();
                let Err(error) = foreign.fold_view(&view).await else {
                    panic!("foreign root")
                };
                assert_eq!(
                    error.task_access_error().unwrap().reason(),
                    TaskAccessReason::ScopeEnded
                );
                assert_eq!(
                    calls.load(Ordering::SeqCst),
                    calls_before,
                    "foreign root selects nothing"
                );
                foreign.finish().await.unwrap();
                let mismatch = store
                    .receive_mounted(&context, |_| false, |_| panic!("mismatch does not assert"))
                    .await
                    .unwrap_err();
                assert!(mismatch.is_action_mismatch());
                assert_eq!(store.fold(&fold).await.unwrap().0.get(), 2);
                let error = store
                    .receive_mounted(&context, |action| *action == 7, |state| *state = Some(9))
                    .await
                    .unwrap_err();
                assert!(error.is_incomplete_assertion());
                assert_eq!(store.fold(&fold).await.unwrap().0.get(), 9);
                assert_eq!(store.fold_view(&view).await.unwrap().0.get(), 9);
                initial.finish().await.unwrap();
                assert!(store.finish().await.unwrap_err().is_incomplete_assertion());
                let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    drop(store);
                }))
                .unwrap_err();
                let message = panic.downcast_ref::<String>().unwrap();
                assert!(
                    message.starts_with(
                        "TestStore teardown failed: TestStore incomplete assertion for transaction boundary at "
                    ),
                    "{message}"
                );
                assert!(
                    captures
                        .lock()
                        .unwrap()
                        .iter()
                        .all(|capture| capture.upgrade().is_none()),
                    "retained view/gate metadata owns no Feature or contribution callback"
                );
                let Err(error) = view.read().await else {
                    panic!("closed captured view")
                };
                assert_eq!(error.reason(), TaskAccessReason::StoreClosed);
            });
        }

        #[test]
        fn foreign_context_rejects_at_held_frontier_without_losing_original_action() {
            block_on(async {
                let mount = OptionalMount::new(
                    Option::as_ref,
                    Option::as_mut,
                    |state: &Option<usize>| {
                        state.map(|_| MountIdentity {
                            key: (),
                            replacement: 0,
                            configuration: 0,
                        })
                    },
                    |_| Child {
                        _lifetime: Arc::new(()),
                    },
                )
                .on_initial(
                    |_| (),
                    |_, _| {
                        Task::run(|context| async move {
                            let _ = context.send(7).await.unwrap();
                        })
                    },
                );
                let (builder, site) = TestStore::builder(Some(2), Root)
                    .mount_with_children(mount, |_, _| {});
                let mut store = builder.build();
                let initial = store.receive_structural(|_| {}).await.unwrap();

                let foreign_mount = OptionalMount::new(
                    Option::as_ref,
                    Option::as_mut,
                    |state: &Option<usize>| {
                        state.map(|_| MountIdentity {
                            key: (),
                            replacement: 0,
                            configuration: 0,
                        })
                    },
                    |_| Child {
                        _lifetime: Arc::new(()),
                    },
                );
                let (foreign_builder, foreign_site) = TestStore::builder(Some(17), Root)
                    .mount_with_children(foreign_mount, |_, _| {});
                let mut foreign = foreign_builder.build();
                foreign
                    .receive_structural(|_| {})
                    .await
                    .unwrap()
                    .finish()
                    .await
                    .unwrap();
                let foreign_context = foreign.acquire(&foreign_site).await.unwrap().unwrap();

                let Err(error) = store.acquire(&foreign_site).await else {
                    panic!("foreign mount site must be rejected");
                };
                let error = error.task_access_error().unwrap();
                assert_eq!(
                    (error.reason(), error.progress()),
                    (TaskAccessReason::ScopeEnded, TaskProgress::NotStarted)
                );
                let fold = Fold::<Option<usize>, Sum>::new();
                let Err(error) = store.acquire_fold(&fold, &foreign_site).await else {
                    panic!("foreign fold site must be rejected");
                };
                let error = error.task_access_error().unwrap();
                assert_eq!(
                    (error.reason(), error.progress()),
                    (TaskAccessReason::ScopeEnded, TaskProgress::NotStarted)
                );

                let error = store
                    .receive_mounted(
                        &foreign_context,
                        |_| panic!("foreign context must not run matcher"),
                        |_| panic!("foreign context must not run expectation"),
                    )
                    .await
                    .unwrap_err();
                let error = error.task_access_error().unwrap();
                assert_eq!(
                    (error.reason(), error.progress()),
                    (TaskAccessReason::ScopeEnded, TaskProgress::NotStarted)
                );

                let context = store.acquire(&site).await.unwrap().unwrap();
                store
                    .receive_mounted(&context, |action| *action == 7, |state| *state = Some(9))
                    .await
                    .unwrap()
                    .finish()
                    .await
                    .unwrap();
                initial.finish().await.unwrap();
                store
                    .assert_state(|state| assert_eq!(*state, Some(9)))
                    .await
                    .unwrap();
                store.finish().await.unwrap();
                foreign.finish().await.unwrap();
            });
        }

    };
}
mod shared {
    use rca_core::{Effect, Feature, FeatureContext, Fold, OptionalMount, Reducer, Task};
    use rca_test::TestStore;
    /// Provides a move-only, `Send` but non-`Sync` fold value.
    struct Owned(Box<std::cell::Cell<usize>>);
    impl Owned {
        /// Allocates a stable owner for a value contributed to the fold.
        fn new(value: usize) -> Self {
            Self(Box::new(std::cell::Cell::new(value)))
        }
    }
    fold_frontier!();

    #[test]
    #[expect(
        clippy::too_many_lines,
        reason = "One public concurrent-driver schedule preserves actor reservation, caller budget and retry ownership"
    )]
    fn acquisition_budget_remains_bounded_while_another_driver_owns_the_actor() {
        use std::future::Future;
        use std::sync::mpsc;
        use std::task::{Context, Waker};
        use std::time::Duration;

        /// Holds finite work while another thread reserves the actor for a read.
        struct GatedWork {
            gate: Option<rca_test::ControlledGate>,
            polls: Arc<AtomicUsize>,
            drops: Arc<AtomicUsize>,
        }
        /// Counts disposal separately from the effect's poll count.
        struct WorkOwner(Arc<AtomicUsize>);
        impl Drop for WorkOwner {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }
        impl Reducer for GatedWork {
            type State = Option<usize>;
            type Action = ();
            fn reduce(
                &mut self,
                _: &mut Option<usize>,
                (): &(),
                _: &mut ReduceContext<'_>,
            ) -> Effect<()> {
                let gate = self.gate.take().expect("one finite effect");
                let polls = Arc::clone(&self.polls);
                let owner = WorkOwner(Arc::clone(&self.drops));
                Effect::run(move |_| async move {
                    let _owner = owner;
                    gate.await.expect("test releases finite work");
                    std::future::poll_fn(move |context| {
                        if polls.fetch_add(1, Ordering::SeqCst) + 1 == 8 {
                            Poll::Ready(())
                        } else {
                            context.waker().wake_by_ref();
                            Poll::Pending
                        }
                    })
                    .await;
                })
            }
        }
        /// Releases the blocking read on normal return or unwinding before the thread joins.
        struct ReleaseRead(Option<mpsc::Sender<()>>);
        impl Drop for ReleaseRead {
            fn drop(&mut self) {
                if let Some(release) = self.0.take() {
                    let _ = release.send(());
                }
            }
        }

        let (control, gate) = rca_test::controlled_gate();
        let polls = Arc::new(AtomicUsize::new(0));
        let drops = Arc::new(AtomicUsize::new(0));
        let mount = OptionalMount::new(
            Option::as_ref,
            Option::as_mut,
            |state: &Option<usize>| {
                state.map(|_| MountIdentity {
                    key: (),
                    replacement: 0,
                    configuration: 0,
                })
            },
            |_| Child {
                _lifetime: Arc::new(()),
            },
        );
        let (builder, site) = TestStore::builder(
            Some(2),
            GatedWork {
                gate: Some(gate),
                polls: Arc::clone(&polls),
                drops: Arc::clone(&drops),
            },
        )
        .with_step_budget(7)
        .mount_with_site(mount);
        let mut store = builder.build();
        block_on(async {
            store
                .receive_structural(|_| {})
                .await
                .unwrap()
                .finish()
                .await
                .unwrap();
        });
        let context = block_on(store.acquire(&site)).unwrap().unwrap();
        let task = block_on(store.send(())).unwrap();
        block_on(control.wait_until_waiting()).unwrap();
        assert!(store.pending_boundary().is_none());

        let (entered, entry) = mpsc::channel();
        let (release, released) = mpsc::channel();
        let callback_owner = Arc::new(());
        let callback_weak = Arc::downgrade(&callback_owner);
        let mut read = Box::pin(context.read(move |state| {
            let _ = entered.send(());
            // No actor operation is awaited here. The external caller releases
            // this finite callback even during unwinding; timeout bounds failure.
            let released = released.recv_timeout(Duration::from_secs(10)).is_ok();
            drop(callback_owner);
            (*state, released)
        }));
        assert!(
            read.as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending()
        );

        let (first_pending, result, consumed, retained, driver, read_result) =
            std::thread::scope(|threads| {
                let other = threads.spawn(|| block_on(task.finish()));
                let release = ReleaseRead(Some(release));
                // Scope joining and ReleaseRead also run if this bounded wait fails.
                entry.recv_timeout(Duration::from_secs(10)).unwrap();
                let origin_line = line!() + 1;
                let mut acquisition = Box::pin(store.acquire(&site));
                let first = acquisition
                    .as_mut()
                    .poll(&mut Context::from_waker(Waker::noop()));
                let first_pending = first.is_pending();
                control.release().unwrap();
                let result = match first {
                    Poll::Ready(result) => result,
                    Poll::Pending => block_on(acquisition.as_mut()),
                };
                drop(acquisition);
                let consumed = polls.load(Ordering::SeqCst);
                let retained = drops.load(Ordering::SeqCst);
                // Release and join before asserting the caller's result. The
                // reserved actor is never polled recursively or concurrently.
                drop(release);
                let driver = other.join();
                let read_result = block_on(read);
                (
                    first_pending,
                    (result, origin_line),
                    consumed,
                    retained,
                    driver,
                    read_result,
                )
            });
        let retry = block_on(store.acquire(&site));
        let finished = block_on(task.finish());
        let cleanup = block_on(store.finish());

        assert!(first_pending);
        let (result, origin_line) = result;
        let error = result.expect_err("the caller cannot execute the reserved actor");
        let drive = error.drive_error().expect("retain the typed drive failure");
        assert!(drive.is_step_budget_exhausted());
        assert_eq!((drive.polls(), drive.step_budget()), (7, 7));
        assert_eq!(error.origin().line(), origin_line);
        assert!(error.task_access_error().is_none());
        assert_eq!((consumed, retained), (7, 0));
        driver.unwrap().unwrap();
        assert_eq!(read_result.unwrap(), (2, true));
        assert!(callback_weak.upgrade().is_none());
        assert!(retry.unwrap().is_some());
        finished.unwrap();
        cleanup.unwrap();
        assert_eq!(polls.load(Ordering::SeqCst), 8);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }
}
#[cfg(feature = "local")]
mod local {
    use rca_core::local::{Effect, Feature, FeatureContext, Fold, OptionalMount, Reducer, Task};
    use rca_test::local::TestStore;
    /// Keeps the fold value owner-local rather than requiring `Send` or `Sync`.
    struct Owned(std::rc::Rc<std::cell::Cell<usize>>);
    impl Owned {
        /// Creates the owner-local value without giving the wrapper a `Clone` implementation.
        fn new(value: usize) -> Self {
            Self(std::rc::Rc::new(std::cell::Cell::new(value)))
        }
    }
    fold_frontier!();

    #[test]
    #[expect(
        clippy::too_many_lines,
        reason = "One owner-local schedule proves bounded reentrant driving without re-entering the reserved actor."
    )]
    fn acquisition_budget_remains_bounded_inside_an_owner_local_read_callback() {
        use std::cell::{Cell, RefCell};
        use std::future::Future;
        use std::rc::Rc;
        use std::task::{Context, Waker};

        let (control, gate) = rca_test::controlled_gate();
        let polls = Rc::new(Cell::new(0));
        let effect_owner = Rc::new(());
        let effect_weak = Rc::downgrade(&effect_owner);
        let mut pending_work = Some((gate, effect_owner));
        let effect_polls = Rc::clone(&polls);
        let reducer = rca_core::local::Reduce::new(
            move |_: &mut Option<usize>, (): &(), _: &mut ReduceContext<'_>| {
                let (gate, owner) = pending_work.take().expect("one finite effect");
                let polls = Rc::clone(&effect_polls);
                Effect::run(move |_| async move {
                    gate.await.expect("callback releases finite work");
                    std::future::poll_fn(move |context| {
                        polls.set(polls.get() + 1);
                        if polls.get() == 8 {
                            Poll::Ready(())
                        } else {
                            context.waker().wake_by_ref();
                            Poll::Pending
                        }
                    })
                    .await;
                    drop(owner);
                })
            },
        );
        let mount = OptionalMount::new(
            Option::as_ref,
            Option::as_mut,
            |state: &Option<usize>| {
                state.map(|_| MountIdentity {
                    key: (),
                    replacement: 0,
                    configuration: 0,
                })
            },
            |_| Child {
                _lifetime: Arc::new(()),
            },
        );
        let (builder, site) = TestStore::builder(Some(2), reducer)
            .with_step_budget(7)
            .mount_with_site(mount);
        let mut store = builder.build();
        block_on(async {
            store
                .receive_structural(|_| {})
                .await
                .unwrap()
                .finish()
                .await
                .unwrap();
        });
        let context = block_on(store.acquire(&site)).unwrap().unwrap();
        let task = block_on(store.send(())).unwrap();
        block_on(control.wait_until_waiting()).unwrap();
        assert!(store.pending_boundary().is_none());

        let store = Rc::new(RefCell::new(store));
        let callback_store = Rc::clone(&store);
        let callback_site = site.clone();
        let callback_polls = Rc::clone(&polls);
        let callback_effect = std::rc::Weak::clone(&effect_weak);
        let callback_owner = Rc::new(());
        let callback_weak = Rc::downgrade(&callback_owner);
        let mut read = Box::pin(context.read(move |state| {
            let mut store = callback_store.borrow_mut();
            let origin_line = line!() + 1;
            let mut acquisition = Box::pin(store.acquire(&callback_site));
            let mut context = Context::from_waker(Waker::noop());
            let first_pending = acquisition.as_mut().poll(&mut context).is_pending();
            control.release().unwrap();
            // Only the independent effect can run while this callback owns the
            // actor. Poll finitely; never await the actor from its own callback.
            let result = (0..7).find_map(|_| match acquisition.as_mut().poll(&mut context) {
                Poll::Pending => None,
                Poll::Ready(result) => Some(result),
            });
            let observed = (
                *state,
                first_pending,
                result,
                origin_line,
                callback_polls.get(),
                callback_effect.upgrade().is_some(),
            );
            drop(callback_owner);
            observed
        }));
        assert!(
            read.as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending()
        );
        block_on(task.finish()).unwrap();
        let (state, first_pending, result, origin_line, consumed, retained) =
            block_on(read).unwrap();
        assert_eq!(state, 2);
        assert!(first_pending);
        let error = result
            .expect("bounded manual polling reaches the caller's budget")
            .expect_err("the acquisition cannot execute the reserved actor");
        let drive = error.drive_error().expect("retain the typed drive failure");
        assert!(drive.is_step_budget_exhausted());
        assert_eq!((drive.polls(), drive.step_budget()), (7, 7));
        assert_eq!(error.origin().line(), origin_line);
        assert!(error.task_access_error().is_none());
        assert_eq!(consumed, 7);
        assert!(retained, "budget exhaustion retains the unfinished effect");
        assert!(callback_weak.upgrade().is_none());
        assert_eq!(polls.get(), 8);
        assert!(effect_weak.upgrade().is_none());
        let mut store = store.borrow_mut();
        assert!(block_on(store.acquire(&site)).unwrap().is_some());
        block_on(store.assert_state(|state| assert_eq!(*state, Some(2)))).unwrap();
        block_on(store.finish()).unwrap();
    }
}
