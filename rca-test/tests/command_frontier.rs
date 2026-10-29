//! Original narrow commands share the production mutation assertion frontier.

/// Reads either standard string panic payload without accepting an unrelated panic.
fn panic_message(panic: &(dyn std::any::Any + Send)) -> &str {
    if let Some(message) = panic.downcast_ref::<String>() {
        message
    } else {
        panic.downcast_ref::<&str>().unwrap()
    }
}

/// Checks the same ownership and admission contracts in both execution modes.
macro_rules! command_frontier {
    () => {
        use futures_executor::block_on;
        use futures_util::FutureExt;
        use rca_core::{MountIdentity, TaskAccessReason, TaskProgress};
        use std::cell::Cell;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::{Arc, Mutex};

        /// Keeps one stable allocation and counts destruction without requiring `Clone`.
        #[derive(Debug)]
        struct Owned {
            value: Payload<Cell<usize>>,
            drops: Arc<AtomicUsize>,
        }
        impl Owned {
            /// Creates one original input whose identity survives a rejected admission.
            fn new(value: usize) -> Self {
                Self {
                    value: Payload::new(Cell::new(value)),
                    drops: Arc::default(),
                }
            }
            /// Observes identity only; the address is never dereferenced.
            fn address(&self) -> usize {
                std::ptr::from_ref(&*self.value).addr()
            }
        }
        impl Drop for Owned {
            fn drop(&mut self) {
                self.drops.fetch_add(1, Ordering::SeqCst);
            }
        }

        /// Leaves the root unchanged while its mounted child handles commands.
        struct Root;
        impl Feature for Root {
            type State = Option<usize>;
            type Action = ();
            fn reduce(
                &mut self,
                _: &mut Self::State,
                _: &(),
                _: &mut FeatureContext<'_, Self::State, ()>,
            ) {
            }
        }
        /// Makes a mapped command visible by adding it to the child state.
        struct Child;
        impl Feature for Child {
            type State = usize;
            type Action = usize;
            fn reduce(
                &mut self,
                state: &mut usize,
                action: &usize,
                _: &mut FeatureContext<'_, usize, usize>,
            ) {
                *state += action;
            }
        }
        /// Captures the child value separately from the root snapshot.
        struct Count;
        impl CaptureKey for Count {
            type Value = usize;
            type Error = ();
        }

        /// Builds a capacity-one command route with a separate mapper-call counter.
        async fn fixture() -> (
            TestStore<Option<usize>, ()>,
            CommandSite<Option<usize>, Owned>,
            Arc<AtomicUsize>,
        ) {
            let declared = Arc::new(Mutex::new(None));
            let captured = Arc::clone(&declared);
            let calls = Arc::new(AtomicUsize::new(0));
            let mapped_calls = Arc::clone(&calls);
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
                |_| Child,
            );
            let (builder, _) = TestStore::feature_builder(Some(2), Root)
                .mailbox_capacity(1)
                .mount_with_children(mount, move |_, children| {
                    let calls = Arc::clone(&mapped_calls);
                    *captured.lock().unwrap() = Some(children.command(move |command: Owned| {
                        calls.fetch_add(1, Ordering::SeqCst);
                        command.value.get()
                    }));
                });
            let mut store = builder
                .with_capture::<Count>(|state| Ok(state.unwrap_or(0)))
                .unwrap()
                .build();
            store
                .receive_structural(|_| {})
                .await
                .unwrap()
                .finish()
                .await
                .unwrap();
            let site = declared.lock().unwrap().take().unwrap();
            (store, site, calls)
        }

        #[test]
        fn command_without_capture_expectations_keeps_existing_baselines() {
            block_on(async {
                let (mut store, site, calls) = fixture().await;
                let port = store.command_port(&site).await.unwrap().unwrap();
                let command = Owned::new(0);
                let drops = Arc::clone(&command.drops);
                let committed = port.try_send(command).unwrap().committed();

                store
                    .receive_command(
                        &port,
                        |command| command.value.get() == 0,
                        |state| assert_eq!(*state, Some(2)),
                    )
                    .await
                    .unwrap()
                    .finish()
                    .await
                    .unwrap();
                let _ = committed.await.unwrap();
                assert_eq!(calls.load(Ordering::SeqCst), 1);
                assert_eq!(drops.load(Ordering::SeqCst), 1);
                store
                    .assert_state(|state| assert_eq!(*state, Some(2)))
                    .await
                    .unwrap();
                store.check_captures(|_| Ok(())).await.unwrap();
                store.finish().await.unwrap();
            });
        }

        #[test]
        fn foreign_port_rejects_at_held_frontier_without_losing_original_command() {
            block_on(async {
                let (mut store, site, calls) = fixture().await;
                let (mut foreign, foreign_site, foreign_calls) = fixture().await;
                let foreign_port = foreign.command_port(&foreign_site).await.unwrap().unwrap();
                let port = store.command_port(&site).await.unwrap().unwrap();
                let command = Owned::new(7);
                let original = command.address();
                let drops = Arc::clone(&command.drops);
                let committed = port.try_send(command).unwrap().committed();

                let Err(error) = store.command_port(&foreign_site).await else {
                    panic!("foreign command site must be rejected");
                };
                assert_eq!(
                    error.task_access_error().unwrap().reason(),
                    TaskAccessReason::ScopeEnded
                );
                assert_eq!(
                    error.task_access_error().unwrap().progress(),
                    TaskProgress::NotStarted
                );
                let error = store
                    .receive_command(
                        &foreign_port,
                        |_| panic!("foreign port must not run matcher"),
                        |_| panic!("foreign port must not run expectation"),
                    )
                    .await
                    .unwrap_err();
                assert_eq!(
                    error.task_access_error().unwrap().reason(),
                    TaskAccessReason::ScopeEnded
                );
                assert_eq!(
                    error.task_access_error().unwrap().progress(),
                    TaskProgress::NotStarted
                );
                assert_eq!(calls.load(Ordering::SeqCst), 0);
                assert_eq!(drops.load(Ordering::SeqCst), 0);

                foreign.finish().await.unwrap();
                drop(foreign);
                let error = store
                    .receive_command(
                        &foreign_port,
                        |_| panic!("closed foreign port must not run matcher"),
                        |_| panic!("closed foreign port must not run expectation"),
                    )
                    .await
                    .unwrap_err();
                let access = error.task_access_error().unwrap();
                assert_eq!(access.reason(), TaskAccessReason::StoreClosed);
                assert_eq!(access.progress(), TaskProgress::NotStarted);
                assert_eq!(foreign_calls.load(Ordering::SeqCst), 0);
                assert_eq!(calls.load(Ordering::SeqCst), 0);
                assert_eq!(drops.load(Ordering::SeqCst), 0);
                assert!(store.pending_boundary().is_some());

                store
                    .receive_command_with_captures(
                        &port,
                        |command| command.address() == original && command.value.get() == 7,
                        |state| *state = Some(9),
                        |captures| captures.set::<Count>(9),
                    )
                    .await
                    .unwrap()
                    .finish()
                    .await
                    .unwrap();
                let _ = committed.await.unwrap();
                assert_eq!(calls.load(Ordering::SeqCst), 1);
                assert_eq!(foreign_calls.load(Ordering::SeqCst), 0);
                assert_eq!(drops.load(Ordering::SeqCst), 1);
                store
                    .assert_state(|state| assert_eq!(*state, Some(9)))
                    .await
                    .unwrap();
                let Err(error) = store.command_port(&foreign_site).await else {
                    panic!("a retained foreign site must expire with its original declaration");
                };
                assert_eq!(
                    error.task_access_error().unwrap().reason(),
                    TaskAccessReason::ScopeEnded
                );
                assert_eq!(
                    error.task_access_error().unwrap().progress(),
                    TaskProgress::NotStarted
                );
                assert_eq!(calls.load(Ordering::SeqCst), 1);
                assert_eq!(foreign_calls.load(Ordering::SeqCst), 0);
                assert_eq!(drops.load(Ordering::SeqCst), 1);
                store.finish().await.unwrap();
            });
        }

        #[test]
        fn capacity_one_preserves_original_command_and_reply_through_mismatch_and_unwind() {
            block_on(async {
                let (mut store, site, calls) = fixture().await;
                let port = store.command_port(&site).await.unwrap().unwrap();
                let command = Owned::new(7);
                let original = command.address();
                let drops = Arc::clone(&command.drops);
                let committed = port.try_send(command).unwrap().committed();
                futures_util::pin_mut!(committed);
                let second = Owned::new(11);
                let second_address = second.address();
                let TryCommandError::Full(second) = port.try_send(second).unwrap_err() else {
                    panic!("capacity one must return the unadmitted command");
                };
                assert_eq!(second.address(), second_address);
                drop(second);
                assert!(futures_util::poll!(committed.as_mut()).is_pending());

                let mismatch = store
                    .receive_command(
                        &port,
                        |command| {
                            assert_eq!(command.address(), original);
                            assert_eq!(command.value.get(), 7);
                            false
                        },
                        |_| panic!("mismatch must not enter expectations"),
                    )
                    .await
                    .unwrap_err();
                assert!(mismatch.is_action_mismatch());
                let current = store.command_port(&site).await.unwrap().unwrap();
                let panic = std::panic::AssertUnwindSafe(store.receive_command(
                    &current,
                    |command| {
                        assert_eq!(command.address(), original);
                        panic!("matcher unwind");
                    },
                    |_| panic!("unreached"),
                ))
                .catch_unwind()
                .await
                .unwrap_err();
                assert_eq!(super::panic_message(panic.as_ref()), "matcher unwind");
                assert_eq!(calls.load(Ordering::SeqCst), 0);
                assert_eq!(drops.load(Ordering::SeqCst), 0);
                assert!(futures_util::poll!(committed.as_mut()).is_pending());

                let error = store
                    .receive_command_with_captures(
                        &current,
                        |command| command.address() == original && command.value.get() == 7,
                        |state| *state = Some(9),
                        |captures| captures.set::<Count>(9),
                    )
                    .await
                    .unwrap_err();
                assert!(error.is_incomplete_assertion());
                let _ = committed
                    .now_or_never()
                    .expect("the original reply resolves")
                    .unwrap();
                assert_eq!(calls.load(Ordering::SeqCst), 1);
                assert_eq!(drops.load(Ordering::SeqCst), 1);
                store
                    .assert_state(|state| assert_eq!(*state, Some(9)))
                    .await
                    .unwrap();
                assert!(store.finish().await.unwrap_err().is_incomplete_assertion());
                let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    drop(store);
                }))
                .unwrap_err();
                assert!(super::panic_message(panic.as_ref()).starts_with(
                    "TestStore teardown failed: TestStore incomplete assertion for transaction boundary at "
                ));
            });
        }

        #[test]
        fn committed_command_uses_existing_full_root_and_capture_debt() {
            block_on(async {
                let (mut store, site, calls) = fixture().await;
                let port = store.command_port(&site).await.unwrap().unwrap();
                let committed = port.try_send(Owned::new(7)).unwrap().committed();
                let result = std::panic::AssertUnwindSafe(store.receive_command_with_captures(
                    &port,
                    |command| command.value.get() == 7,
                    |state| *state = Some(99),
                    |captures| captures.set::<Count>(9),
                ))
                .catch_unwind()
                .await;
                let panic = result.unwrap_err();
                let message = super::panic_message(panic.as_ref());
                assert!(message.starts_with("TestStore state mismatch at "), "{message}");
                assert!(message.contains(" during receive_command:"), "{message}");
                let _ = committed
                    .now_or_never()
                    .expect("commit precedes snapshot comparison")
                    .unwrap();
                assert_eq!(calls.load(Ordering::SeqCst), 1);
                store
                    .assert_state(|state| assert_eq!(*state, Some(9)))
                    .await
                    .unwrap();
                let error = store
                    .receive_command(
                        &port,
                        |_| panic!("debt prevents matcher"),
                        |_| panic!("debt prevents expectation"),
                    )
                    .await
                    .unwrap_err();
                assert!(error.is_incomplete_assertion());
                assert!(store.finish().await.unwrap_err().is_incomplete_assertion());
                let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    drop(store);
                }))
                .unwrap_err();
                assert!(super::panic_message(panic.as_ref()).starts_with(
                    "TestStore teardown failed: TestStore incomplete assertion for receive_command at "
                ));
            });
        }

        #[test]
        fn zero_capacity_keeps_the_existing_build_failure() {
            let panic = std::panic::catch_unwind(|| {
                let _ = TestStore::feature_builder(Some(2), Root)
                    .mailbox_capacity(0)
                    .build();
            })
            .unwrap_err();
            let message = super::panic_message(panic.as_ref());
            assert!(message.starts_with("TestStore configuration must be valid: "), "{message}");
            assert!(message.contains("kind: ZeroCapacity"), "{message}");
        }

        #[test]
        #[expect(
            clippy::too_many_lines,
            reason = "one recipe inspection sequence preserves exact inputs across sibling, refreshed and retired declarations"
        )]
        fn recipe_inspection_preserves_input_across_refresh_and_declaration_retirement() {
            /// Keeps a live sibling while the first declaration changes or retires.
            #[derive(Clone, Debug, PartialEq)]
            struct Routes {
                first: Option<usize>,
                second: Option<usize>,
                revision: u64,
            }
            /// Changes only the route under inspection.
            enum Edit {
                Refresh,
                Remove,
            }
            /// Applies the requested configuration or removal without a child command.
            struct RoutesRoot;
            impl Feature for RoutesRoot {
                type State = Routes;
                type Action = Edit;
                fn reduce(
                    &mut self,
                    state: &mut Routes,
                    action: &Edit,
                    _: &mut FeatureContext<'_, Routes, Edit>,
                ) {
                    match action {
                        Edit::Refresh => state.revision += 1,
                        Edit::Remove => state.first = None,
                    }
                }
            }

            block_on(async {
                let first_declared = Arc::new(Mutex::new(None));
                let second_declared = Arc::new(Mutex::new(None));
                let first_site = Arc::clone(&first_declared);
                let second_site = Arc::clone(&second_declared);
                let first_calls = Arc::new(AtomicUsize::new(0));
                let second_calls = Arc::new(AtomicUsize::new(0));
                let mapped_first = Arc::clone(&first_calls);
                let mapped_second = Arc::clone(&second_calls);
                let capture = Owned::new(0);
                let capture_drops = Arc::clone(&capture.drops);
                let mut capture = Some(capture);
                let first = OptionalMount::new(
                    |state: &Routes| state.first.as_ref(),
                    |state: &mut Routes| state.first.as_mut(),
                    |state: &Routes| {
                        state.first.map(|_| MountIdentity {
                            key: (),
                            replacement: 0,
                            configuration: state.revision,
                        })
                    },
                    |_| Child,
                );
                let second = OptionalMount::new(
                    |state: &Routes| state.second.as_ref(),
                    |state: &mut Routes| state.second.as_mut(),
                    |state: &Routes| {
                        state.second.map(|_| MountIdentity {
                            key: (),
                            replacement: 0,
                            configuration: 0,
                        })
                    },
                    |_| Child,
                );
                let (builder, _) = TestStore::feature_builder(
                    Routes { first: Some(2), second: Some(3), revision: 0 },
                    RoutesRoot,
                )
                .mount_with_children(first, move |_, children| {
                    let capture = capture.take().unwrap();
                    let calls = Arc::clone(&mapped_first);
                    *first_site.lock().unwrap() = Some(children.command(move |command: Owned| {
                        calls.fetch_add(1, Ordering::SeqCst);
                        command.value.get() + capture.value.get()
                    }));
                });
                let (builder, _) = builder.mount_with_children(second, move |_, children| {
                    let calls = Arc::clone(&mapped_second);
                    *second_site.lock().unwrap() = Some(children.command(move |command: Owned| {
                        calls.fetch_add(1, Ordering::SeqCst);
                        command.value.get()
                    }));
                });
                let mut store = builder.build();
                store.receive_structural(|_| {}).await.unwrap().finish().await.unwrap();
                let first_site = first_declared.lock().unwrap().take().unwrap();
                let second_site = second_declared.lock().unwrap().take().unwrap();
                let old = store.command_port(&first_site).await.unwrap().unwrap();
                let sibling = store.command_port(&second_site).await.unwrap().unwrap();

                store.send_with(Edit::Refresh, |state| state.revision = 1)
                    .await.unwrap().finish().await.unwrap();
                let current = store.command_port(&first_site).await.unwrap().unwrap();
                let command = Owned::new(7);
                let original = command.address();
                let drops = Arc::clone(&command.drops);
                let committed = current.try_send(command).unwrap().committed();
                futures_util::pin_mut!(committed);
                let error = store.receive_command(
                    &sibling,
                    |_| panic!("sibling matcher must not borrow another recipe input"),
                    |_| panic!("sibling expectation must not run"),
                ).await.unwrap_err();
                assert!(error.is_no_received_action());
                let error = store.receive_command(
                    &old,
                    |_| panic!("stale capture must not run the matcher"),
                    |_| panic!("stale capture must not run expectations"),
                ).await.unwrap_err();
                let access = error.task_access_error().unwrap();
                assert_eq!(access.reason(), TaskAccessReason::ScopeEnded);
                assert_eq!(access.progress(), TaskProgress::NotStarted);
                assert_eq!(first_calls.load(Ordering::SeqCst), 0);
                assert_eq!(second_calls.load(Ordering::SeqCst), 0);
                assert_eq!(drops.load(Ordering::SeqCst), 0);
                assert_eq!(capture_drops.load(Ordering::SeqCst), 0);
                assert!(futures_util::poll!(committed.as_mut()).is_pending());
                assert!(store.pending_boundary().is_some());
                store.receive_command(
                    &current,
                    |command| command.address() == original && command.value.get() == 7,
                    |state| state.first = Some(9),
                ).await.unwrap().finish().await.unwrap();
                let _ = committed.await.unwrap();
                assert_eq!(first_calls.load(Ordering::SeqCst), 1);
                assert_eq!(drops.load(Ordering::SeqCst), 1);

                store.send_with(Edit::Remove, |state| state.first = None)
                    .await.unwrap().finish().await.unwrap();
                assert_eq!(capture_drops.load(Ordering::SeqCst), 1);
                let command = Owned::new(11);
                let original = command.address();
                let drops = Arc::clone(&command.drops);
                let committed = sibling.try_send(command).unwrap().committed();
                futures_util::pin_mut!(committed);
                let error = store.receive_command(
                    &current,
                    |_| panic!("retired mapper must not run the matcher"),
                    |_| panic!("retired mapper must not run expectations"),
                ).await.unwrap_err();
                let access = error.task_access_error().unwrap();
                assert_eq!(access.reason(), TaskAccessReason::ScopeEnded);
                assert_eq!(access.progress(), TaskProgress::NotStarted);
                assert_eq!(first_calls.load(Ordering::SeqCst), 1);
                assert_eq!(second_calls.load(Ordering::SeqCst), 0);
                assert_eq!(drops.load(Ordering::SeqCst), 0);
                assert!(futures_util::poll!(committed.as_mut()).is_pending());
                assert!(store.pending_boundary().is_some());
                store.receive_command(
                    &sibling,
                    |command| command.address() == original && command.value.get() == 11,
                    |state| state.second = Some(14),
                ).await.unwrap().finish().await.unwrap();
                let _ = committed.await.unwrap();
                assert_eq!(first_calls.load(Ordering::SeqCst), 1);
                assert_eq!(second_calls.load(Ordering::SeqCst), 1);
                assert_eq!(drops.load(Ordering::SeqCst), 1);
                assert_eq!(capture_drops.load(Ordering::SeqCst), 1);
                store.finish().await.unwrap();
            });
        }

        #[test]
        #[expect(
            clippy::too_many_lines,
            reason = "One retirement checks admitted command and blocked child acquisition plus exact capture and payload ownership"
        )]
        fn queued_acquisitions_cannot_retarget_a_retired_recipe() {
            /// Removes its child before the queued acquisitions can be admitted.
            struct RetiringRoot;
            impl Feature for RetiringRoot {
                type State = Option<usize>;
                type Action = bool;
                fn reduce(
                    &mut self,
                    state: &mut Self::State,
                    retire: &bool,
                    _: &mut FeatureContext<'_, Self::State, bool>,
                ) {
                    if *retire {
                        *state = None;
                    }
                }
            }

            block_on(async {
                let host = DeterministicRuntime::new();
                let declared = Arc::new(Mutex::new(None));
                let captured_site = Arc::clone(&declared);
                let calls = Arc::new(AtomicUsize::new(0));
                let mapped_calls = Arc::clone(&calls);
                let capture = Owned::new(0);
                let capture_drops = Arc::clone(&capture.drops);
                let mut capture = Some(capture);
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
                    |_| Child,
                );
                let (builder, child_site) =
                    CoreStore::feature_builder(Some(2), RetiringRoot, host.runtime())
                        .mailbox_capacity(2)
                        .mount_with_children(mount, move |_, children| {
                            let capture = capture.take().unwrap();
                            let calls = Arc::clone(&mapped_calls);
                            let command = children.command(move |command: Owned| {
                                calls.fetch_add(1, Ordering::SeqCst);
                                command.value.get() + capture.value.get()
                            });
                            let child = children.optional(OptionalMount::new(
                                |state: &usize| Some(state),
                                |state: &mut usize| Some(state),
                                |_| Some(MountIdentity {
                                    key: (),
                                    replacement: 0,
                                    configuration: 0,
                                }),
                                |_| Child,
                            ));
                            *captured_site.lock().unwrap() = Some((command, child));
                        });
                let store = builder.build().unwrap();
                host.run(store.ready()).await.unwrap().unwrap();
                let (site, nested_site) = declared.lock().unwrap().take().unwrap();
                let port = host
                    .run(store.command_port(&site))
                    .await
                    .unwrap()
                    .unwrap()
                    .unwrap();

                let retirement = store.try_send(true).unwrap();
                let acquisition = store.command_port(&site);
                futures_util::pin_mut!(acquisition);
                assert!(futures_util::poll!(acquisition.as_mut()).is_pending());
                assert!(store.try_send(false).unwrap_err().is_full());
                let nested_acquisition = store.acquire(&nested_site);
                futures_util::pin_mut!(nested_acquisition);
                assert!(futures_util::poll!(nested_acquisition.as_mut()).is_pending());
                assert_eq!(capture_drops.load(Ordering::SeqCst), 0);
                let error = host.run(acquisition).await.unwrap().unwrap_err();
                assert_eq!(error.reason(), TaskAccessReason::ScopeEnded);
                assert_eq!(error.progress(), TaskProgress::NotStarted);
                host.run(retirement.finish())
                    .await
                    .unwrap()
                    .unwrap()
                    .finish()
                    .await;
                let error = host.run(nested_acquisition).await.unwrap().unwrap_err();
                assert_eq!(error.reason(), TaskAccessReason::ScopeEnded);
                assert_eq!(error.progress(), TaskProgress::NotStarted);
                assert_eq!(capture_drops.load(Ordering::SeqCst), 1);
                assert!(
                    host.run(store.acquire(&child_site))
                        .await
                        .unwrap()
                        .unwrap()
                        .is_none()
                );

                let command = Owned::new(7);
                let identity = command.address();
                let drops = Arc::clone(&command.drops);
                let send = port.send(command);
                futures_util::pin_mut!(send);
                // The live Store admits the request; its actor rejects retired ancestry.
                assert!(futures_util::poll!(send.as_mut()).is_pending());
                assert_eq!(calls.load(Ordering::SeqCst), 0);
                assert_eq!(drops.load(Ordering::SeqCst), 0);
                let rca_core::TaskSendError::Rejected { action, error } =
                    host.run(send).await.unwrap().unwrap_err()
                else {
                    panic!("the retired mapper must not enter");
                };
                assert_eq!(error.reason(), TaskAccessReason::ScopeEnded);
                assert_eq!(error.progress(), TaskProgress::NotStarted);
                assert_eq!(action.address(), identity);
                assert_eq!(action.value.get(), 7);
                assert_eq!(calls.load(Ordering::SeqCst), 0);
                assert_eq!(drops.load(Ordering::SeqCst), 0);
                drop(action);
                assert_eq!(drops.load(Ordering::SeqCst), 1);
                assert_eq!(
                    host.run(store.shutdown_into_state())
                        .await
                        .unwrap()
                        .unwrap(),
                    None
                );
                assert_eq!(capture_drops.load(Ordering::SeqCst), 1);
            });
        }
    };
}

mod shared {
    use rca_core::{
        CommandSite, Feature, FeatureContext, OptionalMount, Store as CoreStore, TryCommandError,
    };
    use rca_test::{CaptureKey, DeterministicRuntime, TestStore};
    /// Keeps shared commands movable without requiring their payload to be `Sync`.
    type Payload<T> = Box<T>;
    command_frontier!();

    /// Detects accidental action admission while tests only acquire command authority.
    struct CountedRoot(Arc<AtomicUsize>);

    impl Feature for CountedRoot {
        type State = Option<usize>;
        type Action = ();

        fn reduce(
            &mut self,
            _: &mut Self::State,
            (): &(),
            _: &mut FeatureContext<'_, Self::State, ()>,
        ) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[test]
    #[expect(
        clippy::too_many_lines,
        reason = "One declaration is checked before binding and through admitted and blocked queries at final owner closure"
    )]
    fn unbound_and_closing_command_acquisitions_do_not_retain_mapper_or_input() {
        block_on(async {
            let host = DeterministicRuntime::new();
            let capture = Owned::new(0);
            let capture_drops = Arc::clone(&capture.drops);
            let calls = Arc::new(AtomicUsize::new(0));
            let mapped_calls = Arc::clone(&calls);
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
                |_| Child,
            );
            let (builder, child_site, site) =
                CoreStore::feature_builder(Some(2), Root, host.runtime())
                    .mailbox_capacity(1)
                    .mount_with_command(mount, move |command: Owned| {
                        mapped_calls.fetch_add(1, Ordering::SeqCst);
                        command.value.get() + capture.value.get()
                    });
            let (mut probe, _, _) = fixture().await;
            let child_error = probe.acquire(&child_site).await.unwrap_err();
            let command_error = probe.command_port(&site).await.unwrap_err();
            for error in [&child_error, &command_error] {
                let access = error.task_access_error().unwrap();
                assert_eq!(access.reason(), TaskAccessReason::StoreClosed);
                assert_eq!(access.progress(), TaskProgress::NotStarted);
            }
            probe.finish().await.unwrap();
            let other = CoreStore::feature_builder(Some(2), Root, host.runtime())
                .build()
                .unwrap();
            host.run(other.ready()).await.unwrap().unwrap();
            let child_error = host
                .run(other.acquire(&child_site))
                .await
                .unwrap()
                .unwrap_err();
            let command_error = host
                .run(other.command_port(&site))
                .await
                .unwrap()
                .unwrap_err();
            for error in [child_error, command_error] {
                assert_eq!(error.reason(), TaskAccessReason::StoreClosed);
                assert_eq!(error.progress(), TaskProgress::NotStarted);
            }
            assert_eq!(capture_drops.load(Ordering::SeqCst), 0);

            let store = builder.build().unwrap();
            host.run(store.ready()).await.unwrap().unwrap();
            let gate = rca_core::testing::TransactionGate::new();
            for error in [
                host.run(other.command_port(&site))
                    .await
                    .unwrap()
                    .unwrap_err(),
                host.run(gate.command_port(&other, &site))
                    .await
                    .unwrap()
                    .unwrap_err(),
            ] {
                assert_eq!(error.reason(), TaskAccessReason::ScopeEnded);
                assert_eq!(error.progress(), TaskProgress::NotStarted);
            }
            assert_eq!(calls.load(Ordering::SeqCst), 0);
            assert!(gate.boundary().is_none());
            assert!(!gate.has_pending());
            assert_eq!(
                host.run(other.shutdown_into_state())
                    .await
                    .unwrap()
                    .unwrap(),
                Some(2)
            );
            assert_eq!(capture_drops.load(Ordering::SeqCst), 0);

            let port = host
                .run(store.command_port(&site))
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            let admitted = store.command_port(&site);
            let blocked = store.command_port(&site);
            let diagnostic = gate.command_port(&store, &site);
            futures_util::pin_mut!(admitted, blocked, diagnostic);
            assert!(futures_util::poll!(admitted.as_mut()).is_pending());
            assert!(store.try_send(()).unwrap_err().is_full());
            assert!(futures_util::poll!(blocked.as_mut()).is_pending());
            assert!(futures_util::poll!(diagnostic.as_mut()).is_pending());
            assert!(!gate.has_pending());
            assert_eq!(capture_drops.load(Ordering::SeqCst), 0);
            drop(store);
            host.run_until_stalled().unwrap();
            for error in [
                admitted
                    .now_or_never()
                    .expect("admitted query receives closure")
                    .unwrap_err(),
                blocked
                    .now_or_never()
                    .expect("blocked query receives closure")
                    .unwrap_err(),
                diagnostic
                    .now_or_never()
                    .expect("blocked diagnostic query receives closure")
                    .unwrap_err(),
            ] {
                assert_eq!(error.reason(), TaskAccessReason::StoreClosed);
                assert_eq!(error.progress(), TaskProgress::NotStarted);
            }
            assert_eq!(capture_drops.load(Ordering::SeqCst), 1);
            assert!(gate.boundary().is_none());
            assert!(!gate.has_pending());

            let command = Owned::new(7);
            let identity = command.address();
            let drops = Arc::clone(&command.drops);
            let rca_core::TaskSendError::Rejected { action, error } = port
                .send(command)
                .now_or_never()
                .expect("closed owner rejects immediately")
                .unwrap_err()
            else {
                panic!("the closed mapper must not enter");
            };
            assert_eq!(error.reason(), TaskAccessReason::StoreClosed);
            assert_eq!(error.progress(), TaskProgress::NotStarted);
            assert_eq!(action.address(), identity);
            assert_eq!(action.value.get(), 7);
            assert_eq!(calls.load(Ordering::SeqCst), 0);
            assert_eq!(drops.load(Ordering::SeqCst), 0);
            drop(action);
            assert_eq!(drops.load(Ordering::SeqCst), 1);
            assert_eq!(capture_drops.load(Ordering::SeqCst), 1);
        });
    }

    #[test]
    fn command_acquisition_validation_tracks_readiness_and_shutdown_without_admission() {
        block_on(async {
            let host = DeterministicRuntime::new();
            let capture = Owned::new(0);
            let capture_drops = Arc::clone(&capture.drops);
            let calls = Arc::new(AtomicUsize::new(0));
            let mapped_calls = Arc::clone(&calls);
            let reductions = Arc::new(AtomicUsize::new(0));
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
                |_| Child,
            );
            let (builder, _, site) = CoreStore::feature_builder(
                Some(2),
                CountedRoot(Arc::clone(&reductions)),
                host.runtime(),
            )
            .mount_with_command(mount, move |command: Owned| {
                mapped_calls.fetch_add(1, Ordering::SeqCst);
                command.value.get() + capture.value.get()
            });
            let store = builder.build().unwrap();
            let gate = rca_core::testing::TransactionGate::new();
            let error = gate
                .validate_command_acquisition(&store, &site)
                .unwrap_err();
            assert_eq!(error.reason(), TaskAccessReason::NotReady);
            assert_eq!(error.progress(), TaskProgress::NotStarted);
            assert_eq!(capture_drops.load(Ordering::SeqCst), 0);

            host.run(store.ready()).await.unwrap().unwrap();
            gate.validate_command_acquisition(&store, &site).unwrap();
            host.run(store.shutdown()).await.unwrap().unwrap();
            assert!(store.is_closed());
            let error = gate
                .validate_command_acquisition(&store, &site)
                .unwrap_err();
            assert_eq!(error.reason(), TaskAccessReason::StoreClosed);
            assert_eq!(error.progress(), TaskProgress::NotStarted);
            assert_eq!(capture_drops.load(Ordering::SeqCst), 1);
            assert_eq!(calls.load(Ordering::SeqCst), 0);
            assert_eq!(reductions.load(Ordering::SeqCst), 0);
            assert!(gate.boundary().is_none());
            assert!(!gate.has_pending());
            assert_eq!(host.drain_deadlines(0).await.unwrap().polls(), 0);
            assert_eq!(Arc::strong_count(&calls), 1);
            assert_eq!(Arc::strong_count(&reductions), 1);
        });
    }

    #[test]
    fn deferred_command_acquisitions_do_not_retain_or_resurrect_their_owner() {
        block_on(async {
            for dispose_actor_before_poll in [false, true] {
                let host = DeterministicRuntime::new();
                let capture = Owned::new(0);
                let capture_drops = Arc::clone(&capture.drops);
                let calls = Arc::new(AtomicUsize::new(0));
                let mapped_calls = Arc::clone(&calls);
                let reductions = Arc::new(AtomicUsize::new(0));
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
                    |_| Child,
                );
                let (builder, _, site) = CoreStore::feature_builder(
                    Some(2),
                    CountedRoot(Arc::clone(&reductions)),
                    host.runtime(),
                )
                .mount_with_command(mount, move |command: Owned| {
                    mapped_calls.fetch_add(1, Ordering::SeqCst);
                    command.value.get() + capture.value.get()
                });
                let store = builder.build().unwrap();
                host.run(store.ready()).await.unwrap().unwrap();
                host.run_until_stalled().unwrap();
                let gate = rca_core::testing::TransactionGate::new();

                // Neither future is polled while its Store is alive. Their
                // construction must not admit work or retain the declaration.
                let ordinary = store.command_port(&site);
                let diagnostic = gate.command_port(&store, &site);
                assert_eq!(host.run_until_stalled().unwrap().polls(), 0);
                assert_eq!(capture_drops.load(Ordering::SeqCst), 0);
                assert_eq!(calls.load(Ordering::SeqCst), 0);
                assert_eq!(reductions.load(Ordering::SeqCst), 0);
                drop(store);

                // The first case retains the closed actor's lifecycle until
                // after polling. The second disposes that real actor first.
                if dispose_actor_before_poll {
                    host.run_until_stalled().unwrap();
                    assert_eq!(capture_drops.load(Ordering::SeqCst), 1);
                } else {
                    assert_eq!(capture_drops.load(Ordering::SeqCst), 0);
                }
                for error in [
                    ordinary
                        .now_or_never()
                        .expect("closed owner rejects an unpolled ordinary acquisition")
                        .unwrap_err(),
                    diagnostic
                        .now_or_never()
                        .expect("closed owner rejects an unpolled diagnostic acquisition")
                        .unwrap_err(),
                ] {
                    assert_eq!(error.reason(), TaskAccessReason::StoreClosed);
                    assert_eq!(error.progress(), TaskProgress::NotStarted);
                }
                assert!(gate.boundary().is_none());
                assert!(!gate.has_pending());
                host.run_until_stalled().unwrap();
                assert_eq!(capture_drops.load(Ordering::SeqCst), 1);
                assert_eq!(calls.load(Ordering::SeqCst), 0);
                assert_eq!(reductions.load(Ordering::SeqCst), 0);
                let report = host.drain_deadlines(0).await.unwrap();
                assert_eq!(report.polls(), 0);
                assert_eq!(Arc::strong_count(&calls), 1);
                assert_eq!(Arc::strong_count(&reductions), 1);
            }
        });
    }

    #[test]
    #[expect(
        clippy::too_many_lines,
        reason = "one original command and reply span capacity rejection, matcher unwind, commitment and sticky teardown debt"
    )]
    fn root_mapped_command_preserves_original_input_until_expectation_commits() {
        block_on(async {
            let calls = Arc::new(AtomicUsize::new(0));
            let mapped_calls = Arc::clone(&calls);
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
                |_| Child,
            );
            let (builder, _, site) = TestStore::feature_builder(Some(2), Root)
                .mailbox_capacity(1)
                .mount_with_command(mount, move |command: Owned| {
                    mapped_calls.fetch_add(1, Ordering::SeqCst);
                    command.value.get() + 10
                });
            let mut store = builder
                .with_capture::<Count>(|state| Ok(state.unwrap_or(0)))
                .unwrap()
                .build();
            store
                .receive_structural(|_| {})
                .await
                .unwrap()
                .finish()
                .await
                .unwrap();
            let port = store.command_port(&site).await.unwrap().unwrap();
            let command = Owned::new(7);
            let original = command.address();
            let drops = Arc::clone(&command.drops);
            let committed = port.try_send(command).unwrap().committed();
            futures_util::pin_mut!(committed);
            let second = Owned::new(11);
            let second_address = second.address();
            let TryCommandError::Full(second) = port.try_send(second).unwrap_err() else {
                panic!("capacity one must return the unadmitted root command");
            };
            assert_eq!(second.address(), second_address);
            drop(second);
            let error = store
                .receive_command(
                    &port,
                    |command| {
                        assert_eq!(command.address(), original);
                        assert_eq!(command.value.get(), 7);
                        false
                    },
                    |_| panic!("mismatch must not run expectations"),
                )
                .await
                .unwrap_err();
            assert!(error.is_action_mismatch());
            let current = store.command_port(&site).await.unwrap().unwrap();
            let panic = std::panic::AssertUnwindSafe(store.receive_command(
                &current,
                |command| {
                    assert_eq!(command.address(), original);
                    panic!("matcher unwind");
                },
                |_| panic!("unreached"),
            ))
            .catch_unwind()
            .await
            .unwrap_err();
            assert_eq!(super::panic_message(panic.as_ref()), "matcher unwind");
            assert_eq!(calls.load(Ordering::SeqCst), 0);
            assert_eq!(drops.load(Ordering::SeqCst), 0);
            assert!(futures_util::poll!(committed.as_mut()).is_pending());
            let error = store
                .receive_command_with_captures(
                    &current,
                    |command| command.address() == original && command.value.get() == 7,
                    |state| *state = Some(19),
                    |captures| captures.set::<Count>(19),
                )
                .await
                .unwrap_err();
            assert!(error.is_incomplete_assertion());
            let _ = committed
                .now_or_never()
                .expect("original reply resolves")
                .unwrap();
            assert_eq!(calls.load(Ordering::SeqCst), 1);
            assert_eq!(drops.load(Ordering::SeqCst), 1);
            store
                .assert_state(|state| assert_eq!(*state, Some(19)))
                .await
                .unwrap();
            assert!(store.finish().await.unwrap_err().is_incomplete_assertion());
            let panic =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(store))).unwrap_err();
            assert!(super::panic_message(panic.as_ref()).starts_with(
                "TestStore teardown failed: TestStore incomplete assertion for transaction boundary at "
            ));
        });
    }

    #[test]
    #[expect(
        clippy::too_many_lines,
        reason = "one mounted command stays owned across sibling, foreign-root and refreshed-configuration checks"
    )]
    fn mounted_command_inspection_rejects_other_routes_without_consuming_input() {
        /// Keeps the sibling value stable while the first route refreshes.
        #[derive(Clone, Debug, PartialEq)]
        struct Routes {
            first: Option<usize>,
            second: Option<usize>,
            configuration: u64,
        }
        /// Refreshes configuration without changing either child's value.
        struct RoutesRoot;
        impl Feature for RoutesRoot {
            type State = Routes;
            type Action = ();
            fn reduce(
                &mut self,
                state: &mut Routes,
                (): &(),
                _: &mut FeatureContext<'_, Routes, ()>,
            ) {
                state.configuration += 1;
            }
        }

        block_on(async {
            let first_calls = Arc::new(AtomicUsize::new(0));
            let second_calls = Arc::new(AtomicUsize::new(0));
            let mapped_first = Arc::clone(&first_calls);
            let mapped_second = Arc::clone(&second_calls);
            let first_mount = || {
                OptionalMount::new(
                    |state: &Routes| state.first.as_ref(),
                    |state: &mut Routes| state.first.as_mut(),
                    |state: &Routes| {
                        state.first.map(|_| MountIdentity {
                            key: (),
                            replacement: 0,
                            configuration: state.configuration,
                        })
                    },
                    |_| Child,
                )
            };
            let second_mount = OptionalMount::new(
                |state: &Routes| state.second.as_ref(),
                |state: &mut Routes| state.second.as_mut(),
                |state: &Routes| {
                    state.second.map(|_| MountIdentity {
                        key: (),
                        replacement: 0,
                        configuration: 0,
                    })
                },
                |_| Child,
            );
            let initial = Routes {
                first: Some(2),
                second: Some(3),
                configuration: 0,
            };
            let (builder, _, first_site) = TestStore::feature_builder(initial.clone(), RoutesRoot)
                .mount_with_command(first_mount(), move |command: Owned| {
                    mapped_first.fetch_add(1, Ordering::SeqCst);
                    command.value.get()
                });
            let (builder, _, second_site) =
                builder.mount_with_command(second_mount, move |command: Owned| {
                    mapped_second.fetch_add(1, Ordering::SeqCst);
                    command.value.get()
                });
            let mut store = builder.build();
            store
                .receive_structural(|_| {})
                .await
                .unwrap()
                .finish()
                .await
                .unwrap();
            let first = store.command_port(&first_site).await.unwrap().unwrap();
            let sibling = store.command_port(&second_site).await.unwrap().unwrap();

            let foreign_calls = Arc::new(AtomicUsize::new(0));
            let mapped_foreign = Arc::clone(&foreign_calls);
            let (builder, _, foreign_site) = TestStore::feature_builder(initial, RoutesRoot)
                .mount_with_command(first_mount(), move |command: Owned| {
                    mapped_foreign.fetch_add(1, Ordering::SeqCst);
                    command.value.get()
                });
            let mut foreign = builder.build();
            foreign
                .receive_structural(|_| {})
                .await
                .unwrap()
                .finish()
                .await
                .unwrap();
            let foreign_port = foreign.command_port(&foreign_site).await.unwrap().unwrap();

            let command = Owned::new(7);
            let original = command.address();
            let drops = Arc::clone(&command.drops);
            let committed = first.try_send(command).unwrap().committed();
            futures_util::pin_mut!(committed);
            let error = store
                .receive_command(
                    &sibling,
                    |_| panic!("sibling matcher must not inspect the command"),
                    |_| panic!("sibling expectation must not run"),
                )
                .await
                .unwrap_err();
            assert!(error.is_no_received_action());
            let error = store
                .receive_command(
                    &foreign_port,
                    |_| panic!("foreign matcher must not inspect the command"),
                    |_| panic!("foreign expectation must not run"),
                )
                .await
                .unwrap_err();
            let access = error.task_access_error().unwrap();
            assert_eq!(access.reason(), TaskAccessReason::ScopeEnded);
            assert_eq!(access.progress(), TaskProgress::NotStarted);
            foreign.finish().await.unwrap();
            drop(foreign);
            let error = store
                .receive_command(
                    &foreign_port,
                    |_| panic!("closed foreign matcher must not inspect the command"),
                    |_| panic!("closed foreign expectation must not run"),
                )
                .await
                .unwrap_err();
            let access = error.task_access_error().unwrap();
            assert_eq!(access.reason(), TaskAccessReason::StoreClosed);
            assert_eq!(access.progress(), TaskProgress::NotStarted);
            assert_eq!(first_calls.load(Ordering::SeqCst), 0);
            assert_eq!(second_calls.load(Ordering::SeqCst), 0);
            assert_eq!(foreign_calls.load(Ordering::SeqCst), 0);
            assert_eq!(drops.load(Ordering::SeqCst), 0);
            assert!(futures_util::poll!(committed.as_mut()).is_pending());
            assert!(store.pending_boundary().is_some());
            store
                .receive_command(
                    &first,
                    |command| command.address() == original && command.value.get() == 7,
                    |state| state.first = Some(9),
                )
                .await
                .unwrap()
                .finish()
                .await
                .unwrap();
            let _ = committed
                .now_or_never()
                .expect("original reply resolves")
                .unwrap();
            assert_eq!(first_calls.load(Ordering::SeqCst), 1);
            assert_eq!(drops.load(Ordering::SeqCst), 1);

            store
                .send_with((), |state| state.configuration = 1)
                .await
                .unwrap()
                .finish()
                .await
                .unwrap();
            let current = store.command_port(&first_site).await.unwrap().unwrap();
            let command = Owned::new(4);
            let original = command.address();
            let refreshed_drops = Arc::clone(&command.drops);
            let committed = current.try_send(command).unwrap().committed();
            futures_util::pin_mut!(committed);
            let error = store
                .receive_command(
                    &first,
                    |_| panic!("old configuration must not inspect the current command"),
                    |_| panic!("old configuration must not run expectations"),
                )
                .await
                .unwrap_err();
            let access = error.task_access_error().unwrap();
            assert_eq!(access.reason(), TaskAccessReason::ScopeEnded);
            assert_eq!(access.progress(), TaskProgress::NotStarted);
            assert_eq!(first_calls.load(Ordering::SeqCst), 1);
            assert_eq!(refreshed_drops.load(Ordering::SeqCst), 0);
            assert!(futures_util::poll!(committed.as_mut()).is_pending());
            assert!(store.pending_boundary().is_some());
            store
                .receive_command(
                    &current,
                    |command| command.address() == original && command.value.get() == 4,
                    |state| state.first = Some(13),
                )
                .await
                .unwrap()
                .finish()
                .await
                .unwrap();
            let _ = committed
                .now_or_never()
                .expect("current reply resolves")
                .unwrap();
            assert_eq!(first_calls.load(Ordering::SeqCst), 2);
            assert_eq!(second_calls.load(Ordering::SeqCst), 0);
            assert_eq!(foreign_calls.load(Ordering::SeqCst), 0);
            assert_eq!(refreshed_drops.load(Ordering::SeqCst), 1);
            store
                .assert_state(|state| {
                    assert_eq!(state.first, Some(13));
                    assert_eq!(state.second, Some(3));
                    assert_eq!(state.configuration, 1);
                })
                .await
                .unwrap();
            store.finish().await.unwrap();
        });
    }
}

#[cfg(feature = "local")]
mod local {
    use rca_core::local::{
        CommandSite, Feature, FeatureContext, OptionalMount, Store as CoreStore, TryCommandError,
    };
    use rca_test::local::{CaptureKey, DeterministicRuntime, TestStore};
    /// Makes local command input explicitly non-`Send`.
    type Payload<T> = std::rc::Rc<T>;
    command_frontier!();
}
