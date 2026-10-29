//! Changed work uses the ordinary held frontier and originating task lineage.

/// Runs the same reaction and task-ownership checks in both execution modes.
macro_rules! changed_frontier {
    () => {
        use futures_executor::block_on;
        use rca_core::{InitialPolicy, MountIdentity, ReactionChange};
        use std::sync::{Arc, Mutex};

        /// Changes the mounted value without sending a child action.
        struct Root;
        impl Feature for Root {
            type State = Option<usize>;
            type Action = usize;
            fn reduce(
                &mut self,
                state: &mut Self::State,
                action: &usize,
                _: &mut FeatureContext<'_, Self::State, usize>,
            ) {
                *state = Some(*action);
            }
        }
        /// Adds child actions so reaction-produced work has a visible result.
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

        #[test]
        #[expect(
            clippy::too_many_lines,
            reason = "Fresh happy and sticky-matcher schedules retain causal ownership checks"
        )]
        fn changed_tasks_preserve_mounted_send_and_update_debt_and_parent_lineage() {
            for failed_match in [false, true] {
                block_on(async {
                    let trace = Arc::new(Mutex::new(Vec::new()));
                    let reactions = Arc::clone(&trace);
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
                    .on_change(
                        |state| *state,
                        InitialPolicy::Skip,
                        move |change, _| {
                            let ReactionChange::Changed { previous, current } = change else {
                                panic!("non-initial")
                            };
                            reactions.lock().unwrap().push((previous, current));
                            Task::run(move |context| async move {
                                if current == 3 {
                                    let _ = context.send(10).await.unwrap();
                                    let _ = context.update(|state| *state += 1).await.unwrap();
                                }
                            })
                        },
                    );
                    let (builder, site) =
                        TestStore::feature_builder(Some(2), Root).mount_with_site(mount);
                    let mut store = builder.build();
                    store
                        .receive_structural(|_| {})
                        .await
                        .unwrap()
                        .finish()
                        .await
                        .unwrap();
                    assert!(trace.lock().unwrap().is_empty());
                    let context = store.acquire(&site).await.unwrap().unwrap();
                    let parent = store.send_with(3, |state| *state = Some(3)).await.unwrap();
                    assert_eq!(*trace.lock().unwrap(), [(2, 3)]);
                    assert!(!parent.is_finished());
                    assert!(
                        store
                            .finish()
                            .await
                            .unwrap_err()
                            .finish_diagnostics()
                            .unwrap()
                            .has_pending_action(),
                        "Changed task's mounted send is debt"
                    );
                    store
                        .assert_state(|state| assert_eq!(*state, Some(3)))
                        .await
                        .unwrap();
                    assert!(
                        store
                            .finish()
                            .await
                            .unwrap_err()
                            .finish_diagnostics()
                            .unwrap()
                            .has_pending_action(),
                        "diagnostic observation cannot acknowledge Changed work"
                    );
                    if failed_match {
                        let mismatch = store
                            .receive_mounted(
                                &context,
                                |_| false,
                                |_| panic!("mismatch is not an expectation"),
                            )
                            .await
                            .expect_err("mismatch keeps original Changed task request");
                        assert!(mismatch.is_action_mismatch());
                    }
                    let received = store
                        .receive_mounted(&context, |action| *action == 10, |state| *state = Some(13))
                        .await;
                    assert_eq!(*trace.lock().unwrap(), [(2, 3), (3, 13)]);
                    assert!(
                        !parent.is_finished(),
                        "parent owns the reaction task through its later update"
                    );
                    if failed_match {
                        assert!(received.unwrap_err().is_incomplete_assertion());
                        store
                            .assert_state(|state| assert_eq!(*state, Some(13)))
                            .await
                            .unwrap();
                        assert!(store.finish().await.unwrap_err().is_incomplete_assertion());
                        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                            drop(store);
                        }))
                        .expect_err("Drop must enforce the failed mounted matcher");
                        assert!(
                            panic.downcast_ref::<String>().unwrap().starts_with(
                                "TestStore teardown failed: TestStore incomplete assertion for transaction boundary at "
                            )
                        );
                        assert!(parent.is_finished(), "teardown releases the reaction lineage");
                        return;
                    }
                    let received = received.unwrap();
                    assert!(
                        store
                            .finish()
                            .await
                            .unwrap_err()
                            .finish_diagnostics()
                            .unwrap()
                            .has_pending_action(),
                        "typed update remains a distinct boundary"
                    );
                    store
                        .receive_update(|state| *state = Some(14))
                        .await
                        .unwrap()
                        .finish()
                        .await
                        .unwrap();
                    assert_eq!(*trace.lock().unwrap(), [(2, 3), (3, 13), (13, 14)]);
                    received.finish().await.unwrap();
                    parent.finish().await.unwrap();
                    store.finish().await.unwrap();
                });
            }
        }
    };
}

mod shared {
    use rca_core::{Feature, FeatureContext, OptionalMount, Task};
    use rca_test::TestStore;
    changed_frontier!();
}

#[cfg(feature = "local")]
mod local {
    use rca_core::local::{Feature, FeatureContext, OptionalMount, Task};
    use rca_test::local::TestStore;
    changed_frontier!();
}
