//! Independently acquired contexts inspect the Initial task's original send.

/// Reads the standard panic payload so a different failing assertion cannot pass the test.
fn panic_message(panic: &(dyn std::any::Any + Send)) -> &str {
    if let Some(message) = panic.downcast_ref::<String>() {
        message
    } else {
        panic.downcast_ref::<&str>().unwrap()
    }
}

/// Checks the same mounted-action contracts with shared and owner-local payloads.
macro_rules! mounted_frontier {
    () => {
        use futures_executor::block_on;
        use futures_util::FutureExt;
        use rca_core::{DependencyKey, KeyedState, MountIdentity, OutputFlow};

        /// Hosts a child without changing its state through root actions.
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

        /// Adds the original owned action's value to the mounted state.
        struct Child;
        impl Feature for Child {
            type State = usize;
            type Action = Owned;
            fn update(
                &mut self,
                state: &mut usize,
                action: &Owned,
                _: &mut FeatureContext<'_, usize, Owned>,
            ) {
                *state += action.value();
            }
        }

        /// Starts one supplied effect when sent zero; all other actions change state.
        struct EffectChild(Option<Effect<Owned>>);
        impl Feature for EffectChild {
            type State = usize;
            type Action = Owned;
            fn update(
                &mut self,
                state: &mut usize,
                action: &Owned,
                context: &mut FeatureContext<'_, usize, Owned>,
            ) {
                if action.value() == 0 {
                    context.effect(self.0.take().expect("one effect starts"));
                } else {
                    *state += action.value();
                }
            }
        }

        /// Keeps the test store and its exact mounted declaration together.
        type EffectFixture = (
            TestStore<Option<usize>, ()>,
            MountSite<Option<usize>, usize, Owned, ()>,
        );

        /// Installs one effect without starting it before the test receives the trigger.
        fn effect_fixture(effect: Effect<Owned>, exhaustivity: rca_test::Exhaustivity) -> EffectFixture {
            let mut effect = Some(effect);
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
                move |_| EffectChild(effect.take()),
            );
            let (builder, site) =
                TestStore::builder(Some(2), Root).mount_with_site(mount);
            (builder.with_exhaustivity(exhaustivity).build(), site)
        }

        #[test]
        fn mounted_effect_action_is_received_with_original_identity_and_reply() {
            block_on(async {
                let action = Owned::new(7);
                let identity = action.identity();
                let drops = action.drops.clone();
                let (replied, reply) = async_channel::bounded(1);
                let effect = Effect::run(move |sender| async move {
                    replied.try_send(sender.send(action).await).unwrap();
                });
                let (mut store, site) = effect_fixture(effect, rca_test::Exhaustivity::Exhaustive);
                store.receive_structural(|_| {}).await.unwrap().finish().await.unwrap();
                let context = store.acquire(&site).await.unwrap().unwrap();
                let send = context.send(Owned::new(0));
                futures_util::pin_mut!(send);
                assert!(futures_util::poll!(send.as_mut()).is_pending());
                let started = store.receive_mounted(&context, |action| action.value() == 0, |_| {})
                    .await.unwrap();
                let submitted = send.now_or_never().expect("start reply completes").unwrap();
                assert_eq!(drops.load(std::sync::atomic::Ordering::SeqCst), 0);
                assert!(reply.is_empty());
                store.receive_mounted(&context, |action| {
                    assert_eq!(action.identity(), identity);
                    action.value() == 7
                }, |state| *state = Some(9)).await.unwrap().finish().await.unwrap();
                started.finish().await.unwrap();
                submitted.finish().await;
                assert!(reply.try_recv().unwrap().is_ok());
                assert_eq!(drops.load(std::sync::atomic::Ordering::SeqCst), 1);
                assert!(store.pending_boundary().is_none());
                store.finish().await.unwrap();
            });
        }

        #[test]
        fn mounted_effect_skip_and_cancellation_dispose_without_reducing() {
            block_on(async {
                for skip in [false, true] {
                    let action = Owned::new(7);
                    let drops = action.drops.clone();
                    let (replied, reply) = async_channel::bounded(1);
                    let effect = Effect::run(move |sender| async move {
                        replied.try_send(sender.send(action).await).unwrap();
                    });
                    let exhaustivity = if skip {
                        rca_test::Exhaustivity::NonExhaustive
                    } else {
                        rca_test::Exhaustivity::Exhaustive
                    };
                    let (mut store, site) = effect_fixture(effect, exhaustivity);
                    store.receive_structural(|_| {}).await.unwrap().finish().await.unwrap();
                    let context = store.acquire(&site).await.unwrap().unwrap();
                    let send = context.send(Owned::new(0));
                    futures_util::pin_mut!(send);
                    assert!(futures_util::poll!(send.as_mut()).is_pending());
                    let started = store.receive_mounted(&context, |action| action.value() == 0, |_| {})
                        .await.unwrap();
                    let submitted = send.now_or_never().expect("start reply completes").unwrap();
                    store.assert_state(|state| assert_eq!(*state, Some(2))).await.unwrap();
                    let boundary = store.pending_boundary().unwrap();
                    assert_eq!(drops.load(std::sync::atomic::Ordering::SeqCst), 0);
                    if skip {
                        store.skip_received().await.unwrap();
                    } else {
                        started.cancel().await.unwrap();
                    }
                    started.finish().await.unwrap();
                    submitted.finish().await;
                    store.assert_state(|state| assert_eq!(*state, Some(2))).await.unwrap();
                    assert_eq!(drops.load(std::sync::atomic::Ordering::SeqCst), 1);
                    if skip {
                        assert!(reply.try_recv().unwrap().unwrap_err().is_cancelled());
                        assert!(store.pending_boundary().is_none());
                    } else {
                        assert!(reply.is_closed() && reply.is_empty());
                        assert!(
                            store.finish().await.unwrap_err().is_incomplete_assertion(),
                            "terminal debt remains"
                        );
                        store.receive_terminal(boundary, |error| {
                            error.reason() == rca_core::TaskAccessReason::Cancelled
                                && error.progress() == rca_core::TaskProgress::NotStarted
                        }).await.unwrap();
                    }
                    store.finish().await.unwrap();
                }
            });
        }

        #[test]
        fn mounted_effect_matcher_restores_owned_request_before_cancellation_or_unwind() {
            block_on(async {
                for cancel in [false, true] {
                    for unwind in [false, true] {
                        let action = Owned::new(7);
                        let identity = action.identity();
                        let drops = action.drops.clone();
                        let (replied, reply) = async_channel::bounded(1);
                        let effect = Effect::run(move |sender| async move {
                            replied.try_send(sender.send(action).await).unwrap();
                        });
                        let (mut store, site) = effect_fixture(effect, rca_test::Exhaustivity::Exhaustive);
                        store.receive_structural(|_| {}).await.unwrap().finish().await.unwrap();
                        let context = store.acquire(&site).await.unwrap().unwrap();
                        let send = context.send(Owned::new(0));
                        futures_util::pin_mut!(send);
                        assert!(futures_util::poll!(send.as_mut()).is_pending());
                        let started = store.receive_mounted(&context, |action| action.value() == 0, |_| {})
                            .await.unwrap();
                        let submitted = send.now_or_never().expect("start reply completes").unwrap();
                        store.assert_state(|state| assert_eq!(*state, Some(2))).await.unwrap();
                        let boundary = store.pending_boundary().unwrap();
                        let result = std::panic::AssertUnwindSafe(store.receive_mounted(
                            &context,
                            |action| {
                                assert_eq!(action.identity(), identity);
                                if cancel {
                                    started.cancel().now_or_never().expect("finite cancellation").unwrap();
                                }
                                assert_eq!(drops.load(std::sync::atomic::Ordering::SeqCst), 0);
                                assert!(!unwind, "effect matcher unwind");
                                false
                            },
                            |_| panic!("unmatched input cannot mutate"),
                        )).catch_unwind().await;
                        if unwind {
                            let panic = result.unwrap_err();
                            assert_eq!(
                                super::panic_message(panic.as_ref()),
                                "effect matcher unwind"
                            );
                        } else {
                            assert!(result.unwrap().unwrap_err().is_action_mismatch());
                        }
                        if cancel {
                            started.finish().await.unwrap();
                            assert!(store.receive_terminal(boundary, |error| {
                                error.reason() == rca_core::TaskAccessReason::Cancelled
                                    && error.progress() == rca_core::TaskProgress::NotStarted
                            }).await.unwrap_err().is_incomplete_assertion());
                            assert!(reply.is_closed() && reply.is_empty());
                        } else {
                            assert_eq!(drops.load(std::sync::atomic::Ordering::SeqCst), 0);
                            let error = store.receive_mounted(&context, |action| {
                                assert_eq!(action.identity(), identity);
                                true
                            }, |state| *state = Some(9)).await.unwrap_err();
                            assert!(error.is_incomplete_assertion());
                            started.finish().await.unwrap();
                            assert!(reply.try_recv().unwrap().is_ok());
                        }
                        submitted.finish().await;
                        assert_eq!(drops.load(std::sync::atomic::Ordering::SeqCst), 1);
                        store.assert_state(move |state| assert_eq!(*state, Some(if cancel { 2 } else { 9 }))).await.unwrap();
                        assert!(store.finish().await.unwrap_err().is_incomplete_assertion());
                        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            drop(store);
                        }))
                        .unwrap_err();
                        assert!(super::panic_message(panic.as_ref()).starts_with(
                            "TestStore teardown failed: TestStore incomplete assertion for transaction boundary at "
                        ));
                    }
                }
            });
        }

        #[test]
        fn immediate_and_timer_child_effects_use_the_same_assertion_frontier() {
            block_on(async {
                for delayed in [false, true] {
                    let action = Owned::new(7);
                    let identity = action.identity();
                    let drops = action.drops.clone();
                    let delay = std::time::Duration::from_secs(5);
                    let effect = if delayed {
                        Effect::send_after(delay, action)
                    } else {
                        Effect::send(action)
                    };
                    let (mut store, site) = effect_fixture(effect, rca_test::Exhaustivity::Exhaustive);
                    store.receive_structural(|_| {}).await.unwrap().finish().await.unwrap();
                    let context = store.acquire(&site).await.unwrap().unwrap();
                    let send = context.send(Owned::new(0));
                    futures_util::pin_mut!(send);
                    assert!(futures_util::poll!(send.as_mut()).is_pending());
                    let started = store.receive_mounted(&context, |action| action.value() == 0, |_| {})
                        .await.unwrap();
                    let submitted = send.now_or_never().expect("start reply completes").unwrap();
                    if delayed {
                        assert!(store.pending_boundary().is_none());
                        store.advance(delay).await.unwrap();
                    }
                    store.receive_mounted(&context, |action| {
                        assert_eq!(action.identity(), identity);
                        true
                    }, |state| *state = Some(9)).await.unwrap().finish().await.unwrap();
                    started.finish().await.unwrap();
                    submitted.finish().await;
                    assert_eq!(drops.load(std::sync::atomic::Ordering::SeqCst), 1);
                    assert!(store.pending_boundary().is_none());
                    store.finish().await.unwrap();
                }
            });
        }

        #[test]
        #[expect(
            clippy::too_many_lines,
            reason = "one startup timeline checks blocked completion, mixed assertions and exact cleanup"
        )]
        fn timed_mounted_task_preserves_mixed_boundaries_and_finishes_without_extra_waits() {
            /// Receives the test clock and output declaration during store construction.
            struct TimedOutput;
            impl DependencyKey for TimedOutput {
                type Value = ();
                fn live_value() -> Self::Value {
                    panic!("the test provides its clock and output declaration")
                }
            }

            block_on(async {
                let delay = std::time::Duration::from_secs(5);
                let action = Owned::new(100);
                let identity = action.identity();
                let action_drops = action.drops.clone();
                let owner = Owned::new(0);
                let owner_drops = owner.drops.clone();
                let mut work = Some((action, owner));
                let inputs = InputsOwner::new(InputsCell::<(Clock, Output<usize>)>::new());
                let configured = InputsOwner::clone(&inputs);
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
                )
                .on_initial(
                    |state| *state,
                    move |_, _| {
                        let (clock, output) = inputs.get().unwrap();
                        let wait = clock.sleep(delay);
                        let output = output.clone();
                        let (action, owner) = work.take().expect("one mounted initial task");
                        Task::run(move |context| async move {
                            wait.await;
                            context.update(|state| *state += 1).await.unwrap();
                            context.output(&output, 10).await.unwrap();
                            drop(context.send(action).await.unwrap());
                            drop(owner);
                        })
                    },
                );
                let (builder, site) = TestStore::builder(Some(2), Root).mount_with_site(mount);
                let (builder, output) = builder.output::<usize>(&site, |handlers| {
                    handlers.at_root(|state, value, _| {
                        *state.as_mut().unwrap() += value;
                        OutputFlow::Consumed
                    });
                });
                let configured_output = output.clone();
                let mut store = builder
                    .with_clock_dependency::<TimedOutput>(move |clock| {
                        configured.set((clock, configured_output)).unwrap();
                    })
                    .build();
                let initial = store.receive_structural(|_| {}).await.unwrap();
                let context = store.acquire(&site).await.unwrap().unwrap();

                // Waiting for startup work must neither advance time nor drop its owners.
                assert!(store.pending_boundary().is_none());
                let blocked = initial.finish().await.expect_err("the initial task is sleeping");
                assert!(blocked.drive_error().is_none());
                assert_eq!(blocked.pending_sleeps().len(), 1);
                assert_eq!(blocked.current_time(), rca_core::MonotonicInstant::ZERO);
                assert_eq!(store.now(), rca_core::MonotonicInstant::ZERO);
                assert!(store.pending_boundary().is_none());
                store.assert_state(|state| assert_eq!(*state, Some(2))).await.unwrap();
                assert_eq!(action_drops.load(std::sync::atomic::Ordering::SeqCst), 0);
                assert_eq!(owner_drops.load(std::sync::atomic::Ordering::SeqCst), 0);

                store.advance(delay - std::time::Duration::from_secs(1)).await.unwrap();
                assert!(store.pending_boundary().is_none());
                assert!(!initial.is_finished());
                assert_eq!(owner_drops.load(std::sync::atomic::Ordering::SeqCst), 0);
                store.advance(std::time::Duration::from_secs(1)).await.unwrap();

                // Completing the same task cannot release its unasserted update.
                let update_boundary = store.pending_boundary().expect("the update is held");
                let blocked = initial.finish().await.expect_err("the update needs an assertion");
                assert!(blocked.drive_error().is_none());
                assert!(blocked.pending_sleeps().is_empty());
                assert_eq!(blocked.current_time(), rca_core::MonotonicInstant::from_elapsed(delay));
                assert_eq!(store.now(), rca_core::MonotonicInstant::from_elapsed(delay));
                // The opaque token exposes its generation and kind through Debug, not Eq.
                assert_eq!(
                    format!("{:?}", store.pending_boundary().expect("the same update stays held")),
                    format!("{update_boundary:?}"),
                );
                store.assert_state(|state| assert_eq!(*state, Some(2))).await.unwrap();
                assert!(!initial.is_finished());
                assert_eq!(action_drops.load(std::sync::atomic::Ordering::SeqCst), 0);
                assert_eq!(owner_drops.load(std::sync::atomic::Ordering::SeqCst), 0);

                let updated = store.receive_update(|state| *state = Some(3)).await.unwrap();
                let output = store.receive_output(
                    &output,
                    |value| *value == 10,
                    |state| *state = Some(13),
                ).await.unwrap();
                let received = store.receive_mounted(
                    &context,
                    |action| {
                        assert_eq!(action.identity(), identity);
                        action.value() == 100
                    },
                    |state| *state = Some(113),
                ).await.unwrap();

                // Completion must not need an extra wait on any task handle.
                store.finish().await.unwrap();
                for task in [initial, updated, output, received] {
                    assert!(task.is_finished());
                }
                assert_eq!(action_drops.load(std::sync::atomic::Ordering::SeqCst), 1);
                assert_eq!(owner_drops.load(std::sync::atomic::Ordering::SeqCst), 1);
                assert!(store.pending_boundary().is_none());
            });
        }

        #[test]
        #[expect(
            clippy::too_many_lines,
            reason = "one keyed Initial timeline verifies source identity, non-consumption and task cleanup"
        )]
        fn keyed_initial_tasks_receive_without_child_sends_and_preserve_source_identity() {
            type Rows = KeyedState<u8, usize>;
            /// Hosts keyed children without generating root mutations.
            struct RowsFeature;
            impl Feature for RowsFeature {
                type State = Rows;
                type Action = ();
                fn update(
                    &mut self,
                    _: &mut Rows,
                    _: &(),
                    _: &mut FeatureContext<'_, Rows, ()>,
                ) {
                }
            }
            /// Installs the same deterministic clock in both keyed initial tasks.
            struct ConfiguredClock;
            impl DependencyKey for ConfiguredClock {
                type Value = ();
            }

            block_on(async {
                let clock = InputsOwner::new(InputsCell::<Clock>::new());
                let configured_clock = InputsOwner::clone(&clock);
                let sites = [
                    InputsOwner::new(InputsCell::new()),
                    InputsOwner::new(InputsCell::new()),
                ];
                let declared_sites = sites.clone();
                let collection = OptionalMount::new(
                    |rows: &Rows| Some(rows),
                    |rows: &mut Rows| Some(rows),
                    |_| Some(MountIdentity { key: (), replacement: 0, configuration: 0 }),
                    |_| RowsFeature,
                );
                let (builder, _) = TestStore::builder(
                    [(0, 2), (1, 2)].into_iter().collect::<Rows>(),
                    RowsFeature,
                ).mount_with_children(collection, move |_, children| {
                    let clock = InputsOwner::clone(&clock);
                    let sites = declared_sites.clone();
                    children.keyed_with_children(
                        |rows| rows,
                        |rows| rows,
                        move |key, _| {
                            let clock = InputsOwner::clone(&clock);
                            let site = InputsOwner::clone(&sites[usize::from(*key)]);
                            let action = Owned::new(7);
                            let identity = action.identity();
                            let action_drops = action.drops.clone();
                            let owner = Owned::new(0);
                            let owner_drops = owner.drops.clone();
                            let mut work = Some((action, owner));
                            let entry = EntryMount::new(Child).on_initial(
                                |state| *state,
                                move |_, _| {
                                    let wait = clock.get().unwrap().sleep(std::time::Duration::from_secs(1));
                                    let (action, owner) = work.take().expect("one Initial task per key");
                                    Task::run(move |context| async move {
                                        wait.await;
                                        drop(context.send(action).await.unwrap());
                                        drop(owner);
                                    })
                                },
                            );
                            (entry, move |_: &usize, children: &mut MountChildren<'_, Rows, (), usize, Owned>| {
                                site.set((children.site().clone(), identity, action_drops.clone(), owner_drops.clone())).unwrap();
                            })
                        },
                    );
                });
                let mut store = builder
                    .with_clock_dependency::<ConfiguredClock>(move |clock| {
                        configured_clock.set(clock).unwrap();
                    })
                    .build();
                let initial = store.receive_structural(|_| {}).await.unwrap();
                let first = store.acquire(&sites[0].get().unwrap().0).await.unwrap().unwrap();
                let second = store.acquire(&sites[1].get().unwrap().0).await.unwrap().unwrap();
                assert!(store.pending_boundary().is_none());
                assert!(!initial.is_finished());
                store.advance(std::time::Duration::from_secs(1)).await.unwrap();

                let contexts = [first, second];
                let mut received = Vec::new();
                for index in 0..2 {
                    let error = store.receive_mounted(
                        &contexts[1 - index],
                        |_| panic!("a sibling cannot match the original Initial action"),
                        |_| panic!("a sibling cannot assert the original Initial action"),
                    ).await.unwrap_err();
                    assert!(error.is_no_received_action());
                    let (_, identity, action_drops, owner_drops) = sites[index].get().unwrap();
                    assert_eq!(action_drops.load(std::sync::atomic::Ordering::SeqCst), 0);
                    assert_eq!(owner_drops.load(std::sync::atomic::Ordering::SeqCst), 0);
                    received.push(store.receive_mounted(
                        &contexts[index],
                        |action| {
                            assert_eq!(action.identity(), *identity);
                            action.value() == 7
                        },
                        |rows| *rows.get_mut(&u8::try_from(index).unwrap()).unwrap() = 9,
                    ).await.unwrap());
                }

                store.finish().await.unwrap();
                assert!(initial.is_finished());
                for task in received {
                    assert!(task.is_finished());
                }
                for site in &sites {
                    let (_, _, action_drops, owner_drops) = site.get().unwrap();
                    assert_eq!(action_drops.load(std::sync::atomic::Ordering::SeqCst), 1);
                    assert_eq!(owner_drops.load(std::sync::atomic::Ordering::SeqCst), 1);
                }
                assert!(store.pending_boundary().is_none());
            });
        }

        #[test]
        fn initial_action_is_inspected_by_an_independently_acquired_context() {
            block_on(async {
                let mut action = Some(Owned::new(7));
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
                )
                .on_initial(
                    |state| *state,
                    move |_, _| {
                        let action = action.take();
                        Task::run(move |context| async move {
                            if let Some(action) = action {
                                let _ = context.send(action).await.unwrap();
                            }
                        })
                    },
                );
                let (builder, site) =
                    TestStore::builder(Some(2), Root).mount_with_site(mount);
                let mut store = builder.build();
                let error = store.acquire(&site).await.unwrap_err();
                let access = error.task_access_error().unwrap();
                assert_eq!(access.reason(), rca_core::TaskAccessReason::NotReady);
                assert_eq!(access.progress(), rca_core::TaskProgress::NotStarted);
                let initial = store.receive_structural(|_| {}).await.unwrap();
                let context = store.acquire(&site).await.unwrap().unwrap();
                let mismatch = store
                    .receive_mounted(&context, |_| false, |_| panic!("mismatch must not assert"))
                    .await
                    .expect_err("mismatch retains the original action and reply");
                assert!(mismatch.is_action_mismatch());
                let panic = std::panic::AssertUnwindSafe(store.receive_mounted(
                    &context,
                    |_| panic!("matcher unwind"),
                    |_| panic!("unreached"),
                ))
                .catch_unwind()
                .await
                .unwrap_err();
                assert_eq!(super::panic_message(panic.as_ref()), "matcher unwind");
                let error = store
                    .receive_mounted(
                        &context,
                        |action| action.value() == 7,
                        |state| *state = Some(9),
                    )
                    .await
                    .unwrap_err();
                assert!(error.is_incomplete_assertion());
                initial.finish().await.unwrap();
                store.assert_state(|state| assert_eq!(*state, Some(9))).await.unwrap();
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

        /// Separates sibling state from replacement and configuration identities.
        #[derive(Clone, Debug, PartialEq)]
        struct Pair {
            left: Option<usize>,
            right: Option<usize>,
            replacement: u64,
            configuration: u64,
        }

        /// Changes one route identity at a time, then removes that child.
        struct PairRoot;
        impl Feature for PairRoot {
            type State = Pair;
            type Action = u8;
            fn update(
                &mut self,
                state: &mut Pair,
                action: &u8,
                _: &mut FeatureContext<'_, Pair, u8>,
            ) {
                match action {
                    1 => state.replacement += 1,
                    2 => state.configuration += 1,
                    3 => state.left = None,
                    _ => {}
                }
            }
        }

        #[test]
        #[expect(
            clippy::too_many_lines,
            reason = "One pending request is preserved across sibling, foreign, replacement and configuration checks"
        )]
        fn sibling_foreign_and_stale_contexts_preserve_the_pending_request() {
            block_on(async {
                let descriptor = || {
                    OptionalMount::new(
                        |state: &Pair| state.left.as_ref(),
                        |state: &mut Pair| state.left.as_mut(),
                        |state: &Pair| {
                            state.left.map(|_| MountIdentity {
                                key: (),
                                replacement: state.replacement,
                                configuration: state.configuration,
                            })
                        },
                        |_| Child,
                    )
                };
                let sibling = OptionalMount::new(
                    |state: &Pair| state.right.as_ref(),
                    |state: &mut Pair| state.right.as_mut(),
                    |state: &Pair| {
                        state.right.map(|_| MountIdentity {
                            key: (),
                            replacement: 0,
                            configuration: 0,
                        })
                    },
                    |_| Child,
                );
                let state = Pair {
                    left: Some(2),
                    right: Some(3),
                    replacement: 0,
                    configuration: 0,
                };
                let (builder, site) = TestStore::builder(state.clone(), PairRoot)
                    .mount_with_site(descriptor());
                let (builder, sibling_site) = builder.mount_with_site(sibling);
                let mut store = builder.build();
                store
                    .receive_structural(|_| {})
                    .await
                    .unwrap()
                    .finish()
                    .await
                    .unwrap();
                store.send(0).await.unwrap().finish().await.unwrap();
                let original = store.acquire(&site).await.unwrap().unwrap();
                let sibling = store.acquire(&sibling_site).await.unwrap().unwrap();
                let (builder, foreign_site) =
                    TestStore::builder(state, PairRoot).mount_with_site(descriptor());
                let mut foreign = builder.build();
                foreign
                    .receive_structural(|_| {})
                    .await
                    .unwrap()
                    .finish()
                    .await
                    .unwrap();
                foreign.send(0).await.unwrap().finish().await.unwrap();
                let foreign_context = foreign.acquire(&foreign_site).await.unwrap().unwrap();
                let error = store.acquire(&foreign_site).await.unwrap_err();
                assert_eq!(
                    error.task_access_error().unwrap().reason(),
                    rca_core::TaskAccessReason::ScopeEnded
                );

                let send = original.send(Owned::new(7));
                futures_util::pin_mut!(send);
                assert!(futures_util::poll!(send.as_mut()).is_pending());
                let error = store
                    .receive_mounted(
                        &sibling,
                        |_| panic!("wrong sibling matcher"),
                        |_| panic!("wrong sibling expectation"),
                    )
                    .await
                    .unwrap_err();
                assert!(error.is_no_received_action());
                let error = store
                    .receive_mounted(
                        &foreign_context,
                        |_| panic!("foreign matcher"),
                        |_| panic!("foreign expectation"),
                    )
                    .await
                    .unwrap_err();
                let access = error.task_access_error().unwrap();
                assert_eq!(access.reason(), rca_core::TaskAccessReason::ScopeEnded);
                assert_eq!(access.progress(), rca_core::TaskProgress::NotStarted);
                store
                    .receive_mounted(
                        &original,
                        |action| action.value() == 7,
                        |state| state.left = Some(9),
                    )
                    .await
                    .unwrap()
                    .finish()
                    .await
                    .unwrap();
                let _ = send
                    .now_or_never()
                    .expect("original reply completed")
                    .unwrap();

                store
                    .send_with(1, |state| state.replacement = 1)
                    .await
                    .unwrap()
                    .finish()
                    .await
                    .unwrap();
                let replacement = store.acquire(&site).await.unwrap().unwrap();
                let send = replacement.send(Owned::new(1));
                futures_util::pin_mut!(send);
                assert!(futures_util::poll!(send.as_mut()).is_pending());
                let error = store
                    .receive_mounted(
                        &original,
                        |_| panic!("stale incarnation matcher"),
                        |_| panic!("stale incarnation expectation"),
                    )
                    .await
                    .unwrap_err();
                let access = error.task_access_error().unwrap();
                assert_eq!(access.reason(), rca_core::TaskAccessReason::ScopeEnded);
                assert_eq!(access.progress(), rca_core::TaskProgress::NotStarted);
                store
                    .receive_mounted(&replacement, |_| true, |state| state.left = Some(10))
                    .await
                    .unwrap()
                    .finish()
                    .await
                    .unwrap();
                let _ = send
                    .now_or_never()
                    .expect("replacement reply completed")
                    .unwrap();

                store
                    .send_with(2, |state| state.configuration = 1)
                    .await
                    .unwrap()
                    .finish()
                    .await
                    .unwrap();
                let configured = store.acquire(&site).await.unwrap().unwrap();
                let send = configured.send(Owned::new(1));
                futures_util::pin_mut!(send);
                assert!(futures_util::poll!(send.as_mut()).is_pending());
                let error = store
                    .receive_mounted(
                        &replacement,
                        |_| panic!("stale configuration matcher"),
                        |_| panic!("stale configuration expectation"),
                    )
                    .await
                    .unwrap_err();
                assert_eq!(
                    error.task_access_error().unwrap().reason(),
                    rca_core::TaskAccessReason::ScopeEnded
                );
                store
                    .receive_mounted(&configured, |_| true, |state| state.left = Some(11))
                    .await
                    .unwrap()
                    .finish()
                    .await
                    .unwrap();
                let _ = send
                    .now_or_never()
                    .expect("configuration reply completed")
                    .unwrap();
                store
                    .send_with(3, |state| state.left = None)
                    .await
                    .unwrap()
                    .finish()
                    .await
                    .unwrap();
                assert!(store.acquire(&site).await.unwrap().is_none());
                foreign.finish().await.unwrap();
                store.finish().await.unwrap();
            });
        }

        #[test]
        fn skip_returns_the_original_owned_action_through_the_original_reply() {
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
                    |_| Child,
                );
                let (builder, site) =
                    TestStore::builder(Some(2), Root).mount_with_site(mount);
                let mut store = builder
                    .with_exhaustivity(rca_test::Exhaustivity::NonExhaustive)
                    .build();
                store
                    .receive_structural(|_| {})
                    .await
                    .unwrap()
                    .finish()
                    .await
                    .unwrap();
                let context = store.acquire(&site).await.unwrap().unwrap();
                let action = Owned::new(7);
                let identity = action.identity();
                let drops = action.drops.clone();
                let send = context.send(action);
                futures_util::pin_mut!(send);
                assert!(futures_util::poll!(send.as_mut()).is_pending());
                store.assert_state(|state| assert_eq!(*state, Some(2))).await.unwrap();
                assert_eq!(drops.load(std::sync::atomic::Ordering::SeqCst), 0);
                store.skip_received().await.unwrap();
                let error = send
                    .now_or_never()
                    .expect("original reply completed")
                    .unwrap_err();
                let rca_core::TaskSendError::Rejected { action, error } = error else {
                    panic!("request never entered");
                };
                assert_eq!(error.reason(), rca_core::TaskAccessReason::Cancelled);
                assert_eq!(error.progress(), rca_core::TaskProgress::NotStarted);
                assert_eq!(action.identity(), identity);
                assert_eq!(drops.load(std::sync::atomic::Ordering::SeqCst), 0);
                drop(action);
                assert_eq!(drops.load(std::sync::atomic::Ordering::SeqCst), 1);
                store.finish().await.unwrap();
            });
        }

        #[test]
        fn cancellation_inside_matcher_restores_before_retiring_the_request() {
            block_on(async {
                for unwind in [false, true] {
                    let action = Owned::new(7);
                    let identity = action.identity();
                    let drops = action.drops.clone();
                    let mut action = Some(action);
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
                    )
                    .on_initial(
                        |state| *state,
                        move |_, _| {
                            let action = action.take();
                            Task::run(move |context| async move {
                                if let Some(action) = action {
                                    let _ = context.send(action).await;
                                }
                            })
                        },
                    );
                    let (builder, site) =
                        TestStore::builder(Some(2), Root).mount_with_site(mount);
                    let mut store = builder.build();
                    let initial = store.receive_structural(|_| {}).await.unwrap();
                    let context = store.acquire(&site).await.unwrap().unwrap();
                    let boundary = store.pending_boundary().unwrap();
                    let result = std::panic::AssertUnwindSafe(store.receive_mounted(
                        &context,
                        |action| {
                            assert_eq!(action.identity(), identity);
                            initial
                                .cancel()
                                .now_or_never()
                                .expect("finite cancellation")
                                .unwrap();
                            assert_eq!(drops.load(std::sync::atomic::Ordering::SeqCst), 0);
                            assert!(!unwind, "terminal matcher unwind");
                            false
                        },
                        |_| panic!("cancelled matcher expectation"),
                    ))
                    .catch_unwind()
                    .await;
                    if unwind {
                        let panic = result.unwrap_err();
                        assert_eq!(super::panic_message(panic.as_ref()), "terminal matcher unwind");
                    } else {
                        assert!(result.unwrap().unwrap_err().is_action_mismatch());
                    }
                    initial.finish().await.unwrap();
                    assert_eq!(drops.load(std::sync::atomic::Ordering::SeqCst), 1);
                    assert!(store.finish().await.unwrap_err().is_incomplete_assertion());
                    assert!(store.receive_terminal(boundary, |error| {
                        error.reason() == rca_core::TaskAccessReason::Cancelled
                            && error.progress() == rca_core::TaskProgress::NotStarted
                    }).await.unwrap_err().is_incomplete_assertion());
                    store.assert_state(|state| assert_eq!(*state, Some(2))).await.unwrap();
                    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        drop(store);
                    }))
                    .unwrap_err();
                    assert!(super::panic_message(panic.as_ref()).starts_with(
                        "TestStore teardown failed: TestStore incomplete assertion for transaction boundary at "
                    ));
                }
            });
        }

        #[test]
        fn terminal_acknowledgement_cannot_release_an_admitted_failed_expectation() {
            use std::sync::Arc;
            use std::sync::atomic::{AtomicBool, Ordering};
            /// Can withdraw mutable access after an action has passed admission.
            struct Selected {
                child: Option<usize>,
                available: Arc<AtomicBool>,
            }
            /// Leaves selection changes under the test's control.
            struct SelectedRoot;
            impl Feature for SelectedRoot {
                type State = Selected;
                type Action = ();
                fn update(&mut self, _: &mut Selected, _: &(), _: &mut FeatureContext<'_, Selected, ()>) {}
            }
            block_on(async {
                let available = Arc::new(AtomicBool::new(true));
                let mount = OptionalMount::new(
                    |state: &Selected| state.child.as_ref(),
                    |state: &mut Selected| {
                        if state.available.load(Ordering::SeqCst) { state.child.as_mut() } else { None }
                    },
                    |state: &Selected| state.child.map(|_| MountIdentity { key: (), replacement: 0, configuration: 0 }),
                    |_| Child,
                );
                let (builder, site) = TestStore::projected_builder(
                    Selected { child: Some(2), available: Arc::clone(&available) },
                    SelectedRoot, |state| state.child,
                ).mount_with_site(mount);
                let mut store = builder.build();
                store.receive_structural(|_| {}).await.unwrap().finish().await.unwrap();
                let context = store.acquire(&site).await.unwrap().unwrap();
                let action = Owned::new(7);
                let action_drops = action.drops.clone();
                let mut send = Box::pin(context.send(action));
                assert!(futures_util::poll!(send.as_mut()).is_pending());
                store.receive_mounted(&context, |_| true, |_| {
                    available.store(false, Ordering::SeqCst);
                }).await.unwrap_err();
                let rca_core::TaskSendError::Failed { error } = send.now_or_never().unwrap().unwrap_err() else {
                    panic!("projection entry prevents action recovery");
                };
                assert_eq!(error.reason(), rca_core::TaskAccessReason::ScopeEnded);
                assert_eq!(error.progress(), rca_core::TaskProgress::MayHaveRun);
                assert_eq!(action_drops.load(Ordering::SeqCst), 1);
                let boundary = store.pending_boundary().unwrap();
                assert!(store.receive_terminal(boundary.clone(), |terminal| *terminal == error)
                    .await.unwrap_err().is_incomplete_assertion());
                available.store(true, Ordering::SeqCst);
                let successor = Owned::new(1);
                let successor_drops = successor.drops.clone();
                let mut successor = Box::pin(context.send(successor));
                assert!(futures_util::poll!(successor.as_mut()).is_pending());
                store.assert_state(|state| assert_eq!(state.child, Some(2))).await.unwrap();
                let original_retained = store.receive_terminal(boundary, |terminal| *terminal == error)
                    .await.unwrap_err().is_incomplete_assertion();
                assert!(futures_util::poll!(successor.as_mut()).is_pending());
                assert!(store.finish().await.unwrap_err().is_incomplete_assertion());
                let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    drop(store);
                }))
                .unwrap_err();
                assert!(super::panic_message(panic.as_ref()).starts_with(
                    "TestStore teardown failed: TestStore incomplete assertion for receive_mounted at "
                ));
                drop(successor);
                assert_eq!(successor_drops.load(Ordering::SeqCst), 1, "failed assertion teardown still disposes queued ownership");
                assert!(original_retained, "terminal acknowledgement must not release earlier admitted snapshot debt");
            });
        }

        /// Checks the child value independently from the full-root snapshot.
        struct Count;
        impl CaptureKey for Count {
            type Value = usize;
            type Error = ();
        }

        #[test]
        fn acquisition_settles_an_initial_send_that_is_ready_but_not_yet_surfaced() {
            block_on(async {
                let (release, receive) = async_channel::bounded::<()>(1);
                let mut receive = Some(receive);
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
                )
                .on_initial(
                    |state| *state,
                    move |_, _| {
                        let receive = receive.take();
                        Task::run(move |context| async move {
                            if let Some(receive) = receive {
                                receive.recv().await.unwrap();
                                let _ = context.send(Owned::new(7)).await.unwrap();
                            }
                        })
                    },
                );
                let (builder, site) =
                    TestStore::builder(Some(2), Root).mount_with_site(mount);
                let mut store = builder.build();
                let initial = store.receive_structural(|_| {}).await.unwrap();
                release.try_send(()).unwrap();
                // Nothing drives the Initial task between readiness and acquire.
                // The precheck must precede settle, then acquisition selects the
                // newly surfaced boundary instead of queuing behind its action.
                let context = store.acquire(&site).await.unwrap().unwrap();
                store
                    .receive_mounted(
                        &context,
                        |action| action.value() == 7,
                        |state| *state = Some(9),
                    )
                    .await
                    .unwrap()
                    .finish()
                    .await
                    .unwrap();
                initial.finish().await.unwrap();
                store.finish().await.unwrap();
            });
        }

        #[test]
        fn mounted_actions_share_full_root_capture_verification_and_sticky_debt() {
            block_on(async {
                for mismatch in [false, true] {
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
                    let (builder, site) =
                        TestStore::builder(Some(2), Root).mount_with_site(mount);
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
                    let context = store.acquire(&site).await.unwrap().unwrap();
                    let send = context.send(Owned::new(7));
                    futures_util::pin_mut!(send);
                    assert!(futures_util::poll!(send.as_mut()).is_pending());
                    let result = std::panic::AssertUnwindSafe(store.receive_mounted_with_captures(
                        &context,
                        |_| true,
                        |state| *state = Some(if mismatch { 99 } else { 9 }),
                        |captures| captures.set::<Count>(9),
                    ))
                    .catch_unwind()
                    .await;
                    let _ = send
                        .now_or_never()
                        .expect("original committed reply")
                        .unwrap();
                    if mismatch {
                        let panic = result.unwrap_err();
                        let message = super::panic_message(panic.as_ref());
                        assert!(message.starts_with("TestStore state mismatch at "), "{message}");
                        assert!(message.contains(" during receive_mounted:"), "{message}");
                        store
                            .assert_state(|state| assert_eq!(*state, Some(9)))
                            .await
                            .unwrap();
                        let error = store
                            .receive_mounted(
                                &context,
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
                            "TestStore teardown failed: TestStore incomplete assertion for receive_mounted at "
                        ));
                    } else {
                        result.unwrap().unwrap().finish().await.unwrap();
                        store.finish().await.unwrap();
                    }
                }
            });
        }
    };
}

mod shared {
    use rca_core::{
        Clock, Effect, EntryMount, Feature, FeatureContext, MountChildren, MountSite,
        OptionalMount, Output, Task,
    };
    use rca_test::{CaptureKey, TestStore};
    use std::cell::Cell;
    use std::sync::{Arc as InputsOwner, OnceLock as InputsCell};

    /// Provides a move-only, `Send` but non-`Sync` action with an independent drop count.
    #[derive(Debug)]
    struct Owned {
        value: Box<Cell<usize>>,
        drops: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }
    impl Owned {
        /// Creates one action whose allocation can be tracked across admission.
        fn new(value: usize) -> Self {
            Self {
                value: Box::new(Cell::new(value)),
                drops: std::sync::Arc::default(),
            }
        }
        /// Reads the payload without cloning its owner.
        fn value(&self) -> usize {
            self.value.get()
        }
        /// Observes allocation identity without dereferencing the returned address.
        fn identity(&self) -> usize {
            std::ptr::from_ref(self.value.as_ref()).addr()
        }
    }
    impl Drop for Owned {
        fn drop(&mut self) {
            self.drops.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    }
    /// Deliberately omits snapshot traits to prove acquisition needs none of them.
    struct Opaque(std::cell::Cell<usize>);
    /// Type-checks acquisition's minimal bounds and `Send` future without running it.
    fn acquire_without_snapshot_traits<'a>(
        store: &'a mut TestStore<Option<usize>, (), Opaque>,
        site: &rca_core::MountSite<Option<usize>, usize, Owned, ()>,
    ) -> impl Future<
        Output = Result<Option<rca_core::TaskContext<usize, Owned>>, rca_test::TestStoreError>,
    > + Send
    + 'a {
        store.acquire(site)
    }
    #[test]
    fn acquisition_signature_keeps_minimal_shared_bounds() {
        let _ = acquire_without_snapshot_traits;
        assert_eq!(Opaque(Cell::new(1)).0.get(), 1);
    }

    #[test]
    fn deferred_mounted_acquisitions_do_not_retain_or_resurrect_their_owner() {
        use rca_core::{Store as CoreStore, TaskAccessReason, TaskProgress};
        use rca_test::DeterministicRuntime;
        use std::sync::{Arc, atomic::Ordering};

        block_on(async {
            for dispose_actor_before_poll in [false, true] {
                let host = DeterministicRuntime::new();
                let capture = Owned::new(0);
                let capture_drops = Arc::clone(&capture.drops);
                let factory_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
                let calls = Arc::clone(&factory_calls);
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
                        let _ = capture.value();
                        calls.fetch_add(1, Ordering::SeqCst);
                        Child
                    },
                );
                let gate = rca_core::testing::TransactionGate::new();
                let (builder, site) = CoreStore::builder(Some(2), Root, host.runtime())
                    .transaction_gate(gate.clone())
                    .mount_with_site(mount);
                let store = builder.build().unwrap();
                host.run(gate.wait_for_pending()).await.unwrap();
                let startup = gate.boundary().unwrap();
                drop(
                    host.run(gate.release_pending_with_receipt().unwrap().finish())
                        .await
                        .unwrap()
                        .unwrap(),
                );
                assert!(gate.acknowledge(startup));
                host.run(store.ready()).await.unwrap().unwrap();
                host.run_until_stalled().unwrap();
                assert_eq!(factory_calls.load(Ordering::SeqCst), 1);

                // Construction must neither retain the owner nor admit work.
                // Poll only after final Store closure, with or without the
                // closed actor's lifecycle still awaiting physical disposal.
                let ordinary = store.acquire(&site);
                let diagnostic = gate.acquire(&store, &site);
                let input = store.input_gate(&site);
                assert_eq!(host.run_until_stalled().unwrap().polls(), 0);
                assert_eq!(capture_drops.load(Ordering::SeqCst), 0);
                drop(store);
                if dispose_actor_before_poll {
                    host.run_until_stalled().unwrap();
                    assert_eq!(capture_drops.load(Ordering::SeqCst), 1);
                } else {
                    assert_eq!(capture_drops.load(Ordering::SeqCst), 0);
                }
                for error in [
                    ordinary
                        .now_or_never()
                        .expect("closed owner rejects an unpolled mounted acquisition")
                        .unwrap_err(),
                    diagnostic
                        .now_or_never()
                        .expect("closed owner rejects an unpolled diagnostic acquisition")
                        .unwrap_err(),
                    input
                        .now_or_never()
                        .expect("closed owner rejects an unpolled input-gate acquisition")
                        .unwrap_err(),
                ] {
                    assert_eq!(error.reason(), TaskAccessReason::StoreClosed);
                    assert_eq!(error.progress(), TaskProgress::NotStarted);
                }
                assert!(gate.boundary().is_none());
                assert!(!gate.has_pending());
                host.run_until_stalled().unwrap();
                assert_eq!(capture_drops.load(Ordering::SeqCst), 1);
                assert_eq!(factory_calls.load(Ordering::SeqCst), 1);
                assert_eq!(host.drain_deadlines(0).await.unwrap().polls(), 0);
                assert_eq!(Arc::strong_count(&factory_calls), 1);
            }
        });
    }
    mounted_frontier!();
}

#[cfg(feature = "local")]
mod local {
    use rca_core::local::{
        Clock, Effect, EntryMount, Feature, FeatureContext, MountChildren, MountSite,
        OptionalMount, Output, Task,
    };
    use rca_test::local::{CaptureKey, TestStore};
    use std::cell::OnceCell as InputsCell;
    use std::rc::Rc;
    use std::rc::Rc as InputsOwner;

    /// Keeps an original action owner-local and counts its destruction separately.
    #[derive(Debug)]
    struct Owned {
        value: Rc<usize>,
        drops: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }
    impl Owned {
        /// Creates one action without making its wrapper cloneable.
        fn new(value: usize) -> Self {
            Self {
                value: Rc::new(value),
                drops: std::sync::Arc::default(),
            }
        }
        /// Reads the original payload through its owner-local handle.
        fn value(&self) -> usize {
            *self.value
        }
        /// Observes the original allocation's identity without using the address as a pointer.
        fn identity(&self) -> usize {
            Rc::as_ptr(&self.value).addr()
        }
    }
    impl Drop for Owned {
        fn drop(&mut self) {
            self.drops.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    }
    /// Omits snapshot traits and thread-transfer traits on purpose.
    struct Opaque(Rc<()>);
    /// Type-checks owner-local acquisition without adding snapshot or `Send` bounds.
    fn acquire_without_snapshot_traits<'a>(
        store: &'a mut TestStore<Option<usize>, (), Opaque>,
        site: &rca_core::local::MountSite<Option<usize>, usize, Owned, ()>,
    ) -> impl Future<
        Output = Result<
            Option<rca_core::local::TaskContext<usize, Owned>>,
            rca_test::local::TestStoreError,
        >,
    > + 'a {
        store.acquire(site)
    }
    #[test]
    fn acquisition_signature_keeps_local_snapshot_and_action_owner_local() {
        let _ = acquire_without_snapshot_traits;
        assert_eq!(Rc::strong_count(&Opaque(Rc::new(())).0), 1);
    }
    mounted_frontier!();
}
