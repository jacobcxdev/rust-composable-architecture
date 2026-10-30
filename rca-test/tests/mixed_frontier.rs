//! Mixed transactions share one ordered frontier in both execution modes.

/// Checks one ordered mutation sequence against shared and owner-local stores.
macro_rules! mixed_frontier {
    () => {
        use futures_executor::block_on;
        use rca_core::{InitialPolicy, MountIdentity, ReactionChange};
        use std::panic::{AssertUnwindSafe, catch_unwind};
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::{Arc, Mutex};

        /// Keeps equal children distinct from their shared output total.
        #[derive(Clone, Debug, PartialEq)]
        struct State {
            children: [Option<usize>; 2],
            output: usize,
        }

        /// Leaves root state unchanged so only the selected child or output can edit it.
        struct Root;
        impl Feature for Root {
            type State = State;
            type Action = ();
            fn update(&mut self, _: &mut State, _: &(), _: &mut FeatureContext<'_, State, ()>) {}
        }

        /// Adds the submitted child action to its own value.
        struct Child;
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

        /// Records both children independently of the full-state expectation.
        struct Children;
        impl CaptureKey for Children {
            type Value = [Option<usize>; 2];
            type Error = ();
        }

        /// Observes a source that can change outside a store transaction.
        struct External;
        impl CaptureKey for External {
            type Value = usize;
            type Error = ();
        }

        /// Owns two child routes and their independent reaction and capture probes.
        struct Fixture {
            store: TestStore<State, ()>,
            contexts: Vec<TaskContext<usize, usize>>,
            outputs: Vec<Output<usize>>,
            changes: Arc<Mutex<Vec<(usize, usize, usize)>>>,
            external: Arc<AtomicUsize>,
        }

        /// Builds equal siblings whose declaration and source identities still differ.
        async fn fixture() -> Fixture {
            let changes = Arc::new(Mutex::new(Vec::new()));
            let external = Arc::new(AtomicUsize::new(0));
            let captured = Arc::clone(&external);
            let mut builder = TestStore::builder(
                State {
                    children: [Some(2), Some(2)],
                    output: 0,
                },
                Root,
            );
            let mut sites = Vec::new();
            let mut outputs = Vec::new();
            for index in 0..2 {
                let reactions = Arc::clone(&changes);
                let select: fn(&State) -> Option<&usize> = if index == 0 {
                    |state| state.children[0].as_ref()
                } else {
                    |state| state.children[1].as_ref()
                };
                let select_mut: fn(&mut State) -> Option<&mut usize> = if index == 0 {
                    |state| state.children[0].as_mut()
                } else {
                    |state| state.children[1].as_mut()
                };
                let mount = OptionalMount::new(
                    select,
                    select_mut,
                    move |state: &State| {
                        state.children[index].map(|_| MountIdentity {
                            key: (),
                            replacement: 0,
                            configuration: 0,
                        })
                    },
                    |_| Child,
                )
                .on_change(
                    |state| *state,
                    InitialPolicy::Skip,
                    move |change, _| {
                        let ReactionChange::Changed { previous, current } = change else {
                            panic!("Initial is skipped")
                        };
                        reactions.lock().unwrap().push((index, previous, current));
                        Task::run(|_| async {})
                    },
                );
                let (next, site) = builder.mount_with_site(mount);
                let (next, output) = next.output::<usize>(&site, |handlers| {
                    handlers.at_root(|state, value, _| {
                        state.output += value;
                        OutputFlow::Consumed
                    });
                });
                builder = next;
                sites.push(site);
                outputs.push(output);
            }
            let mut store = builder
                .with_capture::<Children>(|state| Ok(state.children))
                .unwrap()
                .with_capture::<External>(move |_| Ok(captured.load(Ordering::SeqCst)))
                .unwrap()
                .build();
            store
                .receive_structural(|_| {})
                .await
                .unwrap()
                .finish()
                .await
                .unwrap();
            let mut contexts = Vec::new();
            for site in &sites {
                contexts.push(store.acquire(site).await.unwrap().unwrap());
            }
            Fixture {
                store,
                contexts,
                outputs,
                changes,
                external,
            }
        }

        #[test]
        #[expect(
            clippy::too_many_lines,
            reason = "one mixed frontier trace retains source and boundary order through every assertion"
        )]
        fn equal_sibling_payloads_preserve_mixed_order_and_sticky_capture_debt() {
            block_on(async {
                let Fixture {
                    mut store,
                    contexts,
                    outputs,
                    changes,
                    external,
                } = fixture().await;
                let mut expected = State {
                    children: [Some(2), Some(2)],
                    output: 0,
                };

                // Both siblings submit the same values. Payload equality cannot
                // substitute for the exact declaration or mounted source.
                for source in 0..2 {
                    let context = &contexts[source];
                    let output = &outputs[source];
                    let sequence = async {
                        let first = context.update(|_| ()).await.unwrap(); // A
                        let _ = context.output(output, 7).await.unwrap(); // O
                        let _ = context.send(7).await.unwrap(); // private C
                        let last = context.update(|state| *state += 1).await.unwrap(); // B
                        (first, last)
                    };
                    futures_util::pin_mut!(sequence);
                    assert!(futures_util::poll!(sequence.as_mut()).is_pending());
                    assert!(
                        store
                            .receive_output(
                                output,
                                |_| panic!("A is not an output"),
                                |_| panic!("wrong kind cannot assert"),
                            )
                            .await
                            .unwrap_err()
                            .is_no_received_action()
                    );
                    assert!(
                        store
                            .finish()
                            .await
                            .unwrap_err()
                            .finish_diagnostics()
                            .unwrap()
                            .has_pending_action(),
                        "equal A remains debt"
                    );
                    store
                        .receive_update_with_captures(
                            |_| {},
                            |captures| captures.set::<Children>(expected.children),
                        )
                        .await
                        .unwrap()
                        .finish()
                        .await
                        .unwrap();
                    assert_eq!(changes.lock().unwrap().len(), source * 2);

                    assert!(futures_util::poll!(sequence.as_mut()).is_pending());
                    assert!(
                        store
                            .receive_update(|_| panic!("O is not an update"))
                            .await
                            .unwrap_err()
                            .is_no_received_action()
                    );
                    assert!(
                        store
                            .receive_output(
                                &outputs[1 - source],
                                |_| panic!("equal sibling output has a different declaration"),
                                |_| panic!("wrong declaration cannot assert"),
                            )
                            .await
                            .unwrap_err()
                            .is_no_received_action()
                    );
                    expected.output += 7;
                    store
                        .receive_output_with_captures(
                            output,
                            |value| *value == 7,
                            |state| *state = expected.clone(),
                            |captures| captures.set::<Children>(expected.children),
                        )
                        .await
                        .unwrap()
                        .finish()
                        .await
                        .unwrap();

                    assert!(futures_util::poll!(sequence.as_mut()).is_pending());
                    assert!(
                        store
                            .receive_output(
                                output,
                                |_| panic!("C is not an output"),
                                |_| panic!("wrong kind cannot assert"),
                            )
                            .await
                            .unwrap_err()
                            .is_no_received_action()
                    );
                    assert!(
                        store
                            .receive_mounted(
                                &contexts[1 - source],
                                |_| panic!("equal sibling action has a different source"),
                                |_| panic!("wrong source cannot assert"),
                            )
                            .await
                            .unwrap_err()
                            .is_no_received_action()
                    );
                    *expected.children[source].as_mut().unwrap() += 7;
                    store
                        .receive_mounted_with_captures(
                            context,
                            |action| *action == 7,
                            |state| *state = expected.clone(),
                            |captures| captures.set::<Children>(expected.children),
                        )
                        .await
                        .unwrap()
                        .finish()
                        .await
                        .unwrap();

                    assert!(futures_util::poll!(sequence.as_mut()).is_pending());
                    assert!(
                        store
                            .receive_mounted(
                                context,
                                |_| panic!("B is not an action"),
                                |_| panic!("wrong kind cannot assert"),
                            )
                            .await
                            .unwrap_err()
                            .is_no_received_action()
                    );
                    *expected.children[source].as_mut().unwrap() += 1;
                    store
                        .receive_update_with_captures(
                            |state| *state = expected.clone(),
                            |captures| captures.set::<Children>(expected.children),
                        )
                        .await
                        .unwrap()
                        .finish()
                        .await
                        .unwrap();
                    let (first, last) = sequence.await;
                    assert_eq!(last.revision(), first.revision() + 3);
                }
                assert_eq!(
                    *changes.lock().unwrap(),
                    [(0, 2, 9), (0, 9, 10), (1, 2, 9), (1, 9, 10)]
                );

                // This external declared source can genuinely revert even while
                // the failed comparison blocks later managed transactions.
                external.store(1, Ordering::SeqCst);
                assert!(
                    store
                        .check_captures(|_| Ok(()))
                        .await
                        .unwrap_err()
                        .is_capture_mismatch()
                );
                external.store(0, Ordering::SeqCst);
                assert_eq!(external.load(Ordering::SeqCst), 0);
                assert!(
                    store
                        .check_captures(|_| Ok(()))
                        .await
                        .unwrap_err()
                        .is_incomplete_assertion()
                );
                assert!(store.finish().await.unwrap_err().is_incomplete_assertion());
                let panic = catch_unwind(AssertUnwindSafe(|| drop(store))).unwrap_err();
                let message = panic.downcast_ref::<String>().unwrap();
                assert!(
                    message.starts_with(
                        "TestStore teardown failed: TestStore incomplete assertion for check_captures at "
                    ),
                    "{message}"
                );
            });
        }
    };
}

mod shared {
    use rca_core::OutputFlow;
    use rca_core::{Feature, FeatureContext, OptionalMount, Output, Task, TaskContext};
    use rca_test::{CaptureKey, TestStore};
    mixed_frontier!();
}

#[cfg(feature = "local")]
mod local {
    use rca_core::OutputFlow;
    use rca_core::local::{Feature, FeatureContext, OptionalMount, Output, Task, TaskContext};
    use rca_test::local::{CaptureKey, TestStore};
    mixed_frontier!();

    #[test]
    fn retained_rc_callback_after_pending_commits_one_update_and_reaction() {
        use std::cell::{Cell, RefCell};
        use std::rc::Rc;

        block_on(async {
            let Fixture {
                mut store,
                contexts,
                changes,
                ..
            } = fixture().await;
            let context = contexts[0].clone();
            let baseline = context.update(|_| ());
            futures_util::pin_mut!(baseline);
            assert!(futures_util::poll!(baseline.as_mut()).is_pending());
            store
                .receive_update(|_| {})
                .await
                .unwrap()
                .finish()
                .await
                .unwrap();
            let baseline = baseline.await.unwrap().revision();
            assert!(changes.lock().unwrap().is_empty());

            let (release, gate) = async_channel::bounded::<()>(1);
            let pending = Rc::new(Cell::new(false));
            let calls = Rc::new(Cell::new(0));
            let callback_pending = Rc::clone(&pending);
            let callback_calls = Rc::clone(&calls);
            let callback = Rc::new(move || {
                let context = context.clone();
                let gate = gate.clone();
                let pending = Rc::clone(&callback_pending);
                let calls = Rc::clone(&callback_calls);
                async move {
                    pending.set(true);
                    gate.recv().await.unwrap();
                    context
                        .update(move |state| {
                            calls.set(calls.get() + 1);
                            *state += 1;
                        })
                        .await
                        .unwrap()
                }
            });
            let retained = Rc::new(RefCell::new(Some(callback)));
            let callback = Rc::clone(retained.borrow().as_ref().unwrap());
            let invoked = callback();
            futures_util::pin_mut!(invoked);
            assert!(futures_util::poll!(invoked.as_mut()).is_pending());
            assert!(
                pending.get(),
                "the retained callback reached its controlled wait"
            );
            assert_eq!(calls.get(), 0);
            assert!(changes.lock().unwrap().is_empty());
            store
                .assert_state(|state| assert_eq!(state.children, [Some(2), Some(2)]))
                .await
                .unwrap();

            release.try_send(()).unwrap();
            assert!(futures_util::poll!(invoked.as_mut()).is_pending());
            assert_eq!(
                calls.get(),
                0,
                "the normal assertion frontier still holds entry"
            );
            assert!(
                store
                    .finish()
                    .await
                    .unwrap_err()
                    .finish_diagnostics()
                    .unwrap()
                    .has_pending_action()
            );
            let assertions = Rc::new(Cell::new(0));
            store
                .receive_update_with_captures(
                    |state| {
                        assertions.set(assertions.get() + 1);
                        state.children[0] = Some(3);
                    },
                    |captures| captures.set::<Children>([Some(3), Some(2)]),
                )
                .await
                .unwrap()
                .finish()
                .await
                .unwrap();
            let committed = invoked.await;
            assert_eq!(calls.get(), 1);
            assert_eq!(assertions.get(), 1);
            assert_eq!(*changes.lock().unwrap(), [(0, 2, 3)]);
            // The revision witnesses one publication boundary, not subscriber
            // notification counts (covered by the shared publication engine).
            assert_eq!(committed.revision(), baseline + 1);
            store.finish().await.unwrap();
            assert_eq!(calls.get(), 1);
            assert_eq!(changes.lock().unwrap().len(), 1);
            retained.borrow_mut().take();
        });
    }
}
