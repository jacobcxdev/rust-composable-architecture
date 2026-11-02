//! Checks explicit shared-value expectations and exact capture-error recovery.

#![cfg(feature = "test-util")]

use std::cell::Cell;
use std::convert::Infallible;
use std::panic::{AssertUnwindSafe, catch_unwind};

use futures_executor::block_on;
use rca::{
    CaptureKey, Effect, ReduceContext, Reducer, Shared, SharedAccessError, TestStore,
    TestStoreError,
};

/// Names a capture that is separate from the reducer's unit root state.
struct SharedValue;
impl CaptureKey for SharedValue {
    type Value = u64;
    type Error = SharedAccessError;
}

/// Mutates shared storage without changing the unit root state.
struct Feature(Shared<u64>);
impl Reducer for Feature {
    type State = ();
    type Action = ();

    fn reduce(&mut self, (): &mut (), (): &(), _: &mut ReduceContext<'_>) -> Effect<()> {
        self.0
            .try_update(
                |value| {
                    *value = 7;
                    Ok::<_, Infallible>(())
                },
                || {},
            )
            .unwrap();
        Effect::none()
    }
}

/// Gives each test store its own shared value and explicitly registered capture.
fn fixture() -> TestStore<(), ()> {
    let shared = Shared::new(5);
    let captured = shared.clone();
    TestStore::builder((), Feature(shared))
        .with_capture::<SharedValue>(move |()| captured.read(|value| *value))
        .unwrap()
        .build()
}

#[test]
fn facade_capture_is_store_local_and_requires_explicit_mutation_expectation() {
    let mut omitted = fixture();
    let mut expected = fixture();
    assert!(
        block_on(omitted.send(()))
            .unwrap_err()
            .is_capture_mismatch()
    );
    let _task = block_on(expected.send_with_captures(
        (),
        |()| {},
        |captures| captures.set::<SharedValue>(7),
    ))
    .unwrap();
    block_on(expected.finish()).unwrap();
    let unfinished = block_on(omitted.finish()).unwrap_err();
    assert!(unfinished.is_incomplete_assertion());
    let expected_panic = format!("TestStore teardown failed: {unfinished}; {unfinished:?}");
    let panic = catch_unwind(AssertUnwindSafe(|| drop(omitted)))
        .expect_err("an omitted declared mutation must also fail normal Drop");
    assert_eq!(panic.downcast_ref::<String>(), Some(&expected_panic));
}

/// Uses `Cell` to require `Send` without `Sync`, `Debug` or error-trait bounds.
struct Cause(Cell<u64>);
/// Identifies the capture that actually produced the opaque failure.
struct FallibleValue;
impl CaptureKey for FallibleValue {
    type Value = u64;
    type Error = Cause;
}
/// Has the same associated types but must not recover another key's failure.
struct OtherValue;
impl CaptureKey for OtherValue {
    type Value = u64;
    type Error = Cause;
}

#[test]
fn facade_recovers_the_original_send_only_cause_by_exact_key() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<TestStoreError>();
    let failure = TestStore::builder((), Feature(Shared::new(5)))
        .with_capture::<FallibleValue>(|()| Err(Cause(Cell::new(41))))
        .unwrap_err();
    let Err(failure) = failure.into_capture_error::<OtherValue>() else {
        panic!("the wrong key must preserve the original error");
    };
    let cause = failure.into_capture_error::<FallibleValue>().unwrap();
    assert_eq!(cause.0.get(), 41);
}

/// Deliberately lacks `Debug` because semantic comparison must not require formatting.
#[derive(Clone, PartialEq)]
struct OpaqueSnapshot(u64);
/// Registers opaque snapshots independently of the shared storage handle.
struct OpaqueValue;
impl CaptureKey for OpaqueValue {
    type Value = OpaqueSnapshot;
    type Error = SharedAccessError;
}

#[test]
fn facade_compares_opaque_declared_values_without_formatting_them() {
    for declares_change in [false, true] {
        let shared = Shared::new(5);
        let captured = shared.clone();
        let mut store = TestStore::builder((), Feature(shared))
            .with_capture::<OpaqueValue>(move |()| captured.read(|value| OpaqueSnapshot(*value)))
            .unwrap()
            .build();
        if declares_change {
            let _task = block_on(store.send_with_captures(
                (),
                |()| {},
                |captures| captures.set::<OpaqueValue>(OpaqueSnapshot(7)),
            ))
            .unwrap();
            block_on(store.finish()).unwrap();
        } else {
            let failure = block_on(store.send(())).unwrap_err();
            assert!(failure.is_capture_mismatch());
            assert!(failure.to_string().contains("OpaqueValue"));
            let unfinished = block_on(store.finish()).unwrap_err();
            assert!(unfinished.is_incomplete_assertion());
            let expected_panic = format!("TestStore teardown failed: {unfinished}; {unfinished:?}");
            let panic = catch_unwind(AssertUnwindSafe(|| drop(store)))
                .expect_err("opaque values retain mandatory omitted-mutation failure");
            assert_eq!(panic.downcast_ref::<String>(), Some(&expected_panic));
        }
    }
}
