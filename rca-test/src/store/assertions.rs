//! Captures and compares independent snapshots at production commit boundaries.

use std::fmt;
use std::panic::{AssertUnwindSafe, Location};
use std::sync::Arc;
#[cfg(test)]
use std::sync::Mutex;

use rca_core::diagnostics::format_snapshot_mismatch;

#[cfg(test)]
use super::TestStore;
use super::obligations::{CaptureFamily, CapturedValues, DeclaredCaptures, Shared};
use std::ops::Deref;

#[cfg(test)]
mod query_tests;

/// Chooses the actor query or ordered read before constructing its driver future.
///
/// Callers supply borrowing constructors, so the unused constructor owns no user
/// payload. Each branch retains its concrete future without another allocation.
pub(super) fn state_receipt<A, Q, R>(
    gated: bool,
    assertion: A,
    query: impl FnOnce(A) -> Q,
    read: impl FnOnce(A) -> R,
) -> futures_util::future::Either<Q, R>
where
    Q: Future,
    R: Future<Output = Q::Output>,
{
    // Select before constructing Drive: each branch keeps its existing boxed
    // client future, without an additional erased future or runtime protocol.
    if gated {
        futures_util::future::Either::Left(query(assertion))
    } else {
        futures_util::future::Either::Right(read(assertion))
    }
}

/// Describes which part of production state the root snapshot can verify.
pub(super) enum Coverage {
    /// Captures the complete root state.
    Full,
    /// Captures only the caller-selected historical value.
    Projected,
}

impl Coverage {
    /// Makes the snapshot's coverage limit explicit in mismatch diagnostics.
    fn label(&self) -> &'static str {
        match self {
            Self::Full => "full-state coverage",
            Self::Projected => "projected coverage (exhaustive over the selected snapshot only)",
        }
    }
}

/// Selects transferable or owner-local callback storage without changing policy.
pub(super) trait AssertionFamily: CaptureFamily {
    /// Captures an owned root snapshot from actor-owned state.
    type CaptureFn<State, Snapshot>: ?Sized + Fn(&State) -> Snapshot;
    /// Shares the callback with a queued actor read without sharing its result.
    type Capture<State, Snapshot>: Clone + Deref<Target = Self::CaptureFn<State, Snapshot>>;
    /// Formats a mismatch only after authoritative snapshot equality fails.
    type Renderer<Snapshot>: ?Sized + Fn(&Snapshot) -> String;
}
impl AssertionFamily for Shared {
    type CaptureFn<S, T> = dyn Fn(&S) -> T + Send + Sync;
    type Capture<S, T> = Arc<Self::CaptureFn<S, T>>;
    type Renderer<T> = dyn Fn(&T) -> String + Send + Sync;
}
#[cfg(feature = "local")]
impl AssertionFamily for super::obligations::Local {
    type CaptureFn<S, T> = dyn Fn(&S) -> T;
    type Capture<S, T> = std::rc::Rc<Self::CaptureFn<S, T>>;
    type Renderer<T> = dyn Fn(&T) -> String;
}

/// Retains verified history and unfinished expectations across actor operations.
///
/// The non-poisoning mutexes supply Sync without requiring `Snapshot: Sync`.
/// All access is exclusive through `get_mut`; user Clone, equality and Drop run
/// without a lock guard. The envelopes preserve the previous unwind traits.
pub(super) struct Assertions<State, Snapshot, F: AssertionFamily = Shared> {
    /// Initial or last installed root history used to prepare an expectation.
    pub(super) baseline: AssertUnwindSafe<parking_lot::Mutex<Snapshot>>,
    /// Expected root state retained after admission until verification succeeds.
    pub(super) pending: AssertUnwindSafe<parking_lot::Mutex<Option<AssertionObligation<Snapshot>>>>,
    /// Actor-side root capture, shared with individual inspection commands.
    pub(super) capture: F::Capture<State, Snapshot>,
    /// Optional mismatch renderer; absent means pretty Debug formatting.
    pub(super) render: Option<Box<F::Renderer<Snapshot>>>,
    /// Whether the root snapshot covers all state or a selected projection.
    pub(super) coverage: Coverage,
    /// Independently declared values checked alongside the root snapshot.
    pub(super) declared: DeclaredCaptures<State, F>,
}

/// Owns one admitted root expectation and its original failure location.
pub(super) struct AssertionObligation<Snapshot> {
    /// Independent strict expectation; explicitly omitted changes have no expected value.
    expected: Option<Snapshot>,
    /// Public operation's call site, preserved across actor driving and retries.
    pub(super) origin: &'static Location<'static>,
    /// Public operation name used when reporting unfinished work.
    pub(super) operation: &'static str,
}

impl<State, Snapshot, F: AssertionFamily> Assertions<State, Snapshot, F> {
    /// Reports root debt first, then independently declared capture debt.
    pub(super) fn incomplete_error(&mut self) -> Option<F::Error> {
        self.pending
            .get_mut()
            .as_ref()
            .map(|pending| {
                F::error(crate::error::TestStoreFailure::incomplete_assertion(
                    pending.operation,
                    pending.origin,
                ))
            })
            .or_else(|| self.declared.incomplete_error())
    }

    /// Installs the expectation after public preflight has excluded earlier debt.
    pub(super) fn begin(
        &mut self,
        expected: Snapshot,
        origin: &'static Location<'static>,
        operation: &'static str,
    ) {
        let pending = self.pending.get_mut();
        debug_assert!(
            pending.is_none(),
            "unfinished assertion must not be overwritten"
        );
        *pending = Some(AssertionObligation {
            expected: Some(expected),
            origin,
            operation,
        });
    }

    /// Retains an omitted transaction's obligation without cloning unused history.
    pub(super) fn begin_omission(
        &mut self,
        origin: &'static Location<'static>,
        operation: &'static str,
    ) {
        let pending = self.pending.get_mut();
        debug_assert!(
            pending.is_none(),
            "unfinished assertion must not be overwritten"
        );
        *pending = Some(AssertionObligation {
            expected: None,
            origin,
            operation,
        });
    }
}

impl<State, Snapshot: Clone, F: AssertionFamily> Assertions<State, Snapshot, F> {
    /// Clones verified history and edits it once, before admitting production work.
    pub(super) fn expected(&mut self, update: impl FnOnce(&mut Snapshot)) -> Snapshot {
        let mut expected = self.baseline.get_mut().clone();
        update(&mut expected);
        expected
    }
}

impl<State, Snapshot: fmt::Debug + PartialEq, F: AssertionFamily> Assertions<State, Snapshot, F> {
    /// Compares all captures before installation, then acknowledges the boundary.
    ///
    /// User comparisons, rendering and value destruction may panic. Existing
    /// capture or actor-boundary debt remains until those operations complete.
    pub(super) fn verify(
        &mut self,
        actual: Snapshot,
        declared: CapturedValues<State, F>,
        diagnostics: impl FnOnce() -> Option<rca_core::testing::ActorDiagnostics>,
        acknowledge: impl FnOnce(),
    ) -> Result<(), F::Error> {
        if let Err(error) = DeclaredCaptures::compare(&declared) {
            // Preserve parameter cleanup order with the error still local. A
            // user destructor must not strand an error in the return slot.
            drop(acknowledge);
            drop(diagnostics);
            drop(declared);
            drop(actual);
            return Err(error);
        }
        let pending = self.pending.get_mut();
        let obligation = pending
            .as_ref()
            .expect("snapshot comparison requires an admitted obligation");
        let expected = obligation
            .expected
            .as_ref()
            .expect("an active comparison owns its expectation");
        let origin = obligation.origin;
        if *expected != actual {
            // Equality remains authoritative. Both callbacks run without lock guards;
            // render before panic machinery, retaining the obligation if rendering panics.
            let (expected, actual) = if let Some(render) = &self.render {
                (render(expected), render(&actual))
            } else {
                (format!("{expected:#?}"), format!("{actual:#?}"))
            };
            let excerpt = format_snapshot_mismatch(&expected, &actual);
            let message = format!(
                "TestStore state mismatch at {origin} during {}: {}\n{excerpt}\nruntime: {:?}",
                obligation.operation,
                self.coverage.label(),
                diagnostics()
            );
            panic!("{message}");
        }
        self.declared.install(declared);
        *self.baseline.get_mut() = actual;
        // The store retains the obligation through cancellation, errors and comparison panics.
        // Remove it only after equality succeeded and the verified baseline was installed.
        pending.take();
        self.declared.complete();
        acknowledge();
        Ok(())
    }

    /// Describes an explicit omission without formatting excluded state or capture values.
    pub(super) fn omission_description(
        &mut self,
        actual: &Snapshot,
        declared: &CapturedValues<State, F>,
    ) -> String {
        let previous = self.baseline.get_mut();
        let changes = if previous == actual {
            "selected snapshot unchanged".to_owned()
        } else {
            let (previous, actual) = if let Some(render) = &self.render {
                (render(previous), render(actual))
            } else {
                (format!("{previous:#?}"), format!("{actual:#?}"))
            };
            format_snapshot_mismatch(&previous, &actual)
        };
        let captures_changed = DeclaredCaptures::compare(declared).is_err();
        format!(
            "{}; {changes}; declared captures changed: {captures_changed}",
            self.coverage.label()
        )
    }

    /// Adopts an explicitly omitted boundary while retaining debt through payload destruction.
    pub(super) fn adopt_omission(&mut self, actual: Snapshot, declared: CapturedValues<State, F>) {
        self.declared.install(declared);
        *self.baseline.get_mut() = actual;
        self.declared.complete();
        // Old history has been destroyed; only now can the omitted obligation retire.
        self.pending.get_mut().take();
    }
}

#[cfg(test)]
mod interruption_tests {
    use std::error::Error as _;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::task::{Context, Poll, Waker};

    use futures_executor::block_on;
    use rca_core::{
        Effect, MonotonicInstant, ReduceContext, Reducer, Runtime, RuntimeDriver, RuntimeFuture,
        Store,
    };

    use super::*;
    use crate::{DeterministicRuntime, Exhaustivity};

    #[derive(Debug, Default)]
    struct Pause {
        paused: AtomicBool,
        waker: Mutex<Option<Waker>>,
    }

    impl Pause {
        fn resume(&self) {
            self.paused.store(false, Ordering::SeqCst);
            let waker = self.waker.lock().unwrap().take();
            if let Some(waker) = waker {
                waker.wake();
            }
        }
    }

    #[derive(Debug)]
    struct PausedDriver {
        driver: crate::runtime::DeterministicDriver,
        pause: Arc<Pause>,
    }

    impl RuntimeDriver for PausedDriver {
        fn spawn(&self, mut task: RuntimeFuture) {
            let pause = Arc::clone(&self.pause);
            self.driver.spawn(Box::pin(std::future::poll_fn(move |cx| {
                if pause.paused.load(Ordering::SeqCst) {
                    *pause.waker.lock().unwrap() = Some(cx.waker().clone());
                    Poll::Pending
                } else {
                    task.as_mut().poll(cx)
                }
            })));
        }
        fn now(&self) -> MonotonicInstant {
            self.driver.now()
        }
        fn sleep_until(&self, deadline: MonotonicInstant) -> RuntimeFuture {
            self.driver.sleep_until(deadline)
        }
        fn yield_now(&self) -> RuntimeFuture {
            self.driver.yield_now()
        }
    }

    enum Action {
        Start,
        Increment,
    }

    struct Feature {
        pause: Arc<Pause>,
        commits: Arc<AtomicUsize>,
    }

    impl Reducer for Feature {
        type State = u32;
        type Action = Action;
        fn reduce(
            &mut self,
            state: &mut u32,
            action: &Action,
            _: &mut ReduceContext<'_>,
        ) -> Effect<Action> {
            match action {
                Action::Start => Effect::send(Action::Increment),
                Action::Increment => {
                    *state += 1;
                    self.commits.fetch_add(1, Ordering::SeqCst);
                    self.pause.paused.store(true, Ordering::SeqCst);
                    Effect::none()
                }
            }
        }
    }

    // The real production actor runs on a controllably paused scheduler. No public hook.
    fn paused_store(pause: &Arc<Pause>, commits: &Arc<AtomicUsize>) -> TestStore<u32, Action> {
        paused_store_with_step_budget(pause, commits, crate::DEFAULT_STEP_BUDGET)
    }

    fn paused_store_with_step_budget(
        pause: &Arc<Pause>,
        commits: &Arc<AtomicUsize>,
        step_budget: usize,
    ) -> TestStore<u32, Action> {
        let runtime = DeterministicRuntime::with_step_budget(step_budget);
        let gate = rca_core::testing::TransactionGate::new();
        let issues = rca_core::testing::IssueRecorder::new();
        let store = Store::builder(
            0,
            Feature {
                pause: Arc::clone(pause),
                commits: Arc::clone(commits),
            },
            Runtime::new(PausedDriver {
                driver: runtime.driver.clone(),
                pause: Arc::clone(pause),
            }),
        )
        .transaction_gate(gate.clone())
        .issue_recorder(issues.clone())
        .build()
        .unwrap();
        TestStore {
            completed: false,
            shutdown: false,
            store,
            runtime,
            gate,
            issues,
            omissions: Vec::new(),
            mode: Exhaustivity::NonExhaustive,
            assertions: Assertions {
                baseline: AssertUnwindSafe(parking_lot::Mutex::new(0)),
                pending: AssertUnwindSafe(parking_lot::Mutex::new(None)),
                capture: Arc::new(|s| *s),
                render: None,
                coverage: Coverage::Full,
                declared: DeclaredCaptures::default(),
            },
        }
    }

    fn bounded_ready<F: Future>(future: F) -> F::Output {
        let mut future = std::pin::pin!(future);
        let mut context = Context::from_waker(Waker::noop());
        for _ in 0..64 {
            if let Poll::Ready(output) = future.as_mut().poll(&mut context) {
                return output;
            }
        }
        panic!("finite test operation remained pending");
    }

    fn spawn_finite_work(runtime: &DeterministicRuntime, polls: &Arc<AtomicUsize>) {
        let polls = Arc::clone(polls);
        runtime.spawn_task(Box::pin(std::future::poll_fn(move |cx| {
            if polls.fetch_add(1, Ordering::SeqCst) + 1 == 8 {
                Poll::Ready(())
            } else {
                cx.waker().wake_by_ref();
                Poll::Pending
            }
        })));
    }

    struct CallbackDrop(Arc<AtomicUsize>);

    impl Drop for CallbackDrop {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn actor_loss_after_send_or_receive_admission_retains_assertion_debt() {
        for receive in [false, true] {
            let pause = Arc::new(Pause::default());
            let commits = Arc::new(AtomicUsize::new(0));
            let mut store = paused_store(&pause, &commits);
            let root = receive.then(|| bounded_ready(store.send(Action::Start)).unwrap());
            let core_store = store.store.clone();
            pause.paused.store(true, Ordering::SeqCst);
            let origin_line;
            let error = {
                let mut future: std::pin::Pin<Box<dyn Future<Output = _> + '_>> = if receive {
                    origin_line = line!() + 1;
                    Box::pin(store.receive_with(|_| true, |state| *state = 1))
                } else {
                    origin_line = line!() + 1;
                    Box::pin(store.send_with(Action::Increment, |state| *state = 1))
                };
                assert!(
                    future
                        .as_mut()
                        .poll(&mut Context::from_waker(Waker::noop()))
                        .is_pending()
                );
                core_store.abort_for_testing();
                pause.resume();
                bounded_ready(future).unwrap_err()
            };
            assert!(
                error
                    .source()
                    .unwrap()
                    .downcast_ref::<rca_core::StoreStopped>()
                    .is_some()
            );
            assert!(error.drive_error().is_none());
            assert_eq!(error.origin().file(), file!());
            assert_eq!(error.origin().line(), origin_line);
            assert_eq!(commits.load(Ordering::SeqCst), 0);
            if let Some(root) = root {
                assert!(root.is_finished());
            }
            let obligation = store.assertions.pending.get_mut().as_ref().unwrap();
            assert_eq!(obligation.expected, Some(1));
            assert_eq!(
                obligation.operation,
                if receive { "receive" } else { "send" }
            );
            assert_eq!(*store.assertions.baseline.get_mut(), 0);
            let debt = bounded_ready(store.finish()).unwrap_err();
            assert!(debt.is_incomplete_assertion());
            assert_eq!(debt.origin(), error.origin());
            assert!(
                bounded_ready(store.receive(|_| panic!("debt must refuse the matcher")))
                    .unwrap_err()
                    .is_incomplete_assertion()
            );
            assert!(
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(store))).is_err()
            );
        }
    }

    #[test]
    fn actor_loss_drops_queued_gate_assertion_without_running_callback() {
        let pause = Arc::new(Pause::default());
        let commits = Arc::new(AtomicUsize::new(0));
        let mut store = paused_store(&pause, &commits);
        let root = bounded_ready(store.send(Action::Start)).unwrap();
        let core_store = store.store.clone();
        let calls = Arc::new(AtomicUsize::new(0));
        let drops = Arc::new(AtomicUsize::new(0));
        let callback_calls = Arc::clone(&calls);
        let probe = CallbackDrop(Arc::clone(&drops));
        pause.paused.store(true, Ordering::SeqCst);
        let origin_line = line!() + 1;
        let mut assertion = Box::pin(store.assert_state(move |_| {
            let _probe = probe;
            callback_calls.fetch_add(1, Ordering::SeqCst);
        }));
        assert!(
            assertion
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending()
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        core_store.abort_for_testing();
        pause.resume();
        let error = bounded_ready(assertion).unwrap_err();
        assert!(
            error
                .source()
                .unwrap()
                .downcast_ref::<rca_core::StoreStopped>()
                .is_some()
        );
        assert!(error.drive_error().is_none());
        assert_eq!(error.origin().file(), file!());
        assert_eq!(error.origin().line(), origin_line);
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert_eq!(commits.load(Ordering::SeqCst), 0);
        assert!(root.is_finished());
        assert!(!store.gate.has_pending());
        assert!(store.assertions.incomplete_error().is_none());
        assert_eq!(*store.assertions.baseline.get_mut(), 0);
        assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(store))).is_err());
    }

    #[test]
    fn receive_settle_failure_retains_committed_state_and_capture_debt() {
        struct RootCapture;
        impl crate::CaptureKey for RootCapture {
            type Value = u32;
            type Error = std::convert::Infallible;
        }
        // Public construction and operations; private fields only inspect retained obligations.
        let commits = Arc::new(AtomicUsize::new(0));
        let reducer_commits = Arc::clone(&commits);
        let work_polls = Arc::new(AtomicUsize::new(0));
        let reducer_polls = Arc::clone(&work_polls);
        let reducer = rca_core::Reduce::new(
            move |state: &mut u32, action: &Action, _: &mut ReduceContext<'_>| match action {
                Action::Start => Effect::send(Action::Increment),
                Action::Increment => {
                    *state += 1;
                    reducer_commits.fetch_add(1, Ordering::SeqCst);
                    let polls = Arc::clone(&reducer_polls);
                    Effect::run(move |_| async move {
                        std::future::poll_fn(move |cx| {
                            if polls.fetch_add(1, Ordering::SeqCst) + 1 == 8 {
                                Poll::Ready(())
                            } else {
                                cx.waker().wake_by_ref();
                                Poll::Pending
                            }
                        })
                        .await;
                    })
                }
            },
        );
        let calls = Arc::new(AtomicUsize::new(0));
        let capture_calls = Arc::clone(&calls);
        let mut store = TestStore::builder(0, reducer)
            .with_step_budget(7)
            .with_capture::<RootCapture>(move |state| {
                capture_calls.fetch_add(1, Ordering::SeqCst);
                Ok(*state)
            })
            .unwrap()
            .build();
        let root = bounded_ready(store.send(Action::Start)).unwrap();
        assert_eq!(commits.load(Ordering::SeqCst), 0);
        assert_eq!(work_polls.load(Ordering::SeqCst), 0);
        assert_eq!(calls.load(Ordering::SeqCst), 3);
        let origin_line = line!() + 1;
        let error = bounded_ready(store.receive_with_captures(
            |action| matches!(action, Action::Increment),
            |state| *state = 1,
            |expected| expected.set::<RootCapture>(1),
        ))
        .unwrap_err();
        let drive = error.drive_error().unwrap();
        assert!(drive.is_step_budget_exhausted());
        assert_eq!((drive.polls(), drive.step_budget()), (7, 7));
        assert!(std::ptr::eq(
            error
                .source()
                .unwrap()
                .downcast_ref::<crate::DriveError>()
                .unwrap(),
            drive
        ));
        assert_eq!(error.origin().file(), file!());
        assert_eq!(error.origin().line(), origin_line);
        assert_eq!(commits.load(Ordering::SeqCst), 1);
        assert!(!root.is_finished());
        // Build, send preflight/postflight and receive preflight ran; post-receive capture did not.
        assert_eq!(calls.load(Ordering::SeqCst), 4);
        let obligation = store.assertions.pending.get_mut().as_ref().unwrap();
        assert_eq!(obligation.expected, Some(1));
        assert_eq!(obligation.operation, "receive");
        assert_eq!(*store.assertions.baseline.get_mut(), 0);
        let declared_debt = store.assertions.declared.incomplete_error().unwrap();
        assert!(declared_debt.is_incomplete_assertion());
        assert_eq!(declared_debt.origin(), error.origin());
        let debt = bounded_ready(store.finish()).unwrap_err();
        assert!(debt.is_incomplete_assertion());
        assert_eq!(debt.origin(), error.origin());

        // A fresh bounded inspection finishes the finite effect but cannot discharge either debt.
        bounded_ready(store.assert_state(|state| assert_eq!(*state, 1))).unwrap();
        assert_eq!(work_polls.load(Ordering::SeqCst), 8);
        assert!(root.is_finished());
        assert_eq!(commits.load(Ordering::SeqCst), 1);
        assert_eq!(calls.load(Ordering::SeqCst), 4);
        assert_eq!(*store.assertions.baseline.get_mut(), 0);
        assert!(store.assertions.declared.incomplete_error().is_some());
        let debt = bounded_ready(store.finish()).unwrap_err();
        assert!(debt.is_incomplete_assertion());
        assert_eq!(debt.origin(), error.origin());
        assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(store))).is_err());
    }

    #[test]
    fn receive_drive_failure_retains_released_action_and_assertion_debt() {
        let pause = Arc::new(Pause::default());
        let commits = Arc::new(AtomicUsize::new(0));
        let mut store = paused_store_with_step_budget(&pause, &commits, 7);
        let root = bounded_ready(store.send(Action::Start)).unwrap();
        let runtime = store.runtime.clone();
        let work_polls = Arc::new(AtomicUsize::new(0));
        let matches = AtomicUsize::new(0);
        let updates = AtomicUsize::new(0);
        pause.paused.store(true, Ordering::SeqCst);
        let origin_line = line!() + 1;
        let mut receive = Box::pin(store.receive_with(
            |_| {
                matches.fetch_add(1, Ordering::SeqCst);
                true
            },
            |state| {
                updates.fetch_add(1, Ordering::SeqCst);
                *state = 1;
            },
        ));
        assert!(
            receive
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending()
        );
        // The first poll reached the admitted receipt. Only now add competing work.
        spawn_finite_work(&runtime, &work_polls);
        let error = bounded_ready(receive).unwrap_err();
        let drive = error.drive_error().unwrap();
        assert!(drive.is_step_budget_exhausted());
        assert_eq!((drive.polls(), drive.step_budget()), (7, 7));
        assert!(std::ptr::eq(
            error
                .source()
                .unwrap()
                .downcast_ref::<crate::DriveError>()
                .unwrap(),
            drive
        ));
        assert_eq!(error.origin().file(), file!());
        assert_eq!(error.origin().line(), origin_line);
        assert_eq!(runtime.now(), MonotonicInstant::ZERO);
        assert_eq!(work_polls.load(Ordering::SeqCst), 6);
        assert_eq!(matches.load(Ordering::SeqCst), 1);
        assert_eq!(updates.load(Ordering::SeqCst), 1);
        assert_eq!(commits.load(Ordering::SeqCst), 0);
        assert!(!root.is_finished());
        let obligation = store.assertions.pending.get_mut().as_ref().unwrap();
        assert_eq!(obligation.expected, Some(1));
        assert_eq!(obligation.operation, "receive");
        assert_eq!(*store.assertions.baseline.get_mut(), 0);
        let debt = bounded_ready(store.finish()).unwrap_err();
        assert!(debt.is_incomplete_assertion());
        assert_eq!(debt.origin(), error.origin());

        pause.resume();
        runtime.run_until_stalled().unwrap();
        assert_eq!(work_polls.load(Ordering::SeqCst), 8);
        assert_eq!(commits.load(Ordering::SeqCst), 1);
        // Increment pauses this fixture again; let its sender finish and the actor
        // collect causal completion without committing the released action twice.
        pause.resume();
        runtime.run_until_stalled().unwrap();
        assert!(root.is_finished());
        assert_eq!(commits.load(Ordering::SeqCst), 1);
        assert_eq!(matches.load(Ordering::SeqCst), 1);
        assert_eq!(updates.load(Ordering::SeqCst), 1);
        assert_eq!(*store.assertions.baseline.get_mut(), 0);
        let debt = bounded_ready(store.finish()).unwrap_err();
        assert!(debt.is_incomplete_assertion());
        assert_eq!(debt.origin(), error.origin());
        assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(store))).is_err());
    }

    #[test]
    fn state_assertion_drive_failure_preserves_queued_callback_ownership() {
        for (gated, eventual_calls) in [(false, 1), (true, 0)] {
            let pause = Arc::new(Pause::default());
            let commits = Arc::new(AtomicUsize::new(0));
            let mut store = paused_store_with_step_budget(&pause, &commits, 7);
            if gated {
                let _root = bounded_ready(store.send(Action::Start)).unwrap();
            } else {
                store.runtime.run_until_stalled().unwrap();
            }
            let runtime = store.runtime.clone();
            let work_polls = Arc::new(AtomicUsize::new(0));
            let calls = Arc::new(AtomicUsize::new(0));
            let drops = Arc::new(AtomicUsize::new(0));
            let callback_calls = Arc::clone(&calls);
            let probe = CallbackDrop(Arc::clone(&drops));
            pause.paused.store(true, Ordering::SeqCst);
            let origin_line = line!() + 1;
            let mut assertion = Box::pin(store.assert_state(move |state| {
                let _probe = probe;
                assert_eq!(*state, 0);
                callback_calls.fetch_add(1, Ordering::SeqCst);
            }));
            assert!(
                assertion
                    .as_mut()
                    .poll(&mut Context::from_waker(Waker::noop()))
                    .is_pending()
            );
            assert_eq!(calls.load(Ordering::SeqCst), 0);
            assert_eq!(drops.load(Ordering::SeqCst), 0);
            spawn_finite_work(&runtime, &work_polls);
            let error = bounded_ready(assertion).unwrap_err();
            let drive = error.drive_error().unwrap();
            assert!(drive.is_step_budget_exhausted());
            assert_eq!((drive.polls(), drive.step_budget()), (7, 7));
            assert!(std::ptr::eq(
                error
                    .source()
                    .unwrap()
                    .downcast_ref::<crate::DriveError>()
                    .unwrap(),
                drive
            ));
            assert_eq!(error.origin().file(), file!());
            assert_eq!(error.origin().line(), origin_line);
            assert_eq!(runtime.now(), MonotonicInstant::ZERO);
            assert_eq!(work_polls.load(Ordering::SeqCst), 6);
            assert_eq!(calls.load(Ordering::SeqCst), 0);
            assert_eq!(drops.load(Ordering::SeqCst), 0);
            assert_eq!(commits.load(Ordering::SeqCst), 0);
            assert!(store.assertions.incomplete_error().is_none());
            assert_eq!(*store.assertions.baseline.get_mut(), 0);

            pause.resume();
            runtime.run_until_stalled().unwrap();
            assert_eq!(work_polls.load(Ordering::SeqCst), 8);
            // A cancelled gate assertion skips its callback; an admitted read still runs it.
            assert_eq!(calls.load(Ordering::SeqCst), eventual_calls);
            assert_eq!(drops.load(Ordering::SeqCst), 1);
            assert_eq!(commits.load(Ordering::SeqCst), 0);
            assert_eq!(store.gate.has_pending(), gated);
            bounded_ready(store.assert_state(|state| assert_eq!(*state, 0))).unwrap();
            if gated {
                bounded_ready(store.skip_received()).unwrap();
            }
            bounded_ready(store.finish()).unwrap();
            assert_eq!(drops.load(Ordering::SeqCst), 1);
        }
    }

    #[test]
    fn actor_loss_during_snapshot_read_retains_committed_assertion() {
        // White-box qualification: reuse the paused actor bridge, not a public constructor.
        let pause = Arc::new(Pause::default());
        let commits = Arc::new(AtomicUsize::new(0));
        let mut store = paused_store(&pause, &commits);
        let core_store = store.store.clone();
        let origin_line;
        let error = {
            origin_line = line!() + 1;
            let mut send = Box::pin(store.send_with(Action::Increment, |state| *state = 1));
            assert!(
                send.as_mut()
                    .poll(&mut Context::from_waker(Waker::noop()))
                    .is_pending()
            );
            assert_eq!(commits.load(Ordering::SeqCst), 1);
            // Increment pauses the actor after committing, before the snapshot read completes.
            core_store.abort_for_testing();
            pause.resume();
            bounded_ready(send).unwrap_err()
        };
        assert!(
            error
                .source()
                .unwrap()
                .downcast_ref::<rca_core::StoreStopped>()
                .is_some()
        );
        assert!(error.drive_error().is_none());
        assert_eq!(commits.load(Ordering::SeqCst), 1);
        let obligation = store.assertions.pending.get_mut().as_ref().unwrap();
        assert_eq!(obligation.expected, Some(1));
        assert_eq!(obligation.operation, "send");
        assert_eq!(*store.assertions.baseline.get_mut(), 0);
        let debt = bounded_ready(store.finish()).unwrap_err();
        assert!(debt.is_incomplete_assertion());
        assert_eq!(debt.origin().file(), file!());
        assert_eq!(debt.origin().line(), origin_line);
        assert_eq!(commits.load(Ordering::SeqCst), 1);
        assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(store))).is_err());
    }

    #[test]
    fn interrupted_send_and_receive_refuse_every_later_step() {
        for receive in [false, true] {
            for after_commit in [false, true] {
                let pause = Arc::new(Pause::default());
                let commits = Arc::new(AtomicUsize::new(0));
                let mut store = paused_store(&pause, &commits);
                if receive {
                    let _task = block_on(store.send(Action::Start)).unwrap();
                }
                pause.paused.store(!after_commit, Ordering::SeqCst);
                let origin_line = line!() + 2;
                let mut future: std::pin::Pin<Box<dyn Future<Output = _> + '_>> = if receive {
                    Box::pin(store.receive_with(|_| true, |s| *s = 1))
                } else {
                    Box::pin(store.send_with(Action::Increment, |s| *s = 1))
                };
                assert!(
                    future
                        .as_mut()
                        .poll(&mut Context::from_waker(Waker::noop()))
                        .is_pending()
                );
                assert_eq!(commits.load(Ordering::SeqCst), usize::from(after_commit));
                drop(future);
                let error = {
                    let mut finish = Box::pin(store.finish());
                    match finish
                        .as_mut()
                        .poll(&mut Context::from_waker(Waker::noop()))
                    {
                        Poll::Ready(Err(error)) => error,
                        _ => panic!("interrupted comparison must refuse finish before driving"),
                    }
                };
                assert!(
                    error.to_string().contains("incomplete assertion"),
                    "{error}"
                );
                assert!(error.is_incomplete_assertion());
                assert_eq!(
                    error.origin().line(),
                    origin_line + if receive { 0 } else { 2 }
                );
                assert!(
                    block_on(store.send(Action::Start))
                        .unwrap_err()
                        .to_string()
                        .contains("incomplete assertion")
                );
                assert!(
                    block_on(store.receive(|_| panic!("matcher must not run")))
                        .unwrap_err()
                        .to_string()
                        .contains("incomplete assertion")
                );
                assert!(
                    block_on(store.skip_received())
                        .unwrap_err()
                        .to_string()
                        .contains("incomplete assertion")
                );
                let obligation = store.assertions.pending.get_mut().as_ref().unwrap();
                assert_eq!(obligation.expected, Some(1));
                assert_eq!(*store.assertions.baseline.get_mut(), 0);
                pause.resume();
                assert!(
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(store))).is_err()
                );
            }
        }
    }

    #[test]
    fn interrupted_assertion_refuses_skip_without_discarding_pending_output() {
        let pause = Arc::new(Pause::default());
        let commits = Arc::new(AtomicUsize::new(0));
        let mut store = paused_store(&pause, &commits);
        let mut future = Box::pin(store.send_with(Action::Increment, |s| *s = 1));
        assert!(
            future
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending()
        );
        assert_eq!(commits.load(Ordering::SeqCst), 1);
        drop(future);
        // Queue later output without accepting the old comparison. The typed
        // frontier must not advance to that mutation while this debt survives.
        let boundary = store.gate.boundary().expect("committed assertion debt");
        let _receipt = store.store.try_send(Action::Start).unwrap();
        pause.resume();
        store.runtime.run_until_stalled().unwrap();
        assert_eq!(store.gate.boundary(), Some(boundary));
        assert!(
            !store.gate.has_pending(),
            "later mutation remains behind the committed frontier"
        );
        assert!(
            block_on(store.skip_received())
                .unwrap_err()
                .is_incomplete_assertion()
        );
        assert!(
            store.gate.boundary() == Some(boundary),
            "refused skip must retain the boundary"
        );
        assert_eq!(commits.load(Ordering::SeqCst), 1);
        assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(store))).is_err());
    }

    #[test]
    fn rejected_admission_does_not_create_obligation() {
        let pause = Arc::new(Pause::default());
        let commits = Arc::new(AtomicUsize::new(0));
        let mut store = paused_store(&pause, &commits);
        store.store.abort_for_testing();
        let error = block_on(store.send_with(Action::Increment, |s| *s = 1)).unwrap_err();
        assert!(!error.is_incomplete_assertion());
        assert!(store.assertions.incomplete_error().is_none());
        assert_eq!(*store.assertions.baseline.get_mut(), 0);
        assert_eq!(commits.load(Ordering::SeqCst), 0);
        assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(store))).is_err());
    }

    #[test]
    fn actor_loss_during_declared_read_retains_boundary_debt() {
        struct RootCapture;
        impl crate::CaptureKey for RootCapture {
            type Value = u32;
            type Error = std::convert::Infallible;
        }
        // White-box qualification: reuse the paused actor bridge, not a public constructor.
        let pause = Arc::new(Pause::default());
        let commits = Arc::new(AtomicUsize::new(0));
        let mut store = paused_store(&pause, &commits);
        let calls = Arc::new(AtomicUsize::new(0));
        let capture_calls = Arc::clone(&calls);
        store
            .assertions
            .declared
            .declare::<RootCapture>(
                &0,
                move |state| {
                    capture_calls.fetch_add(1, Ordering::SeqCst);
                    Ok(*state)
                },
                Location::caller(),
            )
            .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let core_store = store.store.clone();
        pause.paused.store(true, Ordering::SeqCst);
        let origin_line;
        let error = {
            origin_line = line!() + 1;
            let mut check = Box::pin(store.check_captures(|_| Ok(())));
            assert!(
                check
                    .as_mut()
                    .poll(&mut Context::from_waker(Waker::noop()))
                    .is_pending()
            );
            assert_eq!(calls.load(Ordering::SeqCst), 1);
            core_store.abort_for_testing();
            pause.resume();
            bounded_ready(check).unwrap_err()
        };
        assert!(
            error
                .source()
                .unwrap()
                .downcast_ref::<rca_core::StoreStopped>()
                .is_some()
        );
        assert!(error.drive_error().is_none());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(commits.load(Ordering::SeqCst), 0);
        assert!(store.assertions.pending.get_mut().is_none());
        assert_eq!(*store.assertions.baseline.get_mut(), 0);
        let debt = store.assertions.declared.incomplete_error().unwrap();
        assert!(debt.is_incomplete_assertion());
        assert_eq!(debt.origin().file(), file!());
        assert_eq!(debt.origin().line(), origin_line);
        let finish = bounded_ready(store.finish()).unwrap_err();
        assert!(finish.is_incomplete_assertion());
        assert_eq!(finish.origin(), debt.origin());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(store))).is_err());
    }

    #[test]
    fn cancelled_declared_external_check_retains_boundary_debt() {
        struct RootCapture;
        impl crate::CaptureKey for RootCapture {
            type Value = u32;
            type Error = std::convert::Infallible;
        }
        let pause = Arc::new(Pause::default());
        let commits = Arc::new(AtomicUsize::new(0));
        let mut store = paused_store(&pause, &commits);
        store
            .assertions
            .declared
            .declare::<RootCapture>(&0, |state| Ok(*state), Location::caller())
            .unwrap();
        pause.paused.store(true, Ordering::SeqCst);
        let mut check = Box::pin(store.check_captures(|_| Ok(())));
        assert!(
            check
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending()
        );
        drop(check);
        let error = block_on(store.finish()).unwrap_err();
        assert!(error.is_incomplete_assertion());
        assert_eq!(commits.load(Ordering::SeqCst), 0);
        pause.resume();
        assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(store))).is_err());
    }

    #[test]
    fn cancelled_read_destroys_carriers_without_releasing_pending_action_or_debt() {
        struct Value(Arc<AtomicUsize>);
        impl Clone for Value {
            fn clone(&self) -> Self {
                Self(Arc::clone(&self.0))
            }
        }
        impl PartialEq for Value {
            fn eq(&self, _: &Self) -> bool {
                true
            }
        }
        impl Drop for Value {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }
        struct Key;
        impl crate::CaptureKey for Key {
            type Value = Value;
            type Error = std::convert::Infallible;
        }
        let pause = Arc::new(Pause::default());
        let commits = Arc::new(AtomicUsize::new(0));
        let drops = Arc::new(AtomicUsize::new(0));
        let captures = Arc::new(AtomicUsize::new(0));
        let mut store = paused_store(&pause, &commits);
        let _task = block_on(store.send(Action::Start)).unwrap();
        let capture_drops = Arc::clone(&drops);
        let capture_calls = Arc::clone(&captures);
        store
            .assertions
            .declared
            .declare::<Key>(
                &0,
                move |_| {
                    capture_calls.fetch_add(1, Ordering::SeqCst);
                    Ok(Value(Arc::clone(&capture_drops)))
                },
                Location::caller(),
            )
            .unwrap();
        assert!(store.gate.has_pending());
        pause.paused.store(true, Ordering::SeqCst);
        let origin_line = line!() + 1;
        let mut check = Box::pin(store.check_captures(|_| Ok(())));
        assert!(
            check
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending()
        );
        drop(check);
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        pause.resume();
        store.runtime.run_until_stalled().unwrap();
        // The gate discards the cancelled assertion without invoking its capturer.
        // Its expected operand is destroyed, but the store keeps boundary debt.
        assert_eq!(captures.load(Ordering::SeqCst), 1);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert!(store.gate.has_pending());
        assert_eq!(commits.load(Ordering::SeqCst), 0);
        let debt = block_on(store.finish()).unwrap_err();
        assert!(debt.is_incomplete_assertion());
        assert_eq!(debt.origin().line(), origin_line);
        assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(store))).is_err());
        assert_eq!(drops.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn stale_empty_diagnostics_cannot_complete_paused_actor() {
        let pause = Arc::new(Pause::default());
        let commits = Arc::new(AtomicUsize::new(0));
        let mut store = paused_store(&pause, &commits);
        store.runtime.run_until_stalled().unwrap();
        let cached = store.gate.latest_diagnostics().unwrap();
        assert!(cached.runtime().live_task_ids.is_empty());
        pause.paused.store(true, Ordering::SeqCst);
        let _receipt = store.store.try_send(Action::Start).unwrap();
        // The actor cannot answer a new diagnostic request. finish must return
        // immediately with failure, even though its cached snapshot is empty.
        let result = {
            let mut finish = std::pin::pin!(store.finish());
            finish
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
        };
        assert!(matches!(result, Poll::Ready(Err(_))));
        assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(store))).is_err());
    }

    mod completion_capture_tests {
        use super::*;
        use futures_util::FutureExt as _;

        struct Captured {
            value: u32,
            during_read: bool,
            abort: bool,
            interrupt: Arc<Mutex<Option<Store<u32, Action>>>>,
            pause: Arc<Pause>,
            drops: Arc<AtomicUsize>,
        }

        impl Captured {
            fn interrupt(&self) {
                let store = self.interrupt.lock().unwrap().take();
                if let Some(store) = store {
                    if self.abort {
                        store.abort_for_testing();
                    } else {
                        self.pause.paused.store(true, Ordering::SeqCst);
                    }
                }
            }
        }

        impl Clone for Captured {
            fn clone(&self) -> Self {
                if self.during_read {
                    self.interrupt();
                }
                // The historical value is independent; only interruption bookkeeping is shared.
                Self {
                    value: self.value,
                    during_read: self.during_read,
                    abort: self.abort,
                    interrupt: Arc::clone(&self.interrupt),
                    pause: Arc::clone(&self.pause),
                    drops: Arc::clone(&self.drops),
                }
            }
        }

        impl PartialEq for Captured {
            fn eq(&self, other: &Self) -> bool {
                if !self.during_read {
                    self.interrupt();
                }
                self.value == other.value
            }
        }

        impl Drop for Captured {
            fn drop(&mut self) {
                self.drops.fetch_add(1, Ordering::SeqCst);
            }
        }

        struct Key;
        impl crate::CaptureKey for Key {
            type Value = Captured;
            type Error = std::convert::Infallible;
        }

        #[test]
        fn failures_preserve_boundary_debt_and_owned_values() {
            for (during_read, abort) in [(true, true), (true, false), (false, true), (false, false)]
            {
                let pause = Arc::new(Pause::default());
                let commits = Arc::new(AtomicUsize::new(0));
                let mut store = paused_store(&pause, &commits);
                let interrupt = Arc::new(Mutex::new(None));
                let calls = Arc::new(AtomicUsize::new(0));
                let drops = Arc::new(AtomicUsize::new(0));
                let capture_interrupt = Arc::clone(&interrupt);
                let capture_pause = Arc::clone(&pause);
                let capture_calls = Arc::clone(&calls);
                let capture_drops = Arc::clone(&drops);
                store
                    .assertions
                    .declared
                    .declare::<Key>(
                        &0,
                        move |state| {
                            assert_eq!(*state, 0);
                            capture_calls.fetch_add(1, Ordering::SeqCst);
                            Ok(Captured {
                                value: 17,
                                during_read,
                                abort,
                                interrupt: Arc::clone(&capture_interrupt),
                                pause: Arc::clone(&capture_pause),
                                drops: Arc::clone(&capture_drops),
                            })
                        },
                        Location::caller(),
                    )
                    .unwrap();
                *interrupt.lock().unwrap() = Some(store.store.clone());

                // Clone interrupts before the captured read; equality interrupts only
                // after that read succeeds, leaving the following shutdown unconfirmed.
                let origin_line = line!() + 1;
                let completion = store.finish();
                let error = completion
                    .now_or_never()
                    .expect("finish never waits for external progress")
                    .expect_err("the interrupted boundary cannot acknowledge completion");
                assert_eq!(error.origin().file(), file!());
                assert_eq!(error.origin().line(), origin_line);
                assert!(error.drive_error().is_none());
                assert!(!error.is_incomplete_assertion());
                if during_read && abort {
                    assert!(
                        std::error::Error::source(&error)
                            .expect("the failed read retains its typed cause")
                            .is::<rca_core::StoreStopped>()
                    );
                } else {
                    assert!(std::error::Error::source(&error).is_none());
                    assert!(
                        error
                            .to_string()
                            .starts_with("production Store unavailable during test operation at ")
                    );
                }
                assert!(!store.completed);
                assert!(!store.shutdown);
                assert_eq!(store.store.is_closed(), abort);
                assert_eq!(store.now(), MonotonicInstant::ZERO);
                assert_eq!(commits.load(Ordering::SeqCst), 0);
                assert_eq!(*store.assertions.baseline.get_mut(), 0);
                assert!(store.assertions.pending.get_mut().is_none());
                assert_eq!(
                    calls.load(Ordering::SeqCst),
                    if during_read { 1 } else { 2 }
                );
                assert_eq!(
                    drops.load(Ordering::SeqCst),
                    if during_read { usize::from(abort) } else { 2 }
                );
                assert!(interrupt.lock().unwrap().is_none());

                pause.resume();
                store.runtime.run_until_stalled().unwrap();
                if during_read {
                    // An abandoned read still executes if authority survived. Its reply
                    // cannot install a new baseline or forgive the retained finish debt.
                    assert_eq!(calls.load(Ordering::SeqCst), if abort { 1 } else { 2 });
                    // The failed or abandoned read now owns its expected value too.
                    assert_eq!(drops.load(Ordering::SeqCst), if abort { 1 } else { 2 });
                    let debt = store
                        .finish()
                        .now_or_never()
                        .expect("incomplete assertions fail before driving")
                        .expect_err("failed captured reads retain their boundary");
                    assert!(debt.is_incomplete_assertion());
                    assert_eq!(debt.origin().line(), origin_line);
                    assert!(debt.to_string().contains("finish"));
                } else {
                    assert!(store.assertions.declared.incomplete_error().is_none());
                    assert_eq!(calls.load(Ordering::SeqCst), 2);
                    assert_eq!(drops.load(Ordering::SeqCst), 2);
                    // The admitted shutdown remains effective after its waiter is dropped.
                    assert!(store.store.is_closed());
                }
                assert!(
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(store))).is_err()
                );
                assert_eq!(
                    drops.load(Ordering::SeqCst),
                    if during_read && abort { 2 } else { 3 }
                );
            }
        }
    }

    #[test]
    fn renderer_panic_keeps_verified_baseline_and_pending_expected_snapshot() {
        for receive in [false, true] {
            let mut store = TestStore::builder(
                0,
                Feature {
                    pause: Arc::new(Pause::default()),
                    commits: Arc::new(AtomicUsize::new(0)),
                },
            )
            .with_snapshot_renderer(|_| panic!("renderer failure"))
            .build();
            if receive {
                let _task = block_on(store.send(Action::Start)).unwrap();
            }
            let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                if receive {
                    block_on(store.receive(|_| true))
                } else {
                    block_on(store.send(Action::Increment))
                }
            }))
            .unwrap_err();
            assert_eq!(panic.downcast_ref::<&str>(), Some(&"renderer failure"));
            assert_eq!(*store.assertions.baseline.get_mut(), 0);
            let obligation = store.assertions.pending.get_mut().as_ref().unwrap();
            assert_eq!(obligation.expected, Some(0));
            assert_eq!(
                obligation.operation,
                if receive { "receive" } else { "send" }
            );
            assert!(
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(store))).is_err()
            );
        }
    }
}
