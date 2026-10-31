//! Coordinates revision-based saves through controlled backend commits, failures and shutdown.

use std::sync::{Arc, Mutex};

use async_channel::{Receiver, Sender};
use futures_executor::block_on;
use rca_core::{DependencyKey, Effect, ReduceContext, Reducer};
use rca_test::TestStore;

// Acknowledging current rather than returned revision loses edits made during save.
#[test]
fn older_save_acknowledgement_does_not_clean_newer_edits() {
    block_on(async {
        let (storage, controls) = storage(Some(7));
        let mut store = store(storage.clone());
        load(&mut store, &controls, Some(7)).await;
        let root = store
            .send_with(Action::Set(8), |s| {
                s.value = 8;
                s.revision = 1;
                s.saving = Some(1);
            })
            .await
            .unwrap();
        let first = controls.try_recv().unwrap();
        assert_eq!(first.value(), Some(8));
        let _ = store
            .send_with(Action::Set(9), |s| {
                s.value = 9;
                s.revision = 2;
            })
            .await
            .unwrap();
        let _ = store
            .send_with(Action::Set(10), |s| {
                s.value = 10;
                s.revision = 3;
            })
            .await
            .unwrap();
        assert!(controls.is_empty());
        first.complete(Decision::Commit);
        let _ = store
            .receive_with(
                |a| *a == Action::Saved(1, Ok(())),
                |s| {
                    s.acknowledged = Some(1);
                    s.saving = Some(3);
                },
            )
            .await
            .unwrap();
        assert_stored_value(&storage, Some(8));
        let last = controls.try_recv().unwrap();
        assert_eq!(last.value(), Some(10));
        last.complete(Decision::Commit);
        let _ = store
            .receive_with(
                |a| *a == Action::Saved(3, Ok(())),
                |s| {
                    s.acknowledged = Some(3);
                    s.saving = None;
                },
            )
            .await
            .unwrap();
        root.finish().await.unwrap();
        let (writes, peak) = {
            let model = storage.model.lock().unwrap();
            (model.writes.clone(), model.peak)
        };
        assert_eq!(writes, [8, 10]);
        assert_eq!(peak, 1);
        store.finish().await.unwrap();
    });
}

// Missing storage must be materialised explicitly; failures must not mark it loaded.
#[test]
fn missing_load_failure_and_replaced_load_have_explicit_outcomes() {
    block_on(async {
        let (storage, controls) = storage(None);
        let mut store = store(storage.clone());
        let _ = store
            .send_with(Action::Load, |s| s.load = LoadState::Loading)
            .await
            .unwrap();
        let old = controls.try_recv().unwrap();
        let _ = store.send(Action::Load).await.unwrap();
        assert!(old.is_closed());
        controls.try_recv().unwrap().complete(Decision::FailBefore);
        let _ = store
            .receive_with(
                |a| *a == Action::Loaded(Err(Failure::BeforeCommit)),
                |s| {
                    s.load = LoadState::Unloaded;
                    s.failure = Some(Failure::BeforeCommit);
                },
            )
            .await
            .unwrap();
        let _ = store.send(Action::Set(99)).await.unwrap();
        let _ = store
            .send_with(Action::Load, |s| {
                s.load = LoadState::Loading;
                s.failure = None;
            })
            .await
            .unwrap();
        controls.try_recv().unwrap().complete(Decision::Commit);
        let _ = store
            .receive_with(
                |a| *a == Action::Loaded(Ok(None)),
                |s| {
                    s.load = LoadState::Ready;
                },
            )
            .await
            .unwrap();
        assert!(controls.is_empty());
        let _ = store
            .send_with(Action::Flush, |s| s.saving = Some(0))
            .await
            .unwrap();
        let save = controls.try_recv().unwrap();
        assert_eq!(save.value(), Some(0));
        save.complete(Decision::Commit);
        let _ = store
            .receive_with(
                |a| *a == Action::Saved(0, Ok(())),
                |s| {
                    s.saving = None;
                    s.acknowledged = Some(0);
                },
            )
            .await
            .unwrap();
        let _ = store.send(Action::Flush).await.unwrap();
        assert!(controls.is_empty());
        assert_stored_value(&storage, Some(0));
        store.finish().await.unwrap();
    });
}

// Closing before loading finishes must terminate without materialising defaults.
#[test]
fn close_before_load_completion_cancels_load_and_fences_reopening() {
    for stage in 0..3 {
        block_on(async move {
            let (storage, controls) = storage(Some(7));
            let mut store = store(storage.clone());
            let request = if stage != 0 {
                let _ = store
                    .send_with(Action::Load, |s| s.load = LoadState::Loading)
                    .await
                    .unwrap();
                let request = controls.try_recv().unwrap();
                if stage == 2 {
                    request.complete(Decision::FailBefore);
                    let _ = store
                        .receive_with(
                            |a| *a == Action::Loaded(Err(Failure::BeforeCommit)),
                            |s| {
                                s.load = LoadState::Unloaded;
                                s.failure = Some(Failure::BeforeCommit);
                            },
                        )
                        .await
                        .unwrap();
                    None
                } else {
                    Some(request)
                }
            } else {
                None
            };
            let _ = store
                .send_with(Action::Close, |s| {
                    s.load = LoadState::Unloaded;
                    s.failure = None;
                    s.closing = true;
                    s.closed = true;
                })
                .await
                .unwrap();
            if let Some(request) = request {
                assert!(request.is_closed());
            }
            let _ = store.send(Action::Load).await.unwrap();
            let _ = store.send(Action::Set(99)).await.unwrap();
            let _ = store.send(Action::Flush).await.unwrap();
            let _ = store.send(Action::Close).await.unwrap();
            assert!(controls.is_empty());
            assert_stored_value(&storage, Some(7));
            let no_writes = storage.model.lock().unwrap().writes.is_empty();
            assert!(no_writes);
            store.finish().await.unwrap();
        });
    }
}

// A failed write is not acknowledged, even if the backend already committed it.
#[test]
fn save_failures_preserve_dirty_state_until_explicit_flush() {
    for after in [false, true] {
        block_on(async move {
            let (storage, controls) = storage(Some(7));
            let mut store = store(storage.clone());
            load(&mut store, &controls, Some(7)).await;
            let _ = store
                .send_with(Action::Set(8), |s| {
                    s.value = 8;
                    s.revision = 1;
                    s.saving = Some(1);
                })
                .await
                .unwrap();
            controls.try_recv().unwrap().complete(if after {
                Decision::FailAfter
            } else {
                Decision::FailBefore
            });
            let failure = if after {
                Failure::AfterCommit
            } else {
                Failure::BeforeCommit
            };
            let _ = store
                .receive_with(
                    move |a| *a == Action::Saved(1, Err(failure)),
                    move |s| {
                        s.saving = None;
                        s.failure = Some(failure);
                    },
                )
                .await
                .unwrap();
            assert_stored_value(&storage, Some(if after { 8 } else { 7 }));
            assert!(controls.is_empty());
            let _ = store
                .send_with(Action::Set(9), |s| {
                    s.value = 9;
                    s.revision = 2;
                })
                .await
                .unwrap();
            assert!(controls.is_empty());
            let root = store
                .send_with(Action::Flush, |s| {
                    s.failure = None;
                    s.saving = Some(2);
                })
                .await
                .unwrap();
            let save = controls.try_recv().unwrap();
            assert_eq!(save.value(), Some(9));
            save.complete(Decision::Commit);
            let _ = store
                .receive_with(
                    |a| *a == Action::Saved(2, Ok(())),
                    |s| {
                        s.saving = None;
                        s.acknowledged = Some(2);
                    },
                )
                .await
                .unwrap();
            root.finish().await.unwrap();
            assert_stored_value(&storage, Some(9));
            store.finish().await.unwrap();
        });
    }
}

// Close must fence edits and wait through older snapshots and an explicit retry.
#[test]
fn close_rejects_edits_and_waits_for_final_revision() {
    block_on(async {
        let (storage, controls) = storage(Some(7));
        let mut store = store(storage.clone());
        load(&mut store, &controls, Some(7)).await;
        let root = store
            .send_with(Action::Set(8), |s| {
                s.value = 8;
                s.revision = 1;
                s.saving = Some(1);
            })
            .await
            .unwrap();
        let first = controls.try_recv().unwrap();
        let _ = store
            .send_with(Action::Set(9), |s| {
                s.value = 9;
                s.revision = 2;
            })
            .await
            .unwrap();
        let _ = store
            .send_with(Action::Close, |s| s.closing = true)
            .await
            .unwrap();
        let _ = store.send(Action::Set(99)).await.unwrap();
        first.complete(Decision::Commit);
        let _ = store
            .receive_with(
                |a| *a == Action::Saved(1, Ok(())),
                |s| {
                    s.acknowledged = Some(1);
                    s.saving = Some(2);
                },
            )
            .await
            .unwrap();
        controls.try_recv().unwrap().complete(Decision::FailBefore);
        let _ = store
            .receive_with(
                |a| *a == Action::Saved(2, Err(Failure::BeforeCommit)),
                |s| {
                    s.saving = None;
                    s.failure = Some(Failure::BeforeCommit);
                },
            )
            .await
            .unwrap();
        root.finish().await.unwrap();
        store
            .assert_state(|s| {
                assert!(s.closing);
                assert!(!s.closed);
            })
            .await
            .unwrap();
        let retry = store
            .send_with(Action::Close, |s| {
                s.failure = None;
                s.saving = Some(2);
            })
            .await
            .unwrap();
        let last = controls.try_recv().unwrap();
        assert_eq!(last.value(), Some(9));
        last.complete(Decision::Commit);
        let _ = store
            .receive_with(
                |a| *a == Action::Saved(2, Ok(())),
                |s| {
                    s.acknowledged = Some(2);
                    s.saving = None;
                    s.closed = true;
                },
            )
            .await
            .unwrap();
        let _ = store.send(Action::Set(100)).await.unwrap();
        retry.finish().await.unwrap();
        store.finish().await.unwrap();
        let mut restarted = self::store(storage.clone());
        load(&mut restarted, &controls, Some(9)).await;
        let _ = restarted
            .send_with(Action::Close, |s| {
                s.closing = true;
                s.closed = true;
            })
            .await
            .unwrap();
        assert!(controls.is_empty());
        restarted.finish().await.unwrap();
    });
}

// Abort cannot flush an uncommitted write or undo a commit awaiting acknowledgement.
#[test]
fn abortive_cancellation_and_shutdown_preserve_backend_commit_boundary() {
    for commit in [false, true] {
        block_on(async move {
            let (storage, controls) = storage(Some(7));
            let host = rca_test::DeterministicRuntime::new();
            let gate = rca_core::testing::AssertionGate::new();
            let initial = State {
                load: LoadState::Ready,
                value: 7,
                acknowledged: Some(0),
                ..State::default()
            };
            let store = rca_core::Store::builder(initial, Document, host.runtime())
                .with_dependency::<StorageKey>(storage.clone())
                .assertion_gate(gate.clone())
                .build()
                .unwrap();
            let root = host.run(store.send(Action::Set(8))).await.unwrap().unwrap();
            host.run_until_stalled().unwrap();
            let save = controls.try_recv().unwrap();
            if commit {
                save.complete(Decision::Commit);
                host.run_until_stalled().unwrap();
                assert!(gate.has_pending());
            } else {
                host.run(store.shutdown()).await.unwrap().unwrap();
                host.run_until_stalled().unwrap();
                assert!(save.is_closed());
            }
            if commit {
                // The assertion gate blocks ordered shutdown; causal cancellation bypasses it.
                root.cancel();
                host.run(store.shutdown()).await.unwrap().unwrap();
                host.run_until_stalled().unwrap();
            }
            assert!(root.is_finished());
            let active = storage.model.lock().unwrap().active;
            assert_eq!(active, 0);
            assert_stored_value(&storage, Some(if commit { 8 } else { 7 }));
            let mut restarted = self::store(storage.clone());
            load(&mut restarted, &controls, Some(if commit { 8 } else { 7 })).await;
            restarted.finish().await.unwrap();
        });
    }
}

/// A wrong backend expectation must leave the actual save guard's cleanup usable.
#[test]
fn failed_backend_assertion_does_not_poison_save_cleanup() {
    let (storage, _controls) = storage(Some(7));
    storage.model.lock().unwrap().active = 1;
    let guard = SaveGuard(Arc::clone(&storage.model));

    let mismatch = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        assert_stored_value(&storage, Some(99));
    }));
    let panic = mismatch.expect_err("the deliberately wrong value must fail");
    let message = panic
        .downcast_ref::<String>()
        .expect("the backend assertion supplies a diagnostic message");
    assert!(message.contains("left: Some(7)"), "{message}");
    assert!(message.contains("right: Some(99)"), "{message}");

    // Separate unwinds let a broken destructor fail the test without aborting it.
    let cleanup = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(guard)));
    assert!(
        cleanup.is_ok(),
        "the original mismatch must not break save cleanup"
    );
    let active = storage.model.lock().unwrap().active;
    assert_eq!(active, 0);
}

/// Copies backend persistence before comparison so pending-save cleanup keeps an unpoisoned lock.
fn assert_stored_value(storage: &Storage, expected: Option<u64>) {
    let actual = storage.model.lock().unwrap().value;
    assert_eq!(actual, expected);
}

/// Distinguishes failures before and after the controlled backend commits a write.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Failure {
    BeforeCommit,
    AfterCommit,
}

/// Separates an unread document, a pending load and an admitted editable value.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum LoadState {
    #[default]
    Unloaded,
    Loading,
    Ready,
}

/// Tracks visible edits separately from saved revisions and the close request.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct State {
    value: u64,
    revision: u64,
    acknowledged: Option<u64>,
    saving: Option<u64>,
    load: LoadState,
    failure: Option<Failure>,
    closing: bool,
    closed: bool,
}

/// Carries user requests and revision-tagged backend responses.
#[derive(Debug, Eq, PartialEq)]
enum Action {
    Load,
    Loaded(Result<Option<u64>, Failure>),
    Set(u64),
    Flush,
    Close,
    Saved(u64, Result<(), Failure>),
}

/// Replaces pending loads without cancelling an unrelated save.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum Id {
    Load,
}

/// Lets each test select the backend's commit and failure boundary.
#[derive(Clone, Copy)]
enum Decision {
    Commit,
    FailBefore,
    FailAfter,
}

/// Exposes the actual load or save request with its one controlled reply.
enum Request {
    Load(Sender<Decision>),
    Save(u64, Sender<Decision>),
}

impl Request {
    /// Reads the captured save value without consulting current document state.
    fn value(&self) -> Option<u64> {
        match self {
            Self::Load(_) => None,
            Self::Save(value, _) => Some(*value),
        }
    }

    /// Releases the pending backend operation with the selected outcome.
    fn complete(self, decision: Decision) {
        match self {
            Self::Load(reply) | Self::Save(_, reply) => reply.try_send(decision).unwrap(),
        }
    }

    /// Observes whether cancellation dropped the backend's reply receiver.
    fn is_closed(&self) -> bool {
        match self {
            Self::Load(reply) | Self::Save(_, reply) => reply.is_closed(),
        }
    }
}

/// Records stored data, write history and real concurrent saves independently of the reducer.
#[derive(Default)]
struct Model {
    value: Option<u64>,
    writes: Vec<u64>,
    active: usize,
    peak: usize,
}

/// Holds the controlled external-I/O boundary and its observable backend state.
#[derive(Clone)]
struct Storage {
    model: Arc<Mutex<Model>>,
    requests: Sender<Request>,
}

/// Releases the active-save count on completion or cancellation of the actual future.
struct SaveGuard(Arc<Mutex<Model>>);
impl Drop for SaveGuard {
    fn drop(&mut self) {
        self.0.lock().unwrap().active -= 1;
    }
}

/// Requires explicit backend injection instead of silently using live storage.
struct StorageKey;
impl DependencyKey for StorageKey {
    type Value = Storage;

    fn live_value() -> Storage {
        panic!("fixture requires injected storage")
    }
}

/// Creates one backend and a bounded stream of its controlled requests.
fn storage(value: Option<u64>) -> (Storage, Receiver<Request>) {
    let (requests, controls) = async_channel::bounded(1);
    (
        Storage {
            model: Arc::new(Mutex::new(Model {
                value,
                ..Model::default()
            })),
            requests,
        },
        controls,
    )
}

/// Coalesces edits while preserving returned revisions, retry requirements and close ordering.
struct Document;
impl Reducer for Document {
    type State = State;
    type Action = Action;

    fn reduce(
        &mut self,
        state: &mut State,
        action: &Action,
        context: &mut ReduceContext<'_>,
    ) -> Effect<Action> {
        match action {
            Action::Close if state.load != LoadState::Ready && !state.closed => {
                state.closing = true;
                state.closed = true;
                state.load = LoadState::Unloaded;
                state.failure = None;
                return Effect::cancel(Id::Load);
            }
            Action::Load if state.load != LoadState::Ready && !state.closing => {
                state.load = LoadState::Loading;
                state.failure = None;
                let storage = context.dependency::<StorageKey>().clone();
                return Effect::run(move |sender| async move {
                    let (reply, decision) = async_channel::bounded(1);
                    storage.requests.send(Request::Load(reply)).await.unwrap();
                    let result = match decision.recv().await {
                        Ok(Decision::Commit) => Ok(storage.model.lock().unwrap().value),
                        _ => Err(Failure::BeforeCommit),
                    };
                    let _ = sender.send(Action::Loaded(result)).await;
                })
                .cancellable_in_flight(Id::Load);
            }
            Action::Loaded(result) => {
                assert!(
                    state.load == LoadState::Loading,
                    "obsolete load reached reducer"
                );
                state.load = LoadState::Unloaded;
                match result {
                    Ok(value) => {
                        state.value = value.unwrap_or(0);
                        state.load = LoadState::Ready;
                        state.acknowledged = value.map(|_| 0);
                    }
                    Err(error) => state.failure = Some(*error),
                }
                return Effect::none();
            }
            Action::Set(value) if state.load == LoadState::Ready && !state.closing => {
                state.value = *value;
                state.revision = state.revision.checked_add(1).unwrap();
            }
            Action::Flush | Action::Close if state.load == LoadState::Ready && !state.closed => {
                if matches!(action, Action::Close) {
                    state.closing = true;
                }
                state.failure = None;
            }
            Action::Saved(revision, result) => {
                assert_eq!(
                    state.saving,
                    Some(*revision),
                    "unexpected save acknowledgement"
                );
                state.saving = None;
                match result {
                    Ok(()) => state.acknowledged = Some(*revision),
                    Err(error) => state.failure = Some(*error),
                }
            }
            _ => return Effect::none(),
        }
        if state.saving.is_some() || state.failure.is_some() {
            return Effect::none();
        }
        if state.acknowledged == Some(state.revision) {
            state.closed = state.closing;
            return Effect::none();
        }
        let revision = state.revision;
        let value = state.value;
        state.saving = Some(revision);
        let storage = context.dependency::<StorageKey>().clone();
        storage.save(revision, value)
    }
}

impl Storage {
    /// Commits a captured value before delivering its revision-tagged acknowledgement.
    fn save(self, revision: u64, value: u64) -> Effect<Action> {
        let storage = self;
        Effect::run(move |sender| async move {
            let guard = {
                let mut model = storage.model.lock().unwrap();
                model.active += 1;
                model.peak = model.peak.max(model.active);
                SaveGuard(Arc::clone(&storage.model))
            };
            let (reply, decision) = async_channel::bounded(1);
            storage
                .requests
                .send(Request::Save(value, reply))
                .await
                .unwrap();
            let result = match decision.recv().await {
                Ok(decision @ (Decision::Commit | Decision::FailAfter)) => {
                    let mut model = storage.model.lock().unwrap();
                    model.value = Some(value);
                    model.writes.push(value);
                    if matches!(decision, Decision::FailAfter) {
                        Err(Failure::AfterCommit)
                    } else {
                        Ok(())
                    }
                }
                _ => Err(Failure::BeforeCommit),
            };
            drop(guard);
            let _ = sender.send(Action::Saved(revision, result)).await;
        })
    }
}

/// Starts an unloaded document with explicitly injected storage.
fn store(storage: Storage) -> TestStore<State, Action> {
    TestStore::builder(State::default(), Document)
        .with_dependency::<StorageKey>(storage)
        .build()
}

/// Completes a real load and checks its independently supplied expected value.
async fn load(
    store: &mut TestStore<State, Action>,
    controls: &Receiver<Request>,
    value: Option<u64>,
) {
    let _ = store
        .send_with(Action::Load, |s| s.load = LoadState::Loading)
        .await
        .unwrap();
    controls.try_recv().unwrap().complete(Decision::Commit);
    let _ = store
        .receive_with(
            move |a| *a == Action::Loaded(Ok(value)),
            move |s| {
                s.load = LoadState::Ready;
                s.value = value.unwrap_or(0);
                s.acknowledged = value.map(|_| 0);
            },
        )
        .await
        .unwrap();
}
