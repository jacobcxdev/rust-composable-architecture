//! Negative and semantic controls for the real observation profiling fixture.

/// Changes real work or ownership so the corresponding validation must reject it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Fault {
    MissingRoot,
    OmittedSubscriber,
    WrongSelectedPayload,
    UnexpectedCoalescedDrain,
    RetainedValue,
    RetainedRegistrationOwner,
    SkippedTeardown,
}

#[derive(Clone, Copy, Debug)]
/// Selects a callback, waiter or cancellation boundary outside measured workloads.
pub(crate) enum SemanticCase {
    EqualCancelledNext,
    MovedWaiter,
    SelectorReentry,
    CallbackDetach,
}

#[test]
fn broken_real_observation_traces_are_rejected_and_cleaned() {
    for native in [true, false] {
        if !native && !cfg!(feature = "deterministic") {
            continue;
        }
        for (fault, error) in [
            (Fault::MissingRoot, "observation root trace invalid"),
            (
                Fault::OmittedSubscriber,
                "observation subscriber trace invalid",
            ),
            (
                Fault::WrongSelectedPayload,
                "observation selected payload invalid",
            ),
            (
                Fault::UnexpectedCoalescedDrain,
                "observation subscriber trace invalid",
            ),
            (
                Fault::RetainedValue,
                "observation phase value lifetime invalid",
            ),
            (
                Fault::RetainedRegistrationOwner,
                "observation final owners invalid",
            ),
            (Fault::SkippedTeardown, "observation final owners invalid"),
        ] {
            assert_eq!(
                crate::observation::control(fault, native),
                Err(error),
                "{fault:?}"
            );
        }
    }
}

#[test]
fn observation_semantics_use_real_ready_drives() {
    for native in [true, false] {
        if !native && !cfg!(feature = "deterministic") {
            continue;
        }
        for case in [
            SemanticCase::EqualCancelledNext,
            SemanticCase::MovedWaiter,
            SemanticCase::SelectorReentry,
            SemanticCase::CallbackDetach,
        ] {
            crate::observation::semantic_control(case, native).unwrap();
        }
    }
}

#[test]
fn copied_observation_scalars_cannot_hide_invalid_work_or_owners() {
    for native in [true, false] {
        if !native && !cfg!(feature = "deterministic") {
            continue;
        }
        crate::observation::scalar_corruption_control(native).unwrap();
    }
}

#[cfg(feature = "allocations")]
#[cfg_attr(not(feature = "allocations"), test)]
pub(crate) fn real_observation_owner_retention_is_rejected_in_fresh_child() {
    if !crate::tests::in_child(
        "observation_controls::real_observation_owner_retention_is_rejected_in_fresh_child",
    ) {
        return;
    }
    crate::observation::retained_owner_control().unwrap();
}

#[cfg(feature = "deterministic")]
#[test]
fn rejected_drives_preserve_actual_admitted_terminal_and_receipt_results() {
    crate::observation::admitted_results_control().unwrap();
}
