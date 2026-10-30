//! Cancellation of any selected mutation retains its explicit terminal component.

/// Checks cancellation and bounded retries for each execution mode.
macro_rules! terminal_frontier {
    () => {
        use futures_executor::block_on;
        use futures_util::FutureExt;

        /// Selects three routes that all create a held mutation boundary.
        #[derive(Clone, Copy, Debug)]
        enum Producer {
            Effect,
            Send,
            Update,
        }

        /// Starts pending work or applies its observable state change.
        #[derive(Clone, Copy, Debug)]
        enum Action {
            Start(Producer),
            Apply,
        }

        /// Produces a single mutation so cancellation cannot hide behind later work.
        struct Root;

        impl Feature for Root {
            type State = usize;
            type Action = Action;

            fn update(
                &mut self,
                state: &mut usize,
                action: &Action,
                context: &mut FeatureContext<'_, usize, Action>,
            ) {
                match action {
                    Action::Start(Producer::Effect) => context.effect(Effect::send(Action::Apply)),
                    Action::Start(Producer::Send) => {
                        context.task(Task::run(|context| async move {
                            let _ = context.send(Action::Apply).await;
                        }));
                    }
                    Action::Start(Producer::Update) => {
                        context.task(Task::run(|context| async move {
                            let _ = context.update(|state| *state += 1).await;
                        }));
                    }
                    Action::Apply => *state += 1,
                }
            }
        }

        /// Separates cancelling execution from acknowledging its terminal result.
        fn cancellation_keeps_selected_boundary(producer: Producer) {
            block_on(async {
                let mut store = TestStore::builder(0, Root).build();
                let task = store.send(Action::Start(producer)).await.unwrap();
                let pending = store.finish().await.unwrap_err();
                assert!(pending.finish_diagnostics().unwrap().has_pending_action());
                let boundary = store.pending_boundary().unwrap();

                task.cancel().await.unwrap();
                task.finish().await.unwrap();
                store
                    .assert_state(|state| assert_eq!(*state, 0))
                    .await
                    .unwrap();
                assert!(
                    store.finish().await.unwrap_err().is_incomplete_assertion(),
                    "cancellation cannot acknowledge an already-selected {producer:?} boundary"
                );
                store
                    .receive_terminal(boundary.clone(), |error| {
                        error.reason() == rca_core::TaskAccessReason::Cancelled
                            && error.progress() == rca_core::TaskProgress::NotStarted
                    })
                    .await
                    .unwrap();
                assert!(
                    store
                        .receive_terminal(boundary, |_| panic!("stale token entered matcher"))
                        .await
                        .unwrap_err()
                        .is_no_received_action()
                );
                store
                    .send_with(Action::Apply, |state| *state = 1)
                    .await
                    .unwrap()
                    .finish()
                    .await
                    .unwrap();
                store.finish().await.unwrap();
            });
        }

        #[test]
        fn cancelled_effect_action_keeps_terminal_assertion_debt() {
            cancellation_keeps_selected_boundary(Producer::Effect);
        }

        #[test]
        fn cancelled_typed_send_keeps_terminal_assertion_debt() {
            cancellation_keeps_selected_boundary(Producer::Send);
        }

        #[test]
        fn cancelled_typed_update_keeps_terminal_assertion_debt() {
            cancellation_keeps_selected_boundary(Producer::Update);
        }

        #[test]
        fn terminal_tokens_reject_foreign_roots_and_unterminated_boundaries_without_callbacks() {
            block_on(async {
                let mut store = TestStore::builder(0, Root).build();
                let task = store.send(Action::Start(Producer::Update)).await.unwrap();
                let boundary = store.pending_boundary().unwrap();
                assert!(
                    store
                        .receive_terminal(boundary.clone(), |_| panic!(
                            "live request is not terminal"
                        ))
                        .await
                        .unwrap_err()
                        .is_no_received_action()
                );
                let mut other = TestStore::builder(0, Root).build();
                let other_task = other.send(Action::Start(Producer::Send)).await.unwrap();
                let foreign = other.pending_boundary().unwrap();
                task.cancel().await.unwrap();
                task.finish().await.unwrap();
                assert!(
                    store
                        .receive_terminal(foreign.clone(), |_| panic!(
                            "foreign token entered matcher"
                        ))
                        .await
                        .unwrap_err()
                        .is_no_received_action()
                );
                store
                    .receive_terminal(boundary.clone(), |_| true)
                    .await
                    .unwrap();
                let adjacent_task = store.send(Action::Start(Producer::Update)).await.unwrap();
                adjacent_task.cancel().await.unwrap();
                adjacent_task.finish().await.unwrap();
                let adjacent = store.pending_boundary().unwrap();
                assert!(
                    store
                        .receive_terminal(boundary, |_| panic!(
                            "stale token selected adjacent terminal"
                        ))
                        .await
                        .unwrap_err()
                        .is_no_received_action()
                );
                store
                    .receive_terminal(adjacent, |error| {
                        error.reason() == rca_core::TaskAccessReason::Cancelled
                            && error.progress() == rca_core::TaskProgress::NotStarted
                    })
                    .await
                    .unwrap();
                store.finish().await.unwrap();
                other_task.cancel().await.unwrap();
                other_task.finish().await.unwrap();
                other.receive_terminal(foreign, |_| true).await.unwrap();
                other.finish().await.unwrap();
            });
        }

        #[test]
        fn failed_terminal_comparison_is_sticky_through_correct_acknowledgement() {
            for panics in [false, true] {
                block_on(async {
                    let mut store = TestStore::builder(0, Root).build();
                    let task = store.send(Action::Start(Producer::Update)).await.unwrap();
                    task.cancel().await.unwrap();
                    task.finish().await.unwrap();
                    let boundary = store.pending_boundary().unwrap();
                    let result = std::panic::AssertUnwindSafe(store.receive_terminal(
                        boundary.clone(),
                        |_| {
                            assert!(!panics, "terminal matcher panic");
                            false
                        },
                    ))
                    .catch_unwind()
                    .await;
                    if panics {
                        let panic = result.unwrap_err();
                        assert_eq!(panic.downcast_ref::<&str>(), Some(&"terminal matcher panic"));
                    } else {
                        assert!(result.unwrap().unwrap_err().is_action_mismatch());
                    }
                    assert!(
                        store
                            .receive_terminal(boundary.clone(), |_| true)
                            .await
                            .unwrap_err()
                            .is_incomplete_assertion()
                    );
                    assert!(
                        store
                            .receive_terminal(boundary, |_| panic!(
                                "acknowledged terminal entered matcher"
                            ))
                            .await
                            .unwrap_err()
                            .is_no_received_action()
                    );
                    store
                        .assert_state(|state| assert_eq!(*state, 0))
                        .await
                        .unwrap();
                    assert!(
                        store
                            .send(Action::Apply)
                            .await
                            .unwrap_err()
                            .is_incomplete_assertion()
                    );
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
                });
            }
        }

        /// Exposes exactly eight polls after an explicit release, exceeding a seven-poll budget.
        struct GatedWork {
            gate: Option<rca_test::ControlledGate>,
            polls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        }

        impl Feature for GatedWork {
            type State = Option<usize>;
            type Action = ();

            fn update(
                &mut self,
                _: &mut Option<usize>,
                (): &(),
                context: &mut FeatureContext<'_, Option<usize>, ()>,
            ) {
                let gate = self.gate.take().expect("one gated effect");
                let polls = std::sync::Arc::clone(&self.polls);
                context.effect(Effect::run(move |_| async move {
                    gate.await.unwrap();
                    std::future::poll_fn(move |context| {
                        if polls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1 == 8 {
                            std::task::Poll::Ready(())
                        } else {
                            context.waker().wake_by_ref();
                            std::task::Poll::Pending
                        }
                    })
                    .await;
                }));
            }
        }

        /// Supplies a mounted value without adding reducer work to budget measurements.
        struct TerminalChild;
        impl Feature for TerminalChild {
            type State = usize;
            type Action = ();

            fn update(&mut self, _: &mut usize, (): &(), _: &mut FeatureContext<'_, usize, ()>) {}
        }

        /// Checks whether exhausting the poll budget happened before or after admission.
        #[expect(
            clippy::too_many_lines,
            reason = "Keeps admission timing, finite work, retry and sticky teardown debt in one causal sequence"
        )]
        fn update_receive_budget(before_expectation: bool) {
            use std::sync::atomic::{AtomicUsize, Ordering};

            block_on(async {
                let (background_control, background_gate) = rca_test::controlled_gate();
                let (update_control, update_gate) = rca_test::controlled_gate();
                let polls = std::sync::Arc::new(AtomicUsize::new(0));
                let mut update_gate = Some(update_gate);
                let mount = OptionalMount::new(
                    Option::as_ref,
                    Option::as_mut,
                    |state: &Option<usize>| {
                        state.map(|_| rca_core::MountIdentity {
                            key: (),
                            replacement: 0,
                            configuration: 0,
                        })
                    },
                    |_| TerminalChild,
                )
                .on_initial(
                    |_| (),
                    move |_, _| {
                        let gate = update_gate.take().expect("one initial update producer");
                        Task::run(move |context| async move {
                            gate.await.unwrap();
                            let _ = context.update(|value| *value += 1).await;
                        })
                    },
                );
                let mut store = TestStore::builder(
                    Some(0),
                    GatedWork {
                        gate: Some(background_gate),
                        polls: std::sync::Arc::clone(&polls),
                    },
                )
                .with_step_budget(7)
                .mount(mount)
                .build();
                let initial = store.receive_structural(|_| {}).await.unwrap();
                let background = store.send(()).await.unwrap();
                update_control.wait_until_waiting().await.unwrap();
                background_control.wait_until_waiting().await.unwrap();
                update_control.release().unwrap();
                assert!(
                    store
                        .finish()
                        .await
                        .unwrap_err()
                        .finish_diagnostics()
                        .unwrap()
                        .has_pending_action()
                );
                assert_eq!(polls.load(Ordering::SeqCst), 0);
                let mut release = Some(background_control);
                if before_expectation {
                    release.take().unwrap().release().unwrap();
                }
                let mut updates = 0;
                let error = store
                    .receive_update(|state| {
                        updates += 1;
                        *state = Some(1);
                        if let Some(control) = release.take() {
                            control.release().unwrap();
                        }
                    })
                    .await
                    .expect_err("finite ready work exceeds the receive budget");
                let drive = error.drive_error().expect("typed driving cause is retained");
                assert!(drive.is_step_budget_exhausted());
                assert_eq!((drive.polls(), drive.step_budget()), (7, 7));
                assert_eq!(updates, usize::from(!before_expectation));
                assert_eq!(polls.load(Ordering::SeqCst), if before_expectation { 7 } else { 6 });

                if before_expectation {
                    assert!(store.pending_boundary().is_some());
                    let received = store.receive_update(|state| *state = Some(1)).await.unwrap();
                    assert_eq!(polls.load(Ordering::SeqCst), 8);
                    received.finish().await.unwrap();
                    initial.finish().await.unwrap();
                    background.finish().await.unwrap();
                    store.finish().await.unwrap();
                } else {
                    assert!(
                        store
                            .receive_update(|_| panic!("admitted assertion remains debt"))
                            .await
                            .unwrap_err()
                            .is_incomplete_assertion()
                    );
                    assert!(store.finish().await.unwrap_err().is_incomplete_assertion());
                    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        drop(store);
                    }))
                    .expect_err("post-release driving failure retains assertion debt");
                    let message = panic.downcast_ref::<String>().unwrap();
                    assert!(
                        message.starts_with(
                            "TestStore teardown failed: TestStore incomplete assertion for receive_update at "
                        ),
                        "{message}"
                    );
                    assert!(initial.is_finished());
                    assert!(background.is_finished());
                }
            });
        }

        #[test]
        fn update_receive_pre_settle_budget_preserves_the_frontier() {
            update_receive_budget(true);
        }

        #[test]
        fn update_receive_post_release_budget_retains_assertion_debt() {
            update_receive_budget(false);
        }

        #[expect(
            clippy::too_many_lines,
            reason = "One finite schedule distinguishes pre-match preservation from post-acknowledgement recovery"
        )]
        /// Separates terminal matching from the work needed to settle its aftermath.
        fn terminal_settle_budget(output_terminal: bool, before_matcher: bool) {
            use std::sync::Arc;
            use std::sync::atomic::{AtomicUsize, Ordering};

            block_on(async {
                let (background_control, background_gate) = rca_test::controlled_gate();
                let (terminal_control, terminal_gate) = rca_test::controlled_gate();
                let polls = Arc::new(AtomicUsize::new(0));
                let calls = AtomicUsize::new(0);
                let output_cell = OutputOwner::new(OutputCell::<Output<()>>::new());
                let initial_output = OutputOwner::clone(&output_cell);
                let mut terminal_gate = Some(terminal_gate);
                let mount = OptionalMount::new(
                    Option::as_ref,
                    Option::as_mut,
                    |state: &Option<usize>| {
                        state.map(|_| rca_core::MountIdentity {
                            key: (),
                            replacement: 0,
                            configuration: 0,
                        })
                    },
                    |_| TerminalChild,
                )
                .on_initial(
                    |_| (),
                    move |_, _| {
                        let gate = terminal_gate.take().expect("one initial terminal producer");
                        let output = initial_output.get().unwrap().clone();
                        Task::run(move |context| async move {
                            gate.await.unwrap();
                            if output_terminal {
                                let _ = context.output(&output, ()).await;
                            } else {
                                let _ = context.update(|value| *value += 1).await;
                            }
                        })
                    },
                );
                let (builder, site) = TestStore::builder(
                    Some(0),
                    GatedWork {
                        gate: Some(background_gate),
                        polls: Arc::clone(&polls),
                    },
                )
                .with_step_budget(7)
                .mount_with_site(mount);
                let (builder, output) = builder.output::<()>(&site, |handlers| {
                    handlers.at_root(|state, (), _| {
                        *state.as_mut().unwrap() += 1;
                        rca_core::OutputFlow::Consumed
                    });
                });
                output_cell.set(output.clone()).unwrap();
                let mut store = builder.build();
                let initial = store.receive_structural(|_| {}).await.unwrap();
                let background = store.send(()).await.unwrap();
                terminal_control.wait_until_waiting().await.unwrap();
                background_control.wait_until_waiting().await.unwrap();
                terminal_control.release().unwrap();
                assert!(
                    store
                        .finish()
                        .await
                        .unwrap_err()
                        .finish_diagnostics()
                        .unwrap()
                        .has_pending_action()
                );
                let boundary = store.pending_boundary().unwrap();
                initial.cancel().await.unwrap();
                initial.finish().await.unwrap();
                let mut release = Some(background_control);
                if before_matcher {
                    release.take().unwrap().release().unwrap();
                }
                let matcher = |error: &rca_core::TaskAccessError| {
                    calls.fetch_add(1, Ordering::SeqCst);
                    if let Some(control) = release.take() {
                        control.release().unwrap();
                    }
                    error.reason() == rca_core::TaskAccessReason::Cancelled
                        && error.progress() == rca_core::TaskProgress::NotStarted
                };
                let error = if output_terminal {
                    store.receive_output_terminal(&output, matcher).await.unwrap_err()
                } else {
                    store.receive_terminal(boundary.clone(), matcher).await.unwrap_err()
                };
                let drive = error.drive_error().expect("settling must retain its failure");
                assert!(drive.is_step_budget_exhausted());
                assert_eq!((drive.polls(), drive.step_budget()), (7, 7));
                assert!((1..8).contains(&polls.load(Ordering::SeqCst)));

                if before_matcher {
                    assert_eq!(calls.load(Ordering::SeqCst), 0);
                    assert!(store.pending_boundary().is_some());
                    let matcher = |error: &rca_core::TaskAccessError| {
                        calls.fetch_add(1, Ordering::SeqCst);
                        error.reason() == rca_core::TaskAccessReason::Cancelled
                            && error.progress() == rca_core::TaskProgress::NotStarted
                    };
                    if output_terminal {
                        store.receive_output_terminal(&output, matcher).await.unwrap();
                    } else {
                        store.receive_terminal(boundary.clone(), matcher).await.unwrap();
                    }
                } else {
                    assert_eq!(calls.load(Ordering::SeqCst), 1);
                    assert!(store.pending_boundary().is_none());
                }
                let stale = if output_terminal {
                    store.receive_output_terminal(&output, |_| panic!("acknowledged terminal reappeared"))
                        .await.unwrap_err()
                } else {
                    store.receive_terminal(boundary, |_| panic!("acknowledged terminal reappeared"))
                        .await.unwrap_err()
                };
                assert!(stale.is_no_received_action());
                assert_eq!(calls.load(Ordering::SeqCst), 1);
                assert_eq!(polls.load(Ordering::SeqCst), 8);
                store.assert_state(|state| assert_eq!(*state, Some(0))).await.unwrap();
                background.finish().await.unwrap();
                store.finish().await.unwrap();
            });
        }

        #[test]
        fn terminal_pre_settle_failure_preserves_boundary_and_matcher() {
            terminal_settle_budget(false, true);
        }

        #[test]
        fn terminal_post_matcher_budget_preserves_acknowledgement() {
            terminal_settle_budget(false, false);
        }

        #[test]
        fn output_terminal_pre_settle_failure_preserves_declaration_and_matcher() {
            terminal_settle_budget(true, true);
        }

        #[test]
        fn output_terminal_post_matcher_budget_preserves_acknowledgement() {
            terminal_settle_budget(true, false);
        }
    };
}

mod shared {
    use rca_core::{Effect, Feature, FeatureContext, OptionalMount, Output, Task};
    use rca_test::TestStore;
    use std::sync::{Arc as OutputOwner, OnceLock as OutputCell};
    terminal_frontier!();
}

#[cfg(feature = "local")]
mod local {
    use rca_core::local::{Effect, Feature, FeatureContext, OptionalMount, Output, Task};
    use rca_test::local::TestStore;
    use std::cell::OnceCell as OutputCell;
    use std::rc::Rc as OutputOwner;
    terminal_frontier!();
}
