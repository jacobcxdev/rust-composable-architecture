//! Declared captures remain mandatory independently of selected root fields.
//!
//! Public Store fixtures distinguish rejected input from admitted assertion debt.
//! Deliberate user panics also check their cause and retained ownership; they do
//! not count as successful execution of production failure-only code.

use std::cell::Cell;
use std::convert::Infallible;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use futures_executor::block_on;
use rca_core::{
    Effect, Issue, IssueCollector, IssueReporter, ReduceContext, Reducer, Shared, SharedAccessError,
};
use rca_test::{CaptureKey, Exhaustivity, TestStore, TestStoreError};

struct PrivateCapture;

impl CaptureKey for PrivateCapture {
    type Value = u64;
    type Error = Infallible;
}

struct SharedCapture;

impl CaptureKey for SharedCapture {
    type Value = u64;
    type Error = SharedAccessError;
}

// Domain state intentionally does not implement Clone, Debug or equality.
struct State {
    private: u64,
}

enum Action {
    Mutate,
    Start,
}

struct Feature {
    shared: Shared<u64>,
    commits: Arc<AtomicUsize>,
}

impl Reducer for Feature {
    type State = State;
    type Action = Action;
    fn reduce(
        &mut self,
        state: &mut State,
        action: &Action,
        _: &mut ReduceContext<'_>,
    ) -> Effect<Action> {
        match action {
            Action::Mutate => {
                state.private = 1;
                self.shared
                    .try_update(
                        |value| {
                            *value = 7;
                            Ok::<_, Infallible>(())
                        },
                        || {},
                    )
                    .unwrap();
                self.commits.fetch_add(1, Ordering::SeqCst);
                Effect::none()
            }
            Action::Start => Effect::send(Action::Mutate),
        }
    }
}

fn finish_without_external_wake<T>(future: impl Future<Output = T>) -> T {
    let mut future = std::pin::pin!(future);
    let mut context = std::task::Context::from_waker(std::task::Waker::noop());
    // These fixtures need no external wake; a stranded client must fail, not hang.
    for _ in 0..64 {
        if let std::task::Poll::Ready(output) = future.as_mut().poll(&mut context) {
            return output;
        }
    }
    panic!("finite test operation remained pending after 64 client polls");
}

#[test]
fn invalid_receive_expectations_preserve_the_pending_action_for_retry() {
    for duplicate in [false, true] {
        let (mut store, shared, commits) = fixture(Exhaustivity::Exhaustive);
        let start = finish_without_external_wake(store.send(Action::Start)).unwrap();
        let mut origin_line = 0;
        let failure = finish_without_external_wake(store.receive_with_captures(
            |action| matches!(action, Action::Mutate),
            |()| {},
            |expected| {
                if duplicate {
                    expected.set::<PrivateCapture>(1)?;
                    origin_line = line!() + 1;
                    expected.set::<PrivateCapture>(1)
                } else {
                    origin_line = line!() + 1;
                    expected.set::<FallibleCapture>(1)
                }
            },
        ))
        .expect_err("invalid expectations must not release the pending action");
        let (key, problem) = if duplicate {
            (
                std::any::type_name::<PrivateCapture>(),
                "DuplicateExpectation",
            )
        } else {
            (
                std::any::type_name::<FallibleCapture>(),
                "UnknownExpectation",
            )
        };
        assert!(failure.to_string().starts_with(&format!(
            "TestStore capture {key} {problem} during declare/expect at "
        )));
        assert_eq!(failure.origin().file(), file!());
        assert_eq!(failure.origin().line(), origin_line);
        assert!(!failure.is_incomplete_assertion());
        assert_eq!(commits.load(Ordering::SeqCst), 0);
        assert_eq!(shared.read(|value| *value).unwrap(), 5);

        let received = finish_without_external_wake(store.receive_with_captures(
            |action| matches!(action, Action::Mutate),
            |()| {},
            |expected| {
                expected.set::<PrivateCapture>(1)?;
                expected.set::<SharedCapture>(7)
            },
        ))
        .expect("valid retry releases the original action without assertion debt");
        assert_eq!(commits.load(Ordering::SeqCst), 1);
        assert_eq!(shared.read(|value| *value).unwrap(), 7);
        finish_without_external_wake(received.finish()).unwrap();
        finish_without_external_wake(start.finish()).unwrap();
        finish_without_external_wake(store.finish()).unwrap();
    }
}

#[test]
fn invalid_check_expectations_do_not_install_partial_values_or_assertion_debt() {
    for duplicate in [false, true] {
        let (mut store, shared, commits) = fixture(Exhaustivity::Exhaustive);
        write(&shared, 9);
        let mut origin_line = 0;
        let failure = finish_without_external_wake(store.check_captures(|expected| {
            if duplicate {
                expected.set::<PrivateCapture>(1)?;
                origin_line = line!() + 1;
                expected.set::<PrivateCapture>(1)
            } else {
                origin_line = line!() + 1;
                expected.set::<FallibleCapture>(1)
            }
        }))
        .expect_err("invalid expectations must not install a partial capture boundary");
        let (key, problem) = if duplicate {
            (
                std::any::type_name::<PrivateCapture>(),
                "DuplicateExpectation",
            )
        } else {
            (
                std::any::type_name::<FallibleCapture>(),
                "UnknownExpectation",
            )
        };
        assert!(failure.to_string().starts_with(&format!(
            "TestStore capture {key} {problem} during declare/expect at "
        )));
        assert_eq!(failure.origin().file(), file!());
        assert_eq!(failure.origin().line(), origin_line);
        assert!(!failure.is_incomplete_assertion());
        assert_eq!(commits.load(Ordering::SeqCst), 0);
        assert_eq!(shared.read(|value| *value).unwrap(), 9);

        finish_without_external_wake(
            store.check_captures(|expected| expected.set::<SharedCapture>(9)),
        )
        .expect("retry keeps the omitted private capture at its original zero baseline");
        let task = finish_without_external_wake(store.send_with_captures(
            Action::Mutate,
            |()| {},
            |expected| {
                expected.set::<PrivateCapture>(1)?;
                expected.set::<SharedCapture>(7)
            },
        ))
        .expect("rejected check leaves subsequent action assertions usable");
        assert_eq!(commits.load(Ordering::SeqCst), 1);
        assert_eq!(shared.read(|value| *value).unwrap(), 7);
        finish_without_external_wake(task.finish()).unwrap();
        finish_without_external_wake(store.finish()).unwrap();
    }
}

fn fixture(mode: Exhaustivity) -> (TestStore<State, Action, ()>, Shared<u64>, Arc<AtomicUsize>) {
    let shared = Shared::new(5);
    let commits = Arc::new(AtomicUsize::new(0));
    let captured = shared.clone();
    let store = TestStore::projected_builder(
        State { private: 0 },
        Feature {
            shared: shared.clone(),
            commits: Arc::clone(&commits),
        },
        |_| (),
    )
    .with_exhaustivity(mode)
    .with_capture::<PrivateCapture>(|state| Ok(state.private))
    .unwrap()
    .with_capture::<SharedCapture>(move |_| captured.read(|value| *value))
    .unwrap()
    .build();
    (store, shared, commits)
}

#[test]
fn updates_select_keys_not_value_types_and_run_once() {
    let (mut store, shared, commits) = fixture(Exhaustivity::Exhaustive);
    let mut calls = 0;
    let _task = block_on(store.send_with_captures(
        Action::Mutate,
        |()| {},
        |values| {
            values.update::<PrivateCapture>(|value| {
                calls += 1;
                *value += 1;
            })?;
            values.update::<SharedCapture>(|value| {
                calls += 1;
                *value += 2;
            })
        },
    ))
    .unwrap();
    assert_eq!(calls, 2);
    assert_eq!(commits.load(Ordering::SeqCst), 1);
    assert_eq!(shared.read(|value| *value).unwrap(), 7);
    block_on(store.finish()).unwrap();
}

#[test]
fn owned_capture_updates_preserve_independent_history_for_each_key() {
    struct First;
    struct Second;
    impl CaptureKey for First {
        type Value = Vec<u64>;
        type Error = Infallible;
    }
    impl CaptureKey for Second {
        type Value = Vec<u64>;
        type Error = Infallible;
    }
    let history = Shared::new(vec![1, 2]);
    let first = history.clone();
    let second = history.clone();
    let mut store = TestStore::projected_builder(
        State { private: 0 },
        Feature {
            shared: Shared::new(5),
            commits: Arc::new(AtomicUsize::new(0)),
        },
        |_| (),
    )
    .with_capture::<First>(move |_| Ok(first.read(Clone::clone).unwrap()))
    .unwrap()
    .with_capture::<Second>(move |_| Ok(second.read(Clone::clone).unwrap()))
    .unwrap()
    .build();
    history
        .try_update(
            |value| {
                value.push(3);
                Ok::<_, Infallible>(())
            },
            || {},
        )
        .unwrap();
    block_on(store.check_captures(|values| {
        values.update::<First>(|value| {
            assert_eq!(*value, [1, 2]);
            value.push(3);
        })?;
        values.update::<Second>(|value| {
            assert_eq!(*value, [1, 2]);
            value.push(3);
        })?;
        assert_eq!(history.read(Clone::clone).unwrap(), [1, 2, 3]);
        Ok(())
    }))
    .unwrap();
    block_on(store.finish()).unwrap();
}

/// Equality and cleanup must see a complete batch without discharging its debt.
mod capture_batch_panics {
    use std::convert::Infallible;
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use futures_executor::block_on;
    use rca_core::Shared;
    use rca_test::{CaptureKey, TestStore};

    use super::{Feature, State};

    struct Value {
        stage: Arc<AtomicUsize>,
        captures: Arc<AtomicUsize>,
        equalities: Arc<AtomicUsize>,
        expected: bool,
    }
    impl Clone for Value {
        fn clone(&self) -> Self {
            Self {
                stage: Arc::clone(&self.stage),
                captures: Arc::clone(&self.captures),
                equalities: Arc::clone(&self.equalities),
                expected: true,
            }
        }
    }
    impl PartialEq for Value {
        fn eq(&self, _: &Self) -> bool {
            assert_eq!(
                self.captures.load(Ordering::SeqCst),
                2,
                "capture the whole batch before equality"
            );
            self.equalities.fetch_add(1, Ordering::SeqCst);
            assert_ne!(
                self.stage.load(Ordering::SeqCst),
                1,
                "declared equality panic"
            );
            true
        }
    }
    impl Drop for Value {
        fn drop(&mut self) {
            let stage = if self.expected { 3 } else { 2 };
            if self
                .stage
                .compare_exchange(stage, 0, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
            {
                panic!("declared destructor panic");
            }
        }
    }
    struct First;
    impl CaptureKey for First {
        type Value = Value;
        type Error = Infallible;
    }
    struct Second;
    impl CaptureKey for Second {
        type Value = u64;
        type Error = u8;
    }
    #[test]
    fn complete_capture_batch_precedes_equality_and_user_panics_preserve_admitted_debt() {
        for stage in 1..=4 {
            let control = Arc::new(AtomicUsize::new(0));
            let captures = Arc::new(AtomicUsize::new(0));
            let equalities = Arc::new(AtomicUsize::new(0));
            let first_control = Arc::clone(&control);
            let first_captures = Arc::clone(&captures);
            let first_equalities = Arc::clone(&equalities);
            let second_control = Arc::clone(&control);
            let second_captures = Arc::clone(&captures);
            let mut store = TestStore::projected_builder(
                State { private: 0 },
                Feature {
                    shared: Shared::new(5),
                    commits: Arc::new(AtomicUsize::new(0)),
                },
                |_| (),
            )
            .with_capture::<First>(move |_| {
                first_captures.fetch_add(1, Ordering::SeqCst);
                Ok(Value {
                    stage: Arc::clone(&first_control),
                    captures: Arc::clone(&first_captures),
                    equalities: Arc::clone(&first_equalities),
                    expected: false,
                })
            })
            .unwrap()
            .with_capture::<Second>(move |_| {
                second_captures.fetch_add(1, Ordering::SeqCst);
                if second_control.load(Ordering::SeqCst) == 4 {
                    Err(7)
                } else {
                    Ok(0)
                }
            })
            .unwrap()
            .build();
            captures.store(0, Ordering::SeqCst);
            let result = catch_unwind(AssertUnwindSafe(|| {
                block_on(store.check_captures(|_| {
                    control.store(stage, Ordering::SeqCst);
                    Ok(())
                }))
            }));
            if stage == 4 {
                let error = result.unwrap().unwrap_err();
                assert_eq!(error.into_capture_error::<Second>().unwrap(), 7);
                assert_eq!(equalities.load(Ordering::SeqCst), 0);
            } else {
                let panic = result.expect_err("the selected user operation must panic");
                if stage == 1 {
                    assert!(
                        panic
                            .downcast_ref::<String>()
                            .expect("assertion panic carries its formatted message")
                            .contains("declared equality panic")
                    );
                } else {
                    assert_eq!(
                        panic.downcast_ref::<&str>(),
                        Some(&"declared destructor panic")
                    );
                }
                assert_eq!(equalities.load(Ordering::SeqCst), 1);
            }
            control.store(0, Ordering::SeqCst);
            assert_eq!(captures.load(Ordering::SeqCst), 2);
            assert!(
                block_on(store.finish())
                    .unwrap_err()
                    .is_incomplete_assertion()
            );
            assert!(catch_unwind(AssertUnwindSafe(|| drop(store))).is_err());
        }
    }
}

#[test]
fn unknown_update_and_every_duplicate_pair_skip_callbacks() {
    for first_update in [false, true] {
        for second_update in [false, true] {
            let (mut store, _, commits) = fixture(Exhaustivity::Exhaustive);
            let mut calls = 0;
            let mut origin_line = 0;
            let error = block_on(store.send_with_captures(
                Action::Mutate,
                |()| {},
                |values| {
                    if first_update {
                        // A no-op still consumes the expectation slot.
                        values.update::<PrivateCapture>(|_| {
                            calls += 1;
                        })?;
                    } else {
                        values.set::<PrivateCapture>(0)?;
                    }
                    if second_update {
                        origin_line = line!() + 1;
                        values.update::<PrivateCapture>(|_| {
                            calls += 10;
                        })
                    } else {
                        origin_line = line!() + 1;
                        values.set::<PrivateCapture>(0)
                    }
                },
            ))
            .unwrap_err();
            assert!(error.to_string().contains("DuplicateExpectation"));
            assert_eq!(error.origin().line(), origin_line);
            assert_eq!(calls, usize::from(first_update));
            assert_eq!(commits.load(Ordering::SeqCst), 0);
            block_on(store.finish()).unwrap();
        }
    }
    let (mut store, _, commits) = fixture(Exhaustivity::Exhaustive);
    let mut calls = 0;
    let mut origin_line = 0;
    let error = block_on(store.send_with_captures(
        Action::Mutate,
        |()| {},
        |values| {
            origin_line = line!() + 1;
            values.update::<FallibleCapture>(|_| {
                calls += 1;
            })
        },
    ))
    .unwrap_err();
    assert!(error.to_string().contains("UnknownExpectation"));
    assert_eq!(error.origin().line(), origin_line);
    assert_eq!(calls, 0);
    assert_eq!(commits.load(Ordering::SeqCst), 0);
    block_on(store.finish()).unwrap();
}

fn write(shared: &Shared<u64>, value: u64) {
    shared
        .try_update(
            |current| {
                *current = value;
                Ok::<_, Infallible>(())
            },
            || {},
        )
        .unwrap();
}

fn failed_drop(store: TestStore<State, Action, ()>) {
    let panic = catch_unwind(AssertUnwindSafe(|| drop(store)))
        .expect_err("unverified captures must fail normal teardown");
    let message = panic
        .downcast_ref::<String>()
        .expect("TestStore teardown formats its diagnostic");
    assert!(
        message.starts_with("TestStore teardown failed: TestStore incomplete assertion ")
            || message.starts_with("TestStore teardown failed: TestStore capture "),
        "unexpected teardown panic: {message}"
    );
}

#[test]
fn omitted_private_or_shared_mutation_fails_in_both_modes() {
    for mode in [Exhaustivity::Exhaustive, Exhaustivity::NonExhaustive] {
        for omit_private in [false, true] {
            let (mut store, _, commits) = fixture(mode);
            let failure = block_on(store.send_with_captures(
                Action::Mutate,
                |()| {},
                move |expected| {
                    if omit_private {
                        expected.set::<SharedCapture>(7)
                    } else {
                        expected.set::<PrivateCapture>(1)
                    }
                },
            ))
            .unwrap_err();
            assert!(failure.is_capture_mismatch());
            assert_eq!(commits.load(Ordering::SeqCst), 1);
            assert!(
                block_on(store.finish())
                    .unwrap_err()
                    .is_incomplete_assertion()
            );
            failed_drop(store);
        }
    }
}

#[test]
fn declared_changes_are_asserted_on_send_and_receive() {
    for receive in [false, true] {
        let (mut store, shared, _) = fixture(Exhaustivity::Exhaustive);
        if receive {
            let _task = block_on(store.send(Action::Start)).unwrap();
            let _task = block_on(store.receive_with_captures(
                |a| matches!(a, Action::Mutate),
                |()| {},
                |expected| {
                    expected.set::<PrivateCapture>(1)?;
                    expected.set::<SharedCapture>(7)
                },
            ))
            .unwrap();
        } else {
            let _task = block_on(store.send_with_captures(
                Action::Mutate,
                |()| {},
                |expected| {
                    expected.set::<PrivateCapture>(1)?;
                    expected.set::<SharedCapture>(7)
                },
            ))
            .unwrap();
        }
        assert_eq!(shared.read(|value| *value).unwrap(), 7);
        block_on(store.finish()).unwrap();
    }
}

#[test]
fn later_external_equality_cannot_clear_an_admitted_mismatch() {
    let (mut store, shared, _) = fixture(Exhaustivity::Exhaustive);
    let error = block_on(store.send_with_captures(
        Action::Mutate,
        |()| {},
        |expected| expected.set::<PrivateCapture>(1),
    ))
    .unwrap_err();
    assert!(error.is_capture_mismatch());
    write(&shared, 5);
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
    failed_drop(store);
}

#[test]
fn outside_change_requires_an_explicit_boundary_before_action() {
    let (mut store, shared, commits) = fixture(Exhaustivity::Exhaustive);
    write(&shared, 9);
    let error = block_on(store.send_with_captures(
        Action::Mutate,
        |()| {},
        |expected| {
            expected.set::<PrivateCapture>(1)?;
            expected.set::<SharedCapture>(7)
        },
    ))
    .unwrap_err();
    assert!(error.is_capture_mismatch());
    assert_eq!(commits.load(Ordering::SeqCst), 0);
    failed_drop(store);

    let (mut store, shared, _) = fixture(Exhaustivity::Exhaustive);
    write(&shared, 9);
    block_on(store.check_captures(|expected| expected.set::<SharedCapture>(9))).unwrap();
    let _task = block_on(store.send_with_captures(
        Action::Mutate,
        |()| {},
        |expected| {
            expected.set::<PrivateCapture>(1)?;
            expected.set::<SharedCapture>(7)
        },
    ))
    .unwrap();
    block_on(store.finish()).unwrap();
}

#[test]
fn finish_and_normal_drop_detect_unchecked_external_changes() {
    for explicit_finish in [false, true] {
        let (mut store, shared, _) = fixture(Exhaustivity::Exhaustive);
        write(&shared, 9);
        if explicit_finish {
            assert!(block_on(store.finish()).unwrap_err().is_capture_mismatch());
        }
        failed_drop(store);
    }
}

#[test]
fn capture_mismatch_and_debt_survive_failed_manual_reporting() {
    // These reporters are not installed in TestStore. This checks that failed
    // caller-side reporting leaves the already returned mismatch and debt intact.
    let collector = IssueCollector::new();
    let closed = collector.reporter();
    assert!(collector.finish().is_empty());
    for reporter in [IssueReporter::default(), closed] {
        let (mut store, _, _) = fixture(Exhaustivity::NonExhaustive);
        let failure = block_on(store.send_with_captures(
            Action::Mutate,
            |()| {},
            |expected| expected.set::<PrivateCapture>(1),
        ))
        .unwrap_err();
        assert!(
            reporter
                .report(Issue::new("declared-capture", "required mismatch"))
                .is_err()
        );
        assert!(failure.is_capture_mismatch());
        assert!(
            block_on(store.finish())
                .unwrap_err()
                .is_incomplete_assertion()
        );
        failed_drop(store);
    }
}

struct OpaqueCause(Cell<u64>);
struct FallibleCapture;
impl CaptureKey for FallibleCapture {
    type Value = u64;
    type Error = OpaqueCause;
}
struct OtherFallibleCapture;
impl CaptureKey for OtherFallibleCapture {
    type Value = u64;
    type Error = OpaqueCause;
}

#[test]
fn capture_cause_preserves_key_identity_and_send_only_value() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<TestStoreError>();
    let builder = TestStore::projected_builder(
        State { private: 0 },
        Feature {
            shared: Shared::new(5),
            commits: Arc::new(AtomicUsize::new(0)),
        },
        |_| (),
    );
    let error = builder
        .with_capture::<FallibleCapture>(|_| Err(OpaqueCause(Cell::new(41))))
        .unwrap_err();
    let Err(error) = error.into_capture_error::<OtherFallibleCapture>() else {
        panic!("equal cause types must not erase declaration identity");
    };
    let cause = error.into_capture_error::<FallibleCapture>().unwrap();
    assert_eq!(cause.0.get(), 41);
}

#[test]
fn later_capture_failure_retains_typed_cause_and_sticky_boundary() {
    let calls = Arc::new(AtomicUsize::new(0));
    let captured_calls = Arc::clone(&calls);
    let mut store = TestStore::projected_builder(
        State { private: 0 },
        Feature {
            shared: Shared::new(5),
            commits: Arc::new(AtomicUsize::new(0)),
        },
        |_| (),
    )
    .with_capture::<FallibleCapture>(move |_| {
        if captured_calls.fetch_add(1, Ordering::SeqCst) == 0 {
            Ok(0)
        } else {
            Err(OpaqueCause(Cell::new(42)))
        }
    })
    .unwrap()
    .build();
    let failure = block_on(store.check_captures(|_| Ok(()))).unwrap_err();
    assert!(failure.to_string().contains("check_captures"));
    assert_eq!(
        failure
            .into_capture_error::<FallibleCapture>()
            .unwrap()
            .0
            .get(),
        42
    );
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert!(
        block_on(store.finish())
            .unwrap_err()
            .is_incomplete_assertion()
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "debt refuses fresh captures"
    );
    failed_drop(store);
}

/// Return values remain owned while cleanup runs at the public API boundary.
mod outer_api_cleanup {
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    };

    use rca_core::{Effect, Feature, FeatureContext, Reduce, Task};
    use rca_test::{CaptureKey, TestStore, TestStoreError};

    use super::finish_without_external_wake;

    struct Cause(Arc<AtomicUsize>);

    impl Drop for Cause {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    struct Captured;

    impl CaptureKey for Captured {
        type Value = usize;
        type Error = Cause;
    }

    #[derive(Debug)]
    struct Snapshot {
        value: usize,
        panic_on_drop: bool,
    }

    impl Clone for Snapshot {
        fn clone(&self) -> Self {
            Self {
                value: self.value,
                panic_on_drop: false,
            }
        }
    }

    impl PartialEq for Snapshot {
        fn eq(&self, other: &Self) -> bool {
            self.value == other.value
        }
    }

    impl Drop for Snapshot {
        fn drop(&mut self) {
            assert!(!self.panic_on_drop, "outer snapshot cleanup");
        }
    }

    struct UnusedInput {
        name: &'static str,
        order: Arc<std::sync::Mutex<Vec<&'static str>>>,
        panic_on_drop: bool,
    }

    impl Drop for UnusedInput {
        fn drop(&mut self) {
            self.order.lock().unwrap().push(self.name);
            assert!(!self.panic_on_drop, "unused input cleanup");
        }
    }

    struct PendingMutation {
        typed: bool,
    }

    impl Feature for PendingMutation {
        type State = ();
        type Action = ();

        fn update(&mut self, (): &mut (), (): &(), context: &mut FeatureContext<'_, (), ()>) {
            let typed = self.typed;
            context.task(Task::<(), ()>::run(move |context| async move {
                if typed {
                    let _ = context.update(|()| ()).await;
                } else {
                    let _ = context.send(()).await;
                }
            }));
        }
    }

    fn capture_error(drops: Arc<AtomicUsize>) -> TestStoreError {
        TestStore::builder((), Reduce::new(|(): &mut (), (): &(), _| Effect::none()))
            .with_capture::<Captured>(move |()| Err(Cause(Arc::clone(&drops))))
            .expect_err("the public declaration returns the original typed cause")
    }

    #[test]
    fn failed_declaration_keeps_cause_owned_while_builder_state_drops() {
        let drops = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&drops);
        let panic = catch_unwind(AssertUnwindSafe(|| {
            let builder = TestStore::projected_builder(
                Snapshot {
                    value: 0,
                    panic_on_drop: true,
                },
                Reduce::new(|_: &mut Snapshot, (): &(), _| Effect::none()),
                |_| (),
            );
            drop(builder.with_capture::<Captured>(move |_| Err(Cause(Arc::clone(&observed)))));
        }))
        .expect_err("builder cleanup must propagate the original destructor panic");
        assert_eq!(
            panic.downcast_ref::<&str>(),
            Some(&"outer snapshot cleanup")
        );
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn invalid_expectation_keeps_cause_owned_while_root_snapshot_drops() {
        for receive in [false, true] {
            let drops = Arc::new(AtomicUsize::new(0));
            let observed = Arc::clone(&drops);
            let panic = catch_unwind(AssertUnwindSafe(|| {
                let mut store = TestStore::projected_builder(
                    0_usize,
                    Reduce::new(|state: &mut usize, action: &bool, _| {
                        if *action {
                            *state += 1;
                            Effect::none()
                        } else {
                            Effect::send(true)
                        }
                    }),
                    |value| Snapshot {
                        value: *value,
                        panic_on_drop: false,
                    },
                )
                .build();
                if receive {
                    let _ = finish_without_external_wake(store.send(false)).unwrap();
                }
                let error = capture_error(observed);
                let update = |snapshot: &mut Snapshot| snapshot.panic_on_drop = true;
                let captures = move |_: &mut rca_test::CaptureExpectations<'_>| Err(error);
                if receive {
                    drop(finish_without_external_wake(store.receive_with_captures(
                        |action| *action,
                        update,
                        captures,
                    )));
                } else {
                    drop(finish_without_external_wake(
                        store.send_with_captures(true, update, captures),
                    ));
                }
            }))
            .expect_err("expected snapshot cleanup must propagate its original panic");
            assert_eq!(
                panic.downcast_ref::<&str>(),
                Some(&"outer snapshot cleanup")
            );
            assert_eq!(drops.load(Ordering::SeqCst), 1, "receive={receive}");
        }
    }

    #[test]
    fn failed_capture_keeps_cause_owned_while_actual_root_snapshot_drops() {
        let drops = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&drops);
        let panic = catch_unwind(AssertUnwindSafe(|| {
            let mut store = TestStore::projected_builder(
                0_usize,
                Reduce::new(|state: &mut usize, (): &(), _| {
                    *state += 1;
                    Effect::none()
                }),
                |value| Snapshot {
                    value: *value,
                    panic_on_drop: *value == 1,
                },
            )
            .with_capture::<Captured>(move |value| {
                if *value == 0 {
                    Ok(0)
                } else {
                    Err(Cause(Arc::clone(&observed)))
                }
            })
            .unwrap()
            .build();
            drop(finish_without_external_wake(
                store.send_with((), |snapshot| snapshot.value = 1),
            ));
        }))
        .expect_err("actual snapshot cleanup must propagate its original panic");
        assert_eq!(
            panic.downcast_ref::<&str>(),
            Some(&"outer snapshot cleanup")
        );
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn invalid_expectation_keeps_cause_owned_while_rejected_action_drops() {
        let drops = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&drops);
        let panic = catch_unwind(AssertUnwindSafe(|| {
            let mut store = TestStore::builder(
                (),
                Reduce::new(|(): &mut (), _: &Snapshot, _| Effect::none()),
            )
            .build();
            let error = capture_error(observed);
            drop(finish_without_external_wake(store.send_with_captures(
                Snapshot {
                    value: 0,
                    panic_on_drop: true,
                },
                |()| {},
                move |_| Err(error),
            )));
        }))
        .expect_err("rejected action cleanup must propagate its original panic");
        assert_eq!(
            panic.downcast_ref::<&str>(),
            Some(&"outer snapshot cleanup")
        );
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn preflight_failure_keeps_cause_owned_while_unused_inputs_drop() {
        let mut counts = [0; 3];
        let mut cleanup_orders = Vec::new();
        for (panicking, count) in counts.iter_mut().enumerate() {
            let drops = Arc::new(AtomicUsize::new(0));
            let observed = Arc::clone(&drops);
            let calls = AtomicUsize::new(0);
            let mut store = TestStore::builder(
                (),
                Reduce::new(|(): &mut (), _: &UnusedInput, _| Effect::none()),
            )
            .with_capture::<Captured>(move |()| {
                if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                    Ok(0)
                } else {
                    Err(Cause(Arc::clone(&observed)))
                }
            })
            .unwrap()
            .build();
            let order = Arc::new(std::sync::Mutex::new(Vec::new()));
            let input = |index, name| UnusedInput {
                name,
                order: Arc::clone(&order),
                panic_on_drop: index == panicking,
            };
            let action = input(0, "action");
            let update = input(1, "update");
            let captures = input(2, "captures");
            let panic = catch_unwind(AssertUnwindSafe(|| {
                drop(finish_without_external_wake(store.send_with_captures(
                    action,
                    move |()| {
                        std::hint::black_box(&update);
                    },
                    move |_| {
                        std::hint::black_box(&captures);
                        Ok(())
                    },
                )));
            }))
            .expect_err("an unused input's destructor must propagate its panic");
            catch_unwind(AssertUnwindSafe(|| drop(store)))
                .expect_err("failed capture preflight must retain assertion debt");

            assert_eq!(panic.downcast_ref::<&str>(), Some(&"unused input cleanup"));
            *count = drops.load(Ordering::SeqCst);
            cleanup_orders.push(order.lock().unwrap().clone());
        }
        println!("preflight_input_cleanup_orders={cleanup_orders:?}");
        assert_eq!(counts, [1; 3]);
    }

    #[test]
    fn receive_preflight_keeps_cause_owned_while_unused_callbacks_drop() {
        let mut counts = [[0; 2]; 2];
        let mut cleanup_orders = Vec::new();
        for (typed, path_counts) in counts.iter_mut().enumerate() {
            for (panicking, count) in path_counts.iter_mut().enumerate() {
                let drops = Arc::new(AtomicUsize::new(0));
                let observed = Arc::clone(&drops);
                let armed = Arc::new(AtomicBool::new(false));
                let capture_armed = Arc::clone(&armed);
                let mut store = TestStore::builder((), PendingMutation { typed: typed == 1 })
                    .with_capture::<Captured>(move |()| {
                        if capture_armed.load(Ordering::SeqCst) {
                            Err(Cause(Arc::clone(&observed)))
                        } else {
                            Ok(0)
                        }
                    })
                    .unwrap()
                    .build();
                let task = finish_without_external_wake(store.send(())).unwrap();
                armed.store(true, Ordering::SeqCst);

                let order = Arc::new(std::sync::Mutex::new(Vec::new()));
                let input = |index, name| UnusedInput {
                    name,
                    order: Arc::clone(&order),
                    panic_on_drop: index == panicking,
                };
                let update = input(0, "update");
                let captures = input(1, "captures");
                let update = move |(): &mut ()| {
                    std::hint::black_box(&update);
                };
                let captures = move |_: &mut rca_test::CaptureExpectations<'_>| {
                    std::hint::black_box(&captures);
                    Ok(())
                };
                let panic = catch_unwind(AssertUnwindSafe(|| {
                    if typed == 1 {
                        drop(finish_without_external_wake(
                            store.receive_update_with_captures(update, captures),
                        ));
                    } else {
                        drop(finish_without_external_wake(store.receive_with_captures(
                            |()| true,
                            update,
                            captures,
                        )));
                    }
                }))
                .expect_err("an unused callback's destructor must propagate its panic");
                catch_unwind(AssertUnwindSafe(|| drop(store)))
                    .expect_err("failed receive preflight must retain assertion debt");
                drop(task);

                assert_eq!(panic.downcast_ref::<&str>(), Some(&"unused input cleanup"));
                *count = drops.load(Ordering::SeqCst);
                cleanup_orders.push(order.lock().unwrap().clone());
            }
        }
        println!("receive_preflight_cleanup_orders={cleanup_orders:?}");
        assert_eq!(counts, [[1; 2]; 2]);
    }
}

/// Internal declaration/history cleanup preserves the original typed cause.
mod capture_error_cleanup {
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    };

    use futures_executor::block_on;
    use rca_core::Shared;
    use rca_test::{CaptureKey, TestStore};

    use super::{Feature, State};

    struct Cause(Arc<AtomicUsize>);

    impl Drop for Cause {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    struct CallbackCapture {
        calls: Arc<AtomicUsize>,
        drops: Arc<AtomicUsize>,
        armed: Arc<AtomicBool>,
    }

    impl CallbackCapture {
        fn called(&self) {
            self.calls.fetch_add(1, Ordering::SeqCst);
        }
    }

    impl Drop for CallbackCapture {
        fn drop(&mut self) {
            self.drops.fetch_add(1, Ordering::SeqCst);
            assert!(
                !self.armed.swap(false, Ordering::SeqCst),
                "declaration callback cleanup panic"
            );
        }
    }

    struct Declaration;

    impl CaptureKey for Declaration {
        type Value = u64;
        type Error = Cause;
    }

    #[test]
    fn declaration_cleanup_panic_drops_the_original_capture_cause() {
        let calls = Arc::new(AtomicUsize::new(0));
        let callback_drops = Arc::new(AtomicUsize::new(0));
        let cause_drops = Arc::new(AtomicUsize::new(0));
        let callback = CallbackCapture {
            calls: Arc::clone(&calls),
            drops: Arc::clone(&callback_drops),
            armed: Arc::new(AtomicBool::new(true)),
        };
        let returned_cause_drops = Arc::clone(&cause_drops);
        let builder = TestStore::projected_builder(
            State { private: 0 },
            Feature {
                shared: Shared::new(5),
                commits: Arc::new(AtomicUsize::new(0)),
            },
            |_| (),
        );
        let result = catch_unwind(AssertUnwindSafe(|| {
            builder.with_capture::<Declaration>(move |_| {
                callback.called();
                Err(Cause(Arc::clone(&returned_cause_drops)))
            })
        }));
        let panic = result.expect_err("the callback's destructor must propagate its panic");
        assert_eq!(
            panic.downcast_ref::<&str>(),
            Some(&"declaration callback cleanup panic")
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(callback_drops.load(Ordering::SeqCst), 1);
        assert_eq!(cause_drops.load(Ordering::SeqCst), 1);
    }

    struct Value {
        armed: Arc<AtomicBool>,
        expected_drops: Arc<AtomicUsize>,
        expected: bool,
    }

    impl Clone for Value {
        fn clone(&self) -> Self {
            Self {
                armed: Arc::clone(&self.armed),
                expected_drops: Arc::clone(&self.expected_drops),
                expected: true,
            }
        }
    }

    impl PartialEq for Value {
        fn eq(&self, _: &Self) -> bool {
            true
        }
    }

    impl Drop for Value {
        fn drop(&mut self) {
            if self.expected {
                self.expected_drops.fetch_add(1, Ordering::SeqCst);
                assert!(
                    !self.armed.swap(false, Ordering::SeqCst),
                    "expected capture cleanup panic"
                );
            }
        }
    }

    struct Boundary;

    impl CaptureKey for Boundary {
        type Value = Value;
        type Error = Cause;
    }

    #[test]
    fn expected_cleanup_panic_drops_the_original_capture_cause_and_preserves_debt() {
        let calls = Arc::new(AtomicUsize::new(0));
        let expected_drops = Arc::new(AtomicUsize::new(0));
        let cause_drops = Arc::new(AtomicUsize::new(0));
        let armed = Arc::new(AtomicBool::new(false));
        let captured_calls = Arc::clone(&calls);
        let captured_expected_drops = Arc::clone(&expected_drops);
        let captured_cause_drops = Arc::clone(&cause_drops);
        let captured_armed = Arc::clone(&armed);
        let mut store = TestStore::projected_builder(
            State { private: 0 },
            Feature {
                shared: Shared::new(5),
                commits: Arc::new(AtomicUsize::new(0)),
            },
            |_| (),
        )
        .with_capture::<Boundary>(move |_| {
            if captured_calls.fetch_add(1, Ordering::SeqCst) == 0 {
                Ok(Value {
                    armed: Arc::clone(&captured_armed),
                    expected_drops: Arc::clone(&captured_expected_drops),
                    expected: false,
                })
            } else {
                Err(Cause(Arc::clone(&captured_cause_drops)))
            }
        })
        .unwrap()
        .build();
        let result = catch_unwind(AssertUnwindSafe(|| {
            block_on(store.check_captures(|_| {
                armed.store(true, Ordering::SeqCst);
                Ok(())
            }))
        }));
        // Disarm before any assertions or teardown, including a failing regression.
        armed.store(false, Ordering::SeqCst);
        let finish = block_on(store.finish());
        let teardown = catch_unwind(AssertUnwindSafe(|| drop(store)));

        let panic = result.expect_err("the expected value's destructor must propagate its panic");
        assert_eq!(
            panic.downcast_ref::<&str>(),
            Some(&"expected capture cleanup panic")
        );
        assert!(finish.unwrap_err().is_incomplete_assertion());
        assert!(teardown.is_err());
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "debt refuses fresh captures"
        );
        assert_eq!(expected_drops.load(Ordering::SeqCst), 1);
        assert_eq!(cause_drops.load(Ordering::SeqCst), 1);
    }
}

fn assert_typed_capture_failure(
    failure: TestStoreError,
    operation: &str,
    origin_line: u32,
    cause: u64,
) {
    assert_eq!(failure.origin().file(), file!());
    assert_eq!(failure.origin().line(), origin_line);
    assert!(failure.to_string().starts_with(&format!(
        "TestStore capture {} Failed during {operation} at ",
        std::any::type_name::<FallibleCapture>()
    )));
    assert_eq!(
        failure
            .into_capture_error::<FallibleCapture>()
            .unwrap()
            .0
            .get(),
        cause
    );
}

#[test]
fn admitted_capture_failure_preserves_committed_state_and_boundary_debt() {
    for (receive, capture_count) in [(false, 3), (true, 5)] {
        for fail in [false, true] {
            let calls = Arc::new(AtomicUsize::new(0));
            let captured_calls = Arc::clone(&calls);
            let weak_capture = Arc::downgrade(&calls);
            let commits = Arc::new(AtomicUsize::new(0));
            let shared = Shared::new(5);
            let mut store = TestStore::projected_builder(
                State { private: 0 },
                Feature {
                    shared: shared.clone(),
                    commits: Arc::clone(&commits),
                },
                |_| (),
            )
            .with_capture::<FallibleCapture>(move |state| {
                captured_calls.fetch_add(1, Ordering::SeqCst);
                if fail && state.private == 1 {
                    Err(OpaqueCause(Cell::new(42)))
                } else {
                    Ok(state.private)
                }
            })
            .unwrap()
            .build();
            let (result, origin_line, operation) = if receive {
                drop(finish_without_external_wake(store.send(Action::Start)).unwrap());
                let origin_line = line!() + 1;
                let result = finish_without_external_wake(store.receive_with_captures(
                    |action| matches!(action, Action::Mutate),
                    |()| {},
                    |expected| expected.set::<FallibleCapture>(1),
                ));
                (result, origin_line, "receive")
            } else {
                let origin_line = line!() + 1;
                let result = finish_without_external_wake(store.send_with_captures(
                    Action::Mutate,
                    |()| {},
                    |expected| expected.set::<FallibleCapture>(1),
                ));
                (result, origin_line, "send")
            };
            assert_eq!(commits.load(Ordering::SeqCst), 1);
            assert_eq!(shared.read(|value| *value).unwrap(), 7);
            finish_without_external_wake(store.assert_state(|state| assert_eq!(state.private, 1)))
                .unwrap();
            assert_eq!(calls.load(Ordering::SeqCst), capture_count);
            if fail {
                assert_typed_capture_failure(result.unwrap_err(), operation, origin_line, 42);

                let receive_error = finish_without_external_wake(store.receive_with_captures(
                    |_| panic!("earlier assertion debt must precede matching"),
                    |()| panic!("earlier assertion debt must precede snapshot expectations"),
                    |_| panic!("earlier assertion debt must precede capture expectations"),
                ))
                .expect_err("ordinary receive must retain the original failed assertion");
                assert!(receive_error.is_incomplete_assertion());
                assert_eq!(receive_error.origin().file(), file!());
                assert_eq!(receive_error.origin().line(), origin_line);

                let debt = finish_without_external_wake(store.finish()).unwrap_err();
                assert!(debt.is_incomplete_assertion());
                assert_eq!(debt.origin().file(), file!());
                assert_eq!(debt.origin().line(), origin_line);
                failed_drop(store);
                assert_eq!(calls.load(Ordering::SeqCst), capture_count);
            } else {
                finish_without_external_wake(result.unwrap().finish()).unwrap();
                finish_without_external_wake(store.finish()).unwrap();
                drop(store);
                assert_eq!(calls.load(Ordering::SeqCst), capture_count + 1);
            }
            assert_eq!(commits.load(Ordering::SeqCst), 1);
            drop(calls);
            assert!(weak_capture.upgrade().is_none());
        }
    }
}

#[test]
fn finish_capture_failure_preserves_typed_cause_and_boundary_debt() {
    for fail in [false, true] {
        let calls = Arc::new(AtomicUsize::new(0));
        let captured_calls = Arc::clone(&calls);
        let weak_capture = Arc::downgrade(&calls);
        let unavailable = Arc::new(AtomicUsize::new(0));
        let captured_unavailable = Arc::clone(&unavailable);
        let commits = Arc::new(AtomicUsize::new(0));
        let shared = Shared::new(5);
        let mut store = TestStore::projected_builder(
            State { private: 0 },
            Feature {
                shared: shared.clone(),
                commits: Arc::clone(&commits),
            },
            |_| (),
        )
        .with_capture::<FallibleCapture>(move |state| {
            captured_calls.fetch_add(1, Ordering::SeqCst);
            if captured_unavailable.load(Ordering::SeqCst) == 0 {
                Ok(state.private)
            } else {
                Err(OpaqueCause(Cell::new(43)))
            }
        })
        .unwrap()
        .build();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        // External source availability changes only between capture boundaries.
        unavailable.store(usize::from(fail), Ordering::SeqCst);
        let origin_line = line!() + 1;
        let result = finish_without_external_wake(store.finish());
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        if fail {
            assert_typed_capture_failure(result.unwrap_err(), "finish", origin_line, 43);
            finish_without_external_wake(store.assert_state(|state| assert_eq!(state.private, 0)))
                .unwrap();
            unavailable.store(0, Ordering::SeqCst);
            let debt = finish_without_external_wake(store.finish()).unwrap_err();
            assert!(debt.is_incomplete_assertion());
            assert_eq!(debt.origin().file(), file!());
            assert_eq!(debt.origin().line(), origin_line);
            failed_drop(store);
        } else {
            result.unwrap();
            drop(store);
        }
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(commits.load(Ordering::SeqCst), 0);
        assert_eq!(shared.read(|value| *value).unwrap(), 5);
        drop(calls);
        assert!(weak_capture.upgrade().is_none());
    }
}

#[test]
fn invalid_expectation_keys_fail_before_action_admission() {
    for duplicate in [false, true] {
        let (mut store, _, commits) = fixture(Exhaustivity::Exhaustive);
        let failure = block_on(store.send_with_captures(
            Action::Mutate,
            |()| {},
            move |expected| {
                if duplicate {
                    expected.set::<PrivateCapture>(1)?;
                    expected.set::<PrivateCapture>(1)
                } else {
                    expected.set::<FallibleCapture>(1)
                }
            },
        ))
        .unwrap_err();
        assert!(!failure.is_capture_mismatch());
        assert_eq!(commits.load(Ordering::SeqCst), 0);
        block_on(store.finish()).unwrap();
    }
}

#[test]
fn duplicate_declaration_rejects_before_the_second_capture_and_drops_the_builder() {
    let first_calls = Arc::new(AtomicUsize::new(0));
    let second_calls = Arc::new(AtomicUsize::new(0));
    let first_capture_calls = Arc::clone(&first_calls);
    let builder = TestStore::projected_builder(
        State { private: 0 },
        Feature {
            shared: Shared::new(5),
            commits: Arc::new(AtomicUsize::new(0)),
        },
        |_| (),
    )
    .with_capture::<PrivateCapture>(move |state| {
        first_capture_calls.fetch_add(1, Ordering::SeqCst);
        Ok(state.private)
    })
    .unwrap();
    assert_eq!(first_calls.load(Ordering::SeqCst), 1);
    assert_eq!(Arc::strong_count(&first_calls), 2);

    let second_capture_calls = Arc::clone(&second_calls);
    let error: TestStoreError = builder
        .with_capture::<PrivateCapture>(move |_| {
            second_capture_calls.fetch_add(1, Ordering::SeqCst);
            Ok(99)
        })
        .unwrap_err();
    let error = error
        .into_capture_error::<PrivateCapture>()
        .expect_err("a duplicate declaration has no capture cause");
    assert!(error.to_string().contains("DuplicateDeclaration"));
    assert!(!error.is_capture_mismatch());
    assert_eq!(first_calls.load(Ordering::SeqCst), 1);
    assert_eq!(second_calls.load(Ordering::SeqCst), 0);
    // with_capture consumes the builder: neither callback may remain retained.
    assert_eq!(Arc::strong_count(&first_calls), 1);
    assert_eq!(Arc::strong_count(&second_calls), 1);
}

#[test]
fn empty_skip_preserves_declared_captures_without_admitting_an_assertion() {
    for omit_shared in [false, true] {
        let (mut store, shared, commits) = fixture(Exhaustivity::NonExhaustive);
        let origin_line = line!() + 1;
        let error = block_on(store.skip_received()).expect_err("no effect action exists");
        let error = error
            .into_capture_error::<PrivateCapture>()
            .expect_err("an empty skip has no capture cause");
        assert!(error.is_no_received_action());
        assert!(!error.is_no_issue());
        assert!(!error.is_unreceived_action());
        assert_eq!(error.origin().file(), file!());
        assert_eq!(error.origin().line(), origin_line);
        let display = error.to_string();
        assert!(display.starts_with("no effect action awaits receive at "));
        let debug = format!("{error:?}");
        assert!(debug.contains("kind: \"NoReceivedAction\""));
        assert!(std::error::Error::source(&error).is_none());
        assert_eq!(commits.load(Ordering::SeqCst), 0);
        assert_eq!(shared.read(|value| *value).unwrap(), 5);

        let _start = block_on(store.send(Action::Start))
            .expect("the rejected skip did not admit an incomplete assertion");
        let received = block_on(store.receive_with_captures(
            |action| matches!(action, Action::Mutate),
            |()| {},
            |expected| {
                expected.set::<PrivateCapture>(1)?;
                if omit_shared {
                    Ok(())
                } else {
                    expected.set::<SharedCapture>(7)
                }
            },
        ));
        assert_eq!(commits.load(Ordering::SeqCst), 1);
        assert_eq!(shared.read(|value| *value).unwrap(), 7);
        if omit_shared {
            let error = received.expect_err("the shared declaration remains mandatory");
            assert!(error.is_capture_mismatch());
            assert!(
                block_on(store.finish())
                    .unwrap_err()
                    .is_incomplete_assertion()
            );
            failed_drop(store);
        } else {
            let _received = received.expect("both retained declarations are acknowledged");
            block_on(store.finish()).expect("clean finish after empty-skip rejection");
        }
        assert_eq!(error.to_string(), display);
        assert_eq!(format!("{error:?}"), debug);
    }
}

#[test]
fn capture_panic_does_not_discharge_the_admitted_check() {
    let calls = Arc::new(AtomicUsize::new(0));
    let captured_calls = Arc::clone(&calls);
    let mut store = TestStore::projected_builder(
        State { private: 0 },
        Feature {
            shared: Shared::new(5),
            commits: Arc::new(AtomicUsize::new(0)),
        },
        |_| (),
    )
    .with_capture::<PrivateCapture>(move |_| {
        assert_eq!(
            captured_calls.fetch_add(1, Ordering::SeqCst),
            0,
            "capture panic"
        );
        Ok(0)
    })
    .unwrap()
    .build();
    let panic = catch_unwind(AssertUnwindSafe(|| {
        block_on(store.check_captures(|_| Ok(())))
    }))
    .expect_err("capture panic must propagate");
    assert!(
        panic
            .downcast_ref::<String>()
            .expect("capture assertion formats its panic")
            .contains("capture panic")
    );
    assert!(
        block_on(store.finish())
            .unwrap_err()
            .is_incomplete_assertion()
    );
    failed_drop(store);
    assert_eq!(calls.load(Ordering::SeqCst), 2, "debt must not recapture");
}

/// Real composition adapters mutate private state and a separately captured alias.
mod scopes {
    use super::*;
    use rca_core::{
        ActionPath, CaseStatePath, KeyedAction, KeyedActionPath, KeyedState, KeyedStatePath,
        OptionalStatePath, ReducerExt,
    };

    enum Screen {
        Off,
        On(u64),
    }
    struct ParentState {
        optional: Option<u64>,
        screen: Screen,
        rows: KeyedState<u8, u64>,
    }
    enum ParentAction {
        Optional(()),
        Case(()),
        Row(KeyedAction<u8, ()>),
        Hide,
    }
    struct Parent;
    impl Reducer for Parent {
        type State = ParentState;
        type Action = ParentAction;
        fn reduce(
            &mut self,
            state: &mut ParentState,
            action: &ParentAction,
            _: &mut ReduceContext<'_>,
        ) -> Effect<ParentAction> {
            if matches!(action, ParentAction::Hide) {
                state.screen = Screen::Off;
            }
            Effect::none()
        }
    }
    struct Child(Shared<u64>);
    impl Reducer for Child {
        type State = u64;
        type Action = ();
        fn reduce(&mut self, state: &mut u64, (): &(), _: &mut ReduceContext<'_>) -> Effect<()> {
            *state = 1;
            write(&self.0, 7);
            Effect::none()
        }
    }
    struct ChildrenCapture;
    impl CaptureKey for ChildrenCapture {
        type Value = (Option<u64>, Option<u64>, Vec<(u8, u64)>);
        type Error = Infallible;
    }

    fn scoped_fixture() -> TestStore<ParentState, ParentAction, ()> {
        let shared = Shared::new(5);
        let reducer = Parent
            .if_some(
                OptionalStatePath::new(
                    |s: &ParentState| s.optional.as_ref(),
                    |s: &mut ParentState| s.optional.as_mut(),
                ),
                ActionPath::new(
                    |a: &ParentAction| match a {
                        ParentAction::Optional(a) => Some(a),
                        _ => None,
                    },
                    ParentAction::Optional,
                ),
                Child(shared.clone()),
            )
            .if_case(
                CaseStatePath::new(
                    |s: &ParentState| match &s.screen {
                        Screen::On(v) => Some(v),
                        Screen::Off => None,
                    },
                    |s: &mut ParentState| match &mut s.screen {
                        Screen::On(v) => Some(v),
                        Screen::Off => None,
                    },
                ),
                ActionPath::new(
                    |a: &ParentAction| match a {
                        ParentAction::Case(a) => Some(a),
                        _ => None,
                    },
                    ParentAction::Case,
                ),
                Child(shared.clone()),
            )
            .for_each(
                KeyedStatePath::new(|s: &ParentState| &s.rows, |s: &mut ParentState| &mut s.rows),
                KeyedActionPath::new(
                    |a: &ParentAction| match a {
                        ParentAction::Row(a) => Some(a),
                        _ => None,
                    },
                    ParentAction::Row,
                ),
                Child(shared.clone()),
            );
        let initial = ParentState {
            optional: Some(0),
            screen: Screen::On(0),
            rows: [(1, 0), (2, 0)].into_iter().collect(),
        };
        TestStore::projected_builder(initial, reducer, |_| ())
            .with_capture::<ChildrenCapture>(|s| {
                Ok((
                    s.optional,
                    match s.screen {
                        Screen::Off => None,
                        Screen::On(v) => Some(v),
                    },
                    s.rows.iter().map(|(id, value)| (*id, *value)).collect(),
                ))
            })
            .unwrap()
            .with_capture::<SharedCapture>(move |_| shared.read(|value| *value))
            .unwrap()
            .build()
    }

    // The real optional/case/keyed reducers mutate private state and a shared alias.
    #[test]
    fn scoped_private_and_shared_omissions_fail_independently() {
        for family in 0..3 {
            for omit_private in [false, true] {
                let mut store = scoped_fixture();
                let (action, wanted) = match family {
                    0 => (
                        ParentAction::Optional(()),
                        (Some(1), Some(0), vec![(1, 0), (2, 0)]),
                    ),
                    1 => (
                        ParentAction::Case(()),
                        (Some(0), Some(1), vec![(1, 0), (2, 0)]),
                    ),
                    _ => (
                        ParentAction::Row(KeyedAction::new(1, ())),
                        (Some(0), Some(0), vec![(1, 1), (2, 0)]),
                    ),
                };
                let error = block_on(store.send_with_captures(
                    action,
                    |()| {},
                    move |expected| {
                        if omit_private {
                            expected.set::<SharedCapture>(7)
                        } else {
                            expected.set::<ChildrenCapture>(wanted)
                        }
                    },
                ))
                .unwrap_err();
                assert!(error.is_capture_mismatch());
                assert!(
                    block_on(store.finish())
                        .unwrap_err()
                        .is_incomplete_assertion()
                );
                assert!(catch_unwind(AssertUnwindSafe(|| drop(store))).is_err());
            }
        }
    }

    #[test]
    fn case_removal_preserves_unrelated_child_and_shared_capture_values() {
        let mut store = scoped_fixture();
        let task = block_on(store.send_with_captures(
            ParentAction::Hide,
            |()| {},
            |expected| expected.set::<ChildrenCapture>((Some(0), None, vec![(1, 0), (2, 0)])),
        ))
        .unwrap();
        block_on(task.finish()).unwrap();
        block_on(store.finish()).unwrap();
    }
}

#[test]
fn receive_preflight_rejection_skips_expectation_callbacks_and_reduction() {
    for acknowledge in [false, true] {
        let (mut store, shared, commits) = fixture(Exhaustivity::Exhaustive);
        let start = finish_without_external_wake(store.send(Action::Start)).unwrap();
        write(&shared, 9);
        if acknowledge {
            finish_without_external_wake(
                store.check_captures(|expected| expected.set::<SharedCapture>(9)),
            )
            .unwrap();
        }
        let mut updates = 0;
        let mut expectations = 0;
        let received = finish_without_external_wake(store.receive_with_captures(
            |action| matches!(action, Action::Mutate),
            |()| updates += 1,
            |expected| {
                expectations += 1;
                expected.set::<PrivateCapture>(1)?;
                expected.set::<SharedCapture>(7)
            },
        ));

        if acknowledge {
            let received = received.expect("acknowledged change permits the pending action");
            assert_eq!(updates, 1);
            assert_eq!(expectations, 1);
            assert_eq!(commits.load(Ordering::SeqCst), 1);
            assert_eq!(shared.read(|value| *value).unwrap(), 7);
            finish_without_external_wake(store.assert_state(|state| {
                assert_eq!(state.private, 1);
            }))
            .unwrap();
            finish_without_external_wake(received.finish()).unwrap();
            finish_without_external_wake(start.finish()).unwrap();
            finish_without_external_wake(store.finish()).unwrap();
        } else {
            let failure = received.expect_err("external change must fail before receive admission");
            assert!(failure.is_capture_mismatch());
            assert!(failure.to_string().starts_with(&format!(
                "TestStore capture {} Mismatch during preflight at ",
                std::any::type_name::<SharedCapture>()
            )));
            assert_eq!(updates, 0);
            assert_eq!(expectations, 0);
            assert_eq!(commits.load(Ordering::SeqCst), 0);
            assert_eq!(shared.read(|value| *value).unwrap(), 9);
            finish_without_external_wake(store.assert_state(|state| {
                assert_eq!(state.private, 0);
            }))
            .unwrap();
            assert!(!start.is_finished());

            // Restoring the old value must not erase the failed preflight obligation.
            write(&shared, 5);
            let debt = finish_without_external_wake(store.check_captures(|expected| {
                expectations += 1;
                expected.set::<SharedCapture>(5)
            }))
            .unwrap_err();
            assert!(debt.is_incomplete_assertion());
            assert_eq!(debt.origin(), failure.origin());
            assert_eq!(expectations, 0);
            assert!(
                finish_without_external_wake(store.finish())
                    .unwrap_err()
                    .is_incomplete_assertion()
            );
            assert_eq!(commits.load(Ordering::SeqCst), 0);
            failed_drop(store);
        }
    }
}

#[test]
fn skip_checks_declared_state_changed_by_cancelled_delivery_cleanup() {
    struct CleanupFeature {
        shared: Shared<u64>,
        commits: Arc<AtomicUsize>,
    }

    impl Reducer for CleanupFeature {
        type State = State;
        type Action = Action;

        fn reduce(
            &mut self,
            state: &mut State,
            action: &Action,
            _: &mut ReduceContext<'_>,
        ) -> Effect<Action> {
            match action {
                Action::Start => {
                    let shared = self.shared.clone();
                    Effect::run(move |send| async move {
                        let error = send.send(Action::Mutate).await.unwrap_err();
                        assert!(error.is_cancelled());
                        write(&shared, 9);
                    })
                }
                Action::Mutate => {
                    state.private += 1;
                    self.commits.fetch_add(1, Ordering::SeqCst);
                    Effect::none()
                }
            }
        }
    }

    let shared = Shared::new(5);
    let capture = shared.clone();
    let commits = Arc::new(AtomicUsize::new(0));
    let mut store = TestStore::projected_builder(
        State { private: 0 },
        CleanupFeature {
            shared: shared.clone(),
            commits: Arc::clone(&commits),
        },
        |_| (),
    )
    .with_exhaustivity(Exhaustivity::NonExhaustive)
    .with_capture::<SharedCapture>(move |_| capture.read(|value| *value))
    .unwrap()
    .build();
    let task = finish_without_external_wake(store.send(Action::Start)).unwrap();
    assert_eq!(shared.read(|value| *value).unwrap(), 5);
    assert!(!task.is_finished());

    let origin_line = line!() + 1;
    let skipped = store.skip_received();
    let failure = finish_without_external_wake(skipped)
        .expect_err("skip must verify changes made after its successful preflight");
    assert!(failure.is_capture_mismatch());
    assert_eq!(failure.origin().file(), file!());
    assert_eq!(failure.origin().line(), origin_line);
    assert!(failure.to_string().contains("during skip_received at "));
    assert_eq!(shared.read(|value| *value).unwrap(), 9);
    assert_eq!(commits.load(Ordering::SeqCst), 0);
    assert!(task.is_finished());
    finish_without_external_wake(store.assert_state(|state| assert_eq!(state.private, 0))).unwrap();
    finish_without_external_wake(task.finish()).unwrap();

    // Restoring the value cannot turn an unverified admitted skip into success.
    write(&shared, 5);
    let debt = finish_without_external_wake(store.finish()).unwrap_err();
    assert!(debt.is_incomplete_assertion());
    assert_eq!(debt.origin().line(), origin_line);
    failed_drop(store);
    assert_eq!(commits.load(Ordering::SeqCst), 0);
}

#[test]
fn skip_rejects_unacknowledged_shared_changes_without_discarding_the_action() {
    for acknowledge in [false, true] {
        let (mut store, shared, commits) = fixture(Exhaustivity::NonExhaustive);
        let start = finish_without_external_wake(store.send(Action::Start)).unwrap();
        write(&shared, 9);
        if acknowledge {
            finish_without_external_wake(
                store.check_captures(|expected| expected.set::<SharedCapture>(9)),
            )
            .unwrap();
        }
        let skipped = finish_without_external_wake(store.skip_received());
        finish_without_external_wake(store.assert_state(|state| {
            assert_eq!(state.private, 0);
        }))
        .unwrap();
        assert_eq!(commits.load(Ordering::SeqCst), 0);
        assert_eq!(shared.read(|value| *value).unwrap(), 9);

        if acknowledge {
            skipped.expect("acknowledged change permits an explicit skip without reduction");
            finish_without_external_wake(start.finish()).unwrap();
            finish_without_external_wake(store.finish()).unwrap();
        } else {
            let failure = skipped.expect_err("external change must fail before action discard");
            assert!(failure.is_capture_mismatch());
            assert!(failure.to_string().starts_with(&format!(
                "TestStore capture {} Mismatch during preflight at ",
                std::any::type_name::<SharedCapture>()
            )));
            assert!(!start.is_finished());

            write(&shared, 5);
            let mut expectations = 0;
            let debt = finish_without_external_wake(store.check_captures(|expected| {
                expectations += 1;
                expected.set::<SharedCapture>(5)
            }))
            .unwrap_err();
            assert!(debt.is_incomplete_assertion());
            assert_eq!(debt.origin(), failure.origin());
            assert_eq!(expectations, 0);
            assert!(
                finish_without_external_wake(store.finish())
                    .unwrap_err()
                    .is_incomplete_assertion()
            );
            assert_eq!(commits.load(Ordering::SeqCst), 0);
            failed_drop(store);
        }
    }
}
