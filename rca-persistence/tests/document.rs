//! Tests observable coordinator behaviour through the real deterministic actor.

use std::collections::VecDeque;
use std::error::Error;
use std::fmt;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

use futures_executor::block_on;
use rca_core::{
    ActionPath, Effect, MonotonicInstant, OptionalStatePath, ReduceContext, Reducer, Runtime,
    RuntimeDriver, RuntimeFuture, Store, if_some,
};
use rca_persistence::{
    Action, Attempt, Backend, BackendSnapshot, Codec, CompareSave, Document, FlushOutcome,
    LoadStatus, Operation, ReceiptError, State,
};
use rca_test::{Completion, ControlledFuture, DeterministicRuntime, TestStore, controlled_future};

/// Carries a secret marker that public persistence diagnostics must hide.
#[derive(Debug)]
struct SecretError(u32);

impl fmt::Display for SecretError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "sensitive-secret-{}", self.0)
    }
}

impl Error for SecretError {}

/// Checks redaction without requiring opaque revisions to implement `Debug`.
#[test]
fn comparison_diagnostics_redact_opaque_revisions_without_consuming_payloads() {
    /// Opaque revision whose lack of `Debug` is part of this compiler witness.
    struct Revision(String);

    let saved = CompareSave::Saved(Revision("secret-saved-revision".into()));
    assert_eq!(format!("{saved:?}"), "CompareSave::Saved(..)");
    let CompareSave::Saved(revision) = saved else {
        panic!("saved comparison changed variants");
    };
    assert_eq!(revision.0, "secret-saved-revision");

    let conflict = CompareSave::Conflict(BackendSnapshot {
        revision: Revision("secret-conflicting-revision".into()),
        bytes: Some(b"secret-document".to_vec()),
    });
    assert_eq!(format!("{conflict:?}"), "CompareSave::Conflict(..)");
    let CompareSave::Conflict(snapshot) = conflict else {
        panic!("conflicting comparison changed variants");
    };
    assert_eq!(format!("{snapshot:?}"), "BackendSnapshot { .. }");
    assert_eq!(snapshot.revision.0, "secret-conflicting-revision");
    assert_eq!(
        snapshot.bytes.as_deref(),
        Some(b"secret-document".as_slice())
    );
}

/// Distinguishes lifecycle failures without inventing an underlying cause.
#[test]
fn receipt_lifecycle_diagnostics_distinguish_outcomes_without_sources() {
    for (error, expected) in [
        (ReceiptError::NotLoaded, "document not loaded"),
        (ReceiptError::Closing, "document closing"),
        (ReceiptError::Aborted, "document aborted"),
        (
            ReceiptError::Abandoned,
            "persistence acknowledgement abandoned",
        ),
    ] {
        assert_eq!(error.to_string(), expected);
        assert!(error.source().is_none());
    }
}

/// Encodes eight-byte integers and rejects `-1` to exercise encoding failure.
struct Binary;
impl Codec<i64> for Binary {
    type Error = SecretError;
    fn encode(&self, value: &i64) -> Result<Vec<u8>, SecretError> {
        if *value == -1 {
            return Err(SecretError(3));
        }
        Ok(value.to_le_bytes().to_vec())
    }
    fn decode(&self, bytes: &[u8]) -> Result<i64, SecretError> {
        bytes
            .try_into()
            .map(i64::from_le_bytes)
            .map_err(|_invalid_length| SecretError(2))
    }
}

/// Uses decimal text to prove that the document owns no fixed format.
struct Text;
impl Codec<i64> for Text {
    type Error = SecretError;
    fn encode(&self, value: &i64) -> Result<Vec<u8>, SecretError> {
        Ok(value.to_string().into_bytes())
    }
    fn decode(&self, bytes: &[u8]) -> Result<i64, SecretError> {
        std::str::from_utf8(bytes)
            .ok()
            .and_then(|text| text.parse().ok())
            .ok_or(SecretError(2))
    }
}

/// Retains one admitted write until the test completes its external work.
struct Save {
    /// Original byte payload submitted by the document.
    bytes: Vec<u8>,
    /// Delivery endpoint, which can close before external work finishes.
    completion: Completion<Result<(), SecretError>>,
}

/// Records independently controlled storage work and completed writes.
#[derive(Default)]
struct Io {
    /// Load replies in admission order.
    loads: VecDeque<Completion<Result<Option<Vec<u8>>, SecretError>>>,
    /// Writes whose physical completion the test still owns.
    saves: VecDeque<Save>,
    /// Successful write history used to detect extra snapshots.
    written: Vec<Vec<u8>>,
    /// Latest persisted bytes, independent of actor state.
    storage: Option<Vec<u8>>,
    /// Waiters for the outstanding physical writes.
    joins: Vec<Completion<()>>,
}

/// Separates actor delivery from test-controlled physical storage completion.
#[derive(Clone, Default)]
struct ControlledBackend(Arc<Mutex<Io>>);

impl Backend for ControlledBackend {
    type Error = SecretError;
    async fn load(&self) -> Result<Option<Vec<u8>>, SecretError> {
        let (completion, future) = controlled_future();
        self.0.lock().unwrap().loads.push_back(completion);
        future.await.expect("controller must complete the load")
    }
    async fn save(&self, bytes: Vec<u8>) -> Result<(), SecretError> {
        let (completion, future) = controlled_future();
        self.0
            .lock()
            .unwrap()
            .saves
            .push_back(Save { bytes, completion });
        future.await.expect("controller must complete the save")
    }
}

impl ControlledBackend {
    /// Completes the next admitted load with the supplied independent result.
    fn load(&self, result: Result<Option<Vec<u8>>, SecretError>) {
        self.0
            .lock()
            .unwrap()
            .loads
            .pop_front()
            .unwrap()
            .complete(result)
            .unwrap();
    }
    /// Completes the next write and returns its original payload for assertions.
    fn save(&self, result: Result<(), SecretError>) -> Vec<u8> {
        let mut io = self.0.lock().unwrap();
        let Save { bytes, completion } = io.saves.pop_front().expect("save must have begun");
        if result.is_ok() {
            io.storage = Some(bytes.clone());
            io.written.push(bytes.clone());
        }
        completion.complete(result).unwrap();
        bytes
    }
    /// Counts admitted writes that still await physical completion.
    fn saves(&self) -> usize {
        self.0.lock().unwrap().saves.len()
    }

    /// Finishes a physical write after its actor delivery was cancelled.
    fn finish_external(&self) -> Vec<u8> {
        let mut io = self.0.lock().unwrap();
        let Save { bytes, completion } = io.saves.pop_front().unwrap();
        io.storage = Some(bytes.clone());
        io.written.push(bytes.clone());
        assert!(
            completion.complete(Ok(())).is_err(),
            "RCA delivery must have been cancelled"
        );
        if io.saves.is_empty() {
            for join in io.joins.drain(..) {
                join.complete(()).unwrap();
            }
        }
        bytes
    }

    /// Waits for outstanding physical writes, not cancelled actor delivery.
    ///
    /// This fixture joins writes only. Tests complete or reject every load separately.
    fn close(&self) -> ControlledFuture<()> {
        let (completion, future) = controlled_future();
        let mut io = self.0.lock().unwrap();
        if io.saves.is_empty() {
            completion.complete(()).unwrap();
        } else {
            io.joins.push(completion);
        }
        future
    }
}

/// Exposes coordination fields without borrowing the document across an await.
#[derive(Clone, Debug, PartialEq)]
struct Snapshot {
    /// Latest in-memory value.
    value: i64,
    /// Number of accepted local edits.
    revision: u64,
    /// Last revision acknowledged by storage.
    acknowledged: Option<u64>,
    /// Initial-load phase that controls edit admission.
    load: LoadStatus,
    /// Revision captured by the active save.
    saving: Option<u64>,
    /// Identity of the active load or save attempt.
    attempt: Option<u64>,
    /// Failed boundary, optional revision and original secret marker.
    failure: Option<(Operation, Option<u64>, u32)>,
    /// Accepted flush or close waiters.
    barriers: usize,
    /// Whether close already fenced edits.
    closing: bool,
    /// Whether close completed successfully.
    closed: bool,
    /// Whether abort permanently fenced this document lifetime.
    aborted: bool,
}

/// Projects public state accessors for literal, field-by-field transition expectations.
fn snapshot(state: &State<i64>) -> Snapshot {
    Snapshot {
        value: *state.value(),
        revision: state.revision(),
        acknowledged: state.acknowledged_revision(),
        load: state.load_status(),
        saving: state.saving_revision(),
        attempt: state.active_attempt().map(Attempt::identifier),
        failure: state.failure().map(|failure| {
            (
                failure.operation(),
                failure.revision(),
                failure
                    .source()
                    .unwrap()
                    .downcast_ref::<SecretError>()
                    .unwrap()
                    .0,
            )
        }),
        barriers: state.pending_barrier_count(),
        closing: state.is_closing(),
        closed: state.is_closed(),
        aborted: state.is_aborted(),
    }
}

/// Drives the real document through an inspectable scalar snapshot.
type TestDocument = TestStore<State<i64>, Action<i64>, Snapshot>;

/// Builds a fresh document with default value seven and controlled storage.
fn document<C: Codec<i64>>(codec: C) -> (TestDocument, ControlledBackend) {
    let io = ControlledBackend::default();
    let (state, reducer) = Document::new(7, io.clone(), codec);
    (TestStore::projected(state, reducer, snapshot), io)
}

/// Loads optional binary bytes and checks the initial acknowledgement boundary.
async fn ready(store: &mut TestDocument, io: &ControlledBackend, value: Option<i64>) {
    let _ = store
        .send_with(Action::load(), |state| {
            state.load = LoadStatus::Loading;
            state.attempt = Some(1);
        })
        .await
        .unwrap();
    io.load(Ok(value.map(|value| value.to_le_bytes().to_vec())));
    let _ = store
        .receive_with(
            |_| true,
            move |state| {
                state.load = LoadStatus::Ready;
                state.attempt = None;
                if let Some(value) = value {
                    state.value = value;
                    state.acknowledged = Some(0);
                }
            },
        )
        .await
        .unwrap();
}

/// Preserves missing defaults and requires storage before acknowledging revision zero.
#[test]
fn missing_load_keeps_default_and_flush_materialises_revision_zero() {
    block_on(async {
        let (mut store, io) = document(Binary);
        let _ = store.send(Action::set(99)).await.unwrap();
        let (action, receipt) = Action::flush();
        let _ = store.send(action).await.unwrap();
        assert!(matches!(receipt.await, Err(ReceiptError::NotLoaded)));
        assert_eq!(io.saves(), 0);
        ready(&mut store, &io, None).await;
        assert_eq!(io.saves(), 0);
        let (action, receipt) = Action::flush();
        let _ = store
            .send_with(action, |state| {
                state.saving = Some(0);
                state.attempt = Some(2);
                state.barriers = 1;
            })
            .await
            .unwrap();
        assert_eq!(io.save(Ok(())), 7_i64.to_le_bytes());
        let _ = store
            .receive_with(
                |_| true,
                |state| {
                    state.acknowledged = Some(0);
                    state.saving = None;
                    state.attempt = None;
                    state.barriers = 0;
                },
            )
            .await
            .unwrap();
        assert_eq!(
            receipt.await.unwrap(),
            FlushOutcome {
                revision: 0,
                closing: false
            }
        );
        store.finish().await.unwrap();
    });
}

/// Avoids redundant clean saves and rejects edits after close.
#[test]
fn present_load_clean_barriers_do_no_io_and_close_fences_edits() {
    block_on(async {
        let (mut store, io) = document(Binary);
        ready(&mut store, &io, Some(23)).await;
        let _ = store.send(Action::load()).await.unwrap();
        let (action, receipt) = Action::flush();
        let _ = store.send(action).await.unwrap();
        assert_eq!(
            receipt.await.unwrap(),
            FlushOutcome {
                revision: 0,
                closing: false
            }
        );
        let (action, receipt) = Action::close();
        let _ = store
            .send_with(action, |state| {
                state.closing = true;
                state.closed = true;
            })
            .await
            .unwrap();
        assert_eq!(
            receipt.await.unwrap(),
            FlushOutcome {
                revision: 0,
                closing: true
            }
        );
        let _ = store.send(Action::set(41)).await.unwrap();
        let (action, receipt) = Action::flush();
        let _ = store.send(action).await.unwrap();
        assert!(matches!(receipt.await, Err(ReceiptError::Closing)));
        assert_eq!(io.saves(), 0);
        store.finish().await.unwrap();
    });
}

/// Preserves typed load failures and permits explicit retry after corruption.
#[test]
fn failed_and_corrupt_loads_preserve_sources_and_explicit_retry() {
    block_on(async {
        let (mut store, io) = document(Binary);
        let _ = store
            .send_with(Action::load(), |state| {
                state.load = LoadStatus::Loading;
                state.attempt = Some(1);
            })
            .await
            .unwrap();
        io.load(Err(SecretError(73)));
        let _ = store
            .receive_with(
                |action| {
                    let rendered = format!("{action:?}");
                    assert_eq!(rendered, "Loaded(..)");
                    assert!(!rendered.contains("SecretError(73)"));
                    assert!(!rendered.contains("sensitive-secret-73"));
                    true
                },
                |state| {
                    state.load = LoadStatus::Unloaded;
                    state.attempt = None;
                    state.failure = Some((Operation::Load, None, 73));
                },
            )
            .await
            .unwrap();
        store
            .assert_state(|state| {
                let failure = state.failure().unwrap();
                assert_eq!(
                    failure
                        .source()
                        .unwrap()
                        .downcast_ref::<SecretError>()
                        .unwrap()
                        .0,
                    73
                );
                assert!(!format!("{failure:?} {failure}").contains("sensitive-secret"));
            })
            .await
            .unwrap();
        let _ = store
            .send_with(Action::load(), |state| {
                state.load = LoadStatus::Loading;
                state.attempt = Some(2);
                state.failure = None;
            })
            .await
            .unwrap();
        io.load(Ok(Some(vec![1])));
        let _ = store
            .receive_with(
                |_| true,
                |state| {
                    state.load = LoadStatus::Unloaded;
                    state.attempt = None;
                    state.failure = Some((Operation::Decode, None, 2));
                },
            )
            .await
            .unwrap();
        let _ = store
            .send_with(Action::load(), |state| {
                state.load = LoadStatus::Loading;
                state.attempt = Some(3);
                state.failure = None;
            })
            .await
            .unwrap();
        io.load(Ok(Some(31_i64.to_le_bytes().to_vec())));
        let _ = store
            .receive_with(
                |action| {
                    let rendered = format!("{action:?}");
                    assert_eq!(rendered, "Loaded(..)");
                    assert!(!rendered.contains("31"));
                    true
                },
                |state| {
                    state.load = LoadStatus::Ready;
                    state.value = 31;
                    state.acknowledged = Some(0);
                    state.attempt = None;
                },
            )
            .await
            .unwrap();
        store.finish().await.unwrap();
    });
}

/// Uses the supplied text codec rather than a built-in byte format.
#[test]
fn alternate_codec_decodes_its_own_format() {
    block_on(async {
        let (mut store, io) = document(Text);
        let _ = store
            .send_with(Action::load(), |state| {
                state.load = LoadStatus::Loading;
                state.attempt = Some(1);
            })
            .await
            .unwrap();
        io.load(Ok(Some(b"43".to_vec())));
        let _ = store
            .receive_with(
                |_| true,
                |state| {
                    state.load = LoadStatus::Ready;
                    state.value = 43;
                    state.acknowledged = Some(0);
                    state.attempt = None;
                },
            )
            .await
            .unwrap();
        let _ = store
            .send_with(Action::set(59), |state| {
                state.value = 59;
                state.revision = 1;
                state.saving = Some(1);
                state.attempt = Some(2);
            })
            .await
            .unwrap();
        assert_eq!(io.save(Ok(())), b"59");
        let _ = store
            .receive_with(
                |_| true,
                |state| {
                    state.acknowledged = Some(1);
                    state.saving = None;
                    state.attempt = None;
                },
            )
            .await
            .unwrap();
        store.finish().await.unwrap();
    });
}

/// Reports abandonment when a barrier action never reaches the actor.
#[test]
fn dropping_unsubmitted_action_abandons_receipt() {
    let (action, receipt) = Action::<i64>::flush();
    assert_eq!(format!("{receipt:?}"), "Receipt { .. }");
    let mut receipt = Box::pin(receipt);
    assert!(
        receipt
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_pending()
    );
    drop(action);
    assert_eq!(
        format!("{:?}", receipt.as_ref().get_ref()),
        "Receipt { .. }"
    );
    assert!(matches!(
        receipt
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop())),
        Poll::Ready(Err(ReceiptError::Abandoned))
    ));
}

/// Coalesces edits without overlapping saves or storing intermediate snapshots.
#[test]
fn one_thousand_edits_coalesce_to_first_and_latest_snapshots() {
    block_on(async {
        let (mut store, io) = document(Binary);
        ready(&mut store, &io, Some(7)).await;
        for value in 1..=1_000 {
            let _ = store
                .send_with(Action::set(value), move |state| {
                    state.value = value;
                    state.revision = u64::try_from(value).unwrap();
                    if value == 1 {
                        state.saving = Some(1);
                        state.attempt = Some(2);
                    }
                })
                .await
                .unwrap();
            assert_eq!(io.saves(), 1);
        }
        assert_eq!(io.save(Ok(())), 1_i64.to_le_bytes());
        let _ = store
            .receive_with(
                |_| true,
                |state| {
                    state.acknowledged = Some(1);
                    state.saving = Some(1_000);
                    state.attempt = Some(3);
                },
            )
            .await
            .unwrap();
        assert_eq!(io.save(Ok(())), 1_000_i64.to_le_bytes());
        let _ = store
            .receive_with(
                |_| true,
                |state| {
                    state.acknowledged = Some(1_000);
                    state.saving = None;
                    state.attempt = None;
                },
            )
            .await
            .unwrap();
        assert_eq!(io.0.lock().unwrap().written.len(), 2);
        store.finish().await.unwrap();
    });
}

/// Resolves a flush at its captured revision while later edits remain dirty.
#[test]
fn flush_resolves_its_revision_while_later_edits_remain_dirty() {
    block_on(async {
        let (mut store, io) = document(Binary);
        ready(&mut store, &io, Some(7)).await;
        let _ = store
            .send_with(Action::set(11), |state| {
                state.value = 11;
                state.revision = 1;
                state.saving = Some(1);
                state.attempt = Some(2);
            })
            .await
            .unwrap();
        let (action, receipt) = Action::flush();
        let _ = store
            .send_with(action, |state| state.barriers = 1)
            .await
            .unwrap();
        let _ = store
            .send_with(Action::set(23), |state| {
                state.value = 23;
                state.revision = 2;
            })
            .await
            .unwrap();
        io.save(Ok(()));
        let _ = store
            .receive_with(
                |_| true,
                |state| {
                    state.acknowledged = Some(1);
                    state.saving = Some(2);
                    state.attempt = Some(3);
                    state.barriers = 0;
                },
            )
            .await
            .unwrap();
        assert_eq!(
            receipt.await.unwrap(),
            FlushOutcome {
                revision: 1,
                closing: false
            }
        );
        store
            .assert_state(|state| {
                assert_eq!(state.acknowledged_revision(), Some(1));
                assert_eq!(state.saving_revision(), Some(2));
            })
            .await
            .unwrap();
        io.save(Ok(()));
        let _ = store
            .receive_with(
                |_| true,
                |state| {
                    state.acknowledged = Some(2);
                    state.saving = None;
                    state.attempt = None;
                },
            )
            .await
            .unwrap();
        store.finish().await.unwrap();
    });
}

/// Keeps persistence live after task handles or receipt receivers are dropped.
#[test]
fn multiple_and_dropped_waiters_do_not_cancel_persistence() {
    block_on(async {
        let (mut store, io) = document(Binary);
        ready(&mut store, &io, Some(7)).await;
        let task = store
            .send_with(Action::set(13), |state| {
                state.value = 13;
                state.revision = 1;
                state.saving = Some(1);
                state.attempt = Some(2);
            })
            .await
            .unwrap();
        drop(task);
        let (first, first_receipt) = Action::flush();
        let _ = store
            .send_with(first, |state| state.barriers = 1)
            .await
            .unwrap();
        let (dropped, dropped_receipt) = Action::flush();
        let _ = store
            .send_with(dropped, |state| state.barriers = 2)
            .await
            .unwrap();
        drop(dropped_receipt);
        let (third, third_receipt) = Action::flush();
        let _ = store
            .send_with(third, |state| state.barriers = 3)
            .await
            .unwrap();
        io.save(Ok(()));
        let _ = store
            .receive_with(
                |_| true,
                |state| {
                    state.acknowledged = Some(1);
                    state.saving = None;
                    state.attempt = None;
                    state.barriers = 0;
                },
            )
            .await
            .unwrap();
        assert_eq!(
            first_receipt.await.unwrap(),
            FlushOutcome {
                revision: 1,
                closing: false
            }
        );
        assert_eq!(
            third_receipt.await.unwrap(),
            FlushOutcome {
                revision: 1,
                closing: false
            }
        );
        store.finish().await.unwrap();
    });
}

/// Suspends failed saves until explicit retry captures the latest edit.
#[test]
fn save_failure_resolves_waiters_suspends_edits_and_flush_retries_latest() {
    block_on(async {
        let (mut store, io) = document(Binary);
        ready(&mut store, &io, Some(7)).await;
        let _ = store
            .send_with(Action::set(17), |state| {
                state.value = 17;
                state.revision = 1;
                state.saving = Some(1);
                state.attempt = Some(2);
            })
            .await
            .unwrap();
        let (action, receipt) = Action::flush();
        let _ = store
            .send_with(action, |state| state.barriers = 1)
            .await
            .unwrap();
        io.save(Err(SecretError(73)));
        let _ = store
            .receive_with(
                |action| {
                    let rendered = format!("{action:?}");
                    assert_eq!(rendered, "Saved(..)");
                    assert!(!rendered.contains("SecretError(73)"));
                    assert!(!rendered.contains("sensitive-secret-73"));
                    true
                },
                |state| {
                    state.saving = None;
                    state.attempt = None;
                    state.barriers = 0;
                    state.failure = Some((Operation::Save, Some(1), 73));
                },
            )
            .await
            .unwrap();
        let error = receipt.await.unwrap_err();
        // Optional backtraces follow the stable first-line summary.
        assert_eq!(
            error.to_string().lines().next(),
            Some("document Save failed")
        );
        let ReceiptError::Persistence(failure) = error else {
            panic!("failed save must fail its barrier");
        };
        assert_eq!(failure.operation(), Operation::Save);
        assert_eq!(failure.revision(), Some(1));
        assert_eq!(
            failure
                .source()
                .unwrap()
                .downcast_ref::<SecretError>()
                .unwrap()
                .0,
            73
        );
        let _ = store
            .send_with(Action::set(29), |state| {
                state.value = 29;
                state.revision = 2;
            })
            .await
            .unwrap();
        assert_eq!(io.saves(), 0);
        let (action, receipt) = Action::flush();
        let _ = store
            .send_with(action, |state| {
                state.saving = Some(2);
                state.attempt = Some(3);
                state.failure = None;
                state.barriers = 1;
            })
            .await
            .unwrap();
        assert_eq!(io.save(Ok(())), 29_i64.to_le_bytes());
        let _ = store
            .receive_with(
                |_| true,
                |state| {
                    state.acknowledged = Some(2);
                    state.saving = None;
                    state.attempt = None;
                    state.barriers = 0;
                },
            )
            .await
            .unwrap();
        assert_eq!(receipt.await.unwrap().revision, 2);
        store.finish().await.unwrap();
    });
}

/// Separates encoding failure from backend writes and explicit close retry.
#[test]
fn encoding_failure_never_calls_backend_and_explicit_close_retries() {
    block_on(async {
        let (mut store, io) = document(Binary);
        ready(&mut store, &io, Some(7)).await;
        let _ = store
            .send_with(Action::set(-1), |state| {
                state.value = -1;
                state.revision = 1;
                state.saving = Some(1);
                state.attempt = Some(2);
            })
            .await
            .unwrap();
        let _ = store
            .receive_with(
                |_| true,
                |state| {
                    state.saving = None;
                    state.attempt = None;
                    state.failure = Some((Operation::Encode, Some(1), 3));
                },
            )
            .await
            .unwrap();
        assert_eq!(io.saves(), 0);
        let _ = store
            .send_with(Action::set(37), |state| {
                state.value = 37;
                state.revision = 2;
            })
            .await
            .unwrap();
        assert_eq!(io.saves(), 0);
        let (action, receipt) = Action::close();
        let _ = store
            .send_with(action, |state| {
                state.closing = true;
                state.saving = Some(2);
                state.attempt = Some(3);
                state.failure = None;
                state.barriers = 1;
            })
            .await
            .unwrap();
        assert_eq!(io.save(Ok(())), 37_i64.to_le_bytes());
        let _ = store
            .receive_with(
                |_| true,
                |state| {
                    state.acknowledged = Some(2);
                    state.saving = None;
                    state.attempt = None;
                    state.barriers = 0;
                    state.closed = true;
                },
            )
            .await
            .unwrap();
        assert_eq!(
            receipt.await.unwrap(),
            FlushOutcome {
                revision: 2,
                closing: true
            }
        );
        store.finish().await.unwrap();
    });
}

/// Keeps close fenced through joined waiters, failure and final-save retry.
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one ordered close/join/failure/retry sequence keeps every intermediate coordination field explicit"
)]
fn close_during_save_joins_and_retries_without_reopening_edits() {
    block_on(async {
        let (mut store, io) = document(Binary);
        ready(&mut store, &io, Some(7)).await;
        let _ = store
            .send_with(Action::set(11), |state| {
                state.value = 11;
                state.revision = 1;
                state.saving = Some(1);
                state.attempt = Some(2);
            })
            .await
            .unwrap();
        let _ = store
            .send_with(Action::set(23), |state| {
                state.value = 23;
                state.revision = 2;
            })
            .await
            .unwrap();
        let (first, first_receipt) = Action::close();
        let _ = store
            .send_with(first, |state| {
                state.closing = true;
                state.barriers = 1;
            })
            .await
            .unwrap();
        let (second, second_receipt) = Action::close();
        let _ = store
            .send_with(second, |state| state.barriers = 2)
            .await
            .unwrap();
        let _ = store.send(Action::set(99)).await.unwrap();
        let (action, receipt) = Action::flush();
        let _ = store.send(action).await.unwrap();
        assert!(matches!(receipt.await, Err(ReceiptError::Closing)));
        io.save(Ok(()));
        let _ = store
            .receive_with(
                |_| true,
                |state| {
                    state.acknowledged = Some(1);
                    state.saving = Some(2);
                    state.attempt = Some(3);
                },
            )
            .await
            .unwrap();
        io.save(Err(SecretError(4)));
        let _ = store
            .receive_with(
                |_| true,
                |state| {
                    state.saving = None;
                    state.attempt = None;
                    state.barriers = 0;
                    state.failure = Some((Operation::Save, Some(2), 4));
                },
            )
            .await
            .unwrap();
        assert!(matches!(
            first_receipt.await,
            Err(ReceiptError::Persistence(_))
        ));
        assert!(matches!(
            second_receipt.await,
            Err(ReceiptError::Persistence(_))
        ));
        let _ = store.send(Action::set(101)).await.unwrap();
        let (action, receipt) = Action::close();
        let _ = store
            .send_with(action, |state| {
                state.saving = Some(2);
                state.attempt = Some(4);
                state.barriers = 1;
                state.failure = None;
            })
            .await
            .unwrap();
        assert_eq!(io.save(Ok(())), 23_i64.to_le_bytes());
        let _ = store
            .receive_with(
                |_| true,
                |state| {
                    state.acknowledged = Some(2);
                    state.saving = None;
                    state.attempt = None;
                    state.barriers = 0;
                    state.closed = true;
                },
            )
            .await
            .unwrap();
        assert_eq!(
            receipt.await.unwrap(),
            FlushOutcome {
                revision: 2,
                closing: true
            }
        );
        store
            .assert_state(|state| {
                assert!(state.is_closed());
                assert_eq!(state.acknowledged_revision(), Some(2));
            })
            .await
            .unwrap();
        let (action, receipt) = Action::close();
        let _ = store.send(action).await.unwrap();
        assert_eq!(
            receipt.await.unwrap(),
            FlushOutcome {
                revision: 2,
                closing: true
            }
        );
        store.finish().await.unwrap();
    });
}

/// Rejects barriers before loading without fencing edits or starting a write.
#[test]
fn unloaded_and_loading_barriers_are_rejected_without_fencing() {
    block_on(async {
        let (mut store, io) = document(Binary);
        for loading in [false, true] {
            if loading {
                let _ = store
                    .send_with(Action::load(), |state| {
                        state.load = LoadStatus::Loading;
                        state.attempt = Some(1);
                    })
                    .await
                    .unwrap();
            }
            for close in [false, true] {
                let (action, receipt) = if close {
                    Action::close()
                } else {
                    Action::flush()
                };
                let _ = store.send(action).await.unwrap();
                assert!(matches!(receipt.await, Err(ReceiptError::NotLoaded)));
            }
            let _ = store.send(Action::set(99)).await.unwrap();
            assert_eq!(io.saves(), 0);
        }
        io.load(Ok(None));
        let _ = store
            .receive_with(
                |_| true,
                |state| {
                    state.load = LoadStatus::Ready;
                    state.attempt = None;
                },
            )
            .await
            .unwrap();
        store.finish().await.unwrap();
    });
}

/// Rejects old load delivery after a newer attempt replaces it.
#[test]
fn replacing_load_cancels_previous_delivery_and_uses_latest_attempt() {
    block_on(async {
        let (mut store, io) = document(Binary);
        let previous = store
            .send_with(Action::load(), |state| {
                state.load = LoadStatus::Loading;
                state.attempt = Some(1);
            })
            .await
            .unwrap();
        let _ = store
            .send_with(Action::load(), |state| state.attempt = Some(2))
            .await
            .unwrap();
        let previous_completion = io.0.lock().unwrap().loads.pop_front().unwrap();
        assert!(
            previous_completion
                .complete(Ok(Some(99_i64.to_le_bytes().to_vec())))
                .is_err()
        );
        assert!(previous.is_finished());
        io.load(Ok(Some(41_i64.to_le_bytes().to_vec())));
        let _ = store
            .receive_with(
                |_| true,
                |state| {
                    state.load = LoadStatus::Ready;
                    state.attempt = None;
                    state.value = 41;
                    state.acknowledged = Some(0);
                },
            )
            .await
            .unwrap();
        store.finish().await.unwrap();
    });
}

/// Requires abort and physical write completion before restarting cancelled persistence.
#[test]
fn causal_save_cancellation_requires_abort_and_backend_quiescence_before_restart() {
    block_on(async {
        let (mut store, io) = document(Binary);
        ready(&mut store, &io, Some(7)).await;
        let task = store
            .send_with(Action::set(61), |state| {
                state.value = 61;
                state.revision = 1;
                state.saving = Some(1);
                state.attempt = Some(2);
            })
            .await
            .unwrap();
        let (action, receipt) = Action::flush();
        let _ = store
            .send_with(action, |state| state.barriers = 1)
            .await
            .unwrap();
        task.cancel().await.unwrap();
        store
            .assert_state(|state| {
                assert_eq!(state.saving_revision(), Some(1));
                assert_eq!(state.pending_barrier_count(), 1);
            })
            .await
            .unwrap();
        let (action, unsafe_recovery) = Action::flush();
        let _ = store
            .send_with(action, |state| state.barriers = 2)
            .await
            .unwrap();
        assert_eq!(io.saves(), 1, "no automatic cancel-then-flush recovery");
        let _ = store
            .send_with(Action::abort(), |state| {
                state.aborted = true;
                state.saving = None;
                state.attempt = None;
                state.barriers = 0;
            })
            .await
            .unwrap();
        assert!(matches!(receipt.await, Err(ReceiptError::Aborted)));
        assert!(matches!(unsafe_recovery.await, Err(ReceiptError::Aborted)));
        for close in [false, true] {
            let (action, receipt) = if close {
                Action::close()
            } else {
                Action::flush()
            };
            let _ = store.send(action).await.unwrap();
            assert!(matches!(receipt.await, Err(ReceiptError::Aborted)));
        }
        let _ = store.send(Action::set(99)).await.unwrap();
        let _ = store.send(Action::load()).await.unwrap();
        let _ = store.send(Action::abort()).await.unwrap();
        store.finish().await.unwrap();
        let mut join = Box::pin(io.close());
        assert!(
            join.as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending()
        );
        assert_eq!(io.finish_external(), 61_i64.to_le_bytes());
        join.await.unwrap();
        let persisted = io.0.lock().unwrap().storage.clone();
        let (state, reducer) = Document::new(0, io.clone(), Binary);
        let mut restarted = TestStore::projected(state, reducer, snapshot);
        let _ = restarted
            .send_with(Action::load(), |state| {
                state.load = LoadStatus::Loading;
                state.attempt = Some(1);
            })
            .await
            .unwrap();
        io.load(Ok(persisted));
        let _ = restarted
            .receive_with(
                |_| true,
                |state| {
                    state.load = LoadStatus::Ready;
                    state.value = 61;
                    state.acknowledged = Some(0);
                    state.attempt = None;
                },
            )
            .await
            .unwrap();
        restarted.finish().await.unwrap();
    });
}

/// Cancels load delivery and rejects every operation after abort.
#[test]
fn abort_while_loading_cancels_delivery_and_fences_all_operations() {
    block_on(async {
        let (mut store, io) = document(Binary);
        let _ = store
            .send_with(Action::load(), |state| {
                state.load = LoadStatus::Loading;
                state.attempt = Some(1);
            })
            .await
            .unwrap();
        let _ = store
            .send_with(Action::abort(), |state| {
                state.load = LoadStatus::Unloaded;
                state.attempt = None;
                state.aborted = true;
            })
            .await
            .unwrap();
        assert!(
            io.0.lock()
                .unwrap()
                .loads
                .pop_front()
                .unwrap()
                .complete(Ok(None))
                .is_err()
        );
        let _ = store.send(Action::load()).await.unwrap();
        let (action, receipt) = Action::close();
        let _ = store.send(action).await.unwrap();
        assert!(matches!(receipt.await, Err(ReceiptError::Aborted)));
        store.finish().await.unwrap();
    });
}

/// Keeps a document terminal when its parent aborts before forwarding load completion.
#[test]
fn parent_abort_before_forwarding_a_load_completion_keeps_document_terminal() {
    /// Separates initial admission from the genuine asynchronous load completion.
    enum ParentAction {
        /// Requests loading through the child reducer.
        Start,
        /// Carries the completion that parent policy must fence.
        Completed(Action<i64>),
    }
    /// Aborts the child before forwarding its admitted load completion.
    struct AbortBeforeCompletion(Document<i64, ControlledBackend, Binary>);
    impl Reducer for AbortBeforeCompletion {
        type State = State<i64>;
        type Action = ParentAction;

        fn reduce(
            &mut self,
            state: &mut Self::State,
            action: &Self::Action,
            context: &mut ReduceContext<'_>,
        ) -> Effect<Self::Action> {
            match action {
                ParentAction::Start => self
                    .0
                    .reduce(state, &Action::load(), context)
                    .map(ParentAction::Completed),
                ParentAction::Completed(action) => {
                    // Parent policy aborts before forwarding the genuine borrowed completion.
                    let abort = self.0.reduce(state, &Action::abort(), context);
                    let completion = self.0.reduce(state, action, context);
                    Effect::merge([abort, completion]).map(ParentAction::Completed)
                }
            }
        }
    }

    block_on(async {
        let io = ControlledBackend::default();
        let (state, document) = Document::new(7, io.clone(), Binary);
        let mut store = TestStore::projected(state, AbortBeforeCompletion(document), snapshot);
        let _ = store
            .send_with(ParentAction::Start, |state| {
                state.load = LoadStatus::Loading;
                state.attempt = Some(1);
            })
            .await
            .unwrap();
        io.load(Ok(Some(99_i64.to_le_bytes().to_vec())));
        let _ = store
            .receive_with(
                |action| matches!(action, ParentAction::Completed(_)),
                |state| {
                    *state = Snapshot {
                        value: 7,
                        revision: 0,
                        acknowledged: None,
                        load: LoadStatus::Unloaded,
                        saving: None,
                        attempt: None,
                        failure: None,
                        barriers: 0,
                        closing: false,
                        closed: false,
                        aborted: true,
                    };
                },
            )
            .await
            .unwrap();
        let _ = store.send(ParentAction::Start).await.unwrap();
        assert!(io.0.lock().unwrap().loads.is_empty());
        assert_eq!(io.saves(), 0);
        store.finish().await.unwrap();
    });
}

/// Rejects save acknowledgement when the parent aborts before forwarding completion.
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "keeps the real save, parent abort, barrier and physical write in one causal trace"
)]
fn parent_abort_before_forwarding_a_save_completion_keeps_barrier_aborted() {
    /// Distinguishes ordinary child traffic from the save completion under test.
    enum ParentAction {
        /// Forwards load and barrier traffic without an abort.
        Forward(Action<i64>),
        /// Starts a save whose completion follows the abort path.
        Edit(i64),
        /// Carries the genuine save completion into the parent policy boundary.
        Saved(Action<i64>),
    }
    /// Aborts before forwarding a successful physical save acknowledgement.
    struct AbortBeforeSave(Document<i64, ControlledBackend, Binary>);
    impl Reducer for AbortBeforeSave {
        type State = State<i64>;
        type Action = ParentAction;

        fn reduce(
            &mut self,
            state: &mut Self::State,
            action: &Self::Action,
            context: &mut ReduceContext<'_>,
        ) -> Effect<Self::Action> {
            match action {
                ParentAction::Forward(action) => self
                    .0
                    .reduce(state, action, context)
                    .map(ParentAction::Forward),
                ParentAction::Edit(value) => self
                    .0
                    .reduce(state, &Action::set(*value), context)
                    .map(ParentAction::Saved),
                ParentAction::Saved(action) => {
                    let abort = self.0.reduce(state, &Action::abort(), context);
                    let completion = self.0.reduce(state, action, context);
                    Effect::merge([abort, completion]).map(ParentAction::Forward)
                }
            }
        }
    }

    block_on(async {
        let io = ControlledBackend::default();
        let (state, document) = Document::new(7, io.clone(), Binary);
        let mut store = TestStore::projected(state, AbortBeforeSave(document), snapshot);
        let _ = store
            .send_with(ParentAction::Forward(Action::load()), |state| {
                state.load = LoadStatus::Loading;
                state.attempt = Some(1);
            })
            .await
            .unwrap();
        io.load(Ok(Some(7_i64.to_le_bytes().to_vec())));
        let _ = store
            .receive_with(
                |action| matches!(action, ParentAction::Forward(_)),
                |state| {
                    state.load = LoadStatus::Ready;
                    state.acknowledged = Some(0);
                    state.attempt = None;
                },
            )
            .await
            .unwrap();
        let _ = store
            .send_with(ParentAction::Edit(61), |state| {
                state.value = 61;
                state.revision = 1;
                state.saving = Some(1);
                state.attempt = Some(2);
            })
            .await
            .unwrap();
        let (close, receipt) = Action::close();
        let _ = store
            .send_with(ParentAction::Forward(close), |state| {
                state.closing = true;
                state.barriers = 1;
            })
            .await
            .unwrap();
        assert_eq!(io.saves(), 1);
        assert_eq!(io.save(Ok(())), 61_i64.to_le_bytes());
        let _ = store
            .receive_with(
                |action| matches!(action, ParentAction::Saved(_)),
                |state| {
                    *state = Snapshot {
                        value: 61,
                        revision: 1,
                        acknowledged: Some(0),
                        load: LoadStatus::Ready,
                        saving: None,
                        attempt: None,
                        failure: None,
                        barriers: 0,
                        closing: true,
                        closed: false,
                        aborted: true,
                    };
                },
            )
            .await
            .unwrap();
        let outcome = receipt.await;
        assert!(matches!(outcome, Err(ReceiptError::Aborted)), "{outcome:?}");
        let _ = store.send(ParentAction::Edit(99)).await.unwrap();
        let _ = store
            .send(ParentAction::Forward(Action::load()))
            .await
            .unwrap();
        store.finish().await.unwrap();

        // Physical success predates the parent's abort; logical cancellation
        // neither rolls back bytes nor converts the barrier into success.
        io.close().await.unwrap();
        let storage = io.0.lock().unwrap();
        assert_eq!(storage.storage, Some(61_i64.to_le_bytes().to_vec()));
        assert_eq!(storage.written, [61_i64.to_le_bytes().to_vec()]);
        assert!(storage.loads.is_empty() && storage.saves.is_empty() && storage.joins.is_empty());
    });
}

/// Abandons pending receipts without treating shutdown as a flush.
#[test]
fn shutdown_is_abortive_not_a_flush() {
    let host = DeterministicRuntime::new();
    let io = ControlledBackend::default();
    let (state, reducer) = Document::new(7, io.clone(), Binary);
    let store = Store::new(state, reducer, host.runtime());
    block_on(host.run(async {
        let _ = store.send(Action::load()).await.unwrap();
        host.run_until_stalled().unwrap();
        io.load(Ok(Some(7_i64.to_le_bytes().to_vec())));
        host.run_until_stalled().unwrap();
        let _ = store.send(Action::set(71)).await.unwrap();
        host.run_until_stalled().unwrap();
        let (action, receipt) = Action::flush();
        let _ = store.send(action).await.unwrap();
        store.shutdown().await.unwrap();
        assert!(matches!(receipt.await, Err(ReceiptError::Abandoned)));
        assert_eq!(io.saves(), 1);
        assert!(io.0.lock().unwrap().written.is_empty());
    }))
    .unwrap();
    host.run_until_stalled().unwrap(); // Store shutdown issues aborts but does not join runtime wrappers.
    io.finish_external();
    block_on(io.close()).unwrap();
}

/// Requires owned effects by deliberately omitting `Sync` and `Debug`.
#[derive(Clone)]
struct OwnedValue(std::cell::Cell<i64>);
/// Applies the binary fixture format to the non-`Sync` value.
struct OwnedCodec;
impl Codec<OwnedValue> for OwnedCodec {
    type Error = SecretError;
    fn encode(&self, value: &OwnedValue) -> Result<Vec<u8>, SecretError> {
        Binary.encode(&value.0.get())
    }
    fn decode(&self, bytes: &[u8]) -> Result<OwnedValue, SecretError> {
        Binary
            .decode(bytes)
            .map(|value| OwnedValue(std::cell::Cell::new(value)))
    }
}

/// Preserves non-`Sync`, non-`Debug` values across owned effect execution.
#[test]
fn non_sync_non_debug_values_are_move_owned_by_effects() {
    block_on(async {
        let io = ControlledBackend::default();
        let (state, reducer) =
            Document::new(OwnedValue(std::cell::Cell::new(7)), io.clone(), OwnedCodec);
        let mut store = TestStore::projected(state, reducer, |state| {
            (
                state.value().0.get(),
                state.revision(),
                state.acknowledged_revision(),
                state.load_status(),
                state.saving_revision(),
                state.active_attempt().map(Attempt::identifier),
                state
                    .failure()
                    .map(|failure| (failure.operation(), failure.revision())),
                state.pending_barrier_count(),
                state.is_closing(),
                state.is_closed(),
                state.is_aborted(),
            )
        });
        let action = Action::load();
        assert_eq!(format!("{action:?}"), "Load");
        let _ = store
            .send_with(action, |state| {
                state.3 = LoadStatus::Loading;
                state.5 = Some(1);
            })
            .await
            .unwrap();
        io.load(Ok(Some(13_i64.to_le_bytes().to_vec())));
        let _ = store
            .receive_with(
                |action| {
                    assert_eq!(format!("{action:?}"), "Loaded(..)");
                    true
                },
                |state| {
                    state.0 = 13;
                    state.2 = Some(0);
                    state.3 = LoadStatus::Ready;
                    state.5 = None;
                },
            )
            .await
            .unwrap();
        let action = Action::set(OwnedValue(std::cell::Cell::new(29)));
        assert_eq!(format!("{action:?}"), "Set(..)");
        let _ = store
            .send_with(action, |state| {
                state.0 = 29;
                state.1 = 1;
                state.4 = Some(1);
                state.5 = Some(2);
            })
            .await
            .unwrap();
        assert_eq!(io.save(Ok(())), 29_i64.to_le_bytes());
        let _ = store
            .receive_with(
                |action| {
                    assert_eq!(format!("{action:?}"), "Saved(..)");
                    true
                },
                |state| {
                    state.2 = Some(1);
                    state.4 = None;
                    state.5 = None;
                },
            )
            .await
            .unwrap();
        for ((action, mut receipt), expected, closing) in [
            (Action::flush(), "Flush(..)", false),
            (Action::close(), "Close(..)", true),
        ] {
            assert_eq!(format!("{action:?}"), expected);
            let _ = store
                .send_with(action, |state| {
                    state.8 = closing;
                    state.9 = closing;
                })
                .await
                .unwrap();
            assert!(matches!(
                std::pin::Pin::new(&mut receipt)
                    .poll(&mut Context::from_waker(Waker::noop())),
                Poll::Ready(Ok(FlushOutcome { revision: 1, closing: actual })) if actual == closing
            ));
        }
        let action = Action::abort();
        assert_eq!(format!("{action:?}"), "Abort");
        let _ = store
            .send_with(action, |state| state.10 = true)
            .await
            .unwrap();
        store.finish().await.unwrap();
    });
}

/// Routes child work and explicitly replaces its structural lifetime.
enum ParentAction {
    /// Forwards a real document action or completion.
    Child(Action<i64>),
    /// Removes the child without claiming external storage has stopped.
    Remove,
    /// Inserts fresh document state after the test joins external work.
    Insert,
}
/// Owns the storage handle used to construct replacement child state.
struct Parent {
    /// Physical storage shared across distinct document lifetimes.
    backend: ControlledBackend,
}
impl Reducer for Parent {
    type State = Option<State<i64>>;
    type Action = ParentAction;
    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &Self::Action,
        _: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        match action {
            ParentAction::Remove => *state = None,
            ParentAction::Insert => *state = Some(Document::new(0, self.backend.clone(), Binary).0),
            ParentAction::Child(_) => {}
        }
        Effect::none()
    }
}

/// Replaces reducer ownership when the application explicitly inserts fresh state.
struct ReplaceableDocument {
    /// Physical storage that survives structural removal.
    backend: ControlledBackend,
    /// Reducer paired with the current child lifetime.
    inner: Document<i64, ControlledBackend, Binary>,
    /// Distinguishes initial construction from later fresh-state insertion.
    lifetime_started: bool,
}
impl Reducer for ReplaceableDocument {
    type State = State<i64>;
    type Action = Action<i64>;
    fn reduce(
        &mut self,
        state: &mut State<i64>,
        action: &Action<i64>,
        context: &mut ReduceContext<'_>,
    ) -> Effect<Action<i64>> {
        if state.load_status() == LoadStatus::Unloaded
            && state.active_attempt().is_none()
            && self.lifetime_started
        {
            self.inner = Document::new(0, self.backend.clone(), Binary).1;
        }
        self.lifetime_started = true;
        self.inner.reduce(state, action, context)
    }
}

/// Joins retired physical writes before fresh child insertion and loading.
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one structural lifetime sequence must retain the external writer, join it, then reinsert and load"
)]
fn structural_removal_requires_external_quiescence_before_fresh_reinsertion() {
    block_on(async {
        let io = ControlledBackend::default();
        let (state, document) = Document::new(7, io.clone(), Binary);
        let reducer = if_some(
            Parent {
                backend: io.clone(),
            },
            OptionalStatePath::new(
                |state: &Option<State<i64>>| state.as_ref(),
                |state: &mut Option<State<i64>>| state.as_mut(),
            ),
            ActionPath::new(
                |action| match action {
                    ParentAction::Child(action) => Some(action),
                    _ => None,
                },
                ParentAction::Child,
            ),
            ReplaceableDocument {
                backend: io.clone(),
                inner: document,
                lifetime_started: false,
            },
        );
        let mut store =
            TestStore::projected(Some(state), reducer, |state| state.as_ref().map(snapshot));
        let _ = store
            .send_with(ParentAction::Child(Action::load()), |state| {
                let state = state.as_mut().unwrap();
                state.load = LoadStatus::Loading;
                state.attempt = Some(1);
            })
            .await
            .unwrap();
        io.load(Ok(Some(7_i64.to_le_bytes().to_vec())));
        let _ = store
            .receive_with(
                |_| true,
                |state| {
                    let state = state.as_mut().unwrap();
                    state.load = LoadStatus::Ready;
                    state.acknowledged = Some(0);
                    state.attempt = None;
                },
            )
            .await
            .unwrap();
        let _ = store
            .send_with(ParentAction::Child(Action::set(83)), |state| {
                let state = state.as_mut().unwrap();
                state.value = 83;
                state.revision = 1;
                state.saving = Some(1);
                state.attempt = Some(2);
            })
            .await
            .unwrap();
        let (action, receipt) = Action::flush();
        let _ = store
            .send_with(ParentAction::Child(action), |state| {
                state.as_mut().unwrap().barriers = 1;
            })
            .await
            .unwrap();
        let _ = store
            .send_with(ParentAction::Remove, |state| *state = None)
            .await
            .unwrap();
        assert!(matches!(receipt.await, Err(ReceiptError::Abandoned)));
        let mut join = Box::pin(io.close());
        assert!(
            join.as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending()
        );
        io.finish_external();
        join.await.unwrap();
        let _ = store
            .send_with(ParentAction::Insert, |state| {
                *state = Some(Snapshot {
                    value: 0,
                    revision: 0,
                    acknowledged: None,
                    load: LoadStatus::Unloaded,
                    saving: None,
                    attempt: None,
                    failure: None,
                    barriers: 0,
                    closing: false,
                    closed: false,
                    aborted: false,
                });
            })
            .await
            .unwrap();
        let _ = store
            .send_with(ParentAction::Child(Action::load()), |state| {
                let state = state.as_mut().unwrap();
                state.load = LoadStatus::Loading;
                state.attempt = Some(1);
            })
            .await
            .unwrap();
        let persisted = io.0.lock().unwrap().storage.clone();
        io.load(Ok(persisted));
        let _ = store
            .receive_with(
                |_| true,
                |state| {
                    let state = state.as_mut().unwrap();
                    state.load = LoadStatus::Ready;
                    state.value = 83;
                    state.acknowledged = Some(0);
                    state.attempt = None;
                },
            )
            .await
            .unwrap();
        store.finish().await.unwrap();
    });
}

/// Lets the actor publish before acknowledgement receives its first poll.
///
/// The clock stays at zero. Future sleeps stay pending until their owners drop them.
/// Yields request one wake before completion. Each scenario must poll or drop its tasks.
#[derive(Clone, Default)]
struct Stepped(Arc<Mutex<VecDeque<RuntimeFuture>>>);

impl fmt::Debug for Stepped {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Stepped(..)")
    }
}

impl RuntimeDriver for Stepped {
    fn spawn(&self, task: RuntimeFuture) {
        self.0.lock().unwrap().push_back(task);
    }

    fn now(&self) -> MonotonicInstant {
        MonotonicInstant::ZERO
    }

    fn sleep_until(&self, deadline: MonotonicInstant) -> RuntimeFuture {
        if deadline <= self.now() {
            Box::pin(std::future::ready(()))
        } else {
            Box::pin(std::future::pending())
        }
    }

    fn yield_now(&self) -> RuntimeFuture {
        let mut yielded = false;
        Box::pin(std::future::poll_fn(move |cx| {
            if yielded {
                Poll::Ready(())
            } else {
                yielded = true;
                cx.waker().wake_by_ref();
                Poll::Pending
            }
        }))
    }
}

impl Stepped {
    /// Finishes the already-cancelled acknowledgement effects in these schedules.
    fn finish_effects(&self) {
        while let Some(mut effect) = self.0.lock().unwrap().pop_front() {
            assert!(
                effect
                    .as_mut()
                    .poll(&mut Context::from_waker(Waker::noop()))
                    .is_ready()
            );
        }
    }
}

/// Keeps future deadlines pending and completes elapsed deadlines on their first poll.
#[test]
fn stepped_sleep_respects_its_fixed_clock() {
    let host = Stepped::default();
    let mut cx = Context::from_waker(Waker::noop());
    assert_eq!(host.now(), MonotonicInstant::ZERO);

    for deadline in [
        MonotonicInstant::ZERO.saturating_add(std::time::Duration::from_nanos(1)),
        MonotonicInstant::MAX,
    ] {
        let mut sleep = host.sleep_until(deadline);
        assert!(sleep.as_mut().poll(&mut cx).is_pending());
        assert!(sleep.as_mut().poll(&mut cx).is_pending());
    }

    let mut elapsed = host.sleep_until(MonotonicInstant::ZERO);
    assert!(elapsed.as_mut().poll(&mut cx).is_ready());
}

/// Yields once, requests one wake, then completes without another wake.
#[test]
fn stepped_yield_defers_completion_and_wakes_once() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::Wake;

    /// Counts wake requests independently from the future's poll results.
    #[derive(Default)]
    struct WakeCount(AtomicUsize);

    impl Wake for WakeCount {
        fn wake(self: Arc<Self>) {
            self.wake_by_ref();
        }

        fn wake_by_ref(self: &Arc<Self>) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }

    let host = Stepped::default();
    let notifications = Arc::new(WakeCount::default());
    let waker = Waker::from(Arc::clone(&notifications));
    let mut cx = Context::from_waker(&waker);
    let mut yielding = host.yield_now();

    assert!(yielding.as_mut().poll(&mut cx).is_pending());
    assert_eq!(notifications.0.load(Ordering::Relaxed), 1);
    assert!(yielding.as_mut().poll(&mut cx).is_ready());
    assert_eq!(notifications.0.load(Ordering::Relaxed), 1);
}

/// Runs one actor turn and requires it to remain live.
fn poll_actor(actor: &mut RuntimeFuture) {
    assert!(
        actor
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_pending()
    );
}

/// Preserves publication while an unpolled, cancelled acknowledgement abandons its receipt.
#[test]
fn cancelling_acknowledgement_before_first_poll_abandons_after_publication() {
    /// Supplies an already-present value without introducing an external wait.
    struct Present;
    impl Backend for Present {
        type Error = SecretError;
        fn load(&self) -> impl Future<Output = Result<Option<Vec<u8>>, SecretError>> + Send {
            std::future::ready(Ok(Some(7_i64.to_le_bytes().to_vec())))
        }
        fn save(&self, _: Vec<u8>) -> impl Future<Output = Result<(), SecretError>> + Send {
            std::future::ready(Ok(()))
        }
    }
    let host = Stepped::default();
    let (state, reducer) = Document::new(7, Present, Binary);
    let store = Store::new(state, reducer, Runtime::new(host.clone()));
    let mut actor = host.0.lock().unwrap().pop_front().unwrap();
    let mut load = Box::pin(store.send(Action::load()));
    assert!(
        load.as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_pending()
    );
    poll_actor(&mut actor);
    assert!(
        load.as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_ready()
    );
    let mut loading = host.0.lock().unwrap().pop_front().unwrap();
    assert!(
        loading
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_pending()
    );
    poll_actor(&mut actor); // Commit the decoded present value.
    assert!(
        loading
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_ready()
    );
    poll_actor(&mut actor);
    let (action, receipt) = Action::close();
    let mut committed = Box::pin(store.send(action));
    assert!(
        committed
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_pending()
    );
    poll_actor(&mut actor);
    let Poll::Ready(Ok(task)) = committed
        .as_mut()
        .poll(&mut Context::from_waker(Waker::noop()))
    else {
        panic!("actor committed before effect poll");
    };
    task.cancel();
    poll_actor(&mut actor); // The actor processes the cancellation request before effect polling.
    host.finish_effects();
    assert!(matches!(block_on(receipt), Err(ReceiptError::Abandoned)));
    let mut read = Box::pin(store.read(|state| (state.acknowledged_revision(), state.is_closed())));
    assert!(
        read.as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_pending()
    );
    poll_actor(&mut actor);
    let Poll::Ready(Ok(value)) = read.as_mut().poll(&mut Context::from_waker(Waker::noop())) else {
        panic!("ordered read must complete");
    };
    assert_eq!(value, (Some(0), true));
    drop(read);
    drop(load);
    drop(committed);
    drop(store);
    drop(actor);
}

/// Keeps terminal acknowledgement attached to its cancellable root task.
#[test]
fn cancelling_terminal_acknowledgement_root_still_abandons_receipt() {
    use std::pin::Pin;

    let host = Stepped::default();
    let (state, reducer) = Document::new(7, ControlledBackend::default(), Binary);
    let store = Store::new(state, reducer, Runtime::new(host.clone()));
    let mut actor = host.0.lock().unwrap().pop_front().unwrap();
    let mut abort = Box::pin(store.send(Action::abort()));
    let mut cx = Context::from_waker(Waker::noop());
    assert!(abort.as_mut().poll(&mut cx).is_pending());
    poll_actor(&mut actor);
    assert!(matches!(abort.as_mut().poll(&mut cx), Poll::Ready(Ok(_))));
    let (action, mut receipt) = Action::close();
    let mut committed = Box::pin(store.send(action));
    assert!(committed.as_mut().poll(&mut cx).is_pending());
    poll_actor(&mut actor);
    let Poll::Ready(Ok(task)) = committed.as_mut().poll(&mut cx) else {
        panic!("terminal barrier must commit before acknowledgement polling");
    };
    assert!(Pin::new(&mut receipt).poll(&mut cx).is_pending());
    task.cancel();
    poll_actor(&mut actor);
    host.finish_effects();
    assert!(matches!(
        Pin::new(&mut receipt).poll(&mut cx),
        Poll::Ready(Err(ReceiptError::Abandoned))
    ));
    poll_actor(&mut actor); // Retire the cancelled effect's completion before checking lineage.
    assert!(task.is_finished());
    let mut read = Box::pin(store.read(|state| {
        (
            state.is_aborted(),
            state.is_closed(),
            state.acknowledged_revision(),
            state.pending_barrier_count(),
        )
    }));
    assert!(read.as_mut().poll(&mut cx).is_pending());
    poll_actor(&mut actor);
    assert!(matches!(
        read.as_mut().poll(&mut cx),
        Poll::Ready(Ok((true, false, None, 0)))
    ));
    drop(read);
    drop(abort);
    drop(committed);
    drop(store);
    drop(actor);
    assert!(host.0.lock().unwrap().is_empty());
}

/// Cancels the instance-owned save when abort resolves a pending close.
#[test]
fn abort_directly_cancels_live_save_and_resolves_pending_close() {
    block_on(async {
        let (mut store, io) = document(Binary);
        ready(&mut store, &io, Some(7)).await;
        let task = store
            .send_with(Action::set(97), |state| {
                state.value = 97;
                state.revision = 1;
                state.saving = Some(1);
                state.attempt = Some(2);
            })
            .await
            .unwrap();
        let (action, receipt) = Action::close();
        let _ = store
            .send_with(action, |state| {
                state.closing = true;
                state.barriers = 1;
            })
            .await
            .unwrap();
        let _ = store
            .send_with(Action::abort(), |state| {
                state.aborted = true;
                state.saving = None;
                state.attempt = None;
                state.barriers = 0;
            })
            .await
            .unwrap();
        assert!(matches!(receipt.await, Err(ReceiptError::Aborted)));
        assert!(task.is_finished());
        store.finish().await.unwrap();
        io.finish_external();
        io.close().await.unwrap();
    });
}

/// Resolves an accepted materialisation barrier with its original encoding failure.
#[test]
fn encoding_failure_resolves_an_accepted_materialisation_barrier() {
    block_on(async {
        let io = ControlledBackend::default();
        let (state, reducer) = Document::new(-1, io.clone(), Binary);
        let mut store = TestStore::projected(state, reducer, snapshot);
        let _ = store
            .send_with(Action::load(), |state| {
                state.load = LoadStatus::Loading;
                state.attempt = Some(1);
            })
            .await
            .unwrap();
        io.load(Ok(None));
        let _ = store
            .receive_with(
                |_| true,
                |state| {
                    state.load = LoadStatus::Ready;
                    state.attempt = None;
                },
            )
            .await
            .unwrap();
        let (action, receipt) = Action::flush();
        let _ = store
            .send_with(action, |state| {
                state.saving = Some(0);
                state.attempt = Some(2);
                state.barriers = 1;
            })
            .await
            .unwrap();
        let _ = store
            .receive_with(
                |_| true,
                |state| {
                    state.saving = None;
                    state.attempt = None;
                    state.barriers = 0;
                    state.failure = Some((Operation::Encode, Some(0), 3));
                },
            )
            .await
            .unwrap();
        let Err(ReceiptError::Persistence(failure)) = receipt.await else {
            panic!("encoding failure must resolve barrier");
        };
        assert_eq!(failure.operation(), Operation::Encode);
        assert_eq!(
            failure
                .source()
                .unwrap()
                .downcast_ref::<SecretError>()
                .unwrap()
                .0,
            3
        );
        assert_eq!(io.saves(), 0);
        store.finish().await.unwrap();
    });
}

/// Routes two document instances through one root cancellation scope.
enum PairAction {
    /// Targets the document whose lifetime the test replaces or aborts.
    Left(Action<i64>),
    /// Targets the sibling that must remain live.
    Right(Action<i64>),
}
/// Keeps independent document identities under a deliberately shared reducer scope.
struct PairDocument {
    /// Instance whose cancellation cannot affect its sibling.
    left: Document<i64, ControlledBackend, Binary>,
    /// Sibling whose pending load and save must remain valid.
    right: Document<i64, ControlledBackend, Binary>,
}
impl Reducer for PairDocument {
    type State = (State<i64>, State<i64>);
    type Action = PairAction;
    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &Self::Action,
        context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        // Deliberately share the root reducer scope: identity must be instance-private.
        match action {
            PairAction::Left(action) => self
                .left
                .reduce(&mut state.0, action, context)
                .map(PairAction::Left),
            PairAction::Right(action) => self
                .right
                .reduce(&mut state.1, action, context)
                .map(PairAction::Right),
        }
    }
}

/// Preserves sibling work when two documents share the root reducer scope.
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "two documents deliberately share a root scope while every load/replacement/abort/save transition is asserted"
)]
fn replacing_and_aborting_one_document_cannot_cancel_its_root_scope_sibling() {
    block_on(async {
        let left_io = ControlledBackend::default();
        let right_io = ControlledBackend::default();
        let (left, left_document) = Document::new(7, left_io.clone(), Binary);
        let (right, right_document) = Document::new(7, right_io.clone(), Binary);
        let mut store = TestStore::projected(
            (left, right),
            PairDocument {
                left: left_document,
                right: right_document,
            },
            |state| (snapshot(&state.0), snapshot(&state.1)),
        );
        let _ = store
            .send_with(PairAction::Left(Action::load()), |state| {
                state.0.load = LoadStatus::Loading;
                state.0.attempt = Some(1);
            })
            .await
            .unwrap();
        let _ = store
            .send_with(PairAction::Right(Action::load()), |state| {
                state.1.load = LoadStatus::Loading;
                state.1.attempt = Some(1);
            })
            .await
            .unwrap();
        let _ = store
            .send_with(PairAction::Left(Action::load()), |state| {
                state.0.attempt = Some(2);
            })
            .await
            .unwrap();
        assert!(
            left_io
                .0
                .lock()
                .unwrap()
                .loads
                .pop_front()
                .unwrap()
                .complete(Ok(None))
                .is_err()
        );
        right_io.load(Ok(Some(13_i64.to_le_bytes().to_vec())));
        let _ = store
            .receive_with(
                |action| matches!(action, PairAction::Right(_)),
                |state| {
                    state.1.load = LoadStatus::Ready;
                    state.1.value = 13;
                    state.1.acknowledged = Some(0);
                    state.1.attempt = None;
                },
            )
            .await
            .unwrap();
        left_io.load(Ok(Some(11_i64.to_le_bytes().to_vec())));
        let _ = store
            .receive_with(
                |action| matches!(action, PairAction::Left(_)),
                |state| {
                    state.0.load = LoadStatus::Ready;
                    state.0.value = 11;
                    state.0.acknowledged = Some(0);
                    state.0.attempt = None;
                },
            )
            .await
            .unwrap();
        let _ = store
            .send_with(PairAction::Left(Action::set(23)), |state| {
                state.0.value = 23;
                state.0.revision = 1;
                state.0.saving = Some(1);
                state.0.attempt = Some(3);
            })
            .await
            .unwrap();
        let _ = store
            .send_with(PairAction::Right(Action::set(37)), |state| {
                state.1.value = 37;
                state.1.revision = 1;
                state.1.saving = Some(1);
                state.1.attempt = Some(2);
            })
            .await
            .unwrap();
        let _ = store
            .send_with(PairAction::Left(Action::abort()), |state| {
                state.0.aborted = true;
                state.0.saving = None;
                state.0.attempt = None;
            })
            .await
            .unwrap();
        left_io.finish_external();
        assert_eq!(right_io.save(Ok(())), 37_i64.to_le_bytes());
        let _ = store
            .receive_with(
                |action| matches!(action, PairAction::Right(_)),
                |state| {
                    state.1.acknowledged = Some(1);
                    state.1.saving = None;
                    state.1.attempt = None;
                },
            )
            .await
            .unwrap();
        store.finish().await.unwrap();
    });
}
