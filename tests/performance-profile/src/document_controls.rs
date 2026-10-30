//! Failure and ownership controls for the coalesced-save workload.
//!
//! The native and deterministic lanes use the same literal outcomes. Allocation
//! builds run measured controls in child processes so profiler instances do not
//! overlap. Cancellation ends delivery; it does not complete a physical request.
use super::*;
use rca_persistence::{FlushOutcome, ReceiptError};
use std::task::{Context, Poll, Waker};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Fault {
    WrongByte,
    UnfinishedTask,
    MissingSecondSave,
    CancelledTask,
}

fn config(native: bool) -> Config {
    Config::parse(
        [
            if native {
                "document-coalesced-native"
            } else {
                "document-coalesced-deterministic"
            },
            "2",
            "1",
        ]
        .into_iter()
        .map(str::to_owned),
    )
    .unwrap()
}
/// Always runs native controls; adds deterministic controls when compiled in.
fn lanes() -> impl Iterator<Item = bool> {
    [true, false]
        .into_iter()
        .filter(|native| *native || cfg!(feature = "deterministic"))
}
/// Isolates each DHAT control in a child process; ordinary tests run in place.
fn child(name: &str) -> bool {
    #[cfg(feature = "allocations")]
    {
        crate::tests::in_child(name)
    }
    #[cfg(not(feature = "allocations"))]
    {
        let _ = name;
        true
    }
}
// Hand-authored totals for each control, including failed and restarted sessions.
// Balance alone would accept an extra matched clone or physical/delivery guard.
/// Checks originals, clones, payload drops, physical starts/drops and waiter starts/drops.
fn ownership_counts(f: &Fixture, expected: [u64; 7]) {
    assert_eq!(
        [
            count(&f.probes.originals),
            count(&f.probes.clones),
            count(&f.probes.dropped),
            count(&f.probes.physical_started),
            count(&f.probes.physical_dropped),
            count(&f.probes.waiter_started),
            count(&f.probes.waiter_dropped),
        ],
        expected
    );
}
/// Measures one control through cleanup and rejects unbalanced or retained owners.
///
/// The callback must return normally to reach these cleanup assertions. A panic
/// already fails the test; this helper does not claim panic-safe cleanup proof.
fn control(native: bool, expected: [u64; 7], check: impl FnOnce(&mut Fixture)) {
    let measurement = Measurement::new(if native {
        Some(ExecutorCache::Native)
    } else {
        None
    });
    let mut fixture = Fixture::new(config(native)).unwrap();
    check(&mut fixture);
    fixture.cleanup().unwrap();
    ownership_counts(&fixture, expected);
    assert_eq!(fixture.controller.pending(), (0, 0));
    assert_eq!(
        count(&fixture.probes.physical_started),
        count(&fixture.probes.physical_dropped)
    );
    assert_eq!(
        count(&fixture.probes.waiter_started),
        count(&fixture.probes.waiter_dropped)
    );
    assert_eq!(
        count(&fixture.probes.originals) + count(&fixture.probes.clones),
        count(&fixture.probes.dropped)
    );
    drop(fixture);
    measurement.finish().unwrap();
}
fn submit(f: &mut Fixture, action: Action<Payload>) -> ActionTask {
    f.commit(action).unwrap()
}
fn settle(f: &mut Fixture, task: &ActionTask) {
    f.finish_task(task).unwrap();
}
fn set(f: &mut Fixture, n: u64) -> ActionTask {
    let payload = Payload::new(bytes(n, f.config.p), &f.probes);
    submit(f, Action::set(payload))
}
fn load(f: &mut Fixture, result: Result<Option<Vec<u8>>, ControlledError>) {
    let task = submit(f, Action::load());
    f.receive().unwrap();
    f.controller.load(result).unwrap();
    settle(f, &task);
}
fn receipt(f: &Fixture, receipt: rca_persistence::Receipt) -> Result<FlushOutcome, ReceiptError> {
    f.drive(receipt).unwrap()
}
/// Checks roots, responses, loads, decodes, encodes, saves and physical writes.
fn counts(f: &Fixture, expected: [u64; 7]) {
    let [roots, responses, loads, decodes, encodes, saves, writes] = expected;
    let actual = f.counts();
    assert_eq!(
        (actual.roots, actual.responses, actual.reductions),
        (roots, responses, roots + responses)
    );
    assert_eq!(
        (
            actual.loads,
            actual.decodes,
            actual.encodes,
            actual.saves,
            actual.writes
        ),
        (loads, decodes, encodes, saves, writes)
    );
}
/// Checks both failure metadata and the concrete error retained by the document.
fn failure(f: &Fixture, operation: Operation, revision: Option<u64>, error: ControlledError) {
    assert_eq!(f.read().unwrap().failure, Some((operation, revision)));
    let actual = f
        .drive(f.store.as_ref().unwrap().read(move |state| {
            state
                .failure()
                .unwrap()
                .error()
                .downcast_ref::<ControlledError>()
                .copied()
        }))
        .unwrap()
        .unwrap();
    assert_eq!(actual, Some(error));
}

#[cfg_attr(not(feature = "allocations"), test)]
pub(crate) fn missing_and_present_clean_load() {
    if !child("document::controls::missing_and_present_clean_load") {
        return;
    }
    for native in lanes() {
        control(native, [3, 1, 4, 2, 2, 2, 2], |f| {
            load(f, Ok(None));
            let p = f.read().unwrap();
            assert_eq!((p.tag, p.ack, p.status), (0, None, LoadStatus::Ready));
            let (action, r) = Action::flush();
            let task = submit(f, action);
            f.receive().unwrap();
            f.controller.save(0, Ok(())).unwrap();
            settle(f, &task);
            assert_eq!(
                receipt(f, r).unwrap(),
                FlushOutcome {
                    revision: 0,
                    closing: false
                }
            );
            counts(f, [2, 2, 1, 0, 1, 1, 1]);
            assert_eq!(
                (count(&f.probes.originals), count(&f.probes.clones)),
                (3, 1)
            );
        });
        control(native, [4, 1, 5, 1, 1, 1, 1], |f| {
            f.cold().unwrap();
            f.flush(0).unwrap();
            counts(f, [2, 1, 1, 1, 0, 0, 0]);
            assert_eq!(
                (count(&f.probes.originals), count(&f.probes.clones)),
                (4, 1)
            );
        });
    }
}

#[cfg_attr(not(feature = "allocations"), test)]
pub(crate) fn single_edit_bursts_have_one_save_and_no_omitted_receipts() {
    if !child("document::controls::single_edit_bursts_have_one_save_and_no_omitted_receipts") {
        return;
    }
    for native in lanes() {
        let measurement = Measurement::new(if native {
            Some(ExecutorCache::Native)
        } else {
            None
        });
        // Bypass parser admission so RED proves the actual one-save work path.
        let mut config = config(native);
        config.b = 1;
        config.i = 3;
        config.scheduler = 4992;
        config.outer = 4993;
        let mut f = Fixture::new(config).unwrap();
        let cold = f.cold().unwrap();
        let work = profile_document_work(&mut f, config);
        if work.is_ok() {
            let expected = Counts {
                roots: 6,
                responses: 3,
                reductions: 9,
                completed: 6,
                encodes: 3,
                saves: 3,
                writes: 3,
                receipts: 3,
                clones: 6,
                dropped: 9,
                ..Counts::default()
            };
            assert_eq!(validate_work(&f, cold, config), Ok(expected));
            for broken in [
                Counts {
                    saves: 4,
                    ..expected
                },
                Counts {
                    responses: 4,
                    ..expected
                },
                Counts {
                    receipts: 2,
                    ..expected
                },
            ] {
                assert!(validate_counts(broken, config).is_err());
            }
            assert_eq!(f.controller.write_sum, 6);
            assert_eq!(f.controller.pending(), (0, 0));
            assert!(f.controller.requests.is_empty());
            bump(&f.probes.physical_started);
            bump(&f.probes.physical_dropped);
            assert!(validate_work(&f, cold, config).is_err());
            f.probes.physical_started.store(4, Ordering::Relaxed);
            f.probes.physical_dropped.store(4, Ordering::Relaxed);
        }
        f.cleanup().unwrap();
        if work.is_ok() {
            ownership_counts(&f, [5, 7, 12, 4, 4, 4, 4]);
        }
        drop(f);
        measurement.finish().unwrap();
        assert_eq!(
            work,
            Ok(()),
            "single-edit burst must not wait for a second save"
        );
    }
}

#[cfg_attr(not(feature = "allocations"), test)]
pub(crate) fn load_decode_failures_require_explicit_retry() {
    if !child("document::controls::load_decode_failures_require_explicit_retry") {
        return;
    }
    for native in lanes() {
        for (operation, result, decodes, typed) in [
            (
                Operation::Load,
                Err(ControlledError::Load),
                0,
                ControlledError::Load,
            ),
            (
                Operation::Decode,
                Ok(Some(vec![0; 4096])),
                1,
                ControlledError::Decode,
            ),
        ] {
            control(native, [5, 1, 6, 2, 2, 2, 2], |f| {
                load(f, result);
                let p = f.read().unwrap();
                assert_eq!(
                    (p.status, p.attempt, p.ack),
                    (LoadStatus::Unloaded, None, None)
                );
                failure(f, operation, None, typed);
                let ignored = set(f, 1);
                settle(f, &ignored);
                counts(f, [2, 1, 1, decodes, 0, 0, 0]);
                load(f, Ok(Some(bytes(7, 4096))));
                clean(f.read().unwrap(), 0, 7).unwrap();
                counts(f, [3, 2, 2, decodes + 1, 0, 0, 0]);
                assert_eq!(count(&f.probes.originals), 5);
                assert_eq!(count(&f.probes.clones), 1);
            });
        }
    }
}

#[cfg_attr(not(feature = "allocations"), test)]
pub(crate) fn encode_failure_latest_close_retry_fences_edits() {
    if !child("document::controls::encode_failure_latest_close_retry_fences_edits") {
        return;
    }
    for native in lanes() {
        control(native, [7, 5, 12, 2, 2, 2, 2], |f| {
            f.cold().unwrap();
            f.probes.fail_encode.store(true, Ordering::Relaxed);
            let first = set(f, 1);
            settle(f, &first);
            failure(f, Operation::Encode, Some(1), ControlledError::Encode);
            counts(f, [2, 2, 1, 1, 1, 0, 0]);
            assert_eq!(f.read().unwrap().ack, Some(0));
            let second = set(f, 2);
            settle(f, &second);
            counts(f, [3, 2, 1, 1, 1, 0, 0]);
            let (action, r) = Action::close();
            let close = submit(f, action);
            f.receive().unwrap();
            let ignored = set(f, 3);
            settle(f, &ignored);
            assert_eq!(f.read().unwrap().revision, 2);
            f.controller.save(2, Ok(())).unwrap();
            settle(f, &close);
            assert_eq!(
                receipt(f, r).unwrap(),
                FlushOutcome {
                    revision: 2,
                    closing: true
                }
            );
            let (action, r) = Action::flush();
            let task = submit(f, action);
            settle(f, &task);
            assert!(matches!(receipt(f, r), Err(ReceiptError::Closing)));
            let p = f.read().unwrap();
            assert!(p.closed && p.closing);
            assert_eq!(p.ack, Some(2));
            counts(f, [6, 3, 1, 1, 2, 1, 1]);
            assert_eq!(
                (count(&f.probes.originals), count(&f.probes.clones)),
                (7, 5)
            );
        });
    }
}

#[cfg_attr(not(feature = "allocations"), test)]
pub(crate) fn save_failure_old_barrier_and_latest_flush_retry() {
    if !child("document::controls::save_failure_old_barrier_and_latest_flush_retry") {
        return;
    }
    for native in lanes() {
        control(native, [7, 6, 13, 3, 3, 3, 3], |f| {
            f.cold().unwrap();
            let first = set(f, 1);
            f.receive().unwrap();
            let (action, r) = Action::flush();
            let barrier = submit(f, action);
            let second = set(f, 2);
            settle(f, &second);
            f.controller.save(1, Err(ControlledError::Save)).unwrap();
            settle(f, &first);
            settle(f, &barrier);
            failure(f, Operation::Save, Some(1), ControlledError::Save);
            let Err(ReceiptError::Persistence(cause)) = receipt(f, r) else {
                panic!("original save failure");
            };
            assert_eq!(cause.revision(), Some(1));
            assert_eq!(
                cause.error().downcast_ref::<ControlledError>(),
                Some(&ControlledError::Save)
            );
            drop(cause);
            assert_eq!(
                (f.read().unwrap().revision, f.read().unwrap().ack),
                (2, Some(0))
            );
            let third = set(f, 3);
            settle(f, &third);
            counts(f, [5, 2, 1, 1, 1, 1, 0]);
            let (action, r) = Action::flush();
            let retry = submit(f, action);
            f.receive().unwrap();
            f.controller.save(3, Ok(())).unwrap();
            settle(f, &retry);
            assert_eq!(
                receipt(f, r).unwrap(),
                FlushOutcome {
                    revision: 3,
                    closing: false
                }
            );
            clean(f.read().unwrap(), 3, 3).unwrap();
            counts(f, [6, 3, 1, 1, 2, 2, 1]);
            assert_eq!(
                (count(&f.probes.originals), count(&f.probes.clones)),
                (7, 6)
            );
        });
    }
}

#[cfg_attr(not(feature = "allocations"), test)]
pub(crate) fn receipt_ownership_does_not_cancel_physical_save() {
    if !child("document::controls::receipt_ownership_does_not_cancel_physical_save") {
        return;
    }
    for native in lanes() {
        control(native, [5, 3, 8, 2, 2, 2, 2], |f| {
            let (action, r) = Action::<Payload>::flush();
            drop(action);
            assert!(matches!(receipt(f, r), Err(ReceiptError::Abandoned)));
            counts(f, [0, 0, 0, 0, 0, 0, 0]);
            f.cold().unwrap();
            let first = set(f, 1);
            f.receive().unwrap();
            drop(first);
            let (a, r1) = Action::flush();
            let t1 = submit(f, a);
            let (a, r2) = Action::flush();
            let t2 = submit(f, a);
            drop(r2);
            let (a, r3) = Action::flush();
            let t3 = submit(f, a);
            assert_eq!(f.read().unwrap().barriers, 3);
            assert_eq!(f.controller.pending(), (1, 1));
            f.controller.save(1, Ok(())).unwrap();
            for task in [&t1, &t2, &t3] {
                settle(f, task);
            }
            assert_eq!(
                receipt(f, r1).unwrap(),
                FlushOutcome {
                    revision: 1,
                    closing: false
                }
            );
            assert_eq!(
                receipt(f, r3).unwrap(),
                FlushOutcome {
                    revision: 1,
                    closing: false
                }
            );
            clean(f.read().unwrap(), 1, 1).unwrap();
            counts(f, [5, 2, 1, 1, 1, 1, 1]);
            assert_eq!(
                (count(&f.probes.originals), count(&f.probes.clones)),
                (5, 3)
            );
        });
    }
}

#[cfg_attr(not(feature = "allocations"), test)]
pub(crate) fn captured_barrier_precedes_newer_acknowledgement() {
    if !child("document::controls::captured_barrier_precedes_newer_acknowledgement") {
        return;
    }
    for native in lanes() {
        control(native, [6, 5, 11, 3, 3, 3, 3], |f| {
            f.cold().unwrap();
            let first = set(f, 1);
            f.receive().unwrap();
            let (a, r) = Action::flush();
            let barrier = submit(f, a);
            let second = set(f, 2);
            settle(f, &second);
            f.controller.save(1, Ok(())).unwrap();
            f.receive().unwrap();
            assert_eq!(
                receipt(f, r).unwrap(),
                FlushOutcome {
                    revision: 1,
                    closing: false
                }
            );
            settle(f, &barrier);
            let p = f.read().unwrap();
            assert_eq!(
                (p.revision, p.ack, p.saving, p.attempt),
                (2, Some(1), Some(2), Some(3))
            );
            f.controller.save(2, Ok(())).unwrap();
            settle(f, &first);
            clean(f.read().unwrap(), 2, 2).unwrap();
            counts(f, [4, 3, 1, 1, 2, 2, 2]);
            assert_eq!(
                (count(&f.probes.originals), count(&f.probes.clones)),
                (6, 5)
            );
        });
    }
}

#[cfg_attr(not(feature = "allocations"), test)]
pub(crate) fn cancellation_abort_external_join_then_new_document() {
    if !child("document::controls::cancellation_abort_external_join_then_new_document") {
        return;
    }
    for native in lanes() {
        let measurement = Measurement::new(if native {
            Some(ExecutorCache::Native)
        } else {
            None
        });
        let mut f = Fixture::new(config(native)).unwrap();
        f.cold().unwrap();
        let first = set(&mut f, 1);
        f.receive().unwrap();
        let (a, r1) = Action::flush();
        let barrier = submit(&mut f, a);
        first.cancel();
        f.drive(first.finish()).unwrap();
        assert!(first.is_cancelled() && first.is_finished());
        f.drive(f.controller.wait_delivery_drop()).unwrap().unwrap();
        let p = f.read().unwrap();
        assert_eq!((p.saving, p.barriers, p.ack), (Some(1), 1, Some(0)));
        let (a, r2) = Action::flush();
        let second = submit(&mut f, a);
        assert_eq!(f.read().unwrap().barriers, 2);
        counts(&f, [4, 1, 1, 1, 1, 1, 0]);
        let abort = submit(&mut f, Action::abort());
        settle(&mut f, &abort);
        for r in [r1, r2] {
            assert!(matches!(receipt(&f, r), Err(ReceiptError::Aborted)));
        }
        settle(&mut f, &barrier);
        settle(&mut f, &second);
        let p = f.read().unwrap();
        assert_eq!((p.saving, p.barriers, p.aborted), (None, 0, true));
        f.controller.fence();
        {
            let mut join = std::pin::pin!(f.controller.join());
            assert_eq!(
                join.as_mut().poll(&mut Context::from_waker(Waker::noop())),
                Poll::Pending
            );
        }
        assert_eq!(f.controller.pending(), (1, 0));
        assert_eq!(
            f.controller.save(1, Ok(())),
            Err("document save delivery rejected")
        );
        f.drive(f.controller.join()).unwrap().unwrap();
        assert_eq!(f.controller.pending(), (0, 0));
        counts(&f, [5, 1, 1, 1, 1, 1, 1]);
        let persisted = f.controller.storage.take().unwrap();
        assert_eq!(tag(&persisted, 4096), Ok(1));
        drop((first, barrier, second, abort));
        f.cleanup().unwrap();
        ownership_counts(&f, [5, 3, 8, 2, 2, 2, 2]);
        drop(f);
        let mut restarted = Fixture::new(config(native)).unwrap();
        load(&mut restarted, Ok(Some(persisted)));
        clean(restarted.read().unwrap(), 0, 1).unwrap();
        let edit = set(&mut restarted, 2);
        restarted.receive().unwrap();
        restarted.controller.save(2, Ok(())).unwrap();
        settle(&mut restarted, &edit);
        clean(restarted.read().unwrap(), 1, 2).unwrap();
        counts(&restarted, [2, 2, 1, 1, 1, 1, 1]);
        restarted.cleanup().unwrap();
        ownership_counts(&restarted, [5, 3, 8, 2, 2, 2, 2]);
        drop((edit, restarted));
        measurement.finish().unwrap();
    }
}

#[cfg_attr(not(feature = "allocations"), test)]
pub(crate) fn abort_and_shutdown_are_not_flush() {
    if !child("document::controls::abort_and_shutdown_are_not_flush") {
        return;
    }
    for native in lanes() {
        control(native, [5, 3, 8, 2, 2, 2, 2], |f| {
            f.cold().unwrap();
            let first = set(f, 1);
            f.receive().unwrap();
            let (a, r) = Action::close();
            let closing = submit(f, a);
            let abort = submit(f, Action::abort());
            settle(f, &abort);
            f.drive(f.controller.wait_delivery_drop()).unwrap().unwrap();
            assert!(matches!(receipt(f, r), Err(ReceiptError::Aborted)));
            settle(f, &first);
            settle(f, &closing);
            assert_eq!(f.controller.pending(), (1, 0));
            assert_eq!(
                f.controller.save(1, Ok(())),
                Err("document save delivery rejected")
            );
            counts(f, [4, 1, 1, 1, 1, 1, 1]);
        });
        control(native, [5, 3, 8, 2, 2, 2, 2], |f| {
            f.cold().unwrap();
            let first = set(f, 1);
            f.receive().unwrap();
            let (a, r) = Action::flush();
            let barrier = submit(f, a);
            let store = f.store.take().unwrap();
            let state = f.drive(store.shutdown_into_state()).unwrap().unwrap();
            assert_eq!(project(&state).saving, Some(1));
            drop(state);
            f.drive(f.controller.wait_delivery_drop()).unwrap().unwrap();
            assert!(matches!(receipt(f, r), Err(ReceiptError::Abandoned)));
            assert_eq!(f.controller.pending(), (1, 0));
            assert_eq!(f.controller.writes, 0);
            drop((first, barrier));
            f.controller.held = None;
            counts(f, [3, 1, 1, 1, 1, 1, 0]);
        });
    }
}

#[cfg_attr(not(feature = "allocations"), test)]
pub(crate) fn broken_wait_byte_task_and_owner_gates_release_every_guard() {
    if !child("document::controls::broken_wait_byte_task_and_owner_gates_release_every_guard") {
        return;
    }
    for native in lanes() {
        for fault in [
            Fault::WrongByte,
            Fault::UnfinishedTask,
            Fault::MissingSecondSave,
            Fault::CancelledTask,
        ] {
            let expected = match fault {
                Fault::WrongByte | Fault::UnfinishedTask => [4, 3, 7, 2, 2, 2, 2],
                Fault::MissingSecondSave => [4, 5, 9, 2, 2, 2, 2],
                Fault::CancelledTask => [4, 5, 9, 3, 3, 3, 3],
            };
            control(native, expected, |f| {
                f.cold().unwrap();
                f.fault = Some(fault);
                let error = profile_document_work(f, f.config).unwrap_err();
                match fault {
                    Fault::WrongByte => assert_eq!(error, "document encoded byte invalid"),
                    Fault::CancelledTask => {
                        assert_eq!(error, "document success task unfinished or cancelled");
                    }
                    Fault::UnfinishedTask | Fault::MissingSecondSave => assert_eq!(
                        error,
                        if native {
                            "native host deadline exceeded"
                        } else {
                            "deterministic ready drive quiescent pending"
                        }
                    ),
                }
                // The failing client emitted no record. The common wrapper's
                // separately bounded cleanup checks each physical/drop count.
                assert_eq!(reject(f, error), error);
                ownership_counts(f, expected);
            });
        }
        control(native, [5, 3, 8, 2, 2, 2, 2], |f| {
            f.cold().unwrap();
            let first = set(f, 1);
            f.receive().unwrap();
            f.controller.fence();
            first.cancel();
            f.drive(first.finish()).unwrap();
            f.drive(f.controller.wait_delivery_drop()).unwrap().unwrap();
            assert_eq!(f.controller.pending(), (1, 0));
            assert_eq!(
                f.drive(f.controller.join()),
                Err(if native {
                    "native host deadline exceeded"
                } else {
                    "deterministic ready drive quiescent pending"
                })
            );
            assert_eq!(
                f.controller.save(1, Ok(())),
                Err("document save delivery rejected")
            );
            f.drive(f.controller.join()).unwrap().unwrap();
            assert_eq!(f.controller.pending(), (0, 0));
        });
    }
}

#[cfg_attr(not(feature = "allocations"), test)]
pub(crate) fn broken_outcome_intermediate_and_capture_gates_release_owners() {
    if !child("document::controls::broken_outcome_intermediate_and_capture_gates_release_owners") {
        return;
    }
    for native in lanes() {
        control(native, [4, 5, 9, 3, 3, 3, 3], |f| {
            f.cold().unwrap();
            profile_document_work(f, f.config).unwrap();
            let p = f.read().unwrap();
            for broken in [
                Projection { ack: Some(1), ..p },
                Projection { revision: 1, ..p },
                Projection { tag: 1, ..p },
            ] {
                assert_eq!(
                    clean(broken, 2, 2),
                    Err("document final projection invalid")
                );
            }
            assert_eq!(f.flush(1), Err("document receipt revision invalid"));
            let actual = Counts {
                roots: 3,
                responses: 2,
                reductions: 5,
                completed: 3,
                encodes: 2,
                saves: 2,
                writes: 2,
                receipts: 1,
                clones: 4,
                dropped: 6,
                ..Counts::default()
            };
            validate_counts(actual, f.config).unwrap();
            for broken in [
                Counts { saves: 1, ..actual },
                Counts { saves: 3, ..actual },
                Counts {
                    completed: 1,
                    ..actual
                },
                Counts {
                    receipts: 0,
                    ..actual
                },
            ] {
                assert_eq!(
                    validate_counts(broken, f.config),
                    Err("document work counters invalid")
                );
            }
        });
        // Genuine early intermediate save: release first before all edits;
        // independent guard rejects the third backend admission.
        control(native, [7, 7, 14, 4, 4, 4, 4], |f| {
            let cold = f.cold().unwrap();
            let first = set(f, 1);
            f.receive().unwrap();
            f.controller.save(1, Ok(())).unwrap();
            settle(f, &first);
            let second = set(f, 2);
            f.receive().unwrap();
            f.controller.save(2, Ok(())).unwrap();
            settle(f, &second);
            let third = set(f, 3);
            f.receive().unwrap();
            assert_eq!(count(&f.probes.saves), 3);
            assert_eq!(f.controller.held_tag(), Ok(3));
            assert_eq!(
                validate_work(f, cold, f.config),
                Err("document work counters invalid")
            );
            f.controller.save(3, Ok(())).unwrap();
            settle(f, &third);
        });
        for index in 0..3 {
            control(native, [4, 5, 9, 3, 3, 3, 3], |f| {
                f.cold().unwrap();
                profile_document_work(f, f.config).unwrap();
                let retained = f.owners[index].upgrade().unwrap();
                f.cleanup().unwrap_err();
                assert_eq!(f.owners[index].strong_count(), 1);
                drop(retained);
                f.check_owners().unwrap();
            });
        }
        control(native, [5, 5, 10, 3, 3, 3, 3], |f| {
            f.cold().unwrap();
            profile_document_work(f, f.config).unwrap();
            let retained = Payload::new(bytes(99, 4096), &f.probes);
            assert_eq!(
                reject(f, "intentional retained payload"),
                "document rejection and cleanup failure"
            );
            drop(retained);
            f.check_owners().unwrap();
        });
    }
}
