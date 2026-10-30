//! Omitted startup, output and command assertions preserve execution, history and failures.

/// Runs the same production routes with transferable and owner-local payloads.
macro_rules! focused_outputs {
    () => {
        use futures_executor::block_on;
        use futures_util::FutureExt;
        use rca_core::{
            MountIdentity, OutputDelivery, OutputFlow, TaskAccessReason, TaskProgress,
            TaskSendError,
        };
        use rca_test::{Exhaustivity, OmissionReporting};
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
        use std::sync::{Arc, Mutex};

        /// Separates child state from the test-controlled mutable projection.
        struct State {
            /// Changes only when a production handler or child reducer runs.
            child: Option<usize>,
            /// Withdraws mutable access after admission to exercise production rejection.
            available: Arc<AtomicBool>,
        }

        /// Leaves all state changes to the mounted child and output handler.
        struct Root;

        impl Feature for Root {
            type State = State;
            type Action = ();

            fn update(&mut self, _: &mut State, (): &(), _: &mut FeatureContext<'_, State, ()>) {}
        }

        /// Adds the original payload to the child state.
        struct Child;

        impl Feature for Child {
            type State = usize;
            type Action = Payload<usize>;

            fn update(
                &mut self,
                state: &mut usize,
                action: &Payload<usize>,
                _: &mut FeatureContext<'_, usize, Payload<usize>>,
            ) {
                *state += **action;
            }
        }

        /// Checks history independently from the selected root snapshot.
        struct Count;

        impl CaptureKey for Count {
            type Value = usize;
            type Error = ();
        }

        /// Keeps the exact production declarations and independently observed call counts.
        struct Fixture {
            /// Uses a selected snapshot because the live projection switch is not history.
            store: TestStore<State, (), Option<usize>>,
            /// Sends mounted actions and outputs through their original authority.
            context: TaskContext<usize, Payload<usize>>,
            /// Admits narrow commands before their mapper runs.
            port: CommandPort<Payload<usize>>,
            /// Selects the one root output handler.
            output: Output<Payload<usize>>,
            /// Counts production output delivery independently of the snapshot.
            deliveries: Arc<AtomicUsize>,
            /// Counts command mapping independently of child reduction.
            mappings: Arc<AtomicUsize>,
            /// Controls the projection without changing the stored snapshot.
            available: Arc<AtomicBool>,
        }

        /// Builds one mounted route without omitting its structural expectation.
        async fn fixture() -> Fixture {
            let available = Arc::new(AtomicBool::new(true));
            let mappings = Arc::new(AtomicUsize::new(0));
            let mapped = Arc::clone(&mappings);
            let declared = Arc::new(Mutex::new(None));
            let command_site = Arc::clone(&declared);
            let mount = OptionalMount::new(
                |state: &State| state.child.as_ref(),
                |state: &mut State| {
                    if state.available.load(Ordering::SeqCst) {
                        state.child.as_mut()
                    } else {
                        None
                    }
                },
                |state: &State| {
                    state.child.map(|_| MountIdentity {
                        key: (),
                        replacement: 0,
                        configuration: 0,
                    })
                },
                |_| Child,
            );
            let (builder, site) = TestStore::projected_builder(
                State {
                    child: Some(2),
                    available: Arc::clone(&available),
                },
                Root,
                |state| state.child,
            )
            .mount_with_children(mount, move |_, children| {
                let mapped = Arc::clone(&mapped);
                *command_site.lock().unwrap() =
                    Some(children.command(move |value: Payload<usize>| {
                        mapped.fetch_add(1, Ordering::SeqCst);
                        value
                    }));
            });
            let deliveries = Arc::new(AtomicUsize::new(0));
            let delivered = Arc::clone(&deliveries);
            let (builder, output) = builder.output::<Payload<usize>>(&site, |handlers| {
                handlers.at_root(move |state, value, _| {
                    delivered.fetch_add(1, Ordering::SeqCst);
                    *state.child.as_mut().unwrap() += *value;
                    OutputFlow::Consumed
                });
            });
            let mut store = builder
                .with_exhaustivity(Exhaustivity::NonExhaustive)
                .with_capture::<Count>(|state| Ok(state.child.unwrap_or(0)))
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
            let command = declared.lock().unwrap().take().unwrap();
            let port = store.command_port(&command).await.unwrap().unwrap();
            Fixture {
                store,
                context,
                port,
                output,
                deliveries,
                mappings,
                available,
            }
        }

        /// Requires teardown to retain the original incomplete assertion.
        fn assert_incomplete_teardown(store: TestStore<State, (), Option<usize>>) {
            let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(store)))
                .expect_err("teardown must reject retained assertion debt");
            let message = panic.downcast_ref::<String>().unwrap();
            assert!(
                message.starts_with("TestStore teardown failed: TestStore incomplete assertion"),
                "{message}"
            );
        }

        #[test]
        fn omitted_startup_constructs_child_and_keeps_initial_task_causality() {
            block_on(async {
                let constructions = Arc::new(AtomicUsize::new(0));
                let constructed = Arc::clone(&constructions);
                let started = Arc::new(AtomicBool::new(false));
                let completed = Arc::new(AtomicBool::new(false));
                let task_started = Arc::clone(&started);
                let task_completed = Arc::clone(&completed);
                let mount = OptionalMount::new(
                    |state: &State| state.child.as_ref(),
                    |state: &mut State| state.child.as_mut(),
                    |state: &State| {
                        state.child.map(|_| MountIdentity {
                            key: (),
                            replacement: 0,
                            configuration: 0,
                        })
                    },
                    move |_| {
                        constructed.fetch_add(1, Ordering::SeqCst);
                        Child
                    },
                )
                .on_initial(
                    |state| *state,
                    move |_, _| {
                        let started = Arc::clone(&task_started);
                        let completed = Arc::clone(&task_completed);
                        Task::run(move |context| async move {
                            started.store(true, Ordering::SeqCst);
                            let _task = context.send(Payload::new(7)).await.unwrap();
                            completed.store(true, Ordering::SeqCst);
                        })
                    },
                );
                let (builder, site) = TestStore::projected_builder(
                    State {
                        child: Some(2),
                        available: Arc::new(AtomicBool::new(true)),
                    },
                    Root,
                    |state| state.child,
                )
                .mount_with_site(mount);
                let mut store = builder
                    .with_exhaustivity(Exhaustivity::NonExhaustive)
                    .build();
                assert_eq!(constructions.load(Ordering::SeqCst), 0);
                assert!(!started.load(Ordering::SeqCst));

                let startup = store
                    .receive_omitting(OmissionReporting::Warn)
                    .await
                    .unwrap();
                let context = store.acquire(&site).await.unwrap().unwrap();
                assert_eq!(constructions.load(Ordering::SeqCst), 1);
                assert!(started.load(Ordering::SeqCst));
                assert!(!completed.load(Ordering::SeqCst));
                assert_eq!(store.take_omissions().len(), 1);
                store
                    .receive_mounted(&context, |value| **value == 7, |state| *state = Some(9))
                    .await
                    .unwrap()
                    .finish()
                    .await
                    .unwrap();
                startup.finish().await.unwrap();
                assert!(completed.load(Ordering::SeqCst));
                assert_eq!(constructions.load(Ordering::SeqCst), 1);
                store.finish().await.unwrap();
            });
        }

        #[test]
        fn omission_warnings_belong_to_each_store() {
            block_on(async {
                let mut first = fixture().await;
                let mut second = fixture().await;
                let first_delivery = first.context.output(&first.output, Payload::new(7));
                let second_delivery = second.context.output(&second.output, Payload::new(11));
                futures_util::pin_mut!(first_delivery, second_delivery);
                assert!(futures_util::poll!(first_delivery.as_mut()).is_pending());
                assert!(futures_util::poll!(second_delivery.as_mut()).is_pending());
                first
                    .store
                    .receive_omitting(OmissionReporting::Warn)
                    .await
                    .unwrap()
                    .finish()
                    .await
                    .unwrap();
                second
                    .store
                    .receive_omitting(OmissionReporting::Warn)
                    .await
                    .unwrap()
                    .finish()
                    .await
                    .unwrap();
                assert!(matches!(
                    first_delivery.now_or_never().unwrap().unwrap(),
                    OutputDelivery::Consumed { .. }
                ));
                assert!(matches!(
                    second_delivery.now_or_never().unwrap().unwrap(),
                    OutputDelivery::Consumed { .. }
                ));

                assert_eq!(first.store.take_omissions().len(), 1);
                assert!(first.store.take_omissions().is_empty());
                assert_eq!(second.store.take_omissions().len(), 1);
                assert!(second.store.take_omissions().is_empty());
                first
                    .store
                    .assert_state(|state| assert_eq!(state.child, Some(9)))
                    .await
                    .unwrap();
                second
                    .store
                    .assert_state(|state| assert_eq!(state.child, Some(13)))
                    .await
                    .unwrap();
                first.store.finish().await.unwrap();
                second.store.finish().await.unwrap();
            });
        }

        #[test]
        fn omitted_output_runs_handler_and_installs_both_histories() {
            block_on(async {
                let mut fixture = fixture().await;
                let delivery = fixture.context.output(&fixture.output, Payload::new(7));
                futures_util::pin_mut!(delivery);
                assert!(futures_util::poll!(delivery.as_mut()).is_pending());

                fixture
                    .store
                    .receive_omitting(OmissionReporting::Warn)
                    .await
                    .unwrap()
                    .finish()
                    .await
                    .unwrap();
                assert!(matches!(
                    delivery
                        .now_or_never()
                        .expect("original output reply completes")
                        .unwrap(),
                    OutputDelivery::Consumed { .. }
                ));
                assert_eq!(fixture.deliveries.load(Ordering::SeqCst), 1);
                assert_eq!(fixture.store.take_omissions().len(), 1);
                assert!(fixture.store.take_omissions().is_empty());

                let next = fixture.context.output(&fixture.output, Payload::new(1));
                futures_util::pin_mut!(next);
                assert!(futures_util::poll!(next.as_mut()).is_pending());
                fixture
                    .store
                    .receive_output_with_captures(
                        &fixture.output,
                        |value| **value == 1,
                        |state| {
                            assert_eq!(*state, Some(9), "the omitted snapshot becomes history");
                            *state = Some(10);
                        },
                        |captures| captures.set::<Count>(10),
                    )
                    .await
                    .unwrap()
                    .finish()
                    .await
                    .unwrap();
                assert!(matches!(
                    next.now_or_never()
                        .expect("checked output reply completes")
                        .unwrap(),
                    OutputDelivery::Consumed { .. }
                ));
                assert_eq!(fixture.deliveries.load(Ordering::SeqCst), 2);
                fixture.store.finish().await.unwrap();
            });
        }

        #[test]
        fn omitted_command_runs_mapper_and_installs_both_histories() {
            block_on(async {
                let mut fixture = fixture().await;
                let committed = fixture.port.try_send(Payload::new(7)).unwrap().committed();
                fixture
                    .store
                    .receive_omitting(OmissionReporting::Silent)
                    .await
                    .unwrap()
                    .finish()
                    .await
                    .unwrap();
                let _task = committed
                    .now_or_never()
                    .expect("original command reply completes")
                    .unwrap();
                assert_eq!(fixture.mappings.load(Ordering::SeqCst), 1);
                assert!(fixture.store.take_omissions().is_empty());

                let next = fixture.port.try_send(Payload::new(1)).unwrap().committed();
                fixture
                    .store
                    .receive_command_with_captures(
                        &fixture.port,
                        |value| **value == 1,
                        |state| {
                            assert_eq!(*state, Some(9), "the omitted snapshot becomes history");
                            *state = Some(10);
                        },
                        |captures| captures.set::<Count>(10),
                    )
                    .await
                    .unwrap()
                    .finish()
                    .await
                    .unwrap();
                let _next_task = next
                    .now_or_never()
                    .expect("checked command reply completes")
                    .unwrap();
                assert_eq!(fixture.mappings.load(Ordering::SeqCst), 2);
                fixture.store.finish().await.unwrap();
            });
        }

        #[test]
        fn failed_output_matcher_blocks_omitted_execution() {
            block_on(async {
                let mut fixture = fixture().await;
                let delivery = fixture.context.output(&fixture.output, Payload::new(7));
                futures_util::pin_mut!(delivery);
                assert!(futures_util::poll!(delivery.as_mut()).is_pending());
                let error = fixture
                    .store
                    .receive_output(
                        &fixture.output,
                        |_| false,
                        |_| panic!("failed matching cannot prepare an expectation"),
                    )
                    .await
                    .unwrap_err();
                assert!(error.is_action_mismatch());

                let error = fixture
                    .store
                    .receive_omitting(OmissionReporting::Warn)
                    .await
                    .unwrap_err();
                assert!(error.is_incomplete_assertion());
                assert_eq!(fixture.deliveries.load(Ordering::SeqCst), 0);
                assert!(futures_util::poll!(delivery.as_mut()).is_pending());
                assert!(fixture.store.take_omissions().is_empty());
                fixture
                    .store
                    .assert_state(|state| assert_eq!(state.child, Some(2)))
                    .await
                    .unwrap();
                assert!(
                    fixture
                        .store
                        .finish()
                        .await
                        .unwrap_err()
                        .is_incomplete_assertion()
                );
                assert_incomplete_teardown(fixture.store);
            });
        }

        #[test]
        fn failed_command_matcher_blocks_omitted_execution() {
            block_on(async {
                let mut fixture = fixture().await;
                let committed = fixture.port.try_send(Payload::new(7)).unwrap().committed();
                futures_util::pin_mut!(committed);
                let error = fixture
                    .store
                    .receive_command(
                        &fixture.port,
                        |_| false,
                        |_| panic!("failed matching cannot prepare an expectation"),
                    )
                    .await
                    .unwrap_err();
                assert!(error.is_action_mismatch());

                let error = fixture
                    .store
                    .receive_omitting(OmissionReporting::Silent)
                    .await
                    .unwrap_err();
                assert!(error.is_incomplete_assertion());
                assert_eq!(fixture.mappings.load(Ordering::SeqCst), 0);
                assert!(futures_util::poll!(committed.as_mut()).is_pending());
                assert!(fixture.store.take_omissions().is_empty());
                fixture
                    .store
                    .assert_state(|state| assert_eq!(state.child, Some(2)))
                    .await
                    .unwrap();
                assert!(
                    fixture
                        .store
                        .finish()
                        .await
                        .unwrap_err()
                        .is_incomplete_assertion()
                );
                assert_incomplete_teardown(fixture.store);
            });
        }

        #[test]
        fn omitted_stale_mount_keeps_terminal_and_failed_commit_debt() {
            block_on(async {
                let mut fixture = fixture().await;
                let send = fixture.context.send(Payload::new(7));
                futures_util::pin_mut!(send);
                assert!(futures_util::poll!(send.as_mut()).is_pending());
                fixture
                    .store
                    .assert_state(|state| assert_eq!(state.child, Some(2)))
                    .await
                    .unwrap();
                let boundary = fixture
                    .store
                    .pending_boundary()
                    .expect("mounted action is held");
                fixture.available.store(false, Ordering::SeqCst);

                fixture
                    .store
                    .receive_omitting(OmissionReporting::Silent)
                    .await
                    .expect_err("production must reject the withdrawn child projection");
                let TaskSendError::Failed { error } = send
                    .now_or_never()
                    .expect("original failure reply completes")
                    .unwrap_err()
                else {
                    panic!("projection entry prevents recovery of the original action");
                };
                assert_eq!(error.reason(), TaskAccessReason::ScopeEnded);
                assert_eq!(error.progress(), TaskProgress::MayHaveRun);
                assert!(fixture.store.take_omissions().is_empty());
                fixture
                    .store
                    .assert_state(|state| assert_eq!(state.child, Some(2)))
                    .await
                    .unwrap();
                assert!(
                    fixture
                        .store
                        .receive_omitting(OmissionReporting::Warn)
                        .await
                        .unwrap_err()
                        .is_incomplete_assertion()
                );
                assert!(
                    fixture
                        .store
                        .receive_terminal(boundary, |actual| *actual == error)
                        .await
                        .unwrap_err()
                        .is_incomplete_assertion()
                );
                assert!(fixture.store.pending_boundary().is_some());
                assert!(
                    fixture
                        .store
                        .finish()
                        .await
                        .unwrap_err()
                        .is_incomplete_assertion()
                );
                assert_incomplete_teardown(fixture.store);
            });
        }
    };
}

mod shared {
    use rca_core::{
        CommandPort, Feature, FeatureContext, OptionalMount, Output, Task, TaskContext,
    };
    use rca_test::{CaptureKey, TestStore};
    use std::sync::Arc as Payload;

    focused_outputs!();
}

#[cfg(feature = "local")]
mod local {
    use rca_core::local::{
        CommandPort, Feature, FeatureContext, OptionalMount, Output, Task, TaskContext,
    };
    use rca_test::local::{CaptureKey, TestStore};
    use std::rc::Rc as Payload;

    focused_outputs!();
}
