//! Manual owned snapshots through existing projected and declared consumers.

use std::convert::Infallible;
use std::fmt;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

use futures_executor::block_on;
use rca_core::{Effect, ReduceContext, Reducer, Shared, SharedAccessError, Snapshot};
use rca_test::{CaptureKey, TestStore, requiring_change};

/// Counts destruction of live authority, which snapshots must not copy.
struct TaskAuthority(Arc<AtomicUsize>);

impl Drop for TaskAuthority {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

/// Combines owned history, shared quota and a live task resource.
struct Importer {
    progress: usize,
    history: Vec<String>,
    bookkeeping: usize,
    quota: Shared<usize>,
    credential: String,
    task: TaskAuthority,
}

/// Owns historical values while redacting only credential diagnostics.
#[derive(Clone, PartialEq)]
struct ImportHistory {
    progress: usize,
    history: Vec<String>,
    bookkeeping: usize,
    quota: usize,
    credential: String,
}

impl fmt::Debug for ImportHistory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ImportHistory")
            .field("progress", &self.progress)
            .field("history", &self.history)
            .field("bookkeeping", &self.bookkeeping)
            .field("quota", &self.quota)
            .field("credential", &"<redacted>")
            .finish()
    }
}

impl Snapshot for Importer {
    type Value = ImportHistory;

    fn snapshot(&self) -> Self::Value {
        ImportHistory {
            progress: self.progress.snapshot(),
            history: self.history.snapshot(),
            bookkeeping: self.bookkeeping.snapshot(),
            // A production consumer needing fallibility keeps its explicit callback.
            quota: self.quota.read(|value| *value).unwrap(),
            credential: self.credential.snapshot(),
            // Live task authority is not historical data; its drop is checked separately.
        }
    }
}

/// Changes public progress, private credentials, or no state at all.
enum Action {
    Advance,
    Secret,
    Noop,
}

/// Applies the three controlled state changes without asynchronous work.
struct Feature;

impl Reducer for Feature {
    type State = Importer;
    type Action = Action;

    fn reduce(
        &mut self,
        state: &mut Importer,
        action: &Action,
        _: &mut ReduceContext<'_>,
    ) -> Effect<Action> {
        match action {
            Action::Advance => {
                state.progress += 1;
                state.history.push("imported".into());
                state.bookkeeping += 1;
                state
                    .quota
                    .try_update(
                        |value| {
                            *value -= 1;
                            Ok::<_, Infallible>(())
                        },
                        || {},
                    )
                    .unwrap();
            }
            Action::Secret => state.credential = "second test secret".into(),
            Action::Noop => {}
        }
        Effect::none()
    }
}

/// Returns source state and independent quota and destruction witnesses.
fn fixture() -> (Importer, Shared<usize>, Arc<AtomicUsize>) {
    let quota = Shared::new(12);
    let drops = Arc::new(AtomicUsize::new(0));
    let state = Importer {
        progress: 0,
        history: vec!["queued".into()],
        bookkeeping: 3,
        quota: quota.clone(),
        credential: "first test secret".into(),
        task: TaskAuthority(Arc::clone(&drops)),
    };
    (state, quota, drops)
}

#[test]
fn projected_snapshot_method_keeps_expected_edits_and_authority_independent() {
    let (source, quota, drops) = fixture();
    let first = source.snapshot();
    assert_eq!(Arc::strong_count(&source.task.0), 2);
    let mut store = TestStore::projected(source, Feature, Snapshot::snapshot);
    let _task = block_on(store.send_with(
        Action::Advance,
        requiring_change(|expected: &mut ImportHistory| {
            expected.progress = 1;
            expected.history.push("imported".into());
            expected.bookkeeping = 4;
            expected.quota = 11;
            // This runs before reduction: editing history must not touch the source.
            assert_eq!(quota.read(|value| *value).unwrap(), 12);
            assert_eq!(drops.load(Ordering::SeqCst), 0);
        }),
    ))
    .unwrap();
    assert_eq!(quota.read(|value| *value).unwrap(), 11);
    assert_eq!(first.history, ["queued"]);
    assert_eq!((first.progress, first.bookkeeping, first.quota), (0, 3, 12));
    block_on(store.assert_state(|state| {
        assert_eq!(state.history, ["queued", "imported"]);
        assert_eq!(Arc::strong_count(&state.task.0), 2);
    }))
    .unwrap();
    block_on(store.finish()).unwrap();
    drop(store);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    assert_eq!(Arc::strong_count(&drops), 1);
}

/// Extracts an assertion message and rejects unrelated panic payload types.
fn panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
    let message = if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        payload.downcast_ref::<&str>().unwrap().to_string()
    };
    drop(payload);
    message
}

#[test]
fn redaction_preserves_inequality_and_projected_mismatch_debt() {
    let (source, _, _) = fixture();
    let first = source.snapshot();
    let mut second = first.clone();
    second.credential = "second test secret".into();
    assert_ne!(first, second);
    assert_eq!(format!("{first:?}"), format!("{second:?}"));
    assert!(!format!("{first:?}").contains("first test secret"));
    let mut store = TestStore::projected(source, Feature, Snapshot::snapshot);
    let failure =
        catch_unwind(AssertUnwindSafe(|| block_on(store.send(Action::Secret)))).unwrap_err();
    let message = panic_message(failure);
    assert!(message.contains("projected coverage"));
    assert!(!message.contains("full-state coverage"));
    assert!(!message.contains("first test secret"));
    assert!(!message.contains("second test secret"));
    assert!(
        block_on(store.finish())
            .unwrap_err()
            .is_incomplete_assertion()
    );
    let teardown = catch_unwind(AssertUnwindSafe(|| drop(store))).unwrap_err();
    assert!(
        panic_message(teardown)
            .starts_with("TestStore teardown failed: TestStore incomplete assertion for send at ")
    );
}

/// Captures quota independently of the selected root history.
struct Quota;

impl CaptureKey for Quota {
    type Value = usize;
    type Error = SharedAccessError;
}

#[test]
fn projected_builder_accepts_owned_capture_update_without_aliasing() {
    let (source, quota, _) = fixture();
    let captured_quota = quota.clone();
    let mut store = TestStore::projected_builder(source, Feature, Snapshot::snapshot)
        .with_capture::<Quota>(move |_| captured_quota.read(Snapshot::snapshot))
        .unwrap()
        .build();
    let _task = block_on(store.send_with_captures(
        Action::Advance,
        |history| {
            history.progress = 1;
            history.history.push("imported".into());
            history.bookkeeping = 4;
            history.quota = 11;
        },
        |captures| {
            captures.update::<Quota>(requiring_change(|expected: &mut usize| {
                *expected = 11;
                assert_eq!(quota.read(|value| *value).unwrap(), 12);
            }))
        },
    ))
    .unwrap();
    block_on(store.finish()).unwrap();
}

#[test]
fn omitted_root_fields_do_not_erase_capture_mismatch_after_source_revert() {
    let (source, quota, _) = fixture();
    let captured = quota.clone();
    // The unit snapshot intentionally omits every root field. Capture debt remains.
    let mut store = TestStore::projected_builder(source, Feature, |_| ())
        .with_capture::<Quota>(move |_| captured.read(Snapshot::snapshot))
        .unwrap()
        .build();
    quota
        .try_update(
            |value| {
                *value = 9;
                Ok::<_, Infallible>(())
            },
            || {},
        )
        .unwrap();
    assert!(
        block_on(store.check_captures(|_| Ok(())))
            .unwrap_err()
            .is_capture_mismatch()
    );
    quota
        .try_update(
            |value| {
                *value = 12;
                Ok::<_, Infallible>(())
            },
            || {},
        )
        .unwrap();
    assert!(
        block_on(store.check_captures(|_| Ok(())))
            .unwrap_err()
            .is_incomplete_assertion()
    );
    assert!(
        block_on(store.finish())
            .unwrap_err()
            .is_incomplete_assertion()
    );
    let teardown = catch_unwind(AssertUnwindSafe(|| drop(store))).unwrap_err();
    assert!(panic_message(teardown).starts_with(
        "TestStore teardown failed: TestStore incomplete assertion for check_captures at "
    ));
}

#[test]
fn unchanged_commit_does_not_clear_an_existing_owned_expectation_failure() {
    let (source, _, _) = fixture();
    let mut store = TestStore::projected(source, Feature, Snapshot::snapshot);
    // The reducer commits but changes nothing; a declared expected change must fail.
    let failure = catch_unwind(AssertUnwindSafe(|| {
        block_on(store.send_with(Action::Noop, |history| {
            history.bookkeeping += 1;
        }))
    }));
    let message = panic_message(failure.unwrap_err());
    assert!(message.starts_with("TestStore state mismatch at "));
    assert!(message.contains(" during send: projected coverage"));
    assert!(
        block_on(store.send(Action::Noop))
            .unwrap_err()
            .is_incomplete_assertion()
    );
    assert!(
        block_on(store.finish())
            .unwrap_err()
            .is_incomplete_assertion()
    );
    let teardown = catch_unwind(AssertUnwindSafe(|| drop(store))).unwrap_err();
    assert!(
        panic_message(teardown)
            .starts_with("TestStore teardown failed: TestStore incomplete assertion for send at ")
    );
}

/// Makes owned-history capture fail without changing the root snapshot policy.
struct FallibleHistory;

impl CaptureKey for FallibleHistory {
    type Value = ImportHistory;
    type Error = &'static str;
}

#[test]
fn failed_explicit_capture_does_not_let_later_success_erase_debt() {
    let (source, _, _) = fixture();
    let fail = Arc::new(AtomicBool::new(false));
    let captured_fail = Arc::clone(&fail);
    let mut store = TestStore::projected_builder(source, Feature, Snapshot::snapshot)
        .with_capture::<FallibleHistory>(move |state| {
            if captured_fail.load(Ordering::SeqCst) {
                Err("controlled capture failure")
            } else {
                Ok(state.snapshot())
            }
        })
        .unwrap()
        .build();
    fail.store(true, Ordering::SeqCst);
    let error = block_on(store.check_captures(|_| Ok(()))).unwrap_err();
    assert_eq!(
        error.into_capture_error::<FallibleHistory>().unwrap(),
        "controlled capture failure"
    );
    fail.store(false, Ordering::SeqCst);
    assert!(
        block_on(store.check_captures(|_| Ok(())))
            .unwrap_err()
            .is_incomplete_assertion()
    );
    assert!(
        block_on(store.finish())
            .unwrap_err()
            .is_incomplete_assertion()
    );
    let teardown = catch_unwind(AssertUnwindSafe(|| drop(store))).unwrap_err();
    assert!(panic_message(teardown).starts_with(
        "TestStore teardown failed: TestStore incomplete assertion for check_captures at "
    ));
}

mod completion_budget {
    //! Checks work released by capture against the remaining deterministic poll budget.

    use super::*;
    use std::sync::Mutex;
    use std::task::Poll;

    /// Starts finite work or commits its separately asserted state change.
    #[derive(Clone, Copy)]
    enum Input {
        Start,
        Apply,
    }

    /// Performs eight self-woken polls after capture releases its gate.
    struct Work {
        gate: Option<rca_test::ControlledGate>,
        polls: Arc<AtomicUsize>,
        follow_up: bool,
    }

    impl Reducer for Work {
        type State = usize;
        type Action = Input;

        fn reduce(
            &mut self,
            state: &mut usize,
            action: &Input,
            _: &mut ReduceContext<'_>,
        ) -> Effect<Input> {
            if matches!(action, Input::Apply) {
                *state = 1;
                return Effect::none();
            }
            let gate = self.gate.take().expect("one gated effect");
            let polls = Arc::clone(&self.polls);
            let work = Effect::run(move |_| async move {
                gate.await.unwrap();
                std::future::poll_fn(move |context| {
                    if polls.fetch_add(1, Ordering::SeqCst) + 1 == 8 {
                        Poll::Ready(())
                    } else {
                        context.waker().wake_by_ref();
                        Poll::Pending
                    }
                })
                .await;
            });
            if self.follow_up {
                Effect::merge([Effect::send(Input::Apply), work])
            } else {
                *state = 1;
                work
            }
        }
    }

    /// Checks send and receive without losing committed state after budget exhaustion.
    fn capture_releases_finite_work(follow_up: bool) {
        block_on(async {
            let (control, gate) = rca_test::controlled_gate();
            let release = Mutex::new(Some(control));
            let polls = Arc::new(AtomicUsize::new(0));
            let mut store = TestStore::projected_builder(
                0,
                Work {
                    gate: Some(gate),
                    polls: Arc::clone(&polls),
                    follow_up,
                },
                move |state| {
                    if *state == 1
                        && let Some(control) = release.lock().unwrap().take()
                    {
                        control.release().unwrap();
                    }
                    *state
                },
            )
            .with_step_budget(7)
            .build();
            let (error, initial) = if follow_up {
                let initial = store.send(Input::Start).await.unwrap();
                let error = store
                    .receive_with(|action| matches!(action, Input::Apply), |value| *value = 1)
                    .await
                    .unwrap_err();
                (error, Some(initial))
            } else {
                (
                    store
                        .send_with(Input::Start, |value| *value = 1)
                        .await
                        .unwrap_err(),
                    None,
                )
            };
            let drive = error
                .drive_error()
                .expect("final settling must retain its failure");
            assert!(drive.is_step_budget_exhausted());
            assert_eq!((drive.polls(), drive.step_budget()), (7, 7));
            assert!(
                polls.load(Ordering::SeqCst) < 8,
                "unfinished work remains owned"
            );
            store
                .assert_state(|state| assert_eq!(*state, 1))
                .await
                .unwrap();
            if let Some(initial) = initial {
                initial.finish().await.unwrap();
            }
            store.finish().await.unwrap();
            assert_eq!(polls.load(Ordering::SeqCst), 8);
        });
    }

    #[test]
    fn send_reports_post_capture_budget_without_losing_committed_state() {
        capture_releases_finite_work(false);
    }

    #[test]
    fn receive_reports_post_capture_budget_without_losing_committed_state() {
        capture_releases_finite_work(true);
    }
}
