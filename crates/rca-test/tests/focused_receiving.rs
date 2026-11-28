//! Focused receiving executes omitted work without weakening explicit assertions.

/// Runs identical behavioural controls against portable and owner-local stores.
macro_rules! focused_controls {
    () => {
        use futures_executor::block_on;
        use futures_util::FutureExt;
        use rca_core::IssueSeverity;
        use rca_test::OmissionReporting;
        use std::panic::{AssertUnwindSafe, catch_unwind};
        use std::sync::{
            Arc,
            atomic::{AtomicBool, AtomicUsize, Ordering},
        };

        /// Names the pipeline's input, intermediate result and final result.
        #[derive(Clone, Copy, Debug, PartialEq)]
        enum Action {
            Start,
            Intermediate,
            Done,
            Again,
        }

        /// Requires both a typed update and an intermediate reduction before completion.
        struct Pipeline;

        /// Starts one real asynchronous wait and records its eventual response.
        struct Deferred(Option<core::Effect<Action>>);

        impl core::Feature for Deferred {
            type State = Vec<u8>;
            type Action = Action;

            fn update(
                &mut self,
                state: &mut Vec<u8>,
                action: &Action,
                context: &mut core::FeatureContext<'_, Vec<u8>, Action>,
            ) {
                if *action == Action::Start {
                    context.effect(self.0.take().expect("the fixture starts once"));
                } else {
                    assert_eq!(*action, Action::Done);
                    state.push(3);
                }
            }
        }

        /// Starts one gated effect beside one selected causal action.
        struct PostReceipt {
            effect: Option<core::Effect<Action>>,
            commits: Arc<AtomicUsize>,
        }

        impl core::Feature for PostReceipt {
            type State = Vec<u8>;
            type Action = Action;

            fn update(
                &mut self,
                state: &mut Vec<u8>,
                action: &Action,
                context: &mut core::FeatureContext<'_, Vec<u8>, Action>,
            ) {
                match action {
                    Action::Start => {
                        context.effect(self.effect.take().expect("one controlled effect"));
                        context.task(core::Task::run(|context| async move {
                            let _ = context.send(Action::Intermediate).await.unwrap();
                        }));
                    }
                    Action::Intermediate => {
                        self.commits.fetch_add(1, Ordering::SeqCst);
                        state.push(2);
                    }
                    _ => panic!("the fixture sends only one intermediate action"),
                }
            }
        }

        /// Captures an independently checked count alongside the root snapshot.
        struct Count;

        impl CaptureKey for Count {
            type Value = usize;
            type Error = ();
        }

        /// Exercises expectation validation without registering a second capture.
        struct UndeclaredCount;

        impl CaptureKey for UndeclaredCount {
            type Value = usize;
            type Error = ();
        }

        impl core::Feature for Pipeline {
            type State = Vec<u8>;
            type Action = Action;

            fn update(
                &mut self,
                state: &mut Vec<u8>,
                action: &Action,
                context: &mut core::FeatureContext<'_, Vec<u8>, Action>,
            ) {
                match action {
                    Action::Start => {
                        context.task(core::Task::<Vec<u8>, Action>::run(|context| async move {
                            context.update(|state| state.push(1)).await.unwrap();
                            let _ = context.send(Action::Intermediate).await.unwrap();
                            let _ = context.send(Action::Done).await.unwrap();
                        }))
                    }
                    Action::Intermediate => state.push(2),
                    Action::Done => state.push(3),
                    Action::Again => {
                        context.task(core::Task::<Vec<u8>, Action>::run(|context| async move {
                            let _ = context.send(Action::Again).await.unwrap();
                        }))
                    }
                }
            }
        }

        #[test]
        fn focused_receive_executes_intermediate_work_and_calls_matcher_once_per_action() {
            block_on(async {
                let mut store = TestStore::builder(Vec::new(), Pipeline).build();
                let task = store.send(Action::Start).await.unwrap();
                let mut seen = Vec::new();
                let _ = store
                    .receive_until(
                        |action| {
                            seen.push(*action);
                            *action == Action::Done
                        },
                        OmissionReporting::Warn,
                        |expected| {
                            assert_eq!(expected, &[1, 2]);
                            expected.push(3);
                        },
                    )
                    .await
                    .unwrap();
                assert_eq!(seen, [Action::Intermediate, Action::Done]);
                store.assert_state_eq(vec![1, 2, 3]).await.unwrap();
                let warnings = store.take_omissions();
                assert_eq!(warnings.len(), 2);
                assert!(
                    warnings
                        .iter()
                        .all(|issue| issue.severity == IssueSeverity::Warning)
                );
                assert!(warnings[0].message.contains("Update"));
                assert!(warnings[1].message.contains("Action"));
                assert!(store.take_omissions().is_empty());
                task.finish().await.unwrap();
                store.finish().await.unwrap();
            });
        }

        #[test]
        fn explicit_wrong_expectation_retains_debt_after_silent_omissions() {
            block_on(async {
                let mut store = TestStore::builder(Vec::new(), Pipeline).build();
                let _ = store.send(Action::Start).await.unwrap();
                let panic = AssertUnwindSafe(store.receive_until(
                    |action| *action == Action::Done,
                    OmissionReporting::Silent,
                    |expected| expected.push(9),
                ))
                .catch_unwind()
                .await
                .expect_err("the selected expectation remains strict");
                assert!(
                    panic
                        .downcast_ref::<String>()
                        .unwrap()
                        .contains("TestStore state mismatch")
                );
                assert!(store.take_omissions().is_empty());
                assert!(store.finish().await.unwrap_err().is_incomplete_assertion());
                assert!(
                    store
                        .receive_omitting(OmissionReporting::Silent)
                        .await
                        .unwrap_err()
                        .is_incomplete_assertion()
                );
                assert!(catch_unwind(AssertUnwindSafe(|| drop(store))).is_err());
            });
        }

        #[test]
        fn one_omission_does_not_finish_the_remaining_pipeline() {
            block_on(async {
                let mut store = TestStore::builder(Vec::new(), Pipeline).build();
                let task = store.send(Action::Start).await.unwrap();
                let _ = store
                    .receive_omitting(OmissionReporting::Silent)
                    .await
                    .unwrap();
                store.assert_state_eq(vec![1]).await.unwrap();
                assert!(
                    store
                        .finish()
                        .await
                        .unwrap_err()
                        .finish_diagnostics()
                        .unwrap()
                        .has_pending_action()
                );
                let _ = store
                    .receive_with(
                        |action| *action == Action::Intermediate,
                        |state| state.push(2),
                    )
                    .await
                    .unwrap();
                let _ = store
                    .receive_with(|action| *action == Action::Done, |state| state.push(3))
                    .await
                    .unwrap();
                task.finish().await.unwrap();
                store.finish().await.unwrap();
            });
        }

        #[test]
        fn skipping_a_request_preserves_its_immediately_queued_successor() {
            /// Continues its causal sequence after the first request is explicitly skipped.
            struct ContinueAfterSkip;

            impl core::Feature for ContinueAfterSkip {
                type State = Vec<u8>;
                type Action = Action;

                fn update(
                    &mut self,
                    state: &mut Vec<u8>,
                    action: &Action,
                    context: &mut core::FeatureContext<'_, Vec<u8>, Action>,
                ) {
                    match action {
                        Action::Start => {
                            context.task(core::Task::run(|context| async move {
                                let error = context.send(Action::Intermediate).await.unwrap_err();
                                let rca_core::TaskSendError::Rejected { action, error } = error
                                else {
                                    panic!("the skipped request must not enter reduction");
                                };
                                assert_eq!(action, Action::Intermediate);
                                assert_eq!(error.reason(), rca_core::TaskAccessReason::Cancelled);
                                assert_eq!(error.progress(), rca_core::TaskProgress::NotStarted);
                                let _ = context.send(Action::Done).await.unwrap();
                            }));
                        }
                        Action::Intermediate => state.push(2),
                        Action::Done => state.push(3),
                        Action::Again => panic!("the fixture sends each request once"),
                    }
                }
            }

            block_on(async {
                let mut store = TestStore::builder(Vec::new(), ContinueAfterSkip)
                    .with_exhaustivity(rca_test::Exhaustivity::NonExhaustive)
                    .build();
                let started = store.send(Action::Start).await.unwrap();

                let skipped = store.skip_received().await;

                store.assert_state_eq(Vec::new()).await.unwrap();
                assert!(store.pending_boundary().is_some());
                assert!(!started.is_finished());
                let _ = store
                    .receive_with(
                        |action| *action == Action::Done,
                        |expected| expected.push(3),
                    )
                    .await
                    .unwrap();
                store.assert_state_eq(vec![3]).await.unwrap();
                assert!(store.take_omissions().is_empty());
                started.finish().await.unwrap();
                store.finish().await.unwrap();

                skipped.expect("a new pending successor is not debt from the skipped request");
            });
        }

        #[test]
        fn endless_unmatched_actions_share_one_finite_budget() {
            block_on(async {
                let mut store = TestStore::builder(Vec::new(), Pipeline)
                    .with_step_budget(100)
                    .build();
                let _ = store.send(Action::Again).await.unwrap();
                let error = store
                    .receive_until(|_| false, OmissionReporting::Silent, |_| {})
                    .await
                    .unwrap_err();
                assert!(error.drive_error().unwrap().is_step_budget_exhausted());
                assert_eq!(error.drive_error().unwrap().step_budget(), 100);
                // Dropping the store still reports the unfinished pipeline after bounded cleanup.
                assert!(catch_unwind(AssertUnwindSafe(|| drop(store))).is_err());
            });
        }

        #[test]
        fn idle_selection_is_ready_only_and_unpolled_omission_is_inert() {
            block_on(async {
                let mut store = TestStore::builder(Vec::new(), Pipeline).build();
                let task = store.send(Action::Start).await.unwrap();
                drop(store.receive_omitting(OmissionReporting::Warn));
                store.assert_state_eq(vec![]).await.unwrap();
                assert!(store.take_omissions().is_empty());
                let _ = store
                    .receive_until(
                        |action| *action == Action::Done,
                        OmissionReporting::Silent,
                        |state| state.push(3),
                    )
                    .await
                    .unwrap();
                task.finish().await.unwrap();
                assert!(
                    store
                        .receive_until(|_| true, OmissionReporting::Warn, |_| {})
                        .await
                        .unwrap_err()
                        .is_no_received_action()
                );
                store.finish().await.unwrap();
            });
        }

        #[test]
        fn selection_does_not_advance_a_real_pending_timer() {
            block_on(async {
                let duration = std::time::Duration::from_secs(1);
                let mut store = TestStore::builder(
                    Vec::new(),
                    Deferred(Some(core::Effect::send_after(duration, Action::Done))),
                )
                .build();
                let started = store.send(Action::Start).await.unwrap();
                let before = store.now();
                assert!(
                    store
                        .receive_until(
                            |_| panic!("the timer has not emitted an action"),
                            OmissionReporting::Warn,
                            |_| panic!("there is no selected action"),
                        )
                        .await
                        .unwrap_err()
                        .is_no_received_action()
                );
                assert_eq!(store.now(), before);
                assert!(!started.is_finished());
                assert!(store.take_omissions().is_empty());
                store.advance(duration).await.unwrap();
                let _ = store
                    .receive_until(
                        |action| *action == Action::Done,
                        OmissionReporting::Silent,
                        |state| state.push(3),
                    )
                    .await
                    .unwrap();
                started.finish().await.unwrap();
                store.finish().await.unwrap();
            });
        }

        #[test]
        fn selection_does_not_wait_for_an_external_completion() {
            block_on(async {
                let (completion, response) = rca_test::controlled_future();
                let effect = core::Effect::future(async move {
                    response.await.unwrap();
                    Some(Action::Done)
                });
                let mut store = TestStore::builder(Vec::new(), Deferred(Some(effect))).build();
                let started = store.send(Action::Start).await.unwrap();
                assert!(
                    store
                        .receive_omitting(OmissionReporting::Warn)
                        .await
                        .unwrap_err()
                        .is_no_received_action()
                );
                assert!(!started.is_finished());
                assert!(store.take_omissions().is_empty());
                completion.complete(()).unwrap();
                let _ = store
                    .receive_until(
                        |action| *action == Action::Done,
                        OmissionReporting::Silent,
                        |state| state.push(3),
                    )
                    .await
                    .unwrap();
                started.finish().await.unwrap();
                store.finish().await.unwrap();
            });
        }

        #[test]
        fn ready_effect_budget_failure_does_not_create_an_omission() {
            block_on(async {
                let (completion, response) = rca_test::controlled_future();
                let polls = Arc::new(AtomicUsize::new(0));
                let observed = Arc::clone(&polls);
                let effect = core::Effect::future(async move {
                    response.await.unwrap();
                    futures_util::future::poll_fn(move |cx| {
                        let count = observed.fetch_add(1, Ordering::Relaxed) + 1;
                        if count <= 8 {
                            cx.waker().wake_by_ref();
                            std::task::Poll::Pending
                        } else {
                            std::task::Poll::Ready(None::<Action>)
                        }
                    })
                    .await
                });
                let mut store = TestStore::builder(Vec::new(), Deferred(Some(effect)))
                    .with_step_budget(7)
                    .build();
                let started = store.send(Action::Start).await.unwrap();
                completion.complete(()).unwrap();

                let error = store
                    .receive_omitting(OmissionReporting::Warn)
                    .await
                    .unwrap_err();
                assert!(error.drive_error().unwrap().is_step_budget_exhausted());
                assert_eq!(error.drive_error().unwrap().step_budget(), 7);
                assert!(store.take_omissions().is_empty());
                assert!(!started.is_finished());

                store.assert_state_eq(Vec::new()).await.unwrap();
                started.finish().await.unwrap();
                assert_eq!(polls.load(Ordering::Relaxed), 9);
                store.finish().await.unwrap();
            });
        }

        #[test]
        fn ready_work_uses_the_selected_receipts_remaining_budget() {
            block_on(async {
                let (release, response) = rca_test::controlled_future();
                let (finish, finished) = rca_test::controlled_future();
                let polls = Arc::new(AtomicUsize::new(0));
                let observed = Arc::clone(&polls);
                let effect = core::Effect::future(async move {
                    response.await.unwrap();
                    futures_util::future::poll_fn(move |context| {
                        if observed.fetch_add(1, Ordering::Relaxed) + 1 == 7 {
                            std::task::Poll::Ready(())
                        } else {
                            context.waker().wake_by_ref();
                            std::task::Poll::Pending
                        }
                    })
                    .await;
                    finished.await.unwrap();
                    None::<Action>
                });
                let commits = Arc::new(AtomicUsize::new(0));
                let mut store = TestStore::builder(
                    Vec::new(),
                    PostReceipt {
                        effect: Some(effect),
                        commits: Arc::clone(&commits),
                    },
                )
                .with_step_budget(7)
                .build();
                let started = store.send(Action::Start).await.unwrap();
                release.complete(()).unwrap();

                let mut prepared = false;
                let error = store
                    .receive_until(
                        |action| *action == Action::Intermediate,
                        OmissionReporting::Silent,
                        |expected| {
                            prepared = true;
                            expected.push(2);
                        },
                    )
                    .await
                    .unwrap_err();
                let drive = error.drive_error().expect("retain the shared poll budget");
                assert!(drive.is_step_budget_exhausted());
                assert_eq!((drive.polls(), drive.step_budget()), (7, 7));
                assert!(
                    prepared,
                    "ready work finished before preparing the assertion"
                );
                assert_eq!(polls.load(Ordering::Relaxed), 7);
                assert_eq!(commits.load(Ordering::SeqCst), 0);
                assert!(store.pending_boundary().is_some());
                assert!(store.take_omissions().is_empty());

                finish.complete(()).unwrap();
                started.finish().await.unwrap();
                assert_eq!(commits.load(Ordering::SeqCst), 1);
                store.assert_state_eq(vec![2]).await.unwrap();
                assert!(store.finish().await.unwrap_err().is_incomplete_assertion());
                assert!(catch_unwind(AssertUnwindSafe(|| drop(store))).is_err());
            });
        }

        #[test]
        fn post_receipt_effect_budget_retains_the_selected_assertion() {
            block_on(async {
                let (release, response) = rca_test::controlled_future();
                let polls = Arc::new(AtomicUsize::new(0));
                let observed = Arc::clone(&polls);
                let effect = core::Effect::future(async move {
                    response.await.unwrap();
                    futures_util::future::poll_fn(move |cx| {
                        let count = observed.fetch_add(1, Ordering::Relaxed) + 1;
                        if count <= 8 {
                            cx.waker().wake_by_ref();
                            std::task::Poll::Pending
                        } else {
                            std::task::Poll::Ready(None::<Action>)
                        }
                    })
                    .await
                });
                let commits = Arc::new(AtomicUsize::new(0));
                let mut store = TestStore::builder(
                    Vec::new(),
                    PostReceipt {
                        effect: Some(effect),
                        commits: Arc::clone(&commits),
                    },
                )
                .with_step_budget(7)
                .build();
                let started = store.send(Action::Start).await.unwrap();

                let error = store
                    .receive_until(
                        |action| *action == Action::Intermediate,
                        OmissionReporting::Silent,
                        |expected| {
                            release.complete(()).unwrap();
                            expected.push(2);
                        },
                    )
                    .await
                    .unwrap_err();
                let drive = error
                    .drive_error()
                    .expect("ready effect exhausts the budget");
                assert!(drive.is_step_budget_exhausted());
                assert_eq!((drive.polls(), drive.step_budget()), (7, 7));
                assert_eq!(commits.load(Ordering::SeqCst), 1);
                assert!(store.take_omissions().is_empty());

                // This finite effect can finish after the failed focused assertion.
                started.finish().await.unwrap();
                assert_eq!(polls.load(Ordering::Relaxed), 9);
                assert!(store.finish().await.unwrap_err().is_incomplete_assertion());
                assert!(catch_unwind(AssertUnwindSafe(|| drop(store))).is_err());
            });
        }

        #[test]
        fn verified_baseline_drop_wakes_only_the_final_budgeted_drain() {
            #[derive(Clone, Debug)]
            struct Snapshot {
                value: usize,
                armed: Arc<AtomicBool>,
                release: Arc<std::sync::Mutex<Option<rca_test::Completion<()>>>>,
                wakes: Arc<AtomicUsize>,
            }

            impl PartialEq for Snapshot {
                fn eq(&self, other: &Self) -> bool {
                    self.value == other.value
                }
            }

            impl Drop for Snapshot {
                fn drop(&mut self) {
                    if self.value == 0 && self.armed.swap(false, Ordering::SeqCst) {
                        self.wakes.fetch_add(1, Ordering::SeqCst);
                        let completion = self
                            .release
                            .lock()
                            .unwrap()
                            .take()
                            .expect("old baseline releases the effect once");
                        completion.complete(()).unwrap();
                    }
                }
            }

            block_on(async {
                let (completion, response) = rca_test::controlled_future();
                let release = Arc::new(std::sync::Mutex::new(Some(completion)));
                let armed = Arc::new(AtomicBool::new(false));
                let wakes = Arc::new(AtomicUsize::new(0));
                let polls = Arc::new(AtomicUsize::new(0));
                let observed = Arc::clone(&polls);
                let effect = core::Effect::future(async move {
                    response.await.unwrap();
                    futures_util::future::poll_fn(move |cx| {
                        let count = observed.fetch_add(1, Ordering::Relaxed) + 1;
                        if count <= 8 {
                            cx.waker().wake_by_ref();
                            std::task::Poll::Pending
                        } else {
                            std::task::Poll::Ready(None::<Action>)
                        }
                    })
                    .await
                });
                let commits = Arc::new(AtomicUsize::new(0));
                let mut store = TestStore::projected_builder(
                    Vec::new(),
                    PostReceipt {
                        effect: Some(effect),
                        commits: Arc::clone(&commits),
                    },
                    {
                        let release = Arc::clone(&release);
                        let armed = Arc::clone(&armed);
                        let wakes = Arc::clone(&wakes);
                        move |state| Snapshot {
                            value: state.len(),
                            armed: Arc::clone(&armed),
                            release: Arc::clone(&release),
                            wakes: Arc::clone(&wakes),
                        }
                    },
                )
                .with_step_budget(7)
                .build();
                let started = store.send(Action::Start).await.unwrap();
                assert_eq!(wakes.load(Ordering::SeqCst), 0);
                armed.store(true, Ordering::SeqCst);

                let error = store
                    .receive_until(
                        |action| *action == Action::Intermediate,
                        OmissionReporting::Silent,
                        |expected| expected.value = 1,
                    )
                    .await
                    .unwrap_err();
                let drive = error
                    .drive_error()
                    .expect("final drain exhausts the budget");
                assert!(drive.is_step_budget_exhausted());
                assert_eq!((drive.polls(), drive.step_budget()), (7, 7));
                assert_eq!(wakes.load(Ordering::SeqCst), 1);
                assert_eq!(commits.load(Ordering::SeqCst), 1);
                assert!(store.take_omissions().is_empty());

                // The same finite effect can require another ready-only budget window.
                let cleanup = started.finish().await.unwrap_err();
                let drive = cleanup
                    .drive_error()
                    .expect("unfinished causal work exhausts the cleanup budget");
                assert!(drive.is_step_budget_exhausted());
                assert_eq!((drive.polls(), drive.step_budget()), (7, 7));
                assert!(!started.is_finished());
                started.finish().await.unwrap();
                assert_eq!(polls.load(Ordering::Relaxed), 9);
                store.finish().await.unwrap();
            });
        }

        #[test]
        fn preparation_cancellation_retires_the_selected_action_before_release() {
            block_on(async {
                let commits = Arc::new(AtomicUsize::new(0));
                let mut store = TestStore::builder(
                    Vec::new(),
                    PostReceipt {
                        effect: Some(core::Effect::none()),
                        commits: Arc::clone(&commits),
                    },
                )
                .build();
                let started = store.send(Action::Start).await.unwrap();
                let cancellation = started.clone();
                let prepared = Arc::new(AtomicUsize::new(0));
                let entered = Arc::clone(&prepared);
                let error = store
                    .receive_until(
                        |action| *action == Action::Intermediate,
                        OmissionReporting::Silent,
                        move |_| {
                            entered.fetch_add(1, Ordering::SeqCst);
                            let mut cancel = std::pin::pin!(cancellation.cancel());
                            let result = std::future::Future::poll(
                                cancel.as_mut(),
                                &mut std::task::Context::from_waker(std::task::Waker::noop()),
                            );
                            assert!(matches!(result, std::task::Poll::Ready(Ok(()))));
                        },
                    )
                    .await
                    .unwrap_err();
                assert!(error.is_no_received_action());
                assert_eq!(prepared.load(Ordering::SeqCst), 1);
                assert!(started.is_cancelled());
                assert_eq!(commits.load(Ordering::SeqCst), 0);
                assert!(store.take_omissions().is_empty());
                assert!(store.pending_boundary().is_some());
                store.assert_state_eq(Vec::new()).await.unwrap();
                started.finish().await.unwrap();
                assert!(store.finish().await.unwrap_err().is_incomplete_assertion());
                assert!(catch_unwind(AssertUnwindSafe(|| drop(store))).is_err());
            });
        }

        /// Re-enters the public test store when an actor-owned timer is cancelled.
        struct TimerDrop(Option<DropCallback>);

        impl Drop for TimerDrop {
            fn drop(&mut self) {
                if let Some(callback) = self.0.take() {
                    callback();
                }
            }
        }

        /// Separates a cancelled timer from the independent action held by the gate.
        enum ReservedAction {
            StartTimer,
            StartSelected,
            Selected,
            Timer(TimerDrop),
        }

        /// Checks ready-only receiving while timer disposal temporarily owns the actor.
        fn assert_focused_stall_during_timer_drop(preflight: bool) {
            let owner = TaskOwner::new(std::sync::Mutex::new(
                None::<TestStore<Vec<u8>, ReservedAction>>,
            ));
            let callback_owner = TaskOwner::clone(&owner);
            let observed = TaskOwner::new(std::sync::Mutex::new(None));
            let callback_result = TaskOwner::clone(&observed);
            let mut timer = Some(TimerDrop(Some(Box::new(move || {
                let mut owner = callback_owner.lock().unwrap();
                let store = owner.as_mut().unwrap();
                // Cancellation drops this timer inside actor control. A nested
                // ready-only receive must not poll that reserved actor again.
                let result = store
                    .receive_omitting(OmissionReporting::Warn)
                    .now_or_never()
                    .expect("focused receiving never waits for external progress");
                *callback_result.lock().unwrap() = Some(result);
            }))));
            let reducer = core::Reduce::new(
                move |state: &mut Vec<u8>,
                      action: &ReservedAction,
                      _: &mut rca_core::ReduceContext<'_>| {
                    match action {
                        ReservedAction::StartTimer => core::Effect::send_after(
                            std::time::Duration::from_secs(60),
                            ReservedAction::Timer(timer.take().unwrap()),
                        ),
                        ReservedAction::StartSelected => {
                            core::Effect::send(ReservedAction::Selected)
                        }
                        ReservedAction::Selected => {
                            state.push(2);
                            core::Effect::none()
                        }
                        ReservedAction::Timer(timer) => {
                            std::hint::black_box(timer);
                            panic!("the cancelled timer must not run");
                        }
                    }
                },
            );
            let mut builder = TestStore::builder(Vec::new(), reducer);
            if preflight {
                builder = builder
                    .with_capture::<Count>(|state| Ok(state.len()))
                    .unwrap();
            }
            let mut store = builder.build();
            let timer = block_on(store.send(ReservedAction::StartTimer)).unwrap();
            let selected = block_on(store.send(ReservedAction::StartSelected)).unwrap();
            assert!(store.pending_boundary().is_some());
            *owner.lock().unwrap() = Some(store);

            block_on(timer.cancel()).unwrap();
            assert!(timer.is_cancelled());
            assert!(timer.is_finished());
            let error = observed.lock().unwrap().take().unwrap().unwrap_err();
            assert!(error.is_incomplete_assertion());
            assert!(error.drive_error().is_none());
            let phase = if preflight {
                "focused capture"
            } else {
                "receive_omitting"
            };
            assert!(error.to_string().contains(phase));

            let mut store = owner.lock().unwrap().take().unwrap();
            assert!(store.take_omissions().is_empty());
            assert!(store.pending_boundary().is_some());
            let expected = if preflight { Vec::new() } else { vec![2] };
            block_on(store.assert_state_eq(expected)).unwrap();
            if preflight {
                block_on(selected.cancel()).unwrap();
                assert!(selected.is_cancelled());
            } else {
                block_on(selected.finish()).unwrap();
                assert!(!selected.is_cancelled());
            }
            assert!(
                block_on(store.finish())
                    .unwrap_err()
                    .is_incomplete_assertion()
            );
            assert!(catch_unwind(AssertUnwindSafe(|| drop(store))).is_err());
            assert_eq!(TaskOwner::strong_count(&owner), 1);
            assert_eq!(TaskOwner::strong_count(&observed), 1);
        }

        #[test]
        fn selected_receipt_does_not_reenter_the_actor_during_timer_disposal() {
            assert_focused_stall_during_timer_drop(false);
        }

        #[test]
        fn preflight_capture_does_not_reenter_the_actor_during_timer_disposal() {
            assert_focused_stall_during_timer_drop(true);
        }

        /// Pairs a reserved actor with independent finite work and one selected mutation.
        struct ReservedReceipt {
            timer: Option<TimerDrop>,
            background: Option<core::Effect<ReservedAction>>,
            commits: Arc<AtomicUsize>,
            typed: bool,
        }

        impl core::Feature for ReservedReceipt {
            type State = Vec<u8>;
            type Action = ReservedAction;

            fn update(
                &mut self,
                state: &mut Vec<u8>,
                action: &ReservedAction,
                context: &mut core::FeatureContext<'_, Vec<u8>, ReservedAction>,
            ) {
                match action {
                    ReservedAction::StartTimer => context.effect(core::Effect::send_after(
                        std::time::Duration::from_secs(60),
                        ReservedAction::Timer(self.timer.take().unwrap()),
                    )),
                    ReservedAction::StartSelected => {
                        context.effect(self.background.take().unwrap());
                        if self.typed {
                            let commits = Arc::clone(&self.commits);
                            context.task(core::Task::run(move |context| async move {
                                context
                                    .update(move |state: &mut Vec<u8>| {
                                        commits.fetch_add(1, Ordering::SeqCst);
                                        state.push(2);
                                    })
                                    .await
                                    .unwrap();
                            }));
                        } else {
                            context.effect(core::Effect::send(ReservedAction::Selected));
                        }
                    }
                    ReservedAction::Selected => {
                        self.commits.fetch_add(1, Ordering::SeqCst);
                        state.push(2);
                    }
                    ReservedAction::Timer(_) => panic!("the cancelled timer must not run"),
                }
            }
        }

        #[test]
        fn ordinary_receipt_budget_retains_debt_while_timer_disposal_owns_the_actor() {
            for typed in [false, true] {
                let owner = TaskOwner::new(std::sync::Mutex::new(
                    None::<TestStore<Vec<u8>, ReservedAction>>,
                ));
                let callback_owner = TaskOwner::clone(&owner);
                let observed = TaskOwner::new(std::sync::Mutex::new(None));
                let callback_result = TaskOwner::clone(&observed);
                let (release, response) = rca_test::controlled_future();
                let commits = Arc::new(AtomicUsize::new(0));
                let callback_commits = Arc::clone(&commits);
                let timer = TimerDrop(Some(Box::new(move || {
                    let mut owner = callback_owner.lock().unwrap();
                    let store = owner.as_mut().unwrap();
                    let update = move |expected: &mut Vec<u8>| {
                        expected.push(2);
                        release.complete(()).unwrap();
                    };
                    // Ready work starts after preflight. The receipt cannot complete
                    // while this destructor owns the actor's current polling turn.
                    let receipt = if typed {
                        store.receive_update(update).left_future()
                    } else {
                        store
                            .receive_with(
                                |action| matches!(action, ReservedAction::Selected),
                                update,
                            )
                            .right_future()
                    };
                    let mut receipt = std::pin::pin!(receipt);
                    // Each ready batch yields. Keep the same receipt alive while
                    // its seven task polls exhaust the configured driving budget.
                    let result = (0..7)
                        .find_map(|_| receipt.as_mut().now_or_never())
                        .expect("finite ready work must report its receipt-driving budget");
                    assert_eq!(callback_commits.load(Ordering::SeqCst), 0);
                    *callback_result.lock().unwrap() = Some(result);
                })));
                let polls = Arc::new(AtomicUsize::new(0));
                let work_polls = Arc::clone(&polls);
                let background = core::Effect::future(async move {
                    response.await.unwrap();
                    futures_util::future::poll_fn(move |context| {
                        if work_polls.fetch_add(1, Ordering::SeqCst) < 8 {
                            context.waker().wake_by_ref();
                            std::task::Poll::Pending
                        } else {
                            std::task::Poll::Ready(None::<ReservedAction>)
                        }
                    })
                    .await
                });
                let mut store = TestStore::builder(
                    Vec::new(),
                    ReservedReceipt {
                        timer: Some(timer),
                        background: Some(background),
                        commits: Arc::clone(&commits),
                        typed,
                    },
                )
                .with_step_budget(7)
                .build();
                let timer = block_on(store.send(ReservedAction::StartTimer)).unwrap();
                let selected = block_on(store.send(ReservedAction::StartSelected)).unwrap();
                assert!(store.pending_boundary().is_some());
                *owner.lock().unwrap() = Some(store);

                block_on(timer.cancel()).unwrap();
                let error = observed.lock().unwrap().take().unwrap().unwrap_err();
                let drive = error
                    .drive_error()
                    .expect("receipt preserves its driving cause");
                assert!(drive.is_step_budget_exhausted());
                assert_eq!((drive.polls(), drive.step_budget()), (7, 7));
                assert!(timer.is_cancelled());
                block_on(selected.finish()).unwrap();
                assert_eq!(commits.load(Ordering::SeqCst), 1);
                assert_eq!(polls.load(Ordering::SeqCst), 9);
                let mut store = owner.lock().unwrap().take().unwrap();
                block_on(store.assert_state_eq(vec![2])).unwrap();
                assert!(store.pending_boundary().is_some());
                assert!(
                    block_on(store.finish())
                        .unwrap_err()
                        .is_incomplete_assertion()
                );
                assert!(catch_unwind(AssertUnwindSafe(|| drop(store))).is_err());
                assert_eq!(TaskOwner::strong_count(&owner), 1);
                assert_eq!(TaskOwner::strong_count(&observed), 1);
            }
        }

        #[test]
        fn preflight_cancellation_retires_the_selected_action_before_preparation() {
            /// Cancels only when the verified capture history is cloned for preflight.
            struct CancelOnClone {
                count: usize,
                armed: Arc<AtomicBool>,
                task: TaskOwner<std::sync::Mutex<Option<TestActionTask>>>,
                cancellations: Arc<AtomicUsize>,
            }

            impl Clone for CancelOnClone {
                fn clone(&self) -> Self {
                    if self.armed.swap(false, Ordering::SeqCst) {
                        let task = self.task.lock().unwrap().as_ref().unwrap().clone();
                        let mut cancel = std::pin::pin!(task.cancel());
                        let result = std::future::Future::poll(
                            cancel.as_mut(),
                            &mut std::task::Context::from_waker(std::task::Waker::noop()),
                        );
                        assert!(matches!(result, std::task::Poll::Ready(Ok(()))));
                        self.cancellations.fetch_add(1, Ordering::SeqCst);
                    }
                    Self {
                        count: self.count,
                        armed: Arc::clone(&self.armed),
                        task: TaskOwner::clone(&self.task),
                        cancellations: Arc::clone(&self.cancellations),
                    }
                }
            }

            impl PartialEq for CancelOnClone {
                fn eq(&self, other: &Self) -> bool {
                    self.count == other.count
                }
            }

            struct CancelCapture;

            impl CaptureKey for CancelCapture {
                type Value = CancelOnClone;
                type Error = ();
            }

            block_on(async {
                let commits = Arc::new(AtomicUsize::new(0));
                let armed = Arc::new(AtomicBool::new(false));
                let task = TaskOwner::new(std::sync::Mutex::new(None));
                let cancellations = Arc::new(AtomicUsize::new(0));
                let mut store = TestStore::builder(
                    Vec::new(),
                    PostReceipt {
                        effect: Some(core::Effect::none()),
                        commits: Arc::clone(&commits),
                    },
                )
                .with_capture::<CancelCapture>({
                    let armed = Arc::clone(&armed);
                    let task = TaskOwner::clone(&task);
                    let cancellations = Arc::clone(&cancellations);
                    move |state| {
                        Ok(CancelOnClone {
                            count: state.len(),
                            armed: Arc::clone(&armed),
                            task: TaskOwner::clone(&task),
                            cancellations: Arc::clone(&cancellations),
                        })
                    }
                })
                .unwrap()
                .build();
                let started = store.send(Action::Start).await.unwrap();
                *task.lock().unwrap() = Some(started.clone());
                armed.store(true, Ordering::SeqCst);
                let error = store
                    .receive_until(
                        |action| *action == Action::Intermediate,
                        OmissionReporting::Silent,
                        |_| panic!("preflight cancellation must precede preparation"),
                    )
                    .await
                    .unwrap_err();
                assert!(error.is_no_received_action());
                assert_eq!(cancellations.load(Ordering::SeqCst), 1);
                assert!(started.is_cancelled());
                assert_eq!(commits.load(Ordering::SeqCst), 0);
                assert!(store.take_omissions().is_empty());
                assert!(store.pending_boundary().is_some());
                store.assert_state_eq(Vec::new()).await.unwrap();
                started.finish().await.unwrap();
                assert!(store.finish().await.unwrap_err().is_incomplete_assertion());
                assert!(catch_unwind(AssertUnwindSafe(|| drop(store))).is_err());
            });
        }

        #[test]
        fn declared_captures_adopt_omissions_but_selected_expectation_remains_explicit() {
            block_on(async {
                let mut store = TestStore::builder(Vec::new(), Pipeline)
                    .with_capture::<Count>(|state| Ok(state.len()))
                    .unwrap()
                    .build();
                let task = store.send(Action::Start).await.unwrap();
                let _ = store
                    .receive_until_with_captures(
                        |action| *action == Action::Done,
                        OmissionReporting::Warn,
                        |state| state.push(3),
                        |captures| {
                            captures.update::<Count>(|count| {
                                assert_eq!(*count, 2);
                                *count = 3;
                            })
                        },
                    )
                    .await
                    .unwrap();
                assert_eq!(store.take_omissions().len(), 2);
                task.finish().await.unwrap();
                store.finish().await.unwrap();
            });
        }

        #[test]
        fn preflight_cancellation_drop_panic_reports_the_stopped_actor() {
            /// Fails when cancellation releases the selected action payload.
            struct PanicOnDrop(Arc<AtomicUsize>);

            impl Drop for PanicOnDrop {
                fn drop(&mut self) {
                    self.0.fetch_add(1, Ordering::SeqCst);
                    panic!("cancelled action drop");
                }
            }

            /// Catches actor teardown failure before the next capture query.
            struct CancelOnClone {
                value: usize,
                armed: Arc<AtomicBool>,
                task: TaskOwner<std::sync::Mutex<Option<TestActionTask>>>,
            }

            impl Clone for CancelOnClone {
                fn clone(&self) -> Self {
                    if self.armed.swap(false, Ordering::SeqCst) {
                        let task = self.task.lock().unwrap().as_ref().unwrap().clone();
                        let panic = catch_unwind(AssertUnwindSafe(|| {
                            let mut cancel = std::pin::pin!(task.cancel());
                            std::future::Future::poll(
                                cancel.as_mut(),
                                &mut std::task::Context::from_waker(std::task::Waker::noop()),
                            )
                        }))
                        .expect_err("cancellation drops the selected payload in the actor");
                        assert_eq!(panic.downcast_ref::<&str>(), Some(&"cancelled action drop"));
                    }
                    Self {
                        value: self.value,
                        armed: Arc::clone(&self.armed),
                        task: TaskOwner::clone(&self.task),
                    }
                }
            }

            impl PartialEq for CancelOnClone {
                fn eq(&self, other: &Self) -> bool {
                    self.value == other.value
                }
            }

            /// Declares the capture whose preflight clone requests cancellation.
            struct CancelCapture;

            impl CaptureKey for CancelCapture {
                type Value = CancelOnClone;
                type Error = ();
            }

            block_on(async {
                let state = Arc::new(AtomicUsize::new(0));
                let drops = Arc::new(AtomicUsize::new(0));
                let payload_drops = Arc::clone(&drops);
                let reducer = core::Reduce::new(
                    move |state: &mut Arc<AtomicUsize>,
                          action: &Option<PanicOnDrop>,
                          _: &mut rca_core::ReduceContext<'_>| {
                        if action.is_none() {
                            core::Effect::send(Some(PanicOnDrop(Arc::clone(&payload_drops))))
                        } else {
                            state.fetch_add(1, Ordering::SeqCst);
                            core::Effect::none()
                        }
                    },
                );
                let armed = Arc::new(AtomicBool::new(false));
                let task = TaskOwner::new(std::sync::Mutex::new(None));
                let mut store =
                    TestStore::projected_builder(Arc::clone(&state), reducer, |state| {
                        state.load(Ordering::SeqCst)
                    })
                    .with_capture::<CancelCapture>({
                        let armed = Arc::clone(&armed);
                        let task = TaskOwner::clone(&task);
                        move |state| {
                            Ok(CancelOnClone {
                                value: state.load(Ordering::SeqCst),
                                armed: Arc::clone(&armed),
                                task: TaskOwner::clone(&task),
                            })
                        }
                    })
                    .unwrap()
                    .build();
                let started = store.send(None).await.unwrap();
                *task.lock().unwrap() = Some(started.clone());
                armed.store(true, Ordering::SeqCst);

                let error = store
                    .receive_omitting(OmissionReporting::Warn)
                    .await
                    .unwrap_err();
                assert!(
                    std::error::Error::source(&error)
                        .unwrap()
                        .is::<rca_core::StoreStopped>()
                );
                assert_eq!(drops.load(Ordering::SeqCst), 1);
                assert_eq!(state.load(Ordering::SeqCst), 0);
                assert!(started.is_cancelled());
                assert!(store.take_omissions().is_empty());
                assert!(store.pending_boundary().is_some());
                assert!(store.finish().await.unwrap_err().is_incomplete_assertion());
                assert!(catch_unwind(AssertUnwindSafe(|| drop(store))).is_err());
            });
        }

        #[test]
        fn unknown_selected_capture_keeps_the_action_pending_for_retry() {
            block_on(async {
                let mut store = TestStore::builder(Vec::new(), Pipeline)
                    .with_capture::<Count>(|state| Ok(state.len()))
                    .unwrap()
                    .build();
                let started = store.send(Action::Start).await.unwrap();
                let error = store
                    .receive_until_with_captures(
                        |action| *action == Action::Done,
                        OmissionReporting::Warn,
                        |state| state.push(3),
                        |captures| captures.set::<UndeclaredCount>(3),
                    )
                    .await
                    .unwrap_err();
                assert!(error.to_string().contains("UnknownExpectation"));
                store.assert_state_eq(vec![1, 2]).await.unwrap();
                assert!(store.pending_boundary().is_some());
                assert_eq!(store.take_omissions().len(), 2);

                store
                    .receive_until_with_captures(
                        |action| *action == Action::Done,
                        OmissionReporting::Silent,
                        |state| state.push(3),
                        |captures| captures.set::<Count>(3),
                    )
                    .await
                    .unwrap()
                    .finish()
                    .await
                    .unwrap();
                assert!(store.take_omissions().is_empty());
                started.finish().await.unwrap();
                store.finish().await.unwrap();
            });
        }

        #[test]
        fn selected_capture_mismatch_retains_the_committed_boundary() {
            block_on(async {
                let mut store = TestStore::builder(Vec::new(), Pipeline)
                    .with_capture::<Count>(|state| Ok(state.len()))
                    .unwrap()
                    .build();
                let started = store.send(Action::Start).await.unwrap();
                let error = store
                    .receive_until_with_captures(
                        |action| *action == Action::Done,
                        OmissionReporting::Warn,
                        |state| state.push(3),
                        |captures| captures.set::<Count>(4),
                    )
                    .await
                    .unwrap_err();
                assert!(error.is_capture_mismatch());
                assert!(error.to_string().starts_with(&format!(
                    "TestStore capture {} Mismatch during receive_until at ",
                    std::any::type_name::<Count>()
                )));
                store.assert_state_eq(vec![1, 2, 3]).await.unwrap();
                assert!(store.pending_boundary().is_some());
                assert_eq!(store.take_omissions().len(), 2);
                drop(started);
                assert!(store.finish().await.unwrap_err().is_incomplete_assertion());
                assert!(catch_unwind(AssertUnwindSafe(|| drop(store))).is_err());
            });
        }

        #[test]
        fn earlier_external_capture_changes_are_not_absorbed_by_omission() {
            block_on(async {
                let external = Arc::new(AtomicUsize::new(0));
                let captured = Arc::clone(&external);
                let mut store = TestStore::builder(Vec::new(), Pipeline)
                    .with_capture::<Count>(move |_| Ok(captured.load(Ordering::Relaxed)))
                    .unwrap()
                    .build();
                let _ = store.send(Action::Start).await.unwrap();
                external.store(1, Ordering::Relaxed);
                let error = store
                    .receive_omitting(OmissionReporting::Warn)
                    .await
                    .unwrap_err();
                assert!(error.is_capture_mismatch());
                store.assert_state_eq(vec![]).await.unwrap();
                assert!(store.take_omissions().is_empty());
                assert!(store.finish().await.unwrap_err().is_incomplete_assertion());
                assert!(catch_unwind(AssertUnwindSafe(|| drop(store))).is_err());
            });
        }

        #[test]
        fn preflight_capture_error_keeps_the_pending_transaction_unreleased() {
            block_on(async {
                let reject = Arc::new(AtomicBool::new(false));
                let captured = Arc::clone(&reject);
                let mut store = TestStore::builder(Vec::new(), Pipeline)
                    .with_capture::<Count>(move |_| {
                        if captured.load(Ordering::Relaxed) {
                            Err(())
                        } else {
                            Ok(0)
                        }
                    })
                    .unwrap()
                    .build();
                let _started = store.send(Action::Start).await.unwrap();
                reject.store(true, Ordering::Relaxed);

                let error = store
                    .receive_omitting(OmissionReporting::Warn)
                    .await
                    .unwrap_err();
                assert_eq!(error.into_capture_error::<Count>().unwrap(), ());
                assert!(store.pending_boundary().is_some());
                store.assert_state_eq(Vec::new()).await.unwrap();
                assert!(store.take_omissions().is_empty());
                assert!(store.finish().await.unwrap_err().is_incomplete_assertion());
                assert!(catch_unwind(AssertUnwindSafe(|| drop(store))).is_err());
            });
        }

        #[test]
        fn focused_preflight_drops_unused_callbacks_in_argument_order() {
            /// Records which unused callback releases its owned input first.
            struct CallbackDrop(&'static str, Arc<std::sync::Mutex<Vec<&'static str>>>);

            impl Drop for CallbackDrop {
                fn drop(&mut self) {
                    self.1.lock().unwrap().push(self.0);
                }
            }

            block_on(async {
                let external = Arc::new(AtomicUsize::new(0));
                let captured = Arc::clone(&external);
                let mut store = TestStore::builder(Vec::new(), Pipeline)
                    .with_capture::<Count>(move |_| Ok(captured.load(Ordering::Relaxed)))
                    .unwrap()
                    .build();
                let started = store.send(Action::Start).await.unwrap();
                external.store(1, Ordering::Relaxed);

                let order = Arc::new(std::sync::Mutex::new(Vec::new()));
                let update = CallbackDrop("update", Arc::clone(&order));
                let captures = CallbackDrop("captures", Arc::clone(&order));
                let error = store
                    .receive_until_with_captures(
                        |action| *action == Action::Start,
                        OmissionReporting::Warn,
                        move |_| {
                            std::hint::black_box(&update);
                            panic!("preflight must precede snapshot expectations");
                        },
                        move |_| {
                            std::hint::black_box(&captures);
                            panic!("preflight must precede declared expectations");
                        },
                    )
                    .await
                    .unwrap_err();
                assert!(error.is_capture_mismatch());
                assert_eq!(order.lock().unwrap().as_slice(), ["update", "captures"]);
                assert!(store.pending_boundary().is_some());
                store.assert_state_eq(Vec::new()).await.unwrap();
                assert!(store.take_omissions().is_empty());
                drop(started);
                assert!(store.finish().await.unwrap_err().is_incomplete_assertion());
                assert!(catch_unwind(AssertUnwindSafe(|| drop(store))).is_err());
            });
        }

        #[test]
        fn warning_renderer_panic_retains_the_committed_boundary() {
            block_on(async {
                let mut store = TestStore::builder(Vec::new(), Pipeline)
                    .with_snapshot_renderer(|_| panic!("omission renderer"))
                    .build();
                let _ = store.send(Action::Start).await.unwrap();
                let panic = AssertUnwindSafe(store.receive_omitting(OmissionReporting::Warn))
                    .catch_unwind()
                    .await
                    .unwrap_err();
                assert_eq!(panic.downcast_ref::<&str>(), Some(&"omission renderer"));
                store.assert_state_eq(vec![1]).await.unwrap();
                assert!(store.take_omissions().is_empty());
                assert!(store.finish().await.unwrap_err().is_incomplete_assertion());
                assert!(catch_unwind(AssertUnwindSafe(|| drop(store))).is_err());
            });
        }

        #[test]
        fn failed_omission_capture_retains_the_committed_boundary() {
            block_on(async {
                let mut store = TestStore::builder(Vec::new(), Pipeline)
                    .with_capture::<Count>(|state| if state.is_empty() { Ok(0) } else { Err(()) })
                    .unwrap()
                    .build();
                let _ = store.send(Action::Start).await.unwrap();
                let error = store
                    .receive_omitting(OmissionReporting::Warn)
                    .await
                    .unwrap_err();
                assert_eq!(error.into_capture_error::<Count>().unwrap(), ());
                store.assert_state_eq(vec![1]).await.unwrap();
                assert!(store.take_omissions().is_empty());
                assert!(store.finish().await.unwrap_err().is_incomplete_assertion());
                assert!(catch_unwind(AssertUnwindSafe(|| drop(store))).is_err());
            });
        }

        /// Distinguishes historical expectations from independently captured actual values.
        #[derive(Debug)]
        struct OwnedSnapshot {
            value: usize,
            expected: bool,
            panic_on_drop: Arc<AtomicBool>,
        }

        impl Clone for OwnedSnapshot {
            fn clone(&self) -> Self {
                Self {
                    value: self.value,
                    expected: true,
                    panic_on_drop: Arc::clone(&self.panic_on_drop),
                }
            }
        }

        impl PartialEq for OwnedSnapshot {
            fn eq(&self, other: &Self) -> bool {
                self.value == other.value
            }
        }

        impl Drop for OwnedSnapshot {
            fn drop(&mut self) {
                if self.expected && self.panic_on_drop.swap(false, Ordering::Relaxed) {
                    panic!("retired expectation");
                }
            }
        }

        /// Counts user snapshot copies independently of production state capture.
        #[derive(Debug)]
        struct CountedSnapshot {
            /// The selected root history used by independent strict expectations.
            value: usize,
            /// Counts user copies without counting fresh captures.
            clones: Arc<AtomicUsize>,
        }

        impl Clone for CountedSnapshot {
            fn clone(&self) -> Self {
                self.clones.fetch_add(1, Ordering::Relaxed);
                Self {
                    value: self.value,
                    clones: Arc::clone(&self.clones),
                }
            }
        }

        impl PartialEq for CountedSnapshot {
            fn eq(&self, other: &Self) -> bool {
                self.value == other.value
            }
        }

        /// An omission adopts actual history; only the later strict selection needs a copy.
        fn assert_no_omission_clone(reporting: OmissionReporting) {
            block_on(async {
                let clones = Arc::new(AtomicUsize::new(0));
                let captured = Arc::clone(&clones);
                let mut store = TestStore::projected_builder(Vec::new(), Pipeline, move |state| {
                    CountedSnapshot {
                        value: state.len(),
                        clones: Arc::clone(&captured),
                    }
                })
                .build();
                let started = store.send(Action::Start).await.unwrap();
                clones.store(0, Ordering::Relaxed);
                let _ = store.receive_omitting(reporting).await.unwrap();
                assert_eq!(
                    clones.load(Ordering::Relaxed),
                    0,
                    "omission must not clone root history"
                );
                let _ = store
                    .receive_until(
                        |action| *action == Action::Done,
                        reporting,
                        |state| state.value = 3,
                    )
                    .await
                    .unwrap();
                assert_eq!(
                    clones.load(Ordering::Relaxed),
                    1,
                    "only the selected strict action clones history"
                );
                started.finish().await.unwrap();
                store.finish().await.unwrap();
            });
        }

        #[test]
        fn silent_omission_does_not_clone_root_history() {
            assert_no_omission_clone(OmissionReporting::Silent);
        }

        #[test]
        fn warned_omission_does_not_clone_root_history() {
            assert_no_omission_clone(OmissionReporting::Warn);
        }

        /// Observes when acknowledgement admits a read between two causal actions.
        struct ReadBetweenActions(Arc<AtomicUsize>);

        impl core::Feature for ReadBetweenActions {
            type State = Vec<u8>;
            type Action = Action;

            fn update(
                &mut self,
                state: &mut Vec<u8>,
                action: &Action,
                context: &mut core::FeatureContext<'_, Vec<u8>, Action>,
            ) {
                match action {
                    Action::Start => {
                        state.push(1);
                        let reads = Arc::clone(&self.0);
                        context.task(core::Task::run(move |context| async move {
                            let _ = context.send(Action::Intermediate).await.unwrap();
                            context
                                .read(move |_| {
                                    reads.fetch_add(1, Ordering::SeqCst);
                                })
                                .await
                                .unwrap();
                            let _ = context.send(Action::Done).await.unwrap();
                        }));
                    }
                    Action::Intermediate => state.push(2),
                    Action::Done => state.push(3),
                    Action::Again => panic!("the fixture sends each action once"),
                }
            }
        }

        /// Runs an armed public control when the selected root history retires.
        #[derive(Clone, Debug)]
        struct RetiredBaselineSnapshot {
            /// Identifies which captured history owns this destructor.
            value: usize,
            /// Arms one destructor without causing a second panic during cleanup.
            panic_on_drop: Arc<AtomicBool>,
            /// Drives causal work once when the one-item baseline is replaced.
            finish_on_drop: TaskOwner<std::sync::Mutex<Option<TestActionTask>>>,
            /// Records the actor read that must wait for this baseline's retirement.
            reads: Arc<AtomicUsize>,
        }

        impl PartialEq for RetiredBaselineSnapshot {
            fn eq(&self, other: &Self) -> bool {
                self.value == other.value
            }
        }

        impl Drop for RetiredBaselineSnapshot {
            fn drop(&mut self) {
                if self.value == 0 && self.panic_on_drop.swap(false, Ordering::Relaxed) {
                    panic!("retired baseline");
                }
                if self.value == 1 {
                    let task = self.finish_on_drop.lock().unwrap().take();
                    if let Some(task) = task {
                        let mut finish = std::pin::pin!(task.finish());
                        let result = std::future::Future::poll(
                            finish.as_mut(),
                            &mut std::task::Context::from_waker(std::task::Waker::noop()),
                        );
                        let std::task::Poll::Ready(Err(error)) = result else {
                            panic!("the pending successor keeps causal work unfinished");
                        };
                        assert!(error.drive_error().is_none());
                        assert!(!task.is_finished());
                        assert_eq!(
                            self.reads.load(Ordering::SeqCst),
                            0,
                            "the selected transaction remains held during baseline disposal"
                        );
                    }
                }
            }
        }

        #[test]
        fn old_baseline_drop_can_drive_causal_work_without_consuming_the_successor() {
            for omit in [false, true] {
                block_on(async {
                    let finish_on_drop = TaskOwner::new(std::sync::Mutex::new(None));
                    let captured = TaskOwner::clone(&finish_on_drop);
                    let reads = Arc::new(AtomicUsize::new(0));
                    let captured_reads = Arc::clone(&reads);
                    let mut store = TestStore::projected_builder(
                        Vec::new(),
                        ReadBetweenActions(Arc::clone(&reads)),
                        move |state| RetiredBaselineSnapshot {
                            value: state.len(),
                            panic_on_drop: Arc::new(AtomicBool::new(false)),
                            finish_on_drop: TaskOwner::clone(&captured),
                            reads: Arc::clone(&captured_reads),
                        },
                    )
                    .build();
                    let started = store
                        .send_with(Action::Start, |expected| expected.value = 1)
                        .await
                        .unwrap();
                    *finish_on_drop.lock().unwrap() = Some(started.clone());

                    if omit {
                        let _ = store
                            .receive_omitting(OmissionReporting::Warn)
                            .await
                            .unwrap();
                    } else {
                        let _ = store
                            .receive_until(
                                |action| *action == Action::Intermediate,
                                OmissionReporting::Warn,
                                |expected| expected.value = 2,
                            )
                            .await
                            .unwrap();
                    }

                    assert!(finish_on_drop.lock().unwrap().is_none());
                    assert_eq!(reads.load(Ordering::SeqCst), 1);
                    store
                        .assert_state(|state| assert_eq!(state, &[1, 2]))
                        .await
                        .unwrap();
                    assert!(store.pending_boundary().is_some());
                    assert!(!started.is_finished());
                    assert_eq!(store.take_omissions().len(), usize::from(omit));
                    let _ = store
                        .receive_with(
                            |action| *action == Action::Done,
                            |expected| {
                                assert_eq!(expected.value, 2);
                                expected.value = 3;
                            },
                        )
                        .await
                        .unwrap();
                    store
                        .assert_state(|state| assert_eq!(state, &[1, 2, 3]))
                        .await
                        .unwrap();
                    assert!(store.take_omissions().is_empty());
                    started.finish().await.unwrap();
                    store.finish().await.unwrap();
                });
            }
        }

        #[test]
        fn old_baseline_drop_panic_preserves_omission_debt() {
            block_on(async {
                let armed = Arc::new(AtomicBool::new(false));
                let captured = Arc::clone(&armed);
                let mut store = TestStore::projected_builder(Vec::new(), Pipeline, move |state| {
                    RetiredBaselineSnapshot {
                        value: state.len(),
                        panic_on_drop: Arc::clone(&captured),
                        finish_on_drop: TaskOwner::new(std::sync::Mutex::new(None)),
                        reads: Arc::new(AtomicUsize::new(0)),
                    }
                })
                .build();
                let _ = store.send(Action::Start).await.unwrap();
                armed.store(true, Ordering::Relaxed);
                let panic = AssertUnwindSafe(store.receive_omitting(OmissionReporting::Silent))
                    .catch_unwind()
                    .await
                    .unwrap_err();
                assert_eq!(panic.downcast_ref::<&str>(), Some(&"retired baseline"));
                assert!(store.finish().await.unwrap_err().is_incomplete_assertion());
                assert!(
                    store
                        .receive_omitting(OmissionReporting::Silent)
                        .await
                        .unwrap_err()
                        .is_incomplete_assertion()
                );
                assert!(catch_unwind(AssertUnwindSafe(|| drop(store))).is_err());
            });
        }

        #[test]
        fn expected_payload_drop_panic_preserves_ordinary_verification_boundary() {
            block_on(async {
                let armed = Arc::new(AtomicBool::new(false));
                let captured = Arc::clone(&armed);
                let mut store = TestStore::projected_builder(Vec::new(), Pipeline, move |state| {
                    OwnedSnapshot {
                        value: state.len(),
                        expected: false,
                        panic_on_drop: Arc::clone(&captured),
                    }
                })
                .build();
                armed.store(true, Ordering::Relaxed);
                let panic =
                    AssertUnwindSafe(store.send_with(Action::Done, |state| state.value = 1))
                        .catch_unwind()
                        .await
                        .unwrap_err();
                assert_eq!(panic.downcast_ref::<&str>(), Some(&"retired expectation"));
                assert!(store.pending_boundary().is_some());
                assert!(store.finish().await.unwrap_err().is_incomplete_assertion());
                assert!(
                    store
                        .receive_omitting(OmissionReporting::Silent)
                        .await
                        .unwrap_err()
                        .is_incomplete_assertion()
                );
                assert!(catch_unwind(AssertUnwindSafe(|| drop(store))).is_err());
            });
        }
    };
}

mod shared {
    use rca_core as core;
    use rca_test::{CaptureKey, TestActionTask, TestStore};
    use std::sync::Arc as TaskOwner;
    /// Allows actor-owned disposal to call a portable public-API control.
    type DropCallback = Box<dyn FnOnce() + Send>;
    focused_controls!();
}

#[cfg(feature = "local")]
mod local {
    use rca_core::local as core;
    use rca_test::local::{CaptureKey, TestActionTask, TestStore};
    use std::rc::Rc as TaskOwner;
    /// Keeps the owner-local disposal control free of `Send` requirements.
    type DropCallback = Box<dyn FnOnce()>;
    focused_controls!();
}
