//! Checks public ownership and trait bounds, dependency isolation and erased clocks.

#[expect(dead_code, reason = "clock and type-bound checks do not start actors")]
mod support;

use std::time::Duration;

use rca_core::{
    DependencyKey, DependencyValues, Effect, EffectSendError, MonotonicInstant, ReduceContext,
    Reducer, Runtime, ShutdownError, StoreBuildError, StoreStopped, TrySendError,
};
use support::ManualRuntime;

/// Supplies a literal default that differs from both tested overrides.
#[derive(Debug)]
struct CountKey;

impl DependencyKey for CountKey {
    type Value = usize;

    fn live_value() -> Self::Value {
        41
    }
}

/// Borrows its action and reads the reduction's dependency scope.
struct Counter;

impl Reducer for Counter {
    type State = usize;
    type Action = ();

    fn reduce(
        &mut self,
        state: &mut Self::State,
        _action: &Self::Action,
        context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        *state += *context.dependency::<CountKey>();
        Effect::none()
    }
}

/// Requires cross-thread ownership at the compiler boundary.
fn assert_send<T: Send>() {}

/// Requires shared cross-thread access at the compiler boundary.
fn assert_sync<T: Sync>() {}

/// Requires an owned operational error that can cross thread boundaries.
fn assert_error<T: std::error::Error + Send + Sync + 'static>() {}

#[test]
fn dependency_defaults_and_overrides_are_store_owned() {
    let mut values = DependencyValues::new();
    assert_eq!(*values.get_or_insert::<CountKey>(), 41);
    values.set::<CountKey>(7);
    assert_eq!(*values.get_or_insert::<CountKey>(), 7);

    let mut child = values.overriding::<CountKey>(9);
    assert_eq!(*values.get_or_insert::<CountKey>(), 7);
    assert_eq!(*child.get_or_insert::<CountKey>(), 9);
}

#[test]
fn monotonic_arithmetic_saturates() {
    assert_eq!(
        MonotonicInstant::MAX.saturating_add(Duration::MAX),
        MonotonicInstant::MAX
    );
}

#[test]
fn runtime_erases_driver_type_and_remains_thread_safe() {
    assert_send::<Runtime>();
    assert_sync::<Runtime>();

    let host = ManualRuntime::new();
    let runtime = host.runtime();
    assert_eq!(runtime.now(), MonotonicInstant::ZERO);
    assert!(format!("{runtime:?}").contains("Runtime"));
}

#[test]
fn erased_runtime_clock_does_not_complete_future_deadlines_early() {
    let host = ManualRuntime::new();
    let runtime = host.runtime();
    let clock = runtime.clock();
    let mut context = std::task::Context::from_waker(std::task::Waker::noop());

    for deadline in [
        MonotonicInstant::from_elapsed(Duration::from_nanos(1)),
        MonotonicInstant::MAX,
    ] {
        let mut wait = clock.sleep_until(deadline);
        let first_poll = wait.as_mut().poll(&mut context);
        assert!(
            first_poll.is_pending() || clock.now() >= deadline,
            "the erased clock completed before its deadline"
        );
    }
}

#[test]
fn effect_shell_has_no_domain_trait_bounds() {
    /// Deliberately omits `Clone` and `Debug` from the action payload.
    struct MoveOnly {
        _value: String,
    }

    let effect = Effect::<MoveOnly>::none();
    let defaulted = Effect::<MoveOnly>::default();
    assert_send::<Effect<MoveOnly>>();
    assert_eq!(format!("{effect:?}"), "Effect::None");
    assert_eq!(format!("{defaulted:?}"), "Effect::None");
}

#[test]
fn reducer_contract_accepts_borrowed_actions() {
    /// Checks the reducer signature without constructing an actor.
    fn assert_reducer<T: Reducer<State = usize, Action = ()>>() {}

    assert_reducer::<Counter>();
}

#[test]
fn operational_errors_have_canonical_contracts() {
    assert_error::<StoreBuildError>();
    assert_error::<StoreStopped>();
    assert_error::<EffectSendError>();
    assert_error::<TrySendError<()>>();
    assert_error::<ShutdownError>();

    let _: fn(&StoreBuildError) -> bool = StoreBuildError::is_zero_capacity;
    let _: fn(&StoreStopped) -> &std::backtrace::Backtrace = StoreStopped::backtrace;
    let _: fn(&EffectSendError) -> bool = EffectSendError::is_cancelled;
    let _: fn(&EffectSendError) -> bool = EffectSendError::is_store_stopped;
}
