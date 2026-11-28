//! Challenges keyed trace validation with real behavioural and ownership faults.
//!
//! Each case requires the expected rejection, not merely process failure. The
//! deadlock control runs in the existing bounded child process and proves cleanup.

/// Selects a broken trace operation without changing the normal CLI workload.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Fault {
    MissingOutput,
    DuplicateOutput,
    SwappedOutput,
    StaleAccepted,
    SiblingCancelled,
    WrongOrder,
    RevivedRoute,
    IncompleteFinish,
    RetainedCapture,
    SkippedTeardown,
    /// Leaves the original child alive while the normal retirement wait runs.
    #[cfg_attr(
        not(feature = "deterministic"),
        expect(
            dead_code,
            reason = "only deterministic ready-driving controls construct this fault"
        )
    )]
    UncancelledRetired,
}

use crate::keyed::control;

#[test]
fn missing_accepted_output_is_rejected() {
    assert_eq!(
        control(Fault::MissingOutput),
        Err("keyed trace or ownership invalid")
    );
}

#[test]
fn duplicate_accepted_output_is_rejected() {
    assert_eq!(
        control(Fault::DuplicateOutput),
        Err("keyed trace or ownership invalid")
    );
}

#[test]
fn swapped_sequential_output_is_rejected() {
    assert_eq!(
        control(Fault::SwappedOutput),
        Err("keyed trace or ownership invalid")
    );
}

#[test]
fn stale_output_acceptance_is_rejected() {
    assert_eq!(
        control(Fault::StaleAccepted),
        Err("keyed trace or ownership invalid")
    );
}

#[test]
fn sibling_cancellation_is_rejected() {
    assert_eq!(
        control(Fault::SiblingCancelled),
        Err("keyed trace or ownership invalid")
    );
}

#[test]
fn wrong_resident_order_is_rejected() {
    assert_eq!(
        control(Fault::WrongOrder),
        Err("keyed trace or ownership invalid")
    );
}

#[test]
fn revived_old_route_is_rejected() {
    assert_eq!(
        control(Fault::RevivedRoute),
        Err("keyed trace or ownership invalid")
    );
}

#[test]
fn incomplete_causal_finish_is_rejected() {
    assert_eq!(
        control(Fault::IncompleteFinish),
        Err("keyed trace or ownership invalid")
    );
}

#[test]
fn retained_future_capture_is_rejected() {
    assert_eq!(
        control(Fault::RetainedCapture),
        Err("keyed trace or ownership invalid")
    );
}

#[test]
fn skipped_teardown_is_rejected() {
    assert_eq!(
        control(Fault::SkippedTeardown),
        Err("keyed trace or ownership invalid")
    );
}

#[test]
fn cold_first_root_is_separate_from_steady_warmup() {
    crate::keyed::cold_control(true).unwrap();
    #[cfg(feature = "deterministic")]
    crate::keyed::cold_control(false).unwrap();
}

#[cfg(feature = "deterministic")]
#[test]
fn uncancelled_retirement_fails_bounded_and_releases_owners() {
    if !crate::tests::in_child(
        "keyed_controls::uncancelled_retirement_fails_bounded_and_releases_owners",
    ) {
        return;
    }
    assert_eq!(crate::keyed::uncancelled_retirement_control(), Ok(()));
}
