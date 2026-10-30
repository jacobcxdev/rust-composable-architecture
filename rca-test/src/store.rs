//! Drives production Store through deterministic exhaustive assertions.

use rca_core as core_api;
use std::fmt;
use std::panic::Location;
#[cfg(test)]
use std::sync::Arc;
use std::sync::Arc as CaptureOwner;
use std::sync::Mutex;
use std::time::Duration;

#[cfg(test)]
use rca_core::Reducer;
use rca_core::testing::{IssueRecorder, RuntimeSnapshot, TransactionGate};
use rca_core::{
    Clock, DependencyKey, DependencyValues, Issue, MonotonicInstant, Store, StoreBuilder,
};

use crate::{
    DEFAULT_STEP_BUDGET, DeterministicRuntime, DriveReport, FinishDiagnostics, TestActionTask,
    TestStoreError,
};

mod api;
mod assertions;
mod focused;
#[doc(inline)]
pub use focused::OmissionReporting;
#[cfg(feature = "local")]
pub(crate) mod local;
mod obligations;
use assertions::{Assertions, Coverage};
use obligations::DeclaredCaptures;
#[cfg(feature = "local")]
pub(crate) use obligations::local as local_captures;
#[doc(inline)]
pub use obligations::{CaptureExpectations, CaptureKey};

/// Stages the portable declaration values for one focused transaction.
type ExpectedCaptures<State> = Vec<obligations::ExpectedCapture<State>>;

/// Controls whether every effect action must be received explicitly.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Exhaustivity {
    /// Requires explicit receive for every effect action.
    #[default]
    Exhaustive,
    /// Allows explicit skipping of pending effect actions.
    NonExhaustive,
}

api::test_store_api!(
    core_api,
    [Send],
    [Sync],
    r#"
```
use rca_core::{Clock, DependencyKey, Effect, ReduceContext, Reducer};
use rca_test::TestStore;
struct Client { clock: Clock }
struct ClientKey;
impl DependencyKey for ClientKey {
    type Value = Client;
    fn live_value() -> Client { panic!("construct from the Store runtime") }
}
struct Feature;
impl Reducer for Feature {
    type State = ();
    type Action = ();
    fn reduce(&mut self, _: &mut (), _: &(), _: &mut ReduceContext<'_>) -> Effect<()> {
        Effect::none()
    }
}
let store = TestStore::builder((), Feature)
    .with_clock_dependency::<ClientKey>(|clock| Client { clock })
    .build();
```
"#,
    r#"
```
# use rca_core::{Effect, ReduceContext, Reducer};
# use rca_test::TestStore;
# struct Feature;
# impl Reducer for Feature {
# type State = (u32, String);
# type Action = ();
# fn reduce(&mut self, _: &mut Self::State, _: &(), _: &mut ReduceContext<'_>) -> Effect<()> { Effect::none() }
# }
let store = TestStore::builder((0, "private".to_owned()), Feature)
    .with_snapshot_renderer(|snapshot| format!("count: {}\nsecret: [redacted]", snapshot.0))
    .build();
# drop(store);
```
"#
);

impl<State, Action, R, Snapshot> TestStoreBuilder<State, Action, R, Snapshot>
where
    State: Send + 'static,
    Action: Send + 'static,
    R: Send + 'static,
{
    /// Attaches a root-declared child with ordinary and narrow command acquisition sites.
    ///
    /// Construction waits for `receive_structural`. The mapper runs only after
    /// `receive_command` matches the original command and releases its frontier.
    #[expect(
        clippy::type_complexity,
        reason = "Construction returns the builder and independently typed child and command sites"
    )]
    pub fn mount_with_command<Child, K, C>(
        mut self,
        descriptor: rca_core::OptionalMount<State, Child, K>,
        mapper: impl FnMut(C) -> Child::Action + Send + 'static,
    ) -> (
        Self,
        rca_core::MountSite<State, Child::State, Child::Action, Action>,
        rca_core::CommandSite<State, C>,
    )
    where
        Child: rca_core::Feature + Send + 'static,
        Child::State: Send + 'static,
        Child::Action: Send + 'static,
        K: Eq + std::hash::Hash + Clone + Send + Sync + 'static,
        C: Send + 'static,
    {
        let (attach, site, command_site) =
            StoreBuilder::<State, Action, R>::defer_mount_with_command(descriptor, mapper);
        self.dependencies
            .push(Box::new(move |builder, _| attach(builder)));
        (self, site, command_site)
    }
}

/// Checks actor-owned work; deterministic scheduler sleeps are checked separately.
fn has_unfinished_work(snapshot: &RuntimeSnapshot) -> bool {
    !snapshot.live_task_ids.is_empty()
        || snapshot.cancellation_count != 0
        || !snapshot.timer_deadlines.is_empty()
        || snapshot.observation_count != 0
}

#[cfg(test)]
mod completion_tests {
    use super::*;
    use futures_executor::block_on;
    use rca_core::{Effect, ReduceContext};
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Feature;
    impl Reducer for Feature {
        type State = ();
        type Action = ();
        fn reduce(&mut self, (): &mut (), (): &(), _: &mut ReduceContext<'_>) -> Effect<()> {
            Effect::none()
        }
    }

    #[test]
    fn new_accepts_a_feature_with_transaction_gated_task_updates() {
        struct Work;

        impl rca_core::Feature for Work {
            type State = usize;
            type Action = ();

            fn update(
                &mut self,
                _: &mut usize,
                (): &(),
                context: &mut rca_core::FeatureContext<'_, usize, ()>,
            ) {
                context.task(rca_core::Task::run(|context| async move {
                    context.update(|state| *state += 1).await.unwrap();
                }));
            }
        }

        block_on(async {
            let mut store = TestStore::new(0, Work);
            let task = store.send(()).await.unwrap();

            let _ = store
                .receive_update(|expected| *expected = 1)
                .await
                .unwrap();

            task.finish().await.unwrap();
            store.finish().await.unwrap();
        });
    }

    #[test]
    fn unfinished_work_classifies_independent_diagnostic_resources() {
        // Constructed diagnostic fixtures test resource accounting, not snapshots
        // observed through public TestStore::finish.
        for (cancellation_count, timer_deadlines, expected) in [
            (0, Vec::new(), false),
            (1, Vec::new(), true),
            (0, vec![MonotonicInstant::ZERO], true),
        ] {
            let snapshot = RuntimeSnapshot {
                commit_version: 1,
                live_task_ids: Vec::new(),
                cancellation_count,
                timer_deadlines,
                observation_count: 0,
            };
            assert_eq!(has_unfinished_work(&snapshot), expected, "{snapshot:?}");
        }
    }

    #[test]
    fn resumed_advance_budget_failure_discards_target_and_preserves_pending_action() {
        struct GatedWork {
            gate: Option<crate::ControlledGate>,
            polls: Arc<AtomicUsize>,
        }

        impl Reducer for GatedWork {
            type State = ();
            type Action = bool;

            fn reduce(
                &mut self,
                (): &mut (),
                action: &bool,
                _: &mut ReduceContext<'_>,
            ) -> Effect<bool> {
                if *action {
                    return Effect::none();
                }
                let gate = self.gate.take().expect("only one action starts work");
                let polls = Arc::clone(&self.polls);
                Effect::merge([
                    Effect::send_after(Duration::from_secs(5), true),
                    Effect::run(move |_| async move {
                        gate.await.expect("test releases the gate");
                        std::future::poll_fn(move |context| {
                            if polls.fetch_add(1, Ordering::Relaxed) + 1 == 8 {
                                std::task::Poll::Ready(())
                            } else {
                                context.waker().wake_by_ref();
                                std::task::Poll::Pending
                            }
                        })
                        .await;
                    }),
                ])
            }
        }

        let (control, gate) = crate::controlled_gate();
        let polls = Arc::new(AtomicUsize::new(0));
        let mut store = TestStore::with_step_budget(
            (),
            GatedWork {
                gate: Some(gate),
                polls: Arc::clone(&polls),
            },
            7,
        );
        block_on(async {
            let task = store
                .send(false)
                .await
                .expect("work waits outside the driver");
            control.wait_until_waiting().await.unwrap();
            store.advance(Duration::from_secs(10)).await.unwrap();
            let before = store.now();
            assert_eq!(
                before,
                MonotonicInstant::from_elapsed(Duration::from_secs(10))
            );
            assert_eq!(polls.load(Ordering::Relaxed), 0);
            // Public advance leaves its target active while the delayed action awaits receive.
            assert!(store.runtime.has_active_advance());

            control.release().unwrap();
            let error = store
                .assert_state_eq(())
                .await
                .expect_err("resuming the advance exceeds its seven-poll budget");
            let drive = error.drive_error().expect("original typed drive cause");
            assert!(drive.is_step_budget_exhausted());
            assert_eq!(drive.polls(), 7);
            assert_eq!(drive.step_budget(), 7);
            assert_eq!(drive.current_time(), before);
            let source = std::error::Error::source(&error)
                .and_then(|source| source.downcast_ref::<crate::DriveError>())
                .expect("typed drive source");
            assert!(std::ptr::eq(source, drive));
            assert_eq!(polls.load(Ordering::Relaxed), 7);
            assert_eq!(store.now(), before);
            // The target has already been reached, so clock checks alone cannot prove cleanup.
            assert!(!store.runtime.has_active_advance());

            let _response = store
                .receive(|response| *response)
                .await
                .expect("the pending action remains available after the failed resume");
            assert_eq!(polls.load(Ordering::Relaxed), 8);
            assert_eq!(store.now(), before);
            assert!(task.is_finished());
            store
                .finish()
                .await
                .expect("failed inspection leaves no assertion debt or abandoned work");
        });
    }

    #[test]
    #[expect(
        clippy::too_many_lines,
        reason = "the held actor, nested drive, and post-commit cleanup form one causal schedule"
    )]
    fn update_receipt_budget_exhausts_while_actor_serves_a_state_query() {
        use rca_core::{Feature, FeatureContext, Task};
        use std::future::Future as _;
        use std::task::{Context, Poll, Waker};

        enum Action {
            Background,
            Update,
        }

        struct Work {
            background: Option<crate::ControlledGate>,
            update: Option<crate::ControlledGate>,
            polls: Arc<AtomicUsize>,
        }

        impl Feature for Work {
            type State = usize;
            type Action = Action;

            fn update(
                &mut self,
                _: &mut usize,
                action: &Action,
                context: &mut FeatureContext<'_, usize, Action>,
            ) {
                match action {
                    Action::Background => {
                        let gate = self.background.take().expect("one background task");
                        let polls = Arc::clone(&self.polls);
                        context.effect(Effect::run(move |_| async move {
                            gate.await.unwrap();
                            std::future::poll_fn(move |cx| {
                                if polls.fetch_add(1, Ordering::SeqCst) + 1 == 8 {
                                    Poll::Ready(())
                                } else {
                                    cx.waker().wake_by_ref();
                                    Poll::Pending
                                }
                            })
                            .await;
                        }));
                    }
                    Action::Update => {
                        let gate = self.update.take().expect("one update task");
                        context.task(Task::run(move |context| async move {
                            gate.await.unwrap();
                            context.update(|value| *value += 1).await.unwrap();
                        }));
                    }
                }
            }
        }

        block_on(async {
            let (background_control, background_gate) = crate::controlled_gate();
            let (update_control, update_gate) = crate::controlled_gate();
            let polls = Arc::new(AtomicUsize::new(0));
            let mut store = TestStore::builder(
                0,
                Work {
                    background: Some(background_gate),
                    update: Some(update_gate),
                    polls: Arc::clone(&polls),
                },
            )
            .with_step_budget(7)
            .build();
            let background = store.send(Action::Background).await.unwrap();
            let update = store.send(Action::Update).await.unwrap();
            background_control.wait_until_waiting().await.unwrap();
            update_control.wait_until_waiting().await.unwrap();
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
            let runtime = store.runtime.clone();
            let gate = store.gate.clone();
            let boundary = gate.boundary().expect("selected update boundary");
            let owned = Arc::new(Mutex::new(store));
            let inside = Arc::clone(&owned);
            let committed = gate.clone();
            let observed = Arc::new(AtomicUsize::new(0));
            let recorded = Arc::clone(&observed);
            let observed_polls = Arc::clone(&polls);
            let mut query = Box::pin(
                gate.assert_state(move |state| {
                    assert_eq!(*state, 0);
                    let mut store = inside.lock().expect("test Store lock");
                    let origin_line = line!() + 1;
                    let mut receive = Box::pin(store.receive_update(|expected| {
                        *expected = 1;
                        background_control.release().unwrap();
                    }));
                    let mut context = Context::from_waker(Waker::noop());
                    let error = (0..16)
                        .find_map(|_| match receive.as_mut().poll(&mut context) {
                            Poll::Ready(Err(error)) => Some(error),
                            Poll::Ready(Ok(_)) => {
                                panic!("receipt cannot complete inside its actor")
                            }
                            Poll::Pending => None,
                        })
                        .expect("bounded nested drive must report budget exhaustion");
                    drop(receive);
                    let drive = error.drive_error().expect("original receipt drive failure");
                    assert!(drive.is_step_budget_exhausted());
                    assert_eq!((drive.polls(), drive.step_budget()), (7, 7));
                    assert_eq!(error.origin().file(), file!());
                    assert_eq!(error.origin().line(), origin_line);
                    assert_eq!(observed_polls.load(Ordering::SeqCst), 7);
                    assert!(
                        !committed.acknowledge(boundary),
                        "actor has not committed the update"
                    );
                    recorded.fetch_add(1, Ordering::SeqCst);
                })
                .finish(),
            );
            assert!(
                query
                    .as_mut()
                    .poll(&mut Context::from_waker(Waker::noop()))
                    .is_pending()
            );
            runtime
                .run_until_stalled()
                .expect("actor finishes after the nested drive");
            query.await.expect("state query completed");
            assert_eq!(observed.load(Ordering::SeqCst), 1);
            assert_eq!(polls.load(Ordering::SeqCst), 8);
            assert!(background.is_finished());
            assert!(update.is_finished());
            let mut final_state =
                Box::pin(gate.assert_state(|state| assert_eq!(*state, 1)).finish());
            assert!(
                final_state
                    .as_mut()
                    .poll(&mut Context::from_waker(Waker::noop()))
                    .is_pending()
            );
            runtime
                .run_until_stalled()
                .expect("committed state remains readable");
            final_state.await.expect("committed state query completed");
            let mut store = Arc::try_unwrap(owned)
                .expect("no retained Store clone")
                .into_inner()
                .unwrap();
            assert!(store.finish().await.unwrap_err().is_incomplete_assertion());
            catch_unwind(AssertUnwindSafe(|| drop(store)))
                .expect_err("post-release failure leaves explicit assertion debt");
        });
    }

    #[test]
    fn completion_preserves_pending_action_during_inspection() {
        use futures_util::FutureExt as _;

        struct EmitResponse;
        impl Reducer for EmitResponse {
            type State = usize;
            type Action = bool;

            fn reduce(
                &mut self,
                state: &mut usize,
                action: &bool,
                _: &mut ReduceContext<'_>,
            ) -> Effect<bool> {
                *state += 1;
                if *action {
                    Effect::none()
                } else {
                    Effect::send(true)
                }
            }
        }

        let mut store = TestStore::new(0, EmitResponse);
        let task = store
            .send_with(false, |state| *state = 1)
            .now_or_never()
            .expect("ready start needs no external wake")
            .expect("start commit");
        let gate = store.gate.clone();
        gate.inspect_pending(|response| {
            assert!(*response);
            let error = store
                .finish()
                .now_or_never()
                .expect("inspection must not strand completion")
                .expect_err("borrowed response is still pending work");
            let diagnostics = error.finish_diagnostics().expect("unfinished work");
            assert!(diagnostics.has_pending_action());
            assert_eq!(diagnostics.runtime().commit_version, 1);
            assert_eq!(diagnostics.runtime().live_task_ids, [0]);
        })
        .expect("the response is available for inspection");

        let _response = store
            .receive_with(|response| *response, |state| *state = 2)
            .now_or_never()
            .expect("restored response needs no external wake")
            .expect("inspection restores the response for exactly one commit");
        assert!(task.is_finished());
        store
            .finish()
            .now_or_never()
            .expect("completed work needs no external wake")
            .expect("clean finish after the response commits");
    }

    #[test]
    fn observation_prevents_automatic_completion() {
        let mut store = TestStore::new((), Feature);
        let observation = block_on(store.runtime.run(store.store.observe(|()| ())))
            .unwrap()
            .unwrap();
        let error = block_on(store.finish()).unwrap_err();
        assert_eq!(
            error
                .finish_diagnostics()
                .unwrap()
                .runtime()
                .observation_count,
            1
        );
        assert!(catch_unwind(AssertUnwindSafe(|| drop(store))).is_err());
        drop(observation);
    }

    #[test]
    fn pending_action_destructor_can_reenter_assertion_gate() {
        struct Action {
            gate: TransactionGate<(), Action>,
            drops: Arc<AtomicUsize>,
        }
        impl Drop for Action {
            fn drop(&mut self) {
                let _diagnostics = self.gate.latest_diagnostics();
                self.drops.fetch_add(1, Ordering::SeqCst);
            }
        }
        struct Send;
        impl Reducer for Send {
            type State = ();
            type Action = Action;
            fn reduce(
                &mut self,
                (): &mut (),
                action: &Action,
                _: &mut ReduceContext<'_>,
            ) -> Effect<Action> {
                Effect::send(Action {
                    gate: action.gate.clone(),
                    drops: Arc::clone(&action.drops),
                })
            }
        }
        let drops = Arc::new(AtomicUsize::new(0));
        let mut store = TestStore::new((), Send);
        let action = Action {
            gate: store.gate.clone(),
            drops: Arc::clone(&drops),
        };
        let _task = block_on(store.send(action)).unwrap();
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert!(catch_unwind(AssertUnwindSafe(|| drop(store))).is_err());
        assert_eq!(drops.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn completion_does_not_resume_partially_advanced_clock() {
        let mut store = TestStore::new((), Feature);
        let host = store.runtime.clone();
        host.advance(Duration::from_secs(1)).unwrap();
        host.start_advance(Duration::from_secs(9));
        let sleep = host.sleep_until(MonotonicInstant::from_elapsed(Duration::from_secs(2)));
        host.spawn_task(sleep);
        assert!(block_on(store.finish()).is_err());
        assert_eq!(
            host.now(),
            MonotonicInstant::from_elapsed(Duration::from_secs(1))
        );
        assert!(catch_unwind(AssertUnwindSafe(|| drop(store))).is_err());
        assert_eq!(
            host.now(),
            MonotonicInstant::from_elapsed(Duration::from_secs(1))
        );
        assert!(host.pending_sleeps().is_empty());
    }

    #[test]
    fn completion_budget_is_shared_and_cleanup_destroys_values_unlocked() {
        struct Probe(DeterministicRuntime, Arc<AtomicUsize>);
        impl Drop for Probe {
            fn drop(&mut self) {
                // Re-enters the scheduler lock while task destruction runs.
                assert_eq!(self.0.now(), MonotonicInstant::ZERO);
                self.1.fetch_add(1, Ordering::SeqCst);
            }
        }
        let mut store = TestStore::with_step_budget((), Feature, 7);
        let drops = Arc::new(AtomicUsize::new(0));
        let probe = Probe(store.runtime.clone(), Arc::clone(&drops));
        store.runtime.spawn_task(Box::pin(async move {
            let _probe = probe;
            std::future::poll_fn::<(), _>(|cx| {
                cx.waker().wake_by_ref();
                std::task::Poll::Pending
            })
            .await;
        }));
        let error = block_on(store.finish()).unwrap_err();
        assert_eq!(error.drive_error().unwrap().polls(), 7);
        assert!(catch_unwind(AssertUnwindSafe(|| drop(store))).is_err());
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn shutdown_does_not_reset_completion_budget() {
        struct MissingCapture;
        impl crate::CaptureKey for MissingCapture {
            type Value = ();
            type Error = ();
        }
        let mut store = TestStore::with_step_budget((), Feature, 2);
        let error = block_on(store.finish()).unwrap_err();
        let origin = error.origin();
        let display = error.to_string();
        let debug = format!("{error:?}");
        let cause = std::ptr::from_ref(error.drive_error().unwrap());
        // A typed recovery miss must preserve the unrelated operation's failure.
        let error = error.into_capture_error::<MissingCapture>().unwrap_err();
        assert!(std::ptr::eq(error.origin(), origin));
        assert_eq!(error.to_string(), display);
        assert_eq!(format!("{error:?}"), debug);
        let drive = error.drive_error().expect("original drive failure kind");
        assert!(std::ptr::eq(drive, cause));
        let source = std::error::Error::source(&error)
            .and_then(|source| source.downcast_ref::<crate::DriveError>())
            .expect("original typed drive source");
        assert!(std::ptr::eq(source, drive));
        assert_eq!(error.drive_error().unwrap().polls(), 2);
        assert!(catch_unwind(AssertUnwindSafe(|| drop(store))).is_err());
    }

    #[test]
    fn shutdown_cleanup_work_keeps_the_budget_and_can_finish_on_retry() {
        use futures_util::FutureExt as _;

        struct Cleanup {
            host: Arc<Mutex<Option<DeterministicRuntime>>>,
            drops: Arc<AtomicUsize>,
            polls: Arc<AtomicUsize>,
        }

        impl Reducer for Cleanup {
            type State = ();
            type Action = ();

            fn reduce(&mut self, (): &mut (), (): &(), _: &mut ReduceContext<'_>) -> Effect<()> {
                Effect::none()
            }
        }

        impl Drop for Cleanup {
            fn drop(&mut self) {
                self.drops.fetch_add(1, Ordering::SeqCst);
                let host = self.host.lock().unwrap().take().unwrap();
                let polls = Arc::clone(&self.polls);
                host.spawn_task(Box::pin(async move {
                    polls.fetch_add(1, Ordering::SeqCst);
                }));
            }
        }

        let host = Arc::new(Mutex::new(None));
        let drops = Arc::new(AtomicUsize::new(0));
        let polls = Arc::new(AtomicUsize::new(0));
        let mut store = TestStore::with_step_budget(
            (),
            Cleanup {
                host: Arc::clone(&host),
                drops: Arc::clone(&drops),
                polls: Arc::clone(&polls),
            },
            2,
        );
        *host.lock().unwrap() = Some(store.runtime.clone());
        store.runtime.run_until_stalled().unwrap();

        // Real actor teardown may enqueue work; acknowledging shutdown must not
        // reset the budget or mark completion before that work has been checked.
        let origin_line = line!() + 1;
        let completion = store.finish();
        let error = completion
            .now_or_never()
            .expect("ready-only completion is bounded")
            .expect_err("diagnostics and shutdown spent both polls");
        assert_eq!(error.origin().line(), origin_line);
        let drive = error.drive_error().expect("cleanup retains budget failure");
        assert!(drive.is_step_budget_exhausted());
        assert_eq!((drive.polls(), drive.step_budget()), (2, 2));
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert_eq!(polls.load(Ordering::SeqCst), 0);
        assert!(store.shutdown);
        assert!(!store.completed);
        assert_eq!(store.now(), MonotonicInstant::ZERO);

        store
            .finish()
            .now_or_never()
            .expect("one retained cleanup task finishes without external input")
            .expect("retry resumes cleanup without shutting the actor down twice");
        assert!(store.completed);
        assert_eq!(polls.load(Ordering::SeqCst), 1);
        drop(store);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert_eq!(polls.load(Ordering::SeqCst), 1);
        assert!(host.lock().unwrap().is_none());
    }

    #[test]
    fn debug_does_not_require_domain_or_snapshot_traits() {
        struct Opaque;
        struct Domain;
        impl Reducer for Domain {
            type State = Opaque;
            type Action = Opaque;
            fn reduce(
                &mut self,
                _: &mut Opaque,
                _: &Opaque,
                _: &mut ReduceContext<'_>,
            ) -> Effect<Opaque> {
                Effect::none()
            }
        }
        fn assert_debug<T: fmt::Debug>() {}
        assert_debug::<TestStore<Opaque, Opaque, Opaque>>();
        assert_debug::<TestStoreBuilder<Opaque, Opaque, Domain, Opaque>>();
    }

    #[test]
    fn pre_existing_unwind_never_polls_ready_user_work() {
        let polls = Arc::new(AtomicUsize::new(0));
        let count = Arc::clone(&polls);
        let panic = catch_unwind(AssertUnwindSafe(|| {
            let store = TestStore::new((), Feature);
            store.runtime.spawn_task(Box::pin(async move {
                count.fetch_add(1, Ordering::SeqCst);
            }));
            panic!("original failure");
        }))
        .unwrap_err();
        assert_eq!(panic.downcast_ref::<&str>(), Some(&"original failure"));
        assert_eq!(polls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn inspection_panic_still_releases_other_scheduler_tasks() {
        struct Probe(Arc<AtomicUsize>);
        impl Drop for Probe {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }
        let drops = Arc::new(AtomicUsize::new(0));
        let store = TestStore::new((), Feature);
        store.runtime.spawn_task(Box::pin(async {
            panic!("inspection panic");
        }));
        let probe = Probe(Arc::clone(&drops));
        store.runtime.spawn_task(Box::pin(async move {
            let _probe = probe;
            std::future::pending::<()>().await;
        }));
        let panic = catch_unwind(AssertUnwindSafe(|| drop(store))).unwrap_err();
        assert_eq!(panic.downcast_ref::<&str>(), Some(&"inspection panic"));
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }

    #[test]
    #[expect(
        clippy::too_many_lines,
        reason = "the coordinated retirement schedule and cleanup must remain auditable together"
    )]
    fn terminal_acknowledgement_rejects_concurrent_actor_retirement() {
        use std::sync::atomic::AtomicBool;
        use std::sync::mpsc;
        use std::task::{Context, Wake, Waker};

        const WAIT: Duration = Duration::from_secs(5);

        #[derive(Clone, Copy)]
        enum Action {
            StartPanic,
            StartTerminal,
            Apply,
        }
        struct Work {
            trigger: Option<crate::ControlledFuture<()>>,
        }
        impl Reducer for Work {
            type State = ();
            type Action = Action;

            fn reduce(
                &mut self,
                (): &mut (),
                action: &Action,
                _: &mut ReduceContext<'_>,
            ) -> Effect<Action> {
                match action {
                    Action::StartPanic => {
                        let trigger = self.trigger.take().expect("one panic task");
                        Effect::run(move |_| async move {
                            trigger.await.expect("test releases the real effect");
                            panic!("terminal retirement effect panic");
                        })
                    }
                    Action::StartTerminal => Effect::send(Action::Apply),
                    Action::Apply => Effect::none(),
                }
            }
        }

        struct QueryCapture {
            entered: mpsc::Sender<()>,
            release: mpsc::Receiver<()>,
            drops: Arc<AtomicUsize>,
            timed_out: Arc<AtomicBool>,
        }
        impl Drop for QueryCapture {
            fn drop(&mut self) {
                self.drops.fetch_add(1, Ordering::SeqCst);
                let _ = self.entered.send(());
                // This destructor runs during the actor's unwind: never panic here.
                if self.release.recv_timeout(WAIT).is_err() {
                    self.timed_out.store(true, Ordering::SeqCst);
                }
            }
        }

        struct AfterInspection {
            start: mpsc::Sender<()>,
            entered: Mutex<mpsc::Receiver<()>>,
            started: AtomicBool,
            held: AtomicBool,
        }
        impl Wake for AfterInspection {
            fn wake(self: Arc<Self>) {
                self.wake_by_ref();
            }

            fn wake_by_ref(self: &Arc<Self>) {
                if !self.started.swap(true, Ordering::SeqCst) {
                    let _ = self.start.send(());
                    let entered = self
                        .entered
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    self.held
                        .store(entered.recv_timeout(WAIT).is_ok(), Ordering::SeqCst);
                }
            }
        }

        let (trigger, effect_wait) = crate::controlled_future();
        let mut store = TestStore::new(
            (),
            Work {
                trigger: Some(effect_wait),
            },
        );
        let task = block_on(store.send(Action::StartPanic)).unwrap();
        let terminal_task = block_on(store.send(Action::StartTerminal)).unwrap();
        block_on(terminal_task.cancel()).unwrap();
        block_on(terminal_task.finish()).unwrap();
        let boundary = store.pending_boundary().expect("cancelled selected action");
        let gate = store.gate.clone();
        let boundary_id = gate.boundary();

        trigger.complete(()).unwrap();
        catch_unwind(AssertUnwindSafe(|| block_on(task.finish())))
            .expect_err("the effect task must propagate its panic");
        // The effect's panic made the actor ready. Do not drive it before arming
        // the real query: process_control must retire it, not execute its callback.
        let (start, begin) = mpsc::channel();
        let (entered, entry) = mpsc::channel();
        let (release, resume) = mpsc::channel();
        let drops = Arc::new(AtomicUsize::new(0));
        let timed_out = Arc::new(AtomicBool::new(false));
        let query_ran = Arc::new(AtomicBool::new(false));
        let worker = std::thread::spawn(move || {
            if begin.recv_timeout(WAIT).is_err() {
                return false;
            }
            catch_unwind(AssertUnwindSafe(|| block_on(task.finish()))).is_err()
        });
        // Always release and join the worker before propagating any assertion
        // failure. Neither its wake callback nor the query destructor panics.
        let observed = catch_unwind(AssertUnwindSafe(|| {
            let capture = QueryCapture {
                entered,
                release: resume,
                drops: Arc::clone(&drops),
                timed_out: Arc::clone(&timed_out),
            };
            let called = Arc::clone(&query_ran);
            let mut query = Box::pin(
                gate.assert_state(move |()| {
                    called.store(true, Ordering::SeqCst);
                    drop(capture);
                })
                .finish(),
            );
            assert!(
                query
                    .as_mut()
                    .poll(&mut Context::from_waker(Waker::noop()))
                    .is_pending()
            );

            let wake = Arc::new(AfterInspection {
                start: start.clone(),
                entered: Mutex::new(entry),
                started: AtomicBool::new(false),
                held: AtomicBool::new(false),
            });
            let waker = Waker::from(Arc::clone(&wake));
            let mut changed = Box::pin(gate.wait_for_pending());
            assert!(
                changed
                    .as_mut()
                    .poll(&mut Context::from_waker(&waker))
                    .is_pending()
            );

            let mut matched = false;
            // Private helper access avoids the outer settle consuming our query.
            // Its own matcher-inspection drop wakes AfterInspection only after
            // releasing the gate mutex and the inspection token.
            let result = store.receive_terminal_checked(
                boundary.clone(),
                |error| {
                    matched = error.reason() == rca_core::TaskAccessReason::Cancelled;
                    matched
                },
                Location::caller(),
            );
            assert!(
                matched,
                "the actual terminal matcher accepted this boundary"
            );
            assert!(
                wake.held.load(Ordering::SeqCst),
                "real query destruction holds retirement"
            );
            assert!(result.unwrap_err().is_no_received_action());
            assert_eq!(
                gate.boundary(),
                boundary_id,
                "failed acknowledgement retains the debt"
            );
            assert!(!query_ran.load(Ordering::SeqCst));
        }));

        let _ = release.send(());
        let _ = start.send(());
        let worker_result = worker.join();
        let retired = catch_unwind(AssertUnwindSafe(|| {
            assert!(worker_result.expect("worker catches the actor panic"));
            assert_eq!(drops.load(Ordering::SeqCst), 1);
            assert!(!timed_out.load(Ordering::SeqCst));
            assert!(!query_ran.load(Ordering::SeqCst));
            observed.unwrap_or_else(|panic| std::panic::resume_unwind(panic));
            block_on(store.receive_terminal(boundary, |_| true))
                .expect("completed retirement permits the same terminal acknowledgement");
            assert!(store.pending_boundary().is_none());
            assert!(
                block_on(store.finish()).is_err(),
                "actor teardown is not graceful completion"
            );
        }));
        // Aborted actors legitimately fail TestStore's completion-on-drop check.
        let cleanup = catch_unwind(AssertUnwindSafe(|| drop(store)));
        retired.unwrap_or_else(|panic| std::panic::resume_unwind(panic));
        assert!(cleanup.is_err());
    }
}
