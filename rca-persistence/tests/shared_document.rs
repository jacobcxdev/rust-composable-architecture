//! Shared-document contracts tested through stores, typed keys and owned sessions.
//!
//! Controlled completions separate permission to publish from physical I/O completion.
//! Literal values, revisions and recorded bytes provide independent test results.

use std::collections::VecDeque;
use std::error::Error;
use std::fmt;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

use futures_executor::block_on;
use rca_core::{
    ActionPath, CaseStatePath, Effect, KeyedAction, KeyedActionPath, KeyedState, KeyedStatePath,
    OptionalStatePath, ReduceContext, Reducer, ReducerExt, SharedKeyCache, SharedKeyIdentity,
    SharedLoadContext, SharedReaderKey, StackAction, StackId, StackState, StatePath, Store, stack,
};
use rca_persistence::{
    Backend, BackendSession, BackendSnapshot, Codec, CompareSave, ConflictDetected, Failure,
    Operation, ReceiptError, RevisionBackend, SharedDocument, SharedDocumentAction,
    SharedDocumentOwner, SharedDocumentState, SharedUpdate,
};
use rca_test::{Completion, DeterministicRuntime, controlled_future};

#[test]
fn same_session_snapshot_rejects_aborted_replacement_without_pending_load() {
    /// Routes old producer delivery through a live session after replacing its state.
    enum ParentAction {
        /// Forwards an ordinary document action.
        Document(SharedDocumentAction<u32>),
        /// Replaces the state without replacing its session, then aborts it.
        ReplaceAndAbort,
    }
    /// Keeps the same interpreter alive while replacing and aborting document state.
    struct Parent(SharedDocument<u32>);
    impl Reducer for Parent {
        type State = SharedDocumentState<u32>;
        type Action = ParentAction;

        fn reduce(
            &mut self,
            state: &mut Self::State,
            action: &Self::Action,
            context: &mut ReduceContext<'_>,
        ) -> Effect<Self::Action> {
            match action {
                ParentAction::Document(action) => self
                    .0
                    .reduce(state, action, context)
                    .map(ParentAction::Document),
                ParentAction::ReplaceAndAbort => {
                    *state = self.0.state(17);
                    self.0
                        .reduce(state, &SharedDocumentAction::abort(), context)
                        .map(ParentAction::Document)
                }
            }
        }
    }

    let runtime = DeterministicRuntime::with_step_budget(128);
    let (state, reducer) = SharedDocument::new(7, Memory::new(), Binary);
    let session = reducer.session();
    let store = Store::new(state, Parent(reducer), runtime.runtime());
    let route = store.scope(
        StatePath::new(|state: &SharedDocumentState<u32>| state, |state| state),
        ActionPath::new(
            |action: &ParentAction| match action {
                ParentAction::Document(action) => Some(action),
                ParentAction::ReplaceAndAbort => None,
            },
            ParentAction::Document,
        ),
    );
    let owner = SharedDocumentOwner::new(route, |value| *value, session);
    block_on(runtime.run(async {
        let mut endpoint = None;
        let subscription = owner
            .observe_snapshots::<StorageError, _, _, _>(|producer| {
                endpoint = Some(producer);
                (
                    SharedUpdate::Present(BackendSnapshot {
                        revision: None,
                        bytes: Some(7_u32.to_le_bytes().to_vec()),
                    }),
                    || {},
                    std::future::ready(Ok(())),
                )
            })
            .unwrap();
        runtime.run_until_stalled().unwrap();
        assert!(subscription.status().is_active());
        assert_eq!(store.read(|state| *state.value()).await.unwrap(), 7);

        let _ = store.send(ParentAction::ReplaceAndAbort).await.unwrap();
        runtime.run_until_stalled().unwrap();
        assert_eq!(
            store
                .read(|state| (*state.value(), state.document().is_aborted(), state.is_loading()))
                .await
                .unwrap(),
            (17, true, false)
        );
        endpoint
            .as_ref()
            .unwrap()
            .publish(SharedUpdate::Present(BackendSnapshot {
                revision: None,
                bytes: Some(99_u32.to_le_bytes().to_vec()),
            }))
            .unwrap();
        runtime.run_until_stalled().unwrap();
        let observed = store
            .read(|state| (*state.value(), state.document().is_aborted(), state.is_loading()))
            .await
            .unwrap();
        let status = subscription.status();
        owner.close().await.unwrap();
        store.shutdown().await.unwrap();

        assert_eq!(observed.0, 17, "an aborted document must retain its value");
        assert!(observed.1);
        assert!(
            status.is_cancelled(),
            "snapshot delivery into an aborted replacement must resolve as cancellation; state={observed:?}, status={status:?}"
        );
        assert!(!observed.2, "rejected snapshot must not leave a pending load");
    }))
    .unwrap();
}

/// A named action route whose selectors do not capture a test's local state.
type TestActionPath<Parent, Child> =
    ActionPath<Parent, Child, fn(&Parent) -> Option<&Child>, fn(Child) -> Parent>;
/// A required-state route used by the document and stack fixtures.
type TestStatePath<Parent, Child> =
    StatePath<Parent, Child, fn(&Parent) -> &Child, fn(&mut Parent) -> &mut Child>;
/// An enum-case route that can lose and regain a child at the same logical path.
type TestCasePath<Parent, Child> = CaseStatePath<
    Parent,
    Child,
    fn(&Parent) -> Option<&Child>,
    fn(&mut Parent) -> Option<&mut Child>,
>;
/// Documents whose public keys can be reused after removing their previous instances.
type DocumentRows = KeyedState<u8, SharedDocumentState<u32, u64>>;
/// Selects the whole keyed collection without hiding its generation checks.
type DocumentRowsPath = KeyedStatePath<
    DocumentRows,
    u8,
    SharedDocumentState<u32, u64>,
    fn(&DocumentRows) -> &DocumentRows,
    fn(&mut DocumentRows) -> &mut DocumentRows,
>;
/// A document action addressed to one exact keyed child instance.
type DocumentRowAction = KeyedAction<u8, SharedDocumentAction<u32, u64>>;
/// Extracts child actions while leaving collection edits to the parent reducer.
type DocumentRowActionPath = KeyedActionPath<
    KeyedDocumentAction,
    u8,
    SharedDocumentAction<u32, u64>,
    fn(&KeyedDocumentAction) -> Option<&DocumentRowAction>,
    fn(DocumentRowAction) -> KeyedDocumentAction,
>;

/// Identifies an injected storage or codec failure without matching display text.
#[derive(Debug)]
struct StorageError(u32);

/// Documents with stack identities that can be removed and reused.
type DocumentStack = StackState<SharedDocumentState<u32, u64>>;
/// A producer return type for callbacks that rejected admission must never invoke.
type UnstartedProducer = (
    SharedUpdate<()>,
    fn(),
    std::future::Ready<Result<(), StorageError>>,
);

impl fmt::Display for StorageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "storage failure {}", self.0)
    }
}

impl Error for StorageError {}

/// Encodes one little-endian integer and rejects every input length except four.
struct Binary;

impl Codec<u32> for Binary {
    type Error = StorageError;

    fn encode(&self, value: &u32) -> Result<Vec<u8>, StorageError> {
        Ok(value.to_le_bytes().to_vec())
    }

    fn decode(&self, bytes: &[u8]) -> Result<u32, StorageError> {
        bytes
            .try_into()
            .map(u32::from_le_bytes)
            .map_err(|_cause| StorageError(2))
    }
}

/// Stored bytes and independent records of admitted, unfinished I/O.
struct MemoryState {
    /// Backend identity, advanced by writes rather than document edits.
    revision: u64,
    /// Persisted bytes, separate from snapshots supplied to load completions.
    bytes: Option<Vec<u8>>,
    /// Prevents new I/O once close starts; existing queues must still drain.
    closed: bool,
    /// Counts revisioned load attempts, including attempts rejected after close.
    revisioned_load_calls: usize,
    /// Counts comparison-save attempts, including attempts rejected after close.
    revisioned_save_calls: usize,
    /// Reads awaiting a test-supplied snapshot or failure.
    loads: VecDeque<Completion<Result<BackendSnapshot<u64>, StorageError>>>,
    /// Writes awaiting an explicit comparison against the current backend revision.
    saves: VecDeque<Save>,
}

/// A pending comparison-save whose input is owned independently of document state.
struct Save {
    /// Revision that must still match when the test commits this write.
    expected: u64,
    /// Captured write bytes, unaffected by later edits.
    bytes: Vec<u8>,
    /// Delivers the result after the storage lock has been released.
    completion: Completion<Result<CompareSave<u64>, StorageError>>,
}

/// In-memory backend with immediate plain I/O and controlled revisioned I/O.
///
/// Close rejects new work and waits for both recorded queues to empty. Producer
/// joins are separate controls owned by each scenario, not by this backend.
#[derive(Clone)]
struct Memory {
    /// Shared storage and pending I/O, with no lock held across an await.
    state: Arc<Mutex<MemoryState>>,
    /// Coalesces notifications when a recorded read or write finishes.
    changed: async_channel::Sender<()>,
    /// Wakes close so it can inspect both queues again.
    notifications: async_channel::Receiver<()>,
}

impl Memory {
    /// Starts with absent bytes at revision one and no admitted work.
    fn new() -> Self {
        let (changed, notifications) = async_channel::bounded(1);
        Self {
            state: Arc::new(Mutex::new(MemoryState {
                revision: 1,
                bytes: None,
                closed: false,
                revisioned_load_calls: 0,
                revisioned_save_calls: 0,
                loads: VecDeque::new(),
                saves: VecDeque::new(),
            })),
            changed,
            notifications,
        }
    }

    /// Finishes a chosen read without changing persisted bytes or its revision.
    fn complete(&self, index: usize, result: Result<BackendSnapshot<u64>, StorageError>) {
        let completion = self.state.lock().unwrap().loads.remove(index).unwrap();
        // Superseded delivery can be dropped, but actual I/O completion remains explicit.
        let _ = completion.complete(result);
        let _ = self.changed.try_send(());
    }

    /// Records an external write without completing or notifying a document read.
    fn external_write(&self, value: u32) {
        let mut state = self.state.lock().unwrap();
        state.revision += 1;
        state.bytes = Some(value.to_le_bytes().to_vec());
    }

    /// Commits the oldest queued write and then delivers its comparison result.
    fn complete_save(&self) {
        let (completion, result) = self.commit_save();
        let _ = completion.complete(Ok(result));
        let _ = self.changed.try_send(());
    }

    /// Separates the persisted comparison result from delivery to the document.
    fn commit_save(
        &self,
    ) -> (
        Completion<Result<CompareSave<u64>, StorageError>>,
        CompareSave<u64>,
    ) {
        {
            let mut state = self.state.lock().unwrap();
            let save = state.saves.pop_front().unwrap();
            let result = if state.revision == save.expected {
                state.revision += 1;
                state.bytes = Some(save.bytes);
                CompareSave::Saved(state.revision)
            } else {
                CompareSave::Conflict(BackendSnapshot {
                    revision: state.revision,
                    bytes: state.bytes.clone(),
                })
            };
            (save.completion, result)
        }
    }
}

impl Backend for Memory {
    type Error = StorageError;

    fn load(&self) -> impl Future<Output = Result<Option<Vec<u8>>, StorageError>> + Send {
        std::future::poll_fn(move |_| {
            let state = self.state.lock().unwrap();
            if state.closed {
                return std::task::Poll::Ready(Err(StorageError(4)));
            }
            std::task::Poll::Ready(Ok(state.bytes.clone()))
        })
    }

    fn save(&self, bytes: Vec<u8>) -> impl Future<Output = Result<(), StorageError>> + Send {
        let mut bytes = Some(bytes);
        std::future::poll_fn(move |_| {
            let mut state = self.state.lock().unwrap();
            if state.closed {
                return std::task::Poll::Ready(Err(StorageError(4)));
            }
            state.revision += 1;
            state.bytes = Some(bytes.take().expect("save future polled after completion"));
            std::task::Poll::Ready(Ok(()))
        })
    }
}

impl BackendSession for Memory {
    async fn close(&self) -> Result<(), StorageError> {
        loop {
            {
                let mut state = self.state.lock().unwrap();
                state.closed = true;
                if state.loads.is_empty() && state.saves.is_empty() {
                    return Ok(());
                }
            }
            self.notifications.recv().await.unwrap();
        }
    }
}

impl RevisionBackend for Memory {
    type Revision = u64;

    async fn load_revisioned(&self) -> Result<BackendSnapshot<u64>, StorageError> {
        let (completion, future) = controlled_future();
        {
            let mut state = self.state.lock().unwrap();
            state.revisioned_load_calls += 1;
            if state.closed {
                return Err(StorageError(4));
            }
            state.loads.push_back(completion);
        };
        future.await.map_err(|_cause| StorageError(5))?
    }

    async fn compare_and_save(
        &self,
        expected: u64,
        bytes: Vec<u8>,
    ) -> Result<CompareSave<u64>, StorageError> {
        let (completion, future) = controlled_future();
        {
            let mut state = self.state.lock().unwrap();
            state.revisioned_save_calls += 1;
            if state.closed {
                return Err(StorageError(4));
            }
            state.saves.push_back(Save {
                expected,
                bytes,
                completion,
            });
        };
        future.await.map_err(|_cause| StorageError(5))?
    }
}

/// Builds literal backend evidence without reading document state.
fn present(revision: u64, value: u32) -> BackendSnapshot<u64> {
    BackendSnapshot {
        revision,
        bytes: Some(value.to_le_bytes().to_vec()),
    }
}

/// Fails encoding with a typed cause while retaining the normal decoder.
struct EncodeFailure;

impl Codec<u32> for EncodeFailure {
    type Error = StorageError;

    fn encode(&self, _: &u32) -> Result<Vec<u8>, StorageError> {
        Err(StorageError(92))
    }

    fn decode(&self, bytes: &[u8]) -> Result<u32, StorageError> {
        Binary.decode(bytes)
    }
}

#[test]
fn action_diagnostics_retain_private_edits_until_abandonment() {
    use std::cell::RefCell;
    use std::rc::Rc;

    /// Records destruction without requiring `Clone` or exposing its value through `Debug`.
    struct PrivateValue(&'static str, Rc<RefCell<Vec<&'static str>>>);
    /// Keeps action construction independent of revision `Clone` and `Debug` bounds.
    struct PrivateRevision;

    impl Drop for PrivateValue {
        fn drop(&mut self) {
            self.1.borrow_mut().push(self.0);
        }
    }

    let released = Rc::new(RefCell::new(Vec::new()));
    let (action, mut receipt) =
        SharedDocumentAction::<PrivateValue, PrivateRevision>::set_and_flush(PrivateValue(
            "sensitive-document-account-4821",
            Rc::clone(&released),
        ));
    let mut cx = Context::from_waker(Waker::noop());
    assert!(Pin::new(&mut receipt).poll(&mut cx).is_pending());

    let diagnostic = format!("{action:?}");
    assert!(diagnostic.starts_with("SharedDocumentAction"));
    assert!(!diagnostic.contains("sensitive-document-account-4821"));
    assert!(released.borrow().is_empty());
    assert!(Pin::new(&mut receipt).poll(&mut cx).is_pending());

    drop(action);
    assert_eq!(
        released.borrow().as_slice(),
        &["sensitive-document-account-4821"]
    );
    assert!(matches!(
        Pin::new(&mut receipt).poll(&mut cx),
        Poll::Ready(Err(ReceiptError::Abandoned))
    ));
}

#[test]
fn initial_plain_load_failures_preserve_seed_and_cause() {
    for provider_failure in [true, false] {
        let runtime = DeterministicRuntime::new();
        let backend = Memory::new();
        {
            let mut storage = backend.state.lock().unwrap();
            storage.closed = provider_failure;
            if !provider_failure {
                storage.bytes = Some(vec![99]);
            }
        };
        let (state, reducer) = SharedDocument::new(7, backend.clone(), Binary);
        let session = reducer.session();
        let store = Store::new(state, reducer, runtime.runtime());
        block_on(runtime.run(async {
            let _ = store.send(SharedDocumentAction::reload()).await.unwrap();
            runtime.run_until_stalled().unwrap();

            store
                .read(move |state| {
                    let failure = state.load_error().unwrap();
                    let source = failure.source().unwrap();
                    assert_eq!(
                        failure.operation(),
                        if provider_failure {
                            Operation::Load
                        } else {
                            Operation::Decode
                        }
                    );
                    assert_eq!(failure.revision(), None);
                    let cause = failure.error().downcast_ref::<StorageError>().unwrap();
                    assert_eq!(cause.0, if provider_failure { 4 } else { 2 });
                    assert!(std::ptr::eq(
                        cause,
                        source.downcast_ref::<StorageError>().unwrap()
                    ));
                    assert_eq!(*state.value(), 7);
                    assert_eq!(state.revision(), 0);
                    assert_eq!(state.acknowledged_revision(), None);
                    assert!(state.backend_revision().is_none());
                    assert!(state.conflict().is_none());
                    assert!(!state.is_loading());
                })
                .await
                .unwrap();
            assert!(backend.state.lock().unwrap().loads.is_empty());
            assert!(backend.state.lock().unwrap().saves.is_empty());
            store.shutdown().await.unwrap();
        }))
        .unwrap();
        block_on(session.close()).unwrap();
    }
}

#[test]
fn plain_save_failures_retain_unacknowledged_edit() {
    for encode_failure in [true, false] {
        let runtime = DeterministicRuntime::new();
        let backend = Memory::new();
        backend.state.lock().unwrap().bytes = Some(7_u32.to_le_bytes().to_vec());
        let (state, reducer) = if encode_failure {
            SharedDocument::new(7, backend.clone(), EncodeFailure)
        } else {
            SharedDocument::new(7, backend.clone(), Binary)
        };
        let session = reducer.session();
        let store = Store::new(state, reducer, runtime.runtime());
        block_on(runtime.run(async {
            let _ = store.send(SharedDocumentAction::reload()).await.unwrap();
            runtime.run_until_stalled().unwrap();

            assert_eq!(
                store
                    .read(SharedDocumentState::acknowledged_revision)
                    .await
                    .unwrap(),
                Some(0)
            );
            if !encode_failure {
                backend.state.lock().unwrap().closed = true;
            }
            let (action, receipt) = SharedDocumentAction::set_and_flush(8);
            let _ = store.send(action).await.unwrap();
            runtime.run_until_stalled().unwrap();

            // Keep the bounded reply unread through completion and a second drain.
            runtime.run_until_stalled().unwrap();
            runtime.run_until_stalled().unwrap();
            let error = receipt.await.unwrap_err();
            let ReceiptError::Persistence(receipt_failure) = &error else {
                panic!("expected persistence failure");
            };
            assert!(std::ptr::eq(
                receipt_failure,
                error.source().unwrap().downcast_ref::<Failure>().unwrap()
            ));
            let receipt_failure = receipt_failure.clone();
            store
                .read(move |state| {
                    let failure = state.document().failure().unwrap();
                    let source = failure.source().unwrap();
                    let receipt_source = receipt_failure.source().unwrap();
                    assert_eq!(
                        failure.operation(),
                        if encode_failure {
                            Operation::Encode
                        } else {
                            Operation::Save
                        }
                    );
                    assert_eq!(failure.revision(), Some(1));
                    let cause = failure.error().downcast_ref::<StorageError>().unwrap();
                    assert_eq!(cause.0, if encode_failure { 92 } else { 4 });
                    assert!(std::ptr::eq(
                        cause,
                        source.downcast_ref::<StorageError>().unwrap()
                    ));
                    assert_eq!(receipt_failure.operation(), failure.operation());
                    assert_eq!(receipt_failure.revision(), failure.revision());
                    assert!(std::ptr::eq(
                        cause,
                        receipt_failure
                            .error()
                            .downcast_ref::<StorageError>()
                            .unwrap()
                    ));
                    assert!(std::ptr::eq(
                        cause,
                        receipt_source.downcast_ref::<StorageError>().unwrap()
                    ));
                    assert_eq!(*state.value(), 8);
                    assert_eq!(state.revision(), 1);
                    assert_eq!(state.acknowledged_revision(), Some(0));
                    assert!(state.backend_revision().is_none());
                    assert_eq!(state.document().pending_barrier_count(), 0);
                })
                .await
                .unwrap();
            {
                let storage = backend.state.lock().unwrap();
                assert_eq!(storage.revision, 1);
                assert_eq!(storage.bytes, Some(7_u32.to_le_bytes().to_vec()));
                assert!(storage.loads.is_empty());
                assert!(storage.saves.is_empty());
            };
            store.shutdown().await.unwrap();
        }))
        .unwrap();
        block_on(session.close()).unwrap();
    }
}

#[test]
fn initial_revisioned_load_failures_preserve_seed_and_cause() {
    for provider_failure in [true, false] {
        let runtime = DeterministicRuntime::new();
        let backend = Memory::new();

        let (state, reducer) = SharedDocument::revisioned(7, backend.clone(), Binary);
        let session = reducer.session();
        let store = Store::new(state, reducer, runtime.runtime());
        block_on(runtime.run(async {
            let _ = store.send(SharedDocumentAction::reload()).await.unwrap();
            runtime.run_until_stalled().unwrap();
            assert_eq!(backend.state.lock().unwrap().loads.len(), 1);
            assert!(store.read(SharedDocumentState::is_loading).await.unwrap());
            backend.complete(
                0,
                if provider_failure {
                    Err(StorageError(91))
                } else {
                    Ok(BackendSnapshot {
                        revision: 3,
                        bytes: Some(vec![99]),
                    })
                },
            );
            runtime.run_until_stalled().unwrap();
            store
                .read(move |state| {
                    let failure = state.load_error().unwrap();
                    let source = failure.source().unwrap();
                    assert_eq!(
                        failure.operation(),
                        if provider_failure {
                            Operation::Load
                        } else {
                            Operation::Decode
                        }
                    );
                    assert_eq!(failure.revision(), None);
                    let cause = failure.error().downcast_ref::<StorageError>().unwrap();
                    assert_eq!(cause.0, if provider_failure { 91 } else { 2 });
                    assert!(std::ptr::eq(
                        cause,
                        source.downcast_ref::<StorageError>().unwrap()
                    ));
                    assert_eq!(*state.value(), 7);
                    assert_eq!(state.revision(), 0);
                    assert_eq!(state.acknowledged_revision(), None);
                    assert!(state.backend_revision().is_none());
                    assert!(state.conflict().is_none());
                    assert!(!state.is_loading());
                })
                .await
                .unwrap();
            assert!(backend.state.lock().unwrap().loads.is_empty());
            assert!(backend.state.lock().unwrap().saves.is_empty());
            store.shutdown().await.unwrap();
        }))
        .unwrap();
        block_on(session.close()).unwrap();
    }
}

#[test]
fn revisioned_save_failures_retain_unacknowledged_edit() {
    for (encode_failure, operation, code) in
        [(true, Operation::Encode, 92), (false, Operation::Save, 93)]
    {
        let runtime = DeterministicRuntime::new();
        let backend = Memory::new();
        backend.state.lock().unwrap().bytes = Some(7_u32.to_le_bytes().to_vec());
        let (state, reducer) = if encode_failure {
            SharedDocument::revisioned(7, backend.clone(), EncodeFailure)
        } else {
            SharedDocument::revisioned(7, backend.clone(), Binary)
        };
        let session = reducer.session();
        let store = Store::new(state, reducer, runtime.runtime());
        block_on(runtime.run(async {
            let context = || Context::from_waker(Waker::noop());
            let _ = store.send(SharedDocumentAction::reload()).await.unwrap();
            runtime.run_until_stalled().unwrap();
            assert_eq!(backend.state.lock().unwrap().loads.len(), 1);
            backend.complete(0, Ok(present(1, 7)));
            runtime.run_until_stalled().unwrap();
            store
                .read(|state| assert_eq!(state.acknowledged_revision(), Some(0)))
                .await
                .unwrap();

            let (action, mut receipt) = SharedDocumentAction::set_and_flush(8);
            let _ = store.send(action).await.unwrap();
            runtime.run_until_stalled().unwrap();
            if encode_failure {
                assert!(backend.state.lock().unwrap().saves.is_empty());
            } else {
                assert!(Pin::new(&mut receipt).poll(&mut context()).is_pending());
                let save = {
                    let mut storage = backend.state.lock().unwrap();
                    assert_eq!(storage.saves.len(), 1);
                    let save = storage.saves.pop_front().unwrap();
                    assert_eq!(save.expected, 1);
                    assert_eq!(save.bytes, 8_u32.to_le_bytes());
                    save
                };
                save.completion.complete(Err(StorageError(93))).unwrap();
                let _ = backend.changed.try_send(());
            }
            // Keep the bounded reply unread through completion and a second drain.
            runtime.run_until_stalled().unwrap();
            runtime.run_until_stalled().unwrap();
            let error = receipt.await.unwrap_err();
            let ReceiptError::Persistence(receipt_failure) = &error else {
                panic!("expected persistence failure");
            };
            assert!(std::ptr::eq(
                receipt_failure,
                error.source().unwrap().downcast_ref::<Failure>().unwrap()
            ));
            let receipt_failure = receipt_failure.clone();
            store
                .read(move |state| {
                    let failure = state.document().failure().unwrap();
                    let source = failure.source().unwrap();
                    let receipt_source = receipt_failure.source().unwrap();
                    assert_eq!(failure.operation(), operation);
                    assert_eq!(failure.revision(), Some(1));
                    let cause = failure.error().downcast_ref::<StorageError>().unwrap();
                    assert_eq!(cause.0, code);
                    assert!(std::ptr::eq(
                        cause,
                        source.downcast_ref::<StorageError>().unwrap()
                    ));
                    assert_eq!(receipt_failure.operation(), failure.operation());
                    assert_eq!(receipt_failure.revision(), failure.revision());
                    assert!(std::ptr::eq(
                        cause,
                        receipt_failure
                            .error()
                            .downcast_ref::<StorageError>()
                            .unwrap()
                    ));
                    assert!(std::ptr::eq(
                        cause,
                        receipt_source.downcast_ref::<StorageError>().unwrap()
                    ));
                    assert_eq!(*state.value(), 8);
                    assert_eq!(state.revision(), 1);
                    assert_eq!(state.acknowledged_revision(), Some(0));
                    assert_eq!(state.backend_revision(), Some(&1));
                    assert_eq!(state.document().pending_barrier_count(), 0);
                })
                .await
                .unwrap();
            {
                let storage = backend.state.lock().unwrap();
                assert_eq!(storage.revision, 1);
                assert_eq!(storage.bytes, Some(7_u32.to_le_bytes().to_vec()));
                assert!(storage.loads.is_empty());
                assert!(storage.saves.is_empty());
            };
            store.shutdown().await.unwrap();
        }))
        .unwrap();
        block_on(session.close()).unwrap();
    }
}

// Catches an older load clearing loading or overwriting a newer completion/error.
#[test]
fn both_completion_orders_obey_latest_started_load_authority() {
    for newest_first in [false, true] {
        let runtime = DeterministicRuntime::new();
        let backend = Memory::new();
        let (state, reducer) = SharedDocument::revisioned(7, backend.clone(), Binary);
        let store = Store::new(state, reducer, runtime.runtime());
        block_on(runtime.run(async {
            let _ = store.send(SharedDocumentAction::reload()).await.unwrap();
            runtime.run_until_stalled().unwrap();
            let _ = store.send(SharedDocumentAction::reload()).await.unwrap();
            runtime.run_until_stalled().unwrap();
            if newest_first {
                backend.complete(1, Ok(present(3, 30)));
                runtime.run_until_stalled().unwrap();
                assert_eq!(
                    store
                        .read(|state| (*state.value(), state.is_loading()))
                        .await
                        .unwrap(),
                    (30, false)
                );
                backend.complete(0, Err(StorageError(91)));
            } else {
                backend.complete(0, Ok(present(2, 20)));
                runtime.run_until_stalled().unwrap();
                assert_eq!(
                    store
                        .read(|state| (*state.value(), state.is_loading()))
                        .await
                        .unwrap(),
                    (7, true)
                );
                backend.complete(0, Ok(present(3, 30)));
            }
            runtime.run_until_stalled().unwrap();
            assert_eq!(
                store
                    .read(|state| (
                        *state.value(),
                        state.is_loading(),
                        state.load_error().is_none()
                    ))
                    .await
                    .unwrap(),
                (30, false, true)
            );
            store.shutdown().await.unwrap();
        }))
        .unwrap();
        runtime.run_until_stalled().unwrap();
        block_on(backend.close()).unwrap();
    }
}

// Catches a late success resurrecting cancelled data, or absence resetting to seed.
#[test]
fn latest_cancel_and_absence_retain_current_value_without_fabricated_persistence() {
    let runtime = DeterministicRuntime::new();
    let backend = Memory::new();
    let (state, reducer) = SharedDocument::revisioned(7, backend.clone(), Binary);
    let store = Store::new(state, reducer, runtime.runtime());
    block_on(runtime.run(async {
        let _ = store.send(SharedDocumentAction::reload()).await.unwrap();
        runtime.run_until_stalled().unwrap();
        let _ = store
            .send(SharedDocumentAction::cancel_load())
            .await
            .unwrap();
        backend.complete(0, Ok(present(2, 20)));
        runtime.run_until_stalled().unwrap();
        assert_eq!(
            store
                .read(|state| (
                    *state.value(),
                    state.is_loading(),
                    state.acknowledged_revision()
                ))
                .await
                .unwrap(),
            (7, false, None)
        );
        let _ = store.send(SharedDocumentAction::reload()).await.unwrap();
        runtime.run_until_stalled().unwrap();
        backend.complete(
            0,
            Ok(BackendSnapshot {
                revision: 3,
                bytes: None,
            }),
        );
        runtime.run_until_stalled().unwrap();
        assert_eq!(
            store
                .read(|state| (
                    *state.value(),
                    state.is_loading(),
                    state.acknowledged_revision()
                ))
                .await
                .unwrap(),
            (7, false, None)
        );
        store.shutdown().await.unwrap();
    }))
    .unwrap();
    runtime.run_until_stalled().unwrap();
    block_on(backend.close()).unwrap();
}

// Catches clearing an error only on completion instead of the newest load start.
#[test]
fn load_start_clears_prior_typed_error_but_retains_value() {
    let runtime = DeterministicRuntime::new();
    let backend = Memory::new();
    let (state, reducer) = SharedDocument::revisioned(7, backend.clone(), Binary);
    let store = Store::new(state, reducer, runtime.runtime());
    block_on(runtime.run(async {
        let _ = store.send(SharedDocumentAction::reload()).await.unwrap();
        runtime.run_until_stalled().unwrap();
        backend.complete(0, Err(StorageError(91)));
        runtime.run_until_stalled().unwrap();
        assert_eq!(
            store
                .read(|state| state
                    .load_error()
                    .unwrap()
                    .error()
                    .downcast_ref::<StorageError>()
                    .unwrap()
                    .0)
                .await
                .unwrap(),
            91
        );
        let _ = store.send(SharedDocumentAction::reload()).await.unwrap();
        runtime.run_until_stalled().unwrap();
        assert_eq!(
            store
                .read(|state| (
                    *state.value(),
                    state.is_loading(),
                    state.load_error().is_none()
                ))
                .await
                .unwrap(),
            (7, true, true)
        );
        backend.complete(0, Ok(present(2, 20)));
        runtime.run_until_stalled().unwrap();
        assert_eq!(
            store
                .read(|state| (*state.value(), state.is_loading()))
                .await
                .unwrap(),
            (20, false)
        );
        store.shutdown().await.unwrap();
    }))
    .unwrap();
    runtime.run_until_stalled().unwrap();
    block_on(backend.close()).unwrap();
}

// Unavailable barriers must reject their receipt without editing, writing or fencing later work.
#[test]
fn unloaded_and_loading_shared_document_barriers_are_rejected_without_fencing() {
    for loading in [false, true] {
        let runtime = DeterministicRuntime::new();
        let backend = Memory::new();
        let (state, reducer) = SharedDocument::revisioned(7, backend.clone(), Binary);
        let session = reducer.session();
        let store = Store::new(state, reducer, runtime.runtime());
        block_on(runtime.run(async {
            if loading {
                let _ = store.send(SharedDocumentAction::reload()).await.unwrap();
                runtime.run_until_stalled().unwrap();
            }
            for (action, receipt) in [
                SharedDocumentAction::set_and_flush(99),
                SharedDocumentAction::flush(),
                SharedDocumentAction::close(),
            ] {
                let _ = store.send(action).await.unwrap();
                runtime.run_until_stalled().unwrap();
                assert!(matches!(receipt.await, Err(ReceiptError::NotLoaded)));
                store
                    .read(move |state| {
                        assert_eq!(*state.value(), 7);
                        assert_eq!(state.revision(), 0);
                        assert_eq!(state.acknowledged_revision(), None);
                        assert_eq!(state.backend_revision(), None);
                        assert_eq!(state.is_loading(), loading);
                        assert_eq!(state.document().pending_barrier_count(), 0);
                        assert!(!state.document().is_closing());
                    })
                    .await
                    .unwrap();
                let storage = backend.state.lock().unwrap();
                assert_eq!(storage.revisioned_load_calls, usize::from(loading));
                assert_eq!(storage.loads.len(), usize::from(loading));
                assert_eq!(storage.revisioned_save_calls, 0);
                assert!(storage.saves.is_empty());
                assert_eq!(storage.revision, 1);
                assert_eq!(storage.bytes, None);
                assert!(!storage.closed);
            }

            if !loading {
                let _ = store.send(SharedDocumentAction::reload()).await.unwrap();
                runtime.run_until_stalled().unwrap();
            }
            backend.complete(0, Ok(present(1, 7)));
            runtime.run_until_stalled().unwrap();
            let (edit, receipt) = SharedDocumentAction::set_and_flush(9);
            let _ = store.send(edit).await.unwrap();
            runtime.run_until_stalled().unwrap();
            {
                let storage = backend.state.lock().unwrap();
                assert_eq!(storage.revisioned_load_calls, 1);
                assert_eq!(storage.revisioned_save_calls, 1);
                assert_eq!(storage.saves.len(), 1);
                assert_eq!(storage.saves[0].expected, 1);
                assert_eq!(storage.saves[0].bytes, 9_u32.to_le_bytes());
            };
            backend.complete_save();
            runtime.run_until_stalled().unwrap();
            let acknowledgement = receipt.await.unwrap();
            assert_eq!(
                (acknowledgement.revision, acknowledgement.closing),
                (1, false)
            );
            assert_eq!(store.read(|state| *state.value()).await.unwrap(), 9);
            {
                let storage = backend.state.lock().unwrap();
                assert_eq!(storage.revision, 2);
                assert_eq!(
                    storage.bytes.as_deref(),
                    Some(9_u32.to_le_bytes().as_slice())
                );
                assert!(storage.loads.is_empty());
                assert!(storage.saves.is_empty());
            };
            store.shutdown().await.unwrap();
        }))
        .unwrap();
        block_on(session.close()).unwrap();
        assert!(backend.state.lock().unwrap().closed);
    }
}

// Catches save1 acknowledging a later local edit or the receipt capturing a
// separate, later flush action instead of the set-and-barrier action's revision.
#[test]
fn captured_save_acknowledges_only_its_revision_and_coalesces_the_latest_edit() {
    let runtime = DeterministicRuntime::new();
    let backend = Memory::new();
    let (state, reducer) = SharedDocument::revisioned(7, backend.clone(), Binary);
    let session = reducer.session();
    let store = Store::new(state, reducer, runtime.runtime());
    block_on(runtime.run(async {
        let _ = store.send(SharedDocumentAction::reload()).await.unwrap();
        runtime.run_until_stalled().unwrap();
        backend.complete(0, Ok(present(1, 7)));
        runtime.run_until_stalled().unwrap();
        let (first, first_receipt) = SharedDocumentAction::set_and_flush(8);
        let _ = store.send(first).await.unwrap();
        runtime.run_until_stalled().unwrap();
        let (second, mut second_receipt) = SharedDocumentAction::set_and_flush(9);
        let _ = store.send(second).await.unwrap();
        runtime.run_until_stalled().unwrap();
        let (close, mut close_receipt) = SharedDocumentAction::close();
        let _ = store.send(close).await.unwrap();
        let (rejected, mut rejected_receipt) = SharedDocumentAction::set_and_flush(99);
        let _ = store.send(rejected).await.unwrap();
        runtime.run_until_stalled().unwrap();
        assert!(matches!(
            Pin::new(&mut rejected_receipt).poll(&mut Context::from_waker(Waker::noop())),
            Poll::Ready(Err(ReceiptError::Closing))
        ));
        assert!(
            store
                .read(|state| state.document().is_closing())
                .await
                .unwrap()
        );
        assert_eq!(backend.state.lock().unwrap().saves.len(), 1);
        backend.complete_save();
        runtime.run_until_stalled().unwrap();
        assert_eq!(first_receipt.await.unwrap().revision, 1);
        assert_eq!(
            store
                .read(|state| (
                    *state.value(),
                    state.revision(),
                    state.acknowledged_revision()
                ))
                .await
                .unwrap(),
            (9, 2, Some(1))
        );
        assert!(
            std::pin::Pin::new(&mut second_receipt)
                .poll(&mut std::task::Context::from_waker(std::task::Waker::noop()))
                .is_pending()
        );
        assert!(
            Pin::new(&mut close_receipt)
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending()
        );
        assert_eq!(backend.state.lock().unwrap().saves.len(), 1);
        backend.complete_save();
        runtime.run_until_stalled().unwrap();
        assert_eq!(second_receipt.await.unwrap().revision, 2);
        let Poll::Ready(Ok(outcome)) =
            Pin::new(&mut close_receipt).poll(&mut Context::from_waker(Waker::noop()))
        else {
            panic!("drained final save must resolve the close receipt");
        };
        assert_eq!((outcome.revision, outcome.closing), (2, true));
        assert_eq!(
            store
                .read(|state| (state.acknowledged_revision(), state.document().is_closed()))
                .await
                .unwrap(),
            (Some(2), true)
        );
        assert_eq!(
            backend.state.lock().unwrap().bytes,
            Some(9_u32.to_le_bytes().to_vec())
        );
        assert!(!backend.state.lock().unwrap().closed);
        store.shutdown().await.unwrap();
    }))
    .unwrap();
    runtime.run_until_stalled().unwrap();
    let mut closing = std::pin::pin!(session.close());
    assert!(matches!(
        closing
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop())),
        Poll::Ready(Ok(()))
    ));
    assert!(backend.state.lock().unwrap().closed);
}

// Catches value-equality echo suppression and ABA hiding an external revision.
#[test]
fn genuine_conflict_retains_local_value_and_exact_external_evidence() {
    for writes in [vec![8], vec![99, 7]] {
        let runtime = DeterministicRuntime::new();
        let backend = Memory::new();
        let (state, reducer) = SharedDocument::revisioned(7, backend.clone(), Binary);
        let store = Store::new(state, reducer, runtime.runtime());
        block_on(runtime.run(async {
            let _ = store.send(SharedDocumentAction::reload()).await.unwrap();
            runtime.run_until_stalled().unwrap();
            backend.complete(0, Ok(present(1, 7)));
            runtime.run_until_stalled().unwrap();
            let (action, receipt) = SharedDocumentAction::set_and_flush(8);
            let _ = store.send(action).await.unwrap();
            runtime.run_until_stalled().unwrap();
            for value in &writes {
                backend.external_write(*value);
            }
            backend.complete_save();
            runtime.run_until_stalled().unwrap();
            let ReceiptError::Persistence(failure) = receipt.await.unwrap_err() else {
                panic!("the real comparison failure retains its persistence cause");
            };
            let cause = failure.error().downcast_ref::<ConflictDetected>().unwrap();
            assert_ne!(cause.to_string(), "");
            assert!(cause.source().is_none());
            let external = *writes.last().unwrap();
            assert_eq!(
                store
                    .read(|state| (
                        *state.value(),
                        state.revision(),
                        state.acknowledged_revision()
                    ))
                    .await
                    .unwrap(),
                (8, 1, Some(0))
            );
            assert_eq!(
                store
                    .read(|state| {
                        let conflict = state.conflict().unwrap();
                        // Debug must not expose the retained bytes, value or revision.
                        assert_eq!(format!("{conflict:?}"), "SharedConflict { .. }");
                        (
                            conflict.revision().to_owned(),
                            conflict.bytes().map(<[u8]>::to_vec),
                            conflict.value().copied(),
                        )
                    })
                    .await
                    .unwrap(),
                (
                    1 + writes.len() as u64,
                    Some(external.to_le_bytes().to_vec()),
                    Some(external)
                )
            );
            assert!(backend.state.lock().unwrap().saves.is_empty());
            assert_eq!(
                backend.state.lock().unwrap().bytes,
                Some(external.to_le_bytes().to_vec())
            );
            store.shutdown().await.unwrap();
        }))
        .unwrap();
        runtime.run_until_stalled().unwrap();
        block_on(backend.close()).unwrap();
    }
}

// A real save completion may trigger a parent abort before forwarding to its child.
#[test]
fn parent_abort_before_shared_save_completion_resolves_terminal_barriers() {
    /// Identifies the save completion that the parent must intercept before forwarding.
    enum ParentAction {
        /// Forwards normal document work without intercepting its reply.
        Forward(SharedDocumentAction<u32, u64>),
        /// Starts the edit whose completion must pass through `Saved`.
        Edit(SharedDocumentAction<u32, u64>),
        /// Aborts first, then presents the original completion to the document.
        Saved(SharedDocumentAction<u32, u64>),
    }
    /// Injects parent abort immediately before reducing a genuine save completion.
    struct AbortBeforeSave(SharedDocument<u32, u64>);
    impl Reducer for AbortBeforeSave {
        type State = SharedDocumentState<u32, u64>;
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
                ParentAction::Edit(action) => self
                    .0
                    .reduce(state, action, context)
                    .map(ParentAction::Saved),
                ParentAction::Saved(action) => {
                    let abort = self
                        .0
                        .reduce(state, &SharedDocumentAction::abort(), context);
                    let completion = self.0.reduce(state, action, context);
                    Effect::merge([abort, completion]).map(ParentAction::Forward)
                }
            }
        }
    }
    let runtime = DeterministicRuntime::with_step_budget(128);
    let backend = Memory::new();
    let (state, reducer) = SharedDocument::revisioned(7, backend.clone(), Binary);
    let session = reducer.session();
    let store = Store::new(state, AbortBeforeSave(reducer), runtime.runtime());
    block_on(runtime.run(async {
        let _ = store
            .send(ParentAction::Forward(SharedDocumentAction::reload()))
            .await
            .unwrap();
        runtime.run_until_stalled().unwrap();
        backend.complete(0, Ok(present(1, 7)));
        runtime.run_until_stalled().unwrap();
        let (edit, saved) = SharedDocumentAction::set_and_flush(61);
        let task = store.send(ParentAction::Edit(edit)).await.unwrap();
        runtime.run_until_stalled().unwrap();
        let (close, closed) = SharedDocumentAction::close();
        let _ = store.send(ParentAction::Forward(close)).await.unwrap();
        assert_eq!(backend.state.lock().unwrap().saves.len(), 1);
        backend.complete_save();
        runtime.run_until_stalled().unwrap();
        assert!(matches!(saved.await, Err(ReceiptError::Aborted)));
        assert!(matches!(closed.await, Err(ReceiptError::Aborted)));
        task.finish().await;
        assert_eq!(
            store
                .read(|state| (
                    *state.value(),
                    state.revision(),
                    state.acknowledged_revision(),
                    state.document().saving_revision(),
                    state.document().is_aborted(),
                    state.document().is_closing(),
                    state.document().is_closed(),
                    state.document().pending_barrier_count(),
                    state.backend_revision().copied(),
                ))
                .await
                .unwrap(),
            (61, 1, Some(0), None, true, true, false, 0, Some(1))
        );
        let (later, rejected) = SharedDocumentAction::set_and_flush(99);
        let _ = store.send(ParentAction::Forward(later)).await.unwrap();
        assert!(matches!(rejected.await, Err(ReceiptError::Aborted)));
        assert_eq!(store.read(|state| *state.value()).await.unwrap(), 61);
        session.close().await.unwrap();
        store.shutdown().await.unwrap();
    }))
    .unwrap();
    runtime.run_until_stalled().unwrap();
    let storage = backend.state.lock().unwrap();
    assert_eq!(
        (storage.revision, storage.bytes.as_deref()),
        (2, Some(61_u32.to_le_bytes().as_slice()))
    );
    assert_eq!(
        (storage.revisioned_load_calls, storage.revisioned_save_calls),
        (1, 1)
    );
    assert!(storage.closed && storage.loads.is_empty() && storage.saves.is_empty());
}

// Catches treating an aborted delivery or a dropped close waiter as a physical
// backend join, and closing a backend other than the reducer's exact resource.
#[test]
fn session_close_retains_the_exact_backend_join_across_waiter_drop() {
    let runtime = DeterministicRuntime::new();
    let backend = Memory::new();
    let (state, reducer) = SharedDocument::revisioned(7, backend.clone(), Binary);
    let session = reducer.session();
    let store = Store::new(state, reducer, runtime.runtime());
    block_on(runtime.run(async {
        let _ = store.send(SharedDocumentAction::reload()).await.unwrap();
        runtime.run_until_stalled().unwrap();
        let _ = store.send(SharedDocumentAction::abort()).await.unwrap();
        runtime.run_until_stalled().unwrap();
        let mut closing = Box::pin(session.close());
        assert!(
            closing
                .as_mut()
                .poll(&mut std::task::Context::from_waker(std::task::Waker::noop()))
                .is_pending()
        );
        assert!(backend.state.lock().unwrap().closed);
        drop(closing);
        let mut resumed = Box::pin(session.close());
        assert!(
            resumed
                .as_mut()
                .poll(&mut std::task::Context::from_waker(std::task::Waker::noop()))
                .is_pending()
        );
        backend.complete(0, Ok(present(2, 99)));
        resumed.await.unwrap();
        runtime.run_until_stalled().unwrap();
        assert_eq!(store.read(|state| *state.value()).await.unwrap(), 7);
        store.shutdown().await.unwrap();
    }))
    .unwrap();
    runtime.run_until_stalled().unwrap();
}

// Catches retaining only one failed join, completing close before every physical
// join, or losing the original causes when close waiters and owners disappear.
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "keeps the failed-join, dropped-waiter and retained-error lifetime sequence together"
)]
fn close_retains_all_failed_joins_until_every_debt_finishes() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let runtime = DeterministicRuntime::new();
    let backend = Memory::new();
    let (state, reducer) = SharedDocument::revisioned(7, backend.clone(), Binary);
    let session = reducer.session();
    let store = Store::new(state, reducer, runtime.runtime());
    let owner = document_owner(&store, session.clone());
    let cancellations = Arc::new([AtomicUsize::new(0), AtomicUsize::new(0)]);
    let mut completions = Vec::new();
    let mut physical_owners = Vec::new();
    let mut subscriptions = Vec::new();
    for index in 0..2 {
        let (completion, join) = controlled_future();
        let physical_owner = Arc::new(());
        physical_owners.push(Arc::downgrade(&physical_owner));
        let cancelling = Arc::clone(&cancellations);
        let reentrant = session.clone();
        subscriptions.push(
            owner
                .observe_changes::<StorageError, _, _, _>(|_| {
                    (
                        SharedUpdate::Present(()),
                        move || {
                            cancelling[index].fetch_add(1, Ordering::SeqCst);
                        },
                        async move {
                            // A nested close must not claim that the debt currently
                            // being polled by the outer close has already finished.
                            let mut nested = Box::pin(reentrant.close());
                            assert!(
                                nested
                                    .as_mut()
                                    .poll(&mut Context::from_waker(Waker::noop()))
                                    .is_pending()
                            );
                            drop(nested);
                            let outcome = join.await.map_err(|_cause| StorageError(80))?;
                            drop(physical_owner);
                            outcome
                        },
                    )
                })
                .unwrap(),
        );
        completions.push(completion);
    }
    for subscription in &subscriptions {
        assert_eq!(format!("{subscription:?}"), "DocumentSubscription { .. }");
        assert!(subscription.status().is_pending());
    }
    let mut closing = Box::pin(session.close());
    assert!(
        closing
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_pending()
    );
    assert!(backend.state.lock().unwrap().closed);
    assert!(
        subscriptions
            .iter()
            .all(|guard| guard.status().is_cancelled())
    );
    assert_eq!(
        cancellations
            .each_ref()
            .map(|count| count.load(Ordering::SeqCst)),
        [1, 1]
    );
    assert!(
        physical_owners
            .iter()
            .all(|owner| owner.upgrade().is_some())
    );

    completions
        .pop()
        .unwrap()
        .complete(Err(StorageError(82)))
        .unwrap();
    assert!(
        closing
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_pending()
    );
    assert!(physical_owners[0].upgrade().is_some());
    assert!(physical_owners[1].upgrade().is_none());
    drop(closing);
    drop(subscriptions);
    drop(owner);
    let mut resumed = Box::pin(session.close());
    let mut concurrent = Box::pin(session.close());
    for closing in [&mut resumed, &mut concurrent] {
        assert!(
            closing
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending()
        );
    }
    assert!(physical_owners[0].upgrade().is_some());
    completions
        .pop()
        .unwrap()
        .complete(Err(StorageError(81)))
        .unwrap();
    let outcomes = [
        block_on(resumed),
        block_on(concurrent),
        block_on(session.close()),
    ]
    .map(Result::unwrap_err);
    assert!(
        physical_owners
            .iter()
            .all(|owner| owner.upgrade().is_none())
    );
    assert_eq!(
        cancellations
            .each_ref()
            .map(|count| count.load(Ordering::SeqCst)),
        [1, 1]
    );
    block_on(runtime.run(store.shutdown())).unwrap().unwrap();
    drop(store);
    drop(session);

    for outcome in &outcomes {
        let display = outcome.to_string();
        assert_ne!(display, "");
        assert!(!display.contains("storage failure 81"));
        assert!(!display.contains("storage failure 82"));
        assert_eq!(outcome.failures().len(), 2);
        let mut identities: Vec<_> = outcome
            .failures()
            .iter()
            .map(|failure| {
                let error = failure.error().downcast_ref::<StorageError>().unwrap();
                assert_eq!(format!("{failure:?}"), "SessionFailure { .. }");
                let display = failure.to_string();
                assert_ne!(display, "");
                assert!(!display.contains(&error.to_string()));
                assert_ne!(display, error.to_string());
                assert!(std::ptr::eq(
                    error,
                    failure
                        .source()
                        .unwrap()
                        .downcast_ref::<StorageError>()
                        .unwrap(),
                ));
                error.0
            })
            .collect();
        identities.sort_unstable();
        assert_eq!(identities, [81, 82]);
        assert!(std::ptr::eq(
            outcome
                .source()
                .unwrap()
                .downcast_ref::<rca_persistence::SessionFailure>()
                .unwrap(),
            std::ptr::from_ref(&outcome.failures()[0]),
        ));
        for failure in outcome.failures() {
            let error = failure.error().downcast_ref::<StorageError>().unwrap();
            let original = outcomes[0]
                .failures()
                .iter()
                .map(|failure| failure.error().downcast_ref::<StorageError>().unwrap())
                .find(|original| original.0 == error.0)
                .unwrap();
            assert!(std::ptr::eq(error, original));
        }
    }
}

// Catches a key retaining the strong Store driver or recreating a captured route.
#[test]
fn document_key_loses_attachment_when_its_explicit_owner_drops() {
    use rca_core::SharedKey;

    let runtime = DeterministicRuntime::new();
    let backend = Memory::new();
    let (state, reducer) = SharedDocument::revisioned(7, backend.clone(), Binary);
    let session = reducer.session();
    // Diagnostic inspection must neither reveal values nor retain hidden owners.
    for diagnostic in [
        format!("{state:?}"),
        format!("{reducer:?}"),
        format!("{session:?}"),
    ] {
        assert!(!diagnostic.contains('7'));
        assert!(diagnostic.starts_with("SharedDocument"));
    }
    assert_eq!(*state.value(), 7);
    let store = Store::new(state, reducer, runtime.runtime());
    let route = store.scope(
        StatePath::new(
            |state: &SharedDocumentState<u32, u64>| state,
            |state: &mut SharedDocumentState<u32, u64>| state,
        ),
        ActionPath::new(
            |action: &SharedDocumentAction<u32, u64>| Some(action),
            |action| action,
        ),
    );
    let owner = SharedDocumentOwner::new(route, |value: &u32| *value, session.clone());
    let key = owner.key(SharedKeyIdentity::new::<Binary>(
        "document-owner",
        "little-endian",
    ));
    for diagnostic in [format!("{owner:?}"), format!("{key:?}")] {
        assert!(!diagnostic.contains("document-owner"));
        assert!(!diagnostic.contains("little-endian"));
        assert!(!diagnostic.contains('7'));
    }
    drop(owner);
    let mut saving = Box::pin(key.save(&99));
    let Poll::Ready(Err(error)) = saving
        .as_mut()
        .poll(&mut Context::from_waker(Waker::noop()))
    else {
        panic!("save must reject the expired owner before actor admission");
    };
    assert!(error.is_ended());
    assert!(!error.is_session());
    assert_eq!(
        error.to_string().lines().next(),
        Some("document key Ended failed")
    );
    assert!(error.error().is_none());
    assert!(error.source().is_none());
    drop(saving);
    run_document_session_contract(&runtime, async {
        assert!(
            key.load(SharedLoadContext::UserInitiated)
                .await
                .unwrap_err()
                .is_ended()
        );
        assert_eq!(
            store
                .read(|state| (
                    *state.value(),
                    state.revision(),
                    state.acknowledged_revision()
                ))
                .await
                .unwrap(),
            (7, 0, None)
        );
        store.shutdown().await.unwrap();
        session.close().await.unwrap();
    });
    let storage = backend.state.lock().unwrap();
    assert!(storage.closed);
    assert!(storage.loads.is_empty() && storage.saves.is_empty());
    assert_eq!(
        (storage.revisioned_load_calls, storage.revisioned_save_calls),
        (0, 0)
    );
    assert!(storage.bytes.is_none());
}

// Removing Subscribe's current-value recapture leaves the first loaded value
// stale. Treating absence as a value would overwrite the retained value/seed.
#[test]
fn document_key_registration_recaptures_present_value_and_reports_absence_in_the_load_gap() {
    for (initial, before_registration, loaded_value, registered_value) in [
        (Some(3), Some(7), 3, 7),
        (Some(3), None, 3, 3),
        (None, None, 5, 5),
    ] {
        let runtime = DeterministicRuntime::new();
        let backend = Memory::new();
        let (state, reducer) = SharedDocument::revisioned(0, backend.clone(), Binary);
        let session = reducer.session();
        let store = Store::new(state, reducer, runtime.runtime());
        let owner = document_owner(&store, session);
        let key = owner.key(SharedKeyIdentity::new::<Binary>("handover", "binary"));
        let cache = SharedKeyCache::new();
        block_on(runtime.run(async {
            let mut constructing = Box::pin(cache.reader(key.clone(), || 5));
            assert!(
                constructing
                    .as_mut()
                    .poll(&mut Context::from_waker(Waker::noop()))
                    .is_pending()
            );
            runtime.run_until_stalled().unwrap();
            backend.complete(
                0,
                Ok(BackendSnapshot {
                    revision: 1,
                    bytes: initial.map(|value: u32| value.to_le_bytes().to_vec()),
                }),
            );
            runtime.run_until_stalled().unwrap();

            // The initial load reply is ready, but construction has not consumed
            // it or submitted Subscribe. A second actor load commits in the cut.
            let _ = store.try_send(SharedDocumentAction::reload()).unwrap();
            runtime.run_until_stalled().unwrap();
            backend.complete(
                0,
                Ok(BackendSnapshot {
                    revision: 2,
                    bytes: before_registration.map(|value: u32| value.to_le_bytes().to_vec()),
                }),
            );
            runtime.run_until_stalled().unwrap();
            assert!(owner.subscription_statuses(&key).is_empty());
            let reader = constructing.await.unwrap();
            assert_eq!(reader.read(|value| *value).unwrap(), loaded_value);
            assert!(owner.subscription_statuses(&key)[0].is_pending());

            runtime.run_until_stalled().unwrap();
            assert_eq!(reader.read(|value| *value).unwrap(), registered_value);
            let statuses = owner.subscription_statuses(&key);
            assert_eq!(statuses.len(), 1);
            assert!(statuses[0].is_active());
            assert_eq!(statuses[0].is_absent(), before_registration.is_none());

            let _ = store.try_send(SharedDocumentAction::reload()).unwrap();
            runtime.run_until_stalled().unwrap();
            backend.complete(0, Ok(present(3, 11)));
            runtime.run_until_stalled().unwrap();
            assert_eq!(reader.read(|value| *value).unwrap(), 11);
            assert!(!owner.subscription_statuses(&key)[0].is_absent());
            owner.close().await.unwrap();
            store.shutdown().await.unwrap();
        }))
        .unwrap();
    }
}

// Catches swallowing a post-admission delivery failure inside the cache's erased
// subscription guard instead of exposing it through the actual document owner.
#[test]
fn owner_observes_initial_delivery_busy_after_asynchronous_registration() {
    let runtime = DeterministicRuntime::new();
    let backend = Memory::new();
    let (state, reducer) = SharedDocument::revisioned(7, backend.clone(), Binary);
    let session = reducer.session();
    let store = Store::new(state, reducer, runtime.runtime());
    let route = store.scope(
        StatePath::new(
            |state: &SharedDocumentState<u32, u64>| state,
            |state: &mut SharedDocumentState<u32, u64>| state,
        ),
        ActionPath::new(
            |action: &SharedDocumentAction<u32, u64>| Some(action),
            |action| action,
        ),
    );
    let owner = SharedDocumentOwner::new(route, |value: &u32| *value, session);
    let key = owner.key(SharedKeyIdentity::new::<Binary>(
        "document-busy",
        "little-endian",
    ));
    let cache = SharedKeyCache::new();
    block_on(runtime.run(async {
        let mut constructing = Box::pin(cache.shared(key.clone(), || 0));
        assert!(
            constructing
                .as_mut()
                .poll(&mut std::task::Context::from_waker(std::task::Waker::noop()))
                .is_pending()
        );
        runtime.run_until_stalled().unwrap();
        backend.complete(0, Ok(present(1, 7)));
        runtime.run_until_stalled().unwrap();
        let shared = constructing.await.unwrap();
        shared
            .read(|_| runtime.run_until_stalled().unwrap())
            .unwrap();
        let statuses = owner.subscription_statuses(&key);
        assert_eq!(statuses.len(), 1);
        assert!(statuses[0].error().unwrap().is_shared_access());
        assert_eq!(shared.read(|value| *value).unwrap(), 7);
        owner.close().await.unwrap();
        store.shutdown().await.unwrap();
    }))
    .unwrap();
}

/// Drives a wholly in-memory schedule with a finite stall guard, not a time limit.
///
/// Do not use this helper for scenarios that require an external thread or I/O wake.
fn run_document_session_contract(
    runtime: &DeterministicRuntime,
    scenario: impl Future<Output = ()> + Send,
) {
    let mut driving = Box::pin(runtime.run(scenario));
    let mut context = Context::from_waker(Waker::noop());
    // These in-memory scenarios need no external wake; stalled replies must fail.
    for _ in 0..64 {
        if let Poll::Ready(result) = driving.as_mut().poll(&mut context) {
            result.unwrap();
            return;
        }
    }
    panic!("document session contract exceeded 64 client polls");
}

// Catches leaking another session's registrations or disturbing their delivery.
#[test]
fn document_owner_statuses_do_not_cross_session_boundaries() {
    use rca_core::SharedKey;

    let runtime = DeterministicRuntime::new();
    let backend_a = Memory::new();
    let backend_b = Memory::new();
    backend_b.external_write(7);
    let (state_a, reducer_a) = SharedDocument::new(3, backend_a.clone(), Binary);
    let (state_b, reducer_b) = SharedDocument::new(7, backend_b.clone(), Binary);
    let session_a = reducer_a.session();
    let session_b = reducer_b.session();
    let store_a = Store::new(state_a, reducer_a, runtime.runtime());
    let store_b = Store::new(state_b, reducer_b, runtime.runtime());
    let owner_a = document_owner(&store_a, session_a);
    let owner_b = document_owner(&store_b, session_b);
    let key = owner_b.key(SharedKeyIdentity::new::<Binary>("session-status", "binary"));
    let cache = SharedKeyCache::new();

    run_document_session_contract(&runtime, async {
        let shared = cache.shared(key.clone(), || 0).await.unwrap();
        runtime.run_until_stalled().unwrap();
        let statuses = owner_b.subscription_statuses(&key);
        assert_eq!(statuses.len(), 1);
        assert!(statuses[0].is_active());
        assert!(owner_a.subscription_statuses(&key).is_empty());
        assert_eq!(shared.read(|value| *value).unwrap(), 7);

        key.save(&9).await.unwrap();
        runtime.run_until_stalled().unwrap();
        assert_eq!(shared.read(|value| *value).unwrap(), 9);
        let statuses = owner_b.subscription_statuses(&key);
        assert_eq!(statuses.len(), 1);
        assert!(statuses[0].is_active());
        assert!(owner_a.subscription_statuses(&key).is_empty());
        assert_eq!(store_a.read(|state| *state.value()).await.unwrap(), 3);
        assert!(backend_a.state.lock().unwrap().bytes.is_none());
        assert_eq!(
            backend_b.state.lock().unwrap().bytes,
            Some(9_u32.to_le_bytes().to_vec())
        );
        owner_a.close().await.unwrap();
        owner_b.close().await.unwrap();
        store_a.shutdown().await.unwrap();
        store_b.shutdown().await.unwrap();
    });
}

// Catches deferring wrong-session rejection until a producer delivery reaches the actor.
#[test]
fn mismatched_observation_rejects_delivery_but_retains_its_physical_join() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let runtime = DeterministicRuntime::with_step_budget(64);
    let backend_a = Memory::new();
    let backend_b = Memory::new();
    let (state_a, mut reducer_a) = SharedDocument::revisioned(7, backend_a.clone(), Binary);
    let (state_b, reducer_b) = SharedDocument::revisioned(23, backend_b.clone(), Binary);
    let session_a = reducer_a.session();
    let session_b = reducer_b.session();
    let reductions = Arc::new(AtomicUsize::new(0));
    let reduction_count = Arc::clone(&reductions);
    let counted = rca_core::Reduce::new(
        move |state: &mut SharedDocumentState<u32, u64>,
              action: &SharedDocumentAction<u32, u64>,
              context| {
            reduction_count.fetch_add(1, Ordering::SeqCst);
            reducer_a.reduce(state, action, context)
        },
    );
    let store_a = Store::new(state_a, counted, runtime.runtime());
    let store_b = Store::new(state_b, reducer_b, runtime.runtime());
    let owner_a = document_owner(&store_a, session_a);
    let owner_b = document_owner(&store_b, session_b.clone());
    let mismatched = document_owner(&store_a, session_b);
    let (join_completion, join) = controlled_future();
    let cancellations = Arc::new(AtomicUsize::new(0));
    let cancelling = Arc::clone(&cancellations);
    let mut starts = 0;
    let mut endpoint = None;
    let guard = mismatched
        .observe_changes::<StorageError, _, _, _>(|producer| {
            starts += 1;
            endpoint = Some(producer);
            (
                SharedUpdate::Present(()),
                move || {
                    cancelling.fetch_add(1, Ordering::SeqCst);
                },
                async move { join.await.map_err(|_cause| StorageError(94))? },
            )
        })
        .unwrap();
    let endpoint = endpoint.unwrap();
    assert_eq!(starts, 1);
    assert!(guard.status().is_pending());
    assert_eq!(reductions.load(Ordering::SeqCst), 0);
    assert_eq!(cancellations.load(Ordering::SeqCst), 0);

    run_document_session_contract(&runtime, async {
        runtime.run_until_stalled().unwrap();
        let status = guard.status();
        let error = status.error().unwrap();
        assert!(error.is_session());
        assert!(!status.is_cancelled());
        assert_eq!(cancellations.load(Ordering::SeqCst), 1);
        // ExternalLoad also rejects mismatches; only one reduction proves admission did it.
        assert_eq!(reductions.load(Ordering::SeqCst), 1);
        assert!(endpoint.publish(SharedUpdate::Present(())).is_err());
        for (store, expected) in [(&store_a, 7), (&store_b, 23)] {
            assert_eq!(
                store
                    .read(|state| (*state.value(), state.revision()))
                    .await
                    .unwrap(),
                (expected, 0)
            );
        }
        for backend in [&backend_a, &backend_b] {
            let state = backend.state.lock().unwrap();
            assert_eq!(
                (state.revisioned_load_calls, state.revisioned_save_calls),
                (0, 0)
            );
            assert!(state.loads.is_empty() && state.saves.is_empty());
            assert!(state.bytes.is_none());
            assert!(!state.closed);
        }
        // This owner pairs A's route with B's session; only rightful owners may close.
        drop(mismatched);
        owner_a.close().await.unwrap();
        let closed = {
            let mut closing = Box::pin(owner_b.close());
            let mut context = Context::from_waker(Waker::noop());
            assert!(closing.as_mut().poll(&mut context).is_pending());
            runtime.run_until_stalled().unwrap();
            assert!(closing.as_mut().poll(&mut context).is_pending());
            assert!(backend_b.state.lock().unwrap().closed);
            join_completion.complete(Ok(())).unwrap();
            closing.as_mut().poll(&mut context)
        };
        assert!(matches!(closed, Poll::Ready(Ok(()))));
        assert!(guard.status().error().unwrap().is_session());
        drop((guard, endpoint));
        assert_eq!(cancellations.load(Ordering::SeqCst), 1);
        store_a.shutdown().await.unwrap();
        store_b.shutdown().await.unwrap();
    });
}

// Catches validating only route liveness before a key save mutates its document.
#[test]
fn mismatched_document_key_save_aborts_without_admitting_storage_work() {
    use rca_core::SharedKey;

    let runtime = DeterministicRuntime::new();
    let backend_a = Memory::new();
    let backend_b = Memory::new();
    let (state_a, reducer_a) = SharedDocument::revisioned(7, backend_a.clone(), Binary);
    let (state_b, reducer_b) = SharedDocument::revisioned(23, backend_b.clone(), Binary);
    let session_a = reducer_a.session();
    let session_b = reducer_b.session();
    let store_a = Store::new(state_a, reducer_a, runtime.runtime());
    let store_b = Store::new(state_b, reducer_b, runtime.runtime());
    let owner_a = document_owner(&store_a, session_a);
    let owner_b = document_owner(&store_b, session_b.clone());
    let mismatched = document_owner(&store_a, session_b);
    let wrong_key = mismatched.key(SharedKeyIdentity::new::<Binary>("wrong-save", "binary"));
    let right_key = owner_a.key(SharedKeyIdentity::new::<Binary>("right-save", "binary"));

    run_document_session_contract(&runtime, async {
        let _ = store_a.send(SharedDocumentAction::reload()).await.unwrap();
        runtime.run_until_stalled().unwrap();
        backend_a.complete(0, Ok(present(1, 7)));
        runtime.run_until_stalled().unwrap();
        let mut rejected = Box::pin(wrong_key.save(&99));
        assert!(
            rejected
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending()
        );
        runtime.run_until_stalled().unwrap();
        let Poll::Ready(Err(error)) = rejected
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
        else {
            panic!("mismatched save must settle without storage work");
        };
        drop(rejected);
        assert!(matches!(
            error.error().unwrap().downcast_ref::<ReceiptError>(),
            Some(ReceiptError::Aborted)
        ));
        for (store, expected, acknowledged) in [(&store_a, 7, Some(0)), (&store_b, 23, None)] {
            assert_eq!(
                store
                    .read(|state| (
                        *state.value(),
                        state.revision(),
                        state.acknowledged_revision(),
                        state.document().pending_barrier_count()
                    ))
                    .await
                    .unwrap(),
                (expected, 0, acknowledged, 0)
            );
        }
        for (backend, loads) in [(&backend_a, 1), (&backend_b, 0)] {
            let state = backend.state.lock().unwrap();
            assert_eq!(
                (state.revisioned_load_calls, state.revisioned_save_calls),
                (loads, 0)
            );
            assert!(state.bytes.is_none());
        }
        // Closing this mismatched owner would fence B while aborting A.
        drop(mismatched);

        let mut save = Box::pin(right_key.save(&11));
        assert!(
            save.as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending()
        );
        runtime.run_until_stalled().unwrap();
        backend_a.complete_save();
        runtime.run_until_stalled().unwrap();
        assert!(matches!(
            save.as_mut().poll(&mut Context::from_waker(Waker::noop())),
            Poll::Ready(Ok(()))
        ));
        drop(save);
        assert_eq!(
            store_a
                .read(|state| (
                    *state.value(),
                    state.revision(),
                    state.acknowledged_revision()
                ))
                .await
                .unwrap(),
            (11, 1, Some(1))
        );
        assert_eq!(
            backend_a.state.lock().unwrap().bytes,
            Some(11_u32.to_le_bytes().to_vec())
        );
        assert_eq!(backend_b.state.lock().unwrap().revisioned_save_calls, 0);
        owner_a.close().await.unwrap();
        owner_b.close().await.unwrap();
        store_a.shutdown().await.unwrap();
        store_b.shutdown().await.unwrap();
    });
}

// Catches erasing a typed poisoned-reference failure after registration already
// succeeded; the intended owner must still expose the terminal delivery error.
#[test]
fn owner_observes_poisoned_delivery_after_active_registration() {
    let runtime = DeterministicRuntime::new();
    let backend = Memory::new();
    let (state, reducer) = SharedDocument::revisioned(7, backend.clone(), Binary);
    let session = reducer.session();
    let store = Store::new(state, reducer, runtime.runtime());
    let owner = document_owner(&store, session);
    let key = owner.key(SharedKeyIdentity::new::<Binary>(
        "poisoned-delivery",
        "binary",
    ));
    let cache = SharedKeyCache::new();
    block_on(runtime.run(async {
        let mut constructing = Box::pin(cache.shared(key.clone(), || 0));
        assert!(
            constructing
                .as_mut()
                .poll(&mut std::task::Context::from_waker(std::task::Waker::noop()))
                .is_pending()
        );
        runtime.run_until_stalled().unwrap();
        backend.complete(0, Ok(present(1, 7)));
        runtime.run_until_stalled().unwrap();
        let shared = constructing.await.unwrap();
        runtime.run_until_stalled().unwrap();
        assert!(owner.subscription_statuses(&key)[0].is_active());
        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = shared.read(|_| panic!("poison this shared reference"));
        }))
        .unwrap_err();
        assert_eq!(
            panic.downcast_ref::<&str>(),
            Some(&"poison this shared reference")
        );
        let (edit, receipt) = SharedDocumentAction::set_and_flush(8);
        let _ = store.send(edit).await.unwrap();
        runtime.run_until_stalled().unwrap();
        let statuses = owner.subscription_statuses(&key);
        assert_eq!(statuses.len(), 1);
        let error = statuses[0].error().unwrap();
        assert!(error.is_shared_access());
        assert!(
            error
                .error()
                .unwrap()
                .downcast_ref::<rca_core::SharedAccessError>()
                .unwrap()
                .is_poisoned()
        );
        assert!(shared.read(|value| *value).unwrap_err().is_poisoned());
        assert_eq!(store.read(|state| *state.value()).await.unwrap(), 8);
        backend.complete_save();
        receipt.await.unwrap();
        owner.close().await.unwrap();
        store.shutdown().await.unwrap();
    }))
    .unwrap();
}

// Catches returning old backend bytes to the cache while the document itself
// correctly retains a pending local edit. Backend identity, not equality, rules.
#[test]
fn pending_edit_same_revision_reload_returns_local_snapshot_without_acknowledging_it() {
    let runtime = DeterministicRuntime::new();
    let backend = Memory::new();
    let (state, reducer) = SharedDocument::revisioned(7, backend.clone(), Binary);
    let session = reducer.session();
    let store = Store::new(state, reducer, runtime.runtime());
    let owner = SharedDocumentOwner::new(
        store.scope(
            StatePath::new(|state: &SharedDocumentState<u32, u64>| state, |state| state),
            ActionPath::new(
                |action: &SharedDocumentAction<u32, u64>| Some(action),
                |action| action,
            ),
        ),
        |value: &u32| *value,
        session,
    );
    let key = owner.key(SharedKeyIdentity::new::<Binary>("pending-local", "binary"));
    block_on(runtime.run(async {
        let _ = store.send(SharedDocumentAction::reload()).await.unwrap();
        runtime.run_until_stalled().unwrap();
        backend.complete(0, Ok(present(1, 7)));
        runtime.run_until_stalled().unwrap();
        let (edit, receipt) = SharedDocumentAction::set_and_flush(8);
        let _ = store.send(edit).await.unwrap();
        runtime.run_until_stalled().unwrap();
        let mut load = Box::pin(key.load(SharedLoadContext::UserInitiated));
        assert!(
            load.as_mut()
                .poll(&mut std::task::Context::from_waker(std::task::Waker::noop()))
                .is_pending()
        );
        runtime.run_until_stalled().unwrap();
        backend.complete(0, Ok(present(1, 7)));
        runtime.run_until_stalled().unwrap();
        assert_eq!(load.await.unwrap(), Some(8));
        assert_eq!(
            store
                .read(|state| (*state.value(), state.acknowledged_revision()))
                .await
                .unwrap(),
            (8, Some(0))
        );
        backend.complete_save();
        receipt.await.unwrap();
        owner.close().await.unwrap();
        store.shutdown().await.unwrap();
    }))
    .unwrap();
}

// Catches losing the external revision and raw bytes when decoding fails before
// the actor can preserve conflict evidence against a pending local revision.
#[test]
fn malformed_external_revision_retains_raw_conflict_and_typed_decode_failure() {
    let runtime = DeterministicRuntime::new();
    let backend = Memory::new();
    let (state, reducer) = SharedDocument::revisioned(7, backend.clone(), Binary);
    let session = reducer.session();
    let store = Store::new(state, reducer, runtime.runtime());
    block_on(runtime.run(async {
        let _ = store.send(SharedDocumentAction::reload()).await.unwrap();
        runtime.run_until_stalled().unwrap();
        backend.complete(0, Ok(present(1, 7)));
        runtime.run_until_stalled().unwrap();
        let (edit, receipt) = SharedDocumentAction::set_and_flush(8);
        let _ = store.send(edit).await.unwrap();
        runtime.run_until_stalled().unwrap();
        let _ = store.send(SharedDocumentAction::reload()).await.unwrap();
        runtime.run_until_stalled().unwrap();
        backend.complete(
            0,
            Ok(BackendSnapshot {
                revision: 3,
                bytes: Some(vec![99]),
            }),
        );
        runtime.run_until_stalled().unwrap();
        assert_eq!(
            store
                .read(|state| {
                    let conflict = state.conflict().unwrap();
                    (
                        *state.value(),
                        *conflict.revision(),
                        conflict.bytes().unwrap().to_vec(),
                        conflict
                            .decode_error()
                            .unwrap()
                            .error()
                            .downcast_ref::<StorageError>()
                            .unwrap()
                            .0,
                    )
                })
                .await
                .unwrap(),
            (8, 3, vec![99], 2)
        );
        let _ = store.send(SharedDocumentAction::abort()).await.unwrap();
        assert!(matches!(receipt.await, Err(ReceiptError::Aborted)));
        backend.complete_save();
        store.shutdown().await.unwrap();
    }))
    .unwrap();
    block_on(session.close()).unwrap();
}

// Catches treating a superseded producer load as terminal, accepting its stale
// completion, or forgetting physical work when the producer later drains.
#[test]
fn superseded_producer_load_continues_observing_and_retains_physical_join() {
    let runtime = DeterministicRuntime::new();
    let backend = Memory::new();
    let (state, reducer) = SharedDocument::revisioned(7, backend.clone(), Binary);
    let session = reducer.session();
    let store = Store::new(state, reducer, runtime.runtime());
    let owner = document_owner(&store, session.clone());
    let (join_completion, join) = controlled_future();
    let cancellations = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let cancelling = Arc::clone(&cancellations);
    let mut endpoint = None;
    let subscription = owner
        .observe_changes::<StorageError, _, _, _>(|producer| {
            endpoint = Some(producer);
            (
                SharedUpdate::Present(()),
                move || {
                    cancelling.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                },
                async move { join.await.map_err(|_cause| StorageError(71))? },
            )
        })
        .unwrap();
    let endpoint = endpoint.unwrap();
    block_on(runtime.run(async {
        runtime.run_until_stalled().unwrap();
        assert_eq!(backend.state.lock().unwrap().loads.len(), 1);
        assert!(subscription.status().is_active());

        // The explicit reload replaces the producer's pending reply with Superseded.
        let _ = store.send(SharedDocumentAction::reload()).await.unwrap();
        runtime.run_until_stalled().unwrap();
        assert_eq!(backend.state.lock().unwrap().loads.len(), 2);
        backend.complete(1, Ok(present(2, 10)));
        runtime.run_until_stalled().unwrap();
        assert_eq!(store.read(|state| *state.value()).await.unwrap(), 10);
        assert!(subscription.status().is_active());
        assert!(subscription.status().error().is_none());

        endpoint.publish(SharedUpdate::Present(())).unwrap();
        runtime.run_until_stalled().unwrap();
        assert_eq!(backend.state.lock().unwrap().loads.len(), 2);
        backend.complete(1, Ok(present(3, 11)));
        runtime.run_until_stalled().unwrap();
        assert_eq!(
            store
                .read(|state| (*state.value(), state.backend_revision().copied()))
                .await
                .unwrap(),
            (11, Some(3))
        );
        assert!(subscription.status().is_active());
        assert!(subscription.status().error().is_none());
        assert_eq!(cancellations.load(std::sync::atomic::Ordering::SeqCst), 0);

        // While the session is open, even a newer revision cannot authorise the old attempt.
        assert_eq!(backend.state.lock().unwrap().loads.len(), 1);
        backend.complete(0, Ok(present(99, 99)));
        runtime.run_until_stalled().unwrap();
        assert_eq!(
            store
                .read(|state| (*state.value(), state.backend_revision().copied()))
                .await
                .unwrap(),
            (11, Some(3))
        );
        assert!(subscription.status().is_active());
        assert!(subscription.status().error().is_none());

        assert!(endpoint.finish());
        runtime.run_until_stalled().unwrap();
        assert!(!subscription.status().is_active());
        assert!(!subscription.status().is_cancelled());
        assert!(subscription.status().error().is_none());
        assert_eq!(cancellations.load(std::sync::atomic::Ordering::SeqCst), 1);
        let mut closing = Box::pin(owner.close());
        assert!(
            closing
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending()
        );
        runtime.run_until_stalled().unwrap();
        assert!(
            closing
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending()
        );
        assert!(backend.state.lock().unwrap().loads.is_empty());
        join_completion.complete(Ok(())).unwrap();
        closing.await.unwrap();
        runtime.run_until_stalled().unwrap();
        assert_eq!(cancellations.load(std::sync::atomic::Ordering::SeqCst), 1);
        owner.close().await.unwrap();
        store.shutdown().await.unwrap();
    }))
    .unwrap();
    let storage = backend.state.lock().unwrap();
    assert!(storage.closed);
    assert!(storage.loads.is_empty());
    assert!(storage.saves.is_empty());
    assert_eq!(
        (storage.revisioned_load_calls, storage.revisioned_save_calls),
        (3, 0)
    );
}

// Catches reporting producer termination before its bounded latest observation
// commits, swallowing the typed terminal cause, or losing join debt with a waiter.
#[test]
fn external_producer_drains_latest_then_reports_failure_and_retains_physical_join() {
    let runtime = DeterministicRuntime::new();
    let backend = Memory::new();
    let (state, reducer) = SharedDocument::revisioned(7, backend.clone(), Binary);
    let session = reducer.session();
    let store = Store::new(state, reducer, runtime.runtime());
    let owner = document_owner(&store, session.clone());
    let (join_completion, join) = controlled_future();
    let cancellations = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let cancelling = Arc::clone(&cancellations);
    let mut endpoint = None;
    let subscription = owner
        .observe_changes(|producer| {
            endpoint = Some(producer);
            (
                SharedUpdate::Present(()),
                move || {
                    cancelling.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                },
                async move { join.await.map_err(|_cause| StorageError(71))? },
            )
        })
        .unwrap();
    let endpoint = endpoint.unwrap();
    block_on(runtime.run(async {
        runtime.run_until_stalled().unwrap();
        assert_eq!(backend.state.lock().unwrap().loads.len(), 1);
        assert!(subscription.status().is_active());
        for _ in 0..1000 {
            endpoint.publish(SharedUpdate::Present(())).unwrap();
        }
        assert!(endpoint.fail(StorageError(72)));
        backend.complete(0, Ok(present(1, 10)));
        runtime.run_until_stalled().unwrap();
        assert_eq!(backend.state.lock().unwrap().loads.len(), 1);
        assert!(subscription.status().error().is_none());
        backend.complete(0, Ok(present(2, 11)));
        runtime.run_until_stalled().unwrap();
        assert_eq!(store.read(|state| *state.value()).await.unwrap(), 11);
        assert_eq!(
            subscription
                .status()
                .error()
                .unwrap()
                .error()
                .unwrap()
                .downcast_ref::<StorageError>()
                .unwrap()
                .0,
            72
        );
        assert_eq!(cancellations.load(std::sync::atomic::Ordering::SeqCst), 1);
        let mut closing = Box::pin(owner.close());
        assert!(
            closing
                .as_mut()
                .poll(&mut std::task::Context::from_waker(std::task::Waker::noop()))
                .is_pending()
        );
        runtime.run_until_stalled().unwrap();
        assert!(
            closing
                .as_mut()
                .poll(&mut std::task::Context::from_waker(std::task::Waker::noop()))
                .is_pending()
        );
        drop(closing);
        let mut resumed = Box::pin(session.close());
        assert!(
            resumed
                .as_mut()
                .poll(&mut std::task::Context::from_waker(std::task::Waker::noop()))
                .is_pending()
        );
        join_completion.complete(Err(StorageError(73))).unwrap();
        assert_eq!(
            resumed.await.unwrap_err().failures()[0]
                .error()
                .downcast_ref::<StorageError>()
                .unwrap()
                .0,
            73
        );
        assert!(
            owner
                .observe_changes::<StorageError, _, _, _>(|_| -> UnstartedProducer {
                    panic!("closed session must not start work")
                })
                .unwrap_err()
                .is_session()
        );
        store.shutdown().await.unwrap();
    }))
    .unwrap();
}

/// Separates child delivery from parent-controlled removal and replacement.
enum OptionalDocumentAction {
    /// Routes work to the current child instance.
    Child(SharedDocumentAction<u32, u64>),
    /// Ends the current child instance.
    Remove,
    /// Installs the explicitly prepared replacement, not the removed instance.
    Reinsert,
}

/// Replaces optional child state while the composed child interpreter stays alive.
struct OptionalDocumentParent {
    /// One replacement state whose separate resource remains owned by the scenario.
    replacement: Option<SharedDocumentState<u32, u64>>,
}

impl Reducer for OptionalDocumentParent {
    type State = Option<SharedDocumentState<u32, u64>>;
    type Action = OptionalDocumentAction;

    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &Self::Action,
        _: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        match action {
            OptionalDocumentAction::Remove => *state = None,
            OptionalDocumentAction::Reinsert => {
                *state = Some(self.replacement.take().expect("one explicit replacement"));
            }
            OptionalDocumentAction::Child(_) => {}
        }
        Effect::none()
    }
}

/// Routes child actions without turning removal or replacement into child work.
fn optional_document_action()
-> TestActionPath<OptionalDocumentAction, SharedDocumentAction<u32, u64>> {
    ActionPath::new(
        |action: &OptionalDocumentAction| match action {
            OptionalDocumentAction::Child(action) => Some(action),
            OptionalDocumentAction::Remove | OptionalDocumentAction::Reinsert => None,
        },
        OptionalDocumentAction::Child,
    )
}

// Catches both same-logical-child ABA admission and an already started physical
// load completing after removal. The retained owner never renews generation.
#[test]
fn optional_document_removal_reinsert_rejects_old_producer_and_old_key() {
    let runtime = DeterministicRuntime::new();
    let backend = Memory::new();
    let (state, reducer) = SharedDocument::revisioned(7, backend.clone(), Binary);
    let session = reducer.session();
    let fresh_backend = Memory::new();
    let (replacement, fresh_resource) =
        SharedDocument::revisioned(40, fresh_backend.clone(), Binary);
    let fresh_session = fresh_resource.session();
    let reducer = OptionalDocumentParent {
        replacement: Some(replacement),
    }
    .if_some(
        OptionalStatePath::new(Option::as_ref, Option::as_mut),
        optional_document_action(),
        reducer,
    );
    let store = Store::new(Some(state), reducer, runtime.runtime());
    block_on(runtime.run(async {
        let route = store
            .scope_some(
                OptionalStatePath::new(Option::as_ref, Option::as_mut),
                optional_document_action(),
            )
            .await
            .unwrap();
        let owner = SharedDocumentOwner::new(route, |value: &u32| *value, session.clone());
        let key = owner.key(SharedKeyIdentity::new::<Binary>("optional-old", "binary"));
        let original = cached_value(&runtime, &backend, &owner, "optional-reader", 7).await;
        let mut endpoint = None;
        let subscription = owner
            .observe_changes::<StorageError, _, _, _>(|producer| {
                endpoint = Some(producer);
                (SharedUpdate::Present(()), || {}, std::future::ready(Ok(())))
            })
            .unwrap();
        runtime.run_until_stalled().unwrap();
        assert_eq!(backend.state.lock().unwrap().loads.len(), 1);
        let _ = store.send(OptionalDocumentAction::Remove).await.unwrap();
        let _ = store.send(OptionalDocumentAction::Reinsert).await.unwrap();
        runtime.run_until_stalled().unwrap();
        backend.complete(0, Ok(present(3, 99)));
        runtime.run_until_stalled().unwrap();
        assert!(
            endpoint
                .unwrap()
                .publish(SharedUpdate::Present(()))
                .is_err()
        );
        assert!(subscription.status().error().unwrap().is_ended());
        assert!(
            key.load(SharedLoadContext::UserInitiated)
                .await
                .unwrap_err()
                .is_ended()
        );
        assert_eq!(
            store
                .read(|state| *state.as_ref().unwrap().value())
                .await
                .unwrap(),
            40
        );
        // Registration after retirement must expose either immediate admission
        // failure or the later actor-side rejection through its own typed guard.
        match owner.observe_changes::<StorageError, _, _, _>(|_| {
            (SharedUpdate::Present(()), || {}, std::future::ready(Ok(())))
        }) {
            Err(error) => assert!(error.is_ended()),
            Ok(late) => {
                runtime.run_until_stalled().unwrap();
                assert!(late.status().error().unwrap().is_ended());
            }
        }
        assert!(backend.state.lock().unwrap().loads.is_empty());
        owner.close().await.unwrap();
        let fresh = SharedDocumentOwner::new(
            store
                .scope_some(
                    OptionalStatePath::new(Option::as_ref, Option::as_mut),
                    optional_document_action(),
                )
                .await
                .unwrap(),
            |value: &u32| *value,
            fresh_session,
        );
        let fresh_reader =
            fresh_load_and_save(&runtime, &fresh_backend, &fresh, "optional-fresh").await;
        assert_eq!(original.read(|value| *value).unwrap(), 7);
        assert_eq!(fresh_reader.read(|value| *value).unwrap(), 42);
        fresh.close().await.unwrap();
        store.shutdown().await.unwrap();
    }))
    .unwrap();
}

// Catches a cache load reply held by its caller overwriting a more recent actor
// subscription delivery. Both observations use the real typed Document bridge.
#[test]
fn later_actor_publication_supersedes_an_unconsumed_cache_reload_reply() {
    let runtime = DeterministicRuntime::new();
    let backend = Memory::new();
    let (state, reducer) = SharedDocument::revisioned(7, backend.clone(), Binary);
    let session = reducer.session();
    let store = Store::new(state, reducer, runtime.runtime());
    let owner = SharedDocumentOwner::new(
        store.scope(
            StatePath::new(|state: &SharedDocumentState<u32, u64>| state, |state| state),
            ActionPath::new(
                |action: &SharedDocumentAction<u32, u64>| Some(action),
                |action| action,
            ),
        ),
        |value: &u32| *value,
        session,
    );
    let key = owner.key(SharedKeyIdentity::new::<Binary>(
        "cache-commit-order",
        "binary",
    ));
    let cache = SharedKeyCache::new();
    block_on(runtime.run(async {
        let mut initial = Box::pin(cache.shared(key.clone(), || 0));
        assert!(
            initial
                .as_mut()
                .poll(&mut std::task::Context::from_waker(std::task::Waker::noop()))
                .is_pending()
        );
        runtime.run_until_stalled().unwrap();
        backend.complete(0, Ok(present(1, 7)));
        runtime.run_until_stalled().unwrap();
        let shared = initial.await.unwrap();
        runtime.run_until_stalled().unwrap();
        let mut old = Box::pin(cache.reload(&shared, &key));
        assert!(
            old.as_mut()
                .poll(&mut std::task::Context::from_waker(std::task::Waker::noop()))
                .is_pending()
        );
        runtime.run_until_stalled().unwrap();
        backend.complete(0, Ok(present(1, 7)));
        runtime.run_until_stalled().unwrap();
        // Keep the completed old reply unconsumed while a newer actor load
        // and its actual cache subscription publication both commit.
        let _ = store.send(SharedDocumentAction::reload()).await.unwrap();
        runtime.run_until_stalled().unwrap();
        backend.complete(0, Ok(present(2, 8)));
        runtime.run_until_stalled().unwrap();
        assert_eq!(shared.read(|value| *value).unwrap(), 8);
        old.await.unwrap();
        assert_eq!(shared.read(|value| *value).unwrap(), 8);
        assert_eq!(store.read(|state| *state.value()).await.unwrap(), 8);
        owner.close().await.unwrap();
        store.shutdown().await.unwrap();
    }))
    .unwrap();
}

/// Allows the same enum case to return with a different document instance.
enum CaseDocumentState {
    /// No document is mounted at this case path.
    Off,
    /// Boxes the current document to keep the enum's inline representation small.
    On(Box<SharedDocumentState<u32, u64>>),
}

/// Re-enters the document case with explicit replacement state.
struct CaseDocumentParent {
    /// Transferred once when the scenario re-enters the active case.
    replacement: Option<SharedDocumentState<u32, u64>>,
}

impl Reducer for CaseDocumentParent {
    type State = CaseDocumentState;
    type Action = OptionalDocumentAction;
    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &Self::Action,
        _: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        match action {
            OptionalDocumentAction::Remove => *state = CaseDocumentState::Off,
            OptionalDocumentAction::Reinsert => {
                *state = CaseDocumentState::On(Box::new(
                    self.replacement.take().expect("one explicit replacement"),
                ));
            }
            OptionalDocumentAction::Child(_) => {}
        }
        Effect::none()
    }
}

/// Selects only the active enum case, retaining the composed route's lifetime checks.
fn case_document_state() -> TestCasePath<CaseDocumentState, SharedDocumentState<u32, u64>> {
    CaseStatePath::new(
        |state: &CaseDocumentState| match state {
            CaseDocumentState::On(state) => Some(state),
            CaseDocumentState::Off => None,
        },
        |state: &mut CaseDocumentState| match state {
            CaseDocumentState::On(state) => Some(state),
            CaseDocumentState::Off => None,
        },
    )
}

// Catches a retained Document attachment retargeting the same enum case after ABA.
#[test]
fn case_document_reentry_rejects_old_load_and_preserves_replacement() {
    let runtime = DeterministicRuntime::new();
    let backend = Memory::new();
    let (state, reducer) = SharedDocument::revisioned(7, backend.clone(), Binary);
    let session = reducer.session();
    let fresh_backend = Memory::new();
    let (replacement, fresh_resource) =
        SharedDocument::revisioned(40, fresh_backend.clone(), Binary);
    let fresh_session = fresh_resource.session();
    let store = Store::new(
        CaseDocumentState::On(Box::new(state)),
        CaseDocumentParent {
            replacement: Some(replacement),
        }
        .if_case(case_document_state(), optional_document_action(), reducer),
        runtime.runtime(),
    );
    block_on(runtime.run(async {
        let route = store
            .scope_case(case_document_state(), optional_document_action())
            .await
            .unwrap();
        let owner = SharedDocumentOwner::new(route, |value: &u32| *value, session.clone());
        let key = owner.key(SharedKeyIdentity::new::<Binary>("case-old", "binary"));
        let original = cached_value(&runtime, &backend, &owner, "case-reader", 7).await;
        let mut load = Box::pin(key.load(SharedLoadContext::UserInitiated));
        assert!(
            load.as_mut()
                .poll(&mut std::task::Context::from_waker(std::task::Waker::noop()))
                .is_pending()
        );
        runtime.run_until_stalled().unwrap();
        let _ = store.send(OptionalDocumentAction::Remove).await.unwrap();
        let _ = store.send(OptionalDocumentAction::Reinsert).await.unwrap();
        runtime.run_until_stalled().unwrap();
        backend.complete(0, Ok(present(3, 99)));
        runtime.run_until_stalled().unwrap();
        assert!(load.await.unwrap_err().is_ended());
        assert!(
            key.load(SharedLoadContext::UserInitiated)
                .await
                .unwrap_err()
                .is_ended()
        );
        assert_eq!(
            store
                .read(|state| match state {
                    CaseDocumentState::On(state) => *state.value(),
                    CaseDocumentState::Off => panic!("case was restored"),
                })
                .await
                .unwrap(),
            40
        );
        owner.close().await.unwrap();
        let fresh = SharedDocumentOwner::new(
            store
                .scope_case(case_document_state(), optional_document_action())
                .await
                .unwrap(),
            |value: &u32| *value,
            fresh_session,
        );
        let fresh_reader =
            fresh_load_and_save(&runtime, &fresh_backend, &fresh, "case-fresh").await;
        assert_eq!(original.read(|value| *value).unwrap(), 7);
        assert_eq!(fresh_reader.read(|value| *value).unwrap(), 42);
        fresh.close().await.unwrap();
        store.shutdown().await.unwrap();
    }))
    .unwrap();
}

/// Distinguishes instance-qualified child work from reuse of a public row key.
enum KeyedDocumentAction {
    /// Carries a document action for one existing child instance.
    Row(KeyedAction<u8, SharedDocumentAction<u32, u64>>),
    /// Removes the current instance and creates a new one with the same public key.
    Recreate(u8),
}

/// Reuses a public row key without retaining the old instance's state.
struct KeyedDocumentParent {
    /// Explicit state transferred into the recreated row.
    replacement: Option<SharedDocumentState<u32, u64>>,
}
impl Reducer for KeyedDocumentParent {
    type State = KeyedState<u8, SharedDocumentState<u32, u64>>;
    type Action = KeyedDocumentAction;
    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &Self::Action,
        _: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        if let KeyedDocumentAction::Recreate(id) = action {
            state.remove(id);
            state.insert(
                *id,
                self.replacement.take().expect("one explicit replacement"),
            );
        }
        Effect::none()
    }
}

/// Exposes the collection to the composed child reducer without copying its rows.
fn keyed_document_state() -> DocumentRowsPath {
    KeyedStatePath::new(
        |state: &KeyedState<u8, SharedDocumentState<u32, u64>>| state,
        |state| state,
    )
}
/// Preserves each child action's instance identity through the parent route.
fn keyed_document_action() -> DocumentRowActionPath {
    KeyedActionPath::new(
        |action: &KeyedDocumentAction| match action {
            KeyedDocumentAction::Row(action) => Some(action),
            KeyedDocumentAction::Recreate(_) => None,
        },
        KeyedDocumentAction::Row,
    )
}

// Catches raw-ID attachment reuse and overbroad load cancellation affecting a sibling.
#[test]
fn keyed_document_replacement_retires_old_key_without_cancelling_sibling_load() {
    let runtime = DeterministicRuntime::new();
    let backend = Memory::new();
    let (first, reducer) = SharedDocument::revisioned(7, backend.clone(), Binary);
    let second = reducer.state(20);
    let replacement = reducer.state(40);
    let session = reducer.session();
    let store = Store::new(
        [(1, first), (2, second)].into_iter().collect(),
        KeyedDocumentParent {
            replacement: Some(replacement),
        }
        .for_each(keyed_document_state(), keyed_document_action(), reducer),
        runtime.runtime(),
    );
    block_on(runtime.run(async {
        let old = SharedDocumentOwner::new(
            store
                .scope_keyed(keyed_document_state(), keyed_document_action(), 1)
                .await
                .unwrap(),
            |value: &u32| *value,
            session.clone(),
        );
        let sibling = SharedDocumentOwner::new(
            store
                .scope_keyed(keyed_document_state(), keyed_document_action(), 2)
                .await
                .unwrap(),
            |value: &u32| *value,
            session.clone(),
        );
        let old_key = old.key(SharedKeyIdentity::new::<Binary>("row1", "binary"));
        let sibling_key = sibling.key(SharedKeyIdentity::new::<Binary>("row2", "binary"));
        let mut old_load = Box::pin(old_key.load(SharedLoadContext::UserInitiated));
        let mut sibling_load = Box::pin(sibling_key.load(SharedLoadContext::UserInitiated));
        assert!(
            old_load
                .as_mut()
                .poll(&mut std::task::Context::from_waker(std::task::Waker::noop()))
                .is_pending()
        );
        runtime.run_until_stalled().unwrap();
        assert!(
            sibling_load
                .as_mut()
                .poll(&mut std::task::Context::from_waker(std::task::Waker::noop()))
                .is_pending()
        );
        runtime.run_until_stalled().unwrap();
        assert_eq!(backend.state.lock().unwrap().loads.len(), 2);
        let _ = store.send(KeyedDocumentAction::Recreate(1)).await.unwrap();
        runtime.run_until_stalled().unwrap();
        backend.complete(0, Ok(present(3, 99)));
        backend.complete(0, Ok(present(4, 23)));
        runtime.run_until_stalled().unwrap();
        assert!(old_load.await.unwrap_err().is_ended());
        assert_eq!(sibling_load.await.unwrap(), Some(23));
        assert_eq!(
            store
                .read(|state| (
                    *state.get(&1).unwrap().value(),
                    *state.get(&2).unwrap().value()
                ))
                .await
                .unwrap(),
            (40, 23)
        );
        assert!(
            old_key
                .load(SharedLoadContext::UserInitiated)
                .await
                .unwrap_err()
                .is_ended()
        );
        sibling.close().await.unwrap();
        store.shutdown().await.unwrap();
    }))
    .unwrap();
}

// Catches recreated child authority reaching an old cache binding, sibling
// cancellation, or logical producer cancellation discarding physical join debt.
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "keeps keyed replacement, explicit cache rebinding and both physical completion orders in one causal scenario"
)]
fn keyed_document_replacement_rebinds_cache_without_losing_retired_join_debt() {
    for producer_first in [false, true] {
        let runtime = DeterministicRuntime::new();
        let old_backend = Memory::new();
        let sibling_backend = Memory::new();
        let fresh_backend = Memory::new();
        let (old_state, interpreter) = SharedDocument::revisioned(7, old_backend.clone(), Binary);
        let old_session = interpreter.session();
        let (sibling_state, sibling_resource) =
            SharedDocument::revisioned(20, sibling_backend.clone(), Binary);
        let (replacement, fresh_resource) =
            SharedDocument::revisioned(40, fresh_backend.clone(), Binary);
        let store = Store::new(
            [(1, old_state), (2, sibling_state)].into_iter().collect(),
            KeyedDocumentParent {
                replacement: Some(replacement),
            }
            .for_each(keyed_document_state(), keyed_document_action(), interpreter),
            runtime.runtime(),
        );
        let cancellations = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        block_on(runtime.run(async {
            let old = SharedDocumentOwner::new(
                store
                    .scope_keyed(keyed_document_state(), keyed_document_action(), 1)
                    .await
                    .unwrap(),
                |value: &u32| *value,
                old_session,
            );
            let sibling = SharedDocumentOwner::new(
                store
                    .scope_keyed(keyed_document_state(), keyed_document_action(), 2)
                    .await
                    .unwrap(),
                |value: &u32| *value,
                sibling_resource.session(),
            );
            let cache = SharedKeyCache::new();
            let identity = SharedKeyIdentity::new::<Binary>("chain02-row1", "binary");
            let old_key = old.key(identity.clone());
            let mut initial = Box::pin(cache.reader(old.key(identity.clone()), || 0));
            assert!(
                initial
                    .as_mut()
                    .poll(&mut Context::from_waker(Waker::noop()))
                    .is_pending()
            );
            runtime.run_until_stalled().unwrap();
            old_backend.complete(0, Ok(present(1, 7)));
            let mut current = initial.await.unwrap();
            runtime.run_until_stalled().unwrap();
            let retained_old = current.clone();
            let surviving =
                cached_value(&runtime, &sibling_backend, &sibling, "chain02-row2", 20).await;

            let (join_completion, join) = controlled_future();
            let cancelling = Arc::clone(&cancellations);
            let mut endpoint = None;
            let subscription = old
                .observe_changes::<StorageError, _, _, _>(|producer| {
                    endpoint = Some(producer);
                    (
                        SharedUpdate::Present(()),
                        move || {
                            cancelling.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        },
                        async move { join.await.map_err(|_cause| StorageError(71))? },
                    )
                })
                .unwrap();
            let endpoint = endpoint.unwrap();
            runtime.run_until_stalled().unwrap();
            assert_eq!(old_backend.state.lock().unwrap().loads.len(), 1);
            assert!(subscription.status().is_active());

            let _ = store.send(KeyedDocumentAction::Recreate(1)).await.unwrap();
            runtime.run_until_stalled().unwrap();
            assert!(
                old_key
                    .load(SharedLoadContext::UserInitiated)
                    .await
                    .unwrap_err()
                    .is_ended()
            );
            assert!(subscription.status().error().unwrap().is_ended());
            assert!(matches!(
                endpoint.publish(SharedUpdate::Present(())),
                Err(SharedUpdate::Present(()))
            ));
            assert_eq!(cancellations.load(std::sync::atomic::Ordering::SeqCst), 1);
            assert_eq!(surviving.read(|value| *value).unwrap(), 20);
            assert_eq!(retained_old.read(|value| *value).unwrap(), 7);

            let fresh = SharedDocumentOwner::new(
                store
                    .scope_keyed(keyed_document_state(), keyed_document_action(), 1)
                    .await
                    .unwrap(),
                |value: &u32| *value,
                fresh_resource.session(),
            );
            // Equal cache identities do not renew child authority. Reset lookup, then
            // explicitly rebind this reader; its earlier clone keeps the old reference.
            cache.reset().unwrap();
            let mut replacing =
                Box::pin(
                    cache.load_reader(&mut current, fresh.key(identity.clone()), |value| {
                        assert_eq!(*value, 7);
                        40
                    }),
                );
            assert!(
                replacing
                    .as_mut()
                    .poll(&mut Context::from_waker(Waker::noop()))
                    .is_pending()
            );
            runtime.run_until_stalled().unwrap();
            fresh_backend.complete(0, Ok(present(2, 42)));
            replacing.await.unwrap();
            runtime.run_until_stalled().unwrap();
            let hit = cache
                .reader(fresh.key(identity), || {
                    panic!("fresh binding must be cached")
                })
                .await
                .unwrap();
            assert_eq!(hit.read(|value| *value).unwrap(), 42);
            assert_eq!(current.read(|value| *value).unwrap(), 42);
            assert_eq!(retained_old.read(|value| *value).unwrap(), 7);
            assert_eq!(surviving.read(|value| *value).unwrap(), 20);
            assert_eq!(fresh_backend.state.lock().unwrap().revisioned_load_calls, 1);

            // Closing the retired owner cannot join either outstanding physical task.
            let mut closing = Box::pin(old.close());
            assert!(
                closing
                    .as_mut()
                    .poll(&mut Context::from_waker(Waker::noop()))
                    .is_pending()
            );
            runtime.run_until_stalled().unwrap();
            assert!(
                closing
                    .as_mut()
                    .poll(&mut Context::from_waker(Waker::noop()))
                    .is_pending()
            );
            // Each order isolates one debt: the other completion cannot finish close.
            let join_completion = if producer_first {
                join_completion.complete(Ok(())).unwrap();
                None
            } else {
                old_backend.complete(0, Ok(present(99, 99)));
                Some(join_completion)
            };
            runtime.run_until_stalled().unwrap();
            assert!(
                closing
                    .as_mut()
                    .poll(&mut Context::from_waker(Waker::noop()))
                    .is_pending()
            );
            assert_eq!(
                store
                    .read(|state| (
                        *state.get(&1).unwrap().value(),
                        state.get(&1).unwrap().backend_revision().copied(),
                        *state.get(&2).unwrap().value(),
                        state.get(&2).unwrap().backend_revision().copied(),
                    ))
                    .await
                    .unwrap(),
                (42, Some(2), 20, Some(1))
            );
            assert_eq!(current.read(|value| *value).unwrap(), 42);
            assert_eq!(retained_old.read(|value| *value).unwrap(), 7);

            // The sibling still accepts its own intended load while old close waits.
            let _ = store
                .send(KeyedDocumentAction::Row(KeyedAction::new(
                    2,
                    SharedDocumentAction::reload(),
                )))
                .await
                .unwrap();
            runtime.run_until_stalled().unwrap();
            sibling_backend.complete(0, Ok(present(3, 23)));
            runtime.run_until_stalled().unwrap();
            assert_eq!(surviving.read(|value| *value).unwrap(), 23);
            assert_eq!(current.read(|value| *value).unwrap(), 42);
            if let Some(join_completion) = join_completion {
                join_completion.complete(Ok(())).unwrap();
            } else {
                old_backend.complete(0, Ok(present(99, 99)));
            }
            closing.await.unwrap();
            assert_eq!(
                store
                    .read(|state| (
                        *state.get(&1).unwrap().value(),
                        state.get(&1).unwrap().backend_revision().copied(),
                        *state.get(&2).unwrap().value(),
                        state.get(&2).unwrap().backend_revision().copied(),
                    ))
                    .await
                    .unwrap(),
                (42, Some(2), 23, Some(3))
            );
            assert_eq!(current.read(|value| *value).unwrap(), 42);
            assert_eq!(hit.read(|value| *value).unwrap(), 42);
            assert_eq!(retained_old.read(|value| *value).unwrap(), 7);
            assert_eq!(surviving.read(|value| *value).unwrap(), 23);
            assert_eq!(cancellations.load(std::sync::atomic::Ordering::SeqCst), 1);
            fresh.close().await.unwrap();
            sibling.close().await.unwrap();
            store.shutdown().await.unwrap();
        }))
        .unwrap();
        for (backend, loads) in [
            (&old_backend, 2),
            (&sibling_backend, 2),
            (&fresh_backend, 1),
        ] {
            let state = backend.state.lock().unwrap();
            assert!(state.closed);
            assert!(state.loads.is_empty());
            assert!(state.saves.is_empty());
            assert_eq!(
                (state.revisioned_load_calls, state.revisioned_save_calls),
                (loads, 0)
            );
        }
        block_on(runtime.drain_deadlines(0)).unwrap();
    }
}

/// Separates stack-child delivery from deliberate reuse of a removed stack ID.
enum StackDocumentAction {
    /// Routes an action to its original stack child instance.
    Path(StackAction<u32, SharedDocumentAction<u32, u64>>),
    /// Replaces the suffix starting at this public ID with one fresh child.
    Recreate(StackId),
}

/// Recreates a stack tail while preserving the original prefix instances.
struct StackDocumentParent {
    /// One explicit replacement state, not a value produced by the stack factory.
    replacement: Option<SharedDocumentState<u32, u64>>,
}
impl Reducer for StackDocumentParent {
    type State = StackState<SharedDocumentState<u32, u64>>;
    type Action = StackDocumentAction;
    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &Self::Action,
        _: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        if let StackDocumentAction::Recreate(id) = action {
            state.try_pop_from(id).unwrap();
            state
                .try_push(
                    id.clone(),
                    self.replacement.take().expect("one explicit replacement"),
                )
                .unwrap_or_else(|_| panic!("old stack ID was removed"));
        }
        Effect::none()
    }
}

/// Selects the complete stack so composition retains its per-instance lifetimes.
fn stack_document_state() -> TestStatePath<DocumentStack, DocumentStack> {
    StatePath::new(
        |state: &StackState<SharedDocumentState<u32, u64>>| state,
        |state| state,
    )
}
/// Routes stack actions without forwarding parent-only recreation commands.
fn stack_document_action()
-> TestActionPath<StackDocumentAction, StackAction<u32, SharedDocumentAction<u32, u64>>> {
    ActionPath::new(
        |action: &StackDocumentAction| match action {
            StackDocumentAction::Path(action) => Some(action),
            StackDocumentAction::Recreate(_) => None,
        },
        StackDocumentAction::Path,
    )
}

// Catches public stack-ID reuse renewing an old key or suffix retirement killing prefix work.
#[test]
fn stack_document_recreation_rejects_old_key_and_keeps_prefix_load() {
    let runtime = DeterministicRuntime::new();
    let backend = Memory::new();
    let (prefix_state, reducer) = SharedDocument::revisioned(20, backend.clone(), Binary);
    let session = reducer.session();
    let mut state = StackState::new();
    let prefix_id = state.push(prefix_state);
    let tail_id = state.push(reducer.state(7));
    let replacement = reducer.state(40);
    let reducer = stack(
        StackDocumentParent {
            replacement: Some(replacement),
        },
        stack_document_state(),
        stack_document_action(),
        |value: &u32, _: &mut ReduceContext<'_>| {
            panic!("this fixture recreates explicitly, not from a push seed: {value}")
        },
        reducer,
    );
    let store = Store::new(state, reducer, runtime.runtime());
    block_on(runtime.run(async {
        let old = SharedDocumentOwner::new(
            store
                .scope_stack(
                    stack_document_state(),
                    stack_document_action(),
                    tail_id.clone(),
                )
                .await
                .unwrap(),
            |value: &u32| *value,
            session.clone(),
        );
        let prefix = SharedDocumentOwner::new(
            store
                .scope_stack(
                    stack_document_state(),
                    stack_document_action(),
                    prefix_id.clone(),
                )
                .await
                .unwrap(),
            |value: &u32| *value,
            session.clone(),
        );
        let old_key = old.key(SharedKeyIdentity::new::<Binary>("tail", "binary"));
        let prefix_key = prefix.key(SharedKeyIdentity::new::<Binary>("prefix", "binary"));
        let mut old_load = Box::pin(old_key.load(SharedLoadContext::UserInitiated));
        let mut prefix_load = Box::pin(prefix_key.load(SharedLoadContext::UserInitiated));
        assert!(
            old_load
                .as_mut()
                .poll(&mut std::task::Context::from_waker(std::task::Waker::noop()))
                .is_pending()
        );
        runtime.run_until_stalled().unwrap();
        assert!(
            prefix_load
                .as_mut()
                .poll(&mut std::task::Context::from_waker(std::task::Waker::noop()))
                .is_pending()
        );
        runtime.run_until_stalled().unwrap();
        let _ = store
            .send(StackDocumentAction::Recreate(tail_id.clone()))
            .await
            .unwrap();
        runtime.run_until_stalled().unwrap();
        backend.complete(0, Ok(present(3, 99)));
        backend.complete(0, Ok(present(4, 23)));
        runtime.run_until_stalled().unwrap();
        assert!(old_load.await.unwrap_err().is_ended());
        assert_eq!(prefix_load.await.unwrap(), Some(23));
        assert_eq!(
            store
                .read(move |state| (
                    *state.get(&tail_id).unwrap().value(),
                    *state.get(&prefix_id).unwrap().value()
                ))
                .await
                .unwrap(),
            (40, 23)
        );
        assert!(
            old_key
                .load(SharedLoadContext::UserInitiated)
                .await
                .unwrap_err()
                .is_ended()
        );
        prefix.close().await.unwrap();
        store.shutdown().await.unwrap();
    }))
    .unwrap();
}

/// Pairs an identity route with the supplied session, including intentional mismatches.
fn document_owner<R: Clone + Eq + Send + 'static>(
    store: &Store<SharedDocumentState<u32, R>, SharedDocumentAction<u32, R>>,
    session: rca_persistence::SharedDocumentSession,
) -> SharedDocumentOwner<u32, R> {
    SharedDocumentOwner::new(
        store.scope(
            StatePath::new(|state: &SharedDocumentState<u32, R>| state, |state| state),
            ActionPath::new(
                |action: &SharedDocumentAction<u32, R>| Some(action),
                |action| action,
            ),
        ),
        |value: &u32| *value,
        session,
    )
}

/// Loads a fresh cache binding and returns a reader that outlives the local cache.
///
/// The caller supplies literal load evidence; this does not write backend bytes.
async fn cached_value(
    runtime: &DeterministicRuntime,
    backend: &Memory,
    owner: &SharedDocumentOwner<u32, u64>,
    name: &'static str,
    value: u32,
) -> rca_core::SharedReader<u32> {
    let key = owner.key(SharedKeyIdentity::new::<Binary>(name, "binary"));
    let cache = SharedKeyCache::new();
    let mut loading = Box::pin(cache.shared(key, || 0));
    assert!(
        loading
            .as_mut()
            .poll(&mut std::task::Context::from_waker(std::task::Waker::noop()))
            .is_pending()
    );
    runtime.run_until_stalled().unwrap();
    backend.complete(0, Ok(present(1, value)));
    runtime.run_until_stalled().unwrap();
    let shared = loading.await.unwrap();
    runtime.run_until_stalled().unwrap();
    shared.reader().unwrap()
}

// Rejecting a live but incorrectly paired session must precede provider admission.
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "keeps the paired live-session schedules and independent ownership oracles together"
)]
fn captured_key_load_validates_exact_session() {
    for matched in [false, true] {
        let runtime = DeterministicRuntime::new();
        let backend_a = Memory::new();
        let backend_b = Memory::new();
        let (state_a, reducer_a) = SharedDocument::revisioned(7, backend_a.clone(), Binary);
        let (state_b, reducer_b) = SharedDocument::revisioned(23, backend_b.clone(), Binary);
        let session_a = reducer_a.session();
        let session_b = reducer_b.session();
        let store_a = Store::new(state_a, reducer_a, runtime.runtime());
        let store_b = Store::new(state_b, reducer_b, runtime.runtime());
        let owner_a = document_owner(&store_a, session_a.clone());
        let owner_b = document_owner(&store_b, session_b.clone());
        let input_owner = document_owner(
            &store_a,
            if matched {
                session_a.clone()
            } else {
                session_b.clone()
            },
        );
        let key = input_owner.key(SharedKeyIdentity::new::<Binary>("session-pair", "binary"));
        block_on(runtime.run(async {
            let context = || Context::from_waker(Waker::noop());
            let mut load = Box::pin(key.load(SharedLoadContext::UserInitiated));
            assert!(load.as_mut().poll(&mut context()).is_pending());
            runtime.run_until_stalled().unwrap();
            runtime.run_until_stalled().unwrap();
            assert_eq!(
                backend_a.state.lock().unwrap().loads.len(),
                usize::from(matched)
            );
            assert!(backend_b.state.lock().unwrap().loads.is_empty());
            for backend in [&backend_a, &backend_b] {
                let storage = backend.state.lock().unwrap();
                assert!(storage.saves.is_empty());
                assert_eq!(storage.revision, 1);
                assert!(storage.bytes.is_none());
                assert!(!storage.closed);
            }
            assert_eq!(
                store_a
                    .read(|state| (
                        *state.value(),
                        state.revision(),
                        state.acknowledged_revision(),
                        state.is_loading()
                    ))
                    .await
                    .unwrap(),
                (7, 0, None, matched)
            );
            if matched {
                assert!(load.as_mut().poll(&mut context()).is_pending());
                backend_a.complete(0, Ok(present(2, 99)));
                runtime.run_until_stalled().unwrap();
                runtime.run_until_stalled().unwrap();
                assert_eq!(load.await.unwrap(), Some(99));
            } else {
                let error = load.await.unwrap_err();
                assert!(error.is_session());
                assert!(!error.is_ended());
                assert!(!error.is_full());
                assert!(!error.is_superseded());
                assert!(!error.is_cancelled());
                assert!(!error.is_conflict());
                assert!(!error.is_shared_access());
                assert!(error.error().is_none());
                assert!(error.source().is_none());
                assert_eq!(
                    error.to_string().lines().next(),
                    Some("document key WrongSession failed")
                );
            }
            for (store, expected_value, expected_ack, expected_backend) in [
                (
                    &store_a,
                    if matched { 99 } else { 7 },
                    matched.then_some(0),
                    matched.then_some(2),
                ),
                (&store_b, 23, None, None),
            ] {
                store
                    .read(move |state| {
                        assert_eq!(*state.value(), expected_value);
                        assert_eq!(state.revision(), 0);
                        assert_eq!(state.acknowledged_revision(), expected_ack);
                        assert_eq!(state.backend_revision().copied(), expected_backend);
                        assert!(state.load_error().is_none());
                        assert!(state.document().failure().is_none());
                        assert!(state.conflict().is_none());
                        assert!(!state.is_loading());
                        assert_eq!(state.document().pending_barrier_count(), 0);
                    })
                    .await
                    .unwrap();
            }
            for backend in [&backend_a, &backend_b] {
                let storage = backend.state.lock().unwrap();
                assert!(storage.loads.is_empty());
                assert!(storage.saves.is_empty());
                assert_eq!(storage.revision, 1);
                assert!(storage.bytes.is_none());
                assert!(!storage.closed);
            }
            drop(key);
            // Closing the mismatched input would fence B while sending Abort to A.
            drop(input_owner);
            owner_a.close().await.unwrap();
            owner_b.close().await.unwrap();
            store_a.shutdown().await.unwrap();
            store_b.shutdown().await.unwrap();
        }))
        .unwrap();
        for backend in [&backend_a, &backend_b] {
            let storage = backend.state.lock().unwrap();
            assert!(storage.closed);
            assert!(storage.loads.is_empty());
            assert!(storage.saves.is_empty());
        }
    }
}

// Logical cancellation and abort must reply without joining physical I/O.
#[test]
fn cancelled_or_aborted_live_key_load_replies_without_publishing_late_completion() {
    for abort in [false, true] {
        let runtime = DeterministicRuntime::new();
        let backend = Memory::new();
        let (state, reducer) = SharedDocument::revisioned(7, backend.clone(), Binary);
        let session = reducer.session();
        let store = Store::new(state, reducer, runtime.runtime());
        let owner = document_owner(&store, session.clone());
        let key = owner.key(SharedKeyIdentity::new::<Binary>(
            "cancelled-live-load",
            "binary",
        ));
        block_on(runtime.run(async {
            let context = || Context::from_waker(Waker::noop());
            let assert_unchanged_state = move |state: &SharedDocumentState<u32, u64>| {
                assert_eq!(*state.value(), 7);
                assert_eq!(state.revision(), 0);
                assert_eq!(state.acknowledged_revision(), None);
                assert!(state.backend_revision().is_none());
                assert!(!state.is_loading());
                assert!(state.load_error().is_none());
                assert!(state.document().failure().is_none());
                assert!(state.conflict().is_none());
                assert_eq!(state.document().pending_barrier_count(), 0);
                assert_eq!(state.document().is_aborted(), abort);
            };
            let mut load = Box::pin(key.load(SharedLoadContext::UserInitiated));
            assert!(load.as_mut().poll(&mut context()).is_pending());
            runtime.run_until_stalled().unwrap();
            assert_eq!(backend.state.lock().unwrap().loads.len(), 1);
            assert!(store.read(SharedDocumentState::is_loading).await.unwrap());
            store
                .send(if abort {
                    SharedDocumentAction::abort()
                } else {
                    SharedDocumentAction::cancel_load()
                })
                .await
                .unwrap()
                .finish()
                .await;
            runtime.run_until_stalled().unwrap();
            runtime.run_until_stalled().unwrap();
            let error = load.await.unwrap_err();
            assert!(error.is_cancelled());
            assert!(!error.is_superseded());
            assert!(!error.is_session());
            assert!(!error.is_ended());
            assert!(!error.is_full());
            assert!(!error.is_conflict());
            assert!(!error.is_shared_access());
            assert!(error.error().is_none());
            assert!(error.source().is_none());
            store.read(assert_unchanged_state).await.unwrap();
            let mut closing = Box::pin(session.close());
            assert!(closing.as_mut().poll(&mut context()).is_pending());
            assert_eq!(backend.state.lock().unwrap().loads.len(), 1);
            assert!(backend.state.lock().unwrap().saves.is_empty());
            backend.complete(0, Ok(present(2, 99)));
            closing.await.unwrap();
            runtime.run_until_stalled().unwrap();
            runtime.run_until_stalled().unwrap();
            store.read(assert_unchanged_state).await.unwrap();
            owner.close().await.unwrap();
            store.shutdown().await.unwrap();
        }))
        .unwrap();
        let storage = backend.state.lock().unwrap();
        assert!(storage.closed);
        assert!(storage.loads.is_empty());
        assert!(storage.saves.is_empty());
        assert_eq!(storage.revision, 1);
        assert!(storage.bytes.is_none());
    }
}

/// Rejects one-byte initial snapshots while retaining the producer's unfinished join.
///
/// Plain and revisioned callers share decode and ownership checks, not revision input.
fn malformed_snapshot_preserves_seed_and_join<R: Clone + Eq + Send + 'static>(
    construct: impl FnOnce(Memory) -> (SharedDocumentState<u32, R>, SharedDocument<u32, R>),
    revision: Option<R>,
) {
    let runtime = DeterministicRuntime::new();
    let backend = Memory::new();
    let (state, reducer) = construct(backend.clone());
    let session = reducer.session();
    let store = Store::new(state, reducer, runtime.runtime());
    let owner = document_owner(&store, session.clone());
    let (join_completion, join) = controlled_future();
    let cancellations = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let cancelling = Arc::clone(&cancellations);
    let mut endpoint = None;
    let subscription = owner
        .observe_snapshots::<StorageError, _, _, _>(|producer| {
            endpoint = Some(producer);
            (
                SharedUpdate::Present(BackendSnapshot {
                    revision,
                    bytes: Some(vec![99]),
                }),
                move || {
                    cancelling.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                },
                async move { join.await.map_err(|_cause| StorageError(71))? },
            )
        })
        .unwrap();
    let endpoint = endpoint.unwrap();
    block_on(runtime.run(async {
        let context = || Context::from_waker(Waker::noop());
        runtime.run_until_stalled().unwrap();
        runtime.run_until_stalled().unwrap();
        assert!(backend.state.lock().unwrap().loads.is_empty());
        let status = subscription.status();
        let key_error = status.error().unwrap();
        let delivered = key_error
            .error()
            .unwrap()
            .downcast_ref::<Failure>()
            .unwrap();
        assert!(std::ptr::eq(
            delivered,
            key_error
                .source()
                .unwrap()
                .downcast_ref::<Failure>()
                .unwrap()
        ));
        let delivered = delivered.clone();
        store
            .read(move |state| {
                let failure = state.load_error().unwrap();
                let source = failure.source().unwrap();
                let delivered_source = delivered.source().unwrap();
                assert_eq!(failure.operation(), Operation::Decode);
                assert_eq!(failure.revision(), None);
                let cause = failure.error().downcast_ref::<StorageError>().unwrap();
                assert_eq!(cause.0, 2);
                assert!(std::ptr::eq(
                    cause,
                    source.downcast_ref::<StorageError>().unwrap()
                ));
                assert_eq!(delivered.operation(), Operation::Decode);
                assert_eq!(delivered.revision(), None);
                assert!(std::ptr::eq(
                    cause,
                    delivered.error().downcast_ref::<StorageError>().unwrap()
                ));
                assert!(std::ptr::eq(
                    cause,
                    delivered_source.downcast_ref::<StorageError>().unwrap()
                ));
                assert_eq!(*state.value(), 7);
                assert_eq!(state.revision(), 0);
                assert_eq!(state.acknowledged_revision(), None);
                assert!(state.backend_revision().is_none());
                assert!(state.conflict().is_none());
                assert!(!state.is_loading());
            })
            .await
            .unwrap();
        assert_eq!(cancellations.load(std::sync::atomic::Ordering::SeqCst), 1);
        let mut closing = Box::pin(owner.close());
        assert!(closing.as_mut().poll(&mut context()).is_pending());
        runtime.run_until_stalled().unwrap();
        assert!(closing.as_mut().poll(&mut context()).is_pending());
        drop(closing);
        let mut resumed = Box::pin(session.close());
        assert!(resumed.as_mut().poll(&mut context()).is_pending());
        join_completion.complete(Ok(())).unwrap();
        resumed.await.unwrap();
        runtime.run_until_stalled().unwrap();
        assert_eq!(cancellations.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(store.read(|state| *state.value()).await.unwrap(), 7);
        assert!(backend.state.lock().unwrap().loads.is_empty());
        assert!(backend.state.lock().unwrap().saves.is_empty());
        store.shutdown().await.unwrap();
    }))
    .unwrap();
    drop(endpoint);
}

#[test]
fn malformed_plain_snapshot_preserves_seed_and_join() {
    malformed_snapshot_preserves_seed_and_join(
        |backend| SharedDocument::new(7, backend, Binary),
        None,
    );
}

#[test]
fn malformed_revisioned_snapshot_preserves_seed_and_join() {
    malformed_snapshot_preserves_seed_and_join(
        |backend| SharedDocument::revisioned(7, backend, Binary),
        Some(3),
    );
}

#[test]
fn malformed_compare_save_conflict_retains_raw_evidence_and_local_edit() {
    let runtime = DeterministicRuntime::new();
    let backend = Memory::new();
    backend.state.lock().unwrap().bytes = Some(7_u32.to_le_bytes().to_vec());
    let (state, reducer) = SharedDocument::revisioned(7, backend.clone(), Binary);
    let session = reducer.session();
    let store = Store::new(state, reducer, runtime.runtime());
    block_on(runtime.run(async {
        let context = || Context::from_waker(Waker::noop());
        let _ = store.send(SharedDocumentAction::reload()).await.unwrap();
        runtime.run_until_stalled().unwrap();
        assert_eq!(backend.state.lock().unwrap().loads.len(), 1);
        backend.complete(0, Ok(present(1, 7)));
        runtime.run_until_stalled().unwrap();
        let (action, mut receipt) = SharedDocumentAction::set_and_flush(8);
        let _ = store.send(action).await.unwrap();
        runtime.run_until_stalled().unwrap();
        assert!(Pin::new(&mut receipt).poll(&mut context()).is_pending());
        {
            let mut storage = backend.state.lock().unwrap();
            assert_eq!(storage.saves.len(), 1);
            assert_eq!(storage.saves[0].expected, 1);
            assert_eq!(storage.saves[0].bytes, 8_u32.to_le_bytes());
            storage.revision = 3;
            storage.bytes = Some(vec![99]);
        };
        backend.complete_save();
        // The receipt stays unread through completion and quiescence.
        runtime.run_until_stalled().unwrap();
        runtime.run_until_stalled().unwrap();
        let error = receipt.await.unwrap_err();
        let ReceiptError::Persistence(receipt_failure) = &error else {
            panic!("expected save conflict");
        };
        assert!(std::ptr::eq(
            receipt_failure,
            error.source().unwrap().downcast_ref::<Failure>().unwrap()
        ));
        let receipt_failure = receipt_failure.clone();
        store
            .read(move |state| {
                let failure = state.document().failure().unwrap();
                let source = failure.source().unwrap();
                let receipt_source = receipt_failure.source().unwrap();
                assert_eq!(failure.operation(), Operation::Save);
                assert_eq!(failure.revision(), Some(1));
                let cause = failure.error().downcast_ref::<ConflictDetected>().unwrap();
                assert!(std::ptr::eq(
                    cause,
                    source.downcast_ref::<ConflictDetected>().unwrap()
                ));
                assert_eq!(receipt_failure.operation(), Operation::Save);
                assert_eq!(receipt_failure.revision(), Some(1));
                assert!(std::ptr::eq(
                    cause,
                    receipt_failure
                        .error()
                        .downcast_ref::<ConflictDetected>()
                        .unwrap()
                ));
                assert!(std::ptr::eq(
                    cause,
                    receipt_source.downcast_ref::<ConflictDetected>().unwrap()
                ));
                let conflict = state.conflict().unwrap();
                assert_eq!(*conflict.revision(), 3);
                assert_eq!(conflict.bytes(), Some([99].as_slice()));
                assert_eq!(conflict.value(), None);
                let decode = conflict.decode_error().unwrap();
                assert_eq!(decode.operation(), Operation::Decode);
                assert_eq!(decode.revision(), Some(1));
                let decode_source = decode.source().unwrap();
                let decode_cause = decode.error().downcast_ref::<StorageError>().unwrap();
                assert_eq!(decode_cause.0, 2);
                assert!(std::ptr::eq(
                    decode_cause,
                    decode_source.downcast_ref::<StorageError>().unwrap()
                ));
                assert_eq!(*state.value(), 8);
                assert_eq!(state.revision(), 1);
                assert_eq!(state.acknowledged_revision(), Some(0));
                assert_eq!(state.backend_revision(), Some(&1));
                assert_eq!(state.document().pending_barrier_count(), 0);
            })
            .await
            .unwrap();
        {
            let storage = backend.state.lock().unwrap();
            assert_eq!(storage.revision, 3);
            assert_eq!(storage.bytes, Some(vec![99]));
            assert!(storage.loads.is_empty());
            assert!(storage.saves.is_empty());
        };
        store.shutdown().await.unwrap();
    }))
    .unwrap();
    block_on(session.close()).unwrap();
}

// Catches an unconsumed superseded reply revoking the replacement load.
#[test]
fn dropping_superseded_key_load_does_not_cancel_current_load() {
    let runtime = DeterministicRuntime::with_step_budget(64);
    let backend = Memory::new();
    let (state, reducer) = SharedDocument::revisioned(7, backend.clone(), Binary);
    let session = reducer.session();
    let store = Store::new(state, reducer, runtime.runtime());
    let owner = document_owner(&store, session);
    let key = owner.key(SharedKeyIdentity::new::<Binary>(
        "superseded-cancellation",
        "binary",
    ));
    run_document_session_contract(&runtime, async {
        let context = || Context::from_waker(Waker::noop());
        let mut first = Box::pin(key.load(SharedLoadContext::UserInitiated));
        assert!(first.as_mut().poll(&mut context()).is_pending());
        runtime.run_until_stalled().unwrap();
        assert_eq!(backend.state.lock().unwrap().loads.len(), 1);

        let mut second = Box::pin(key.load(SharedLoadContext::UserInitiated));
        assert!(second.as_mut().poll(&mut context()).is_pending());
        runtime.run_until_stalled().unwrap();
        assert_eq!(backend.state.lock().unwrap().loads.len(), 2);

        // Leave the superseded reply unconsumed so Drop sends its old token.
        drop(first);
        runtime.run_until_stalled().unwrap();
        assert!(second.as_mut().poll(&mut context()).is_pending());
        assert_eq!(
            store
                .read(|state| (*state.value(), state.revision(), state.is_loading()))
                .await
                .unwrap(),
            (7, 0, true)
        );

        backend.complete(1, Ok(present(2, 9)));
        runtime.run_until_stalled().unwrap();
        assert!(matches!(
            second.as_mut().poll(&mut context()),
            Poll::Ready(Ok(Some(9)))
        ));
        drop(second);
        assert_eq!(
            store
                .read(|state| (
                    *state.value(),
                    state.revision(),
                    state.acknowledged_revision(),
                    state.backend_revision().copied(),
                    state.is_loading()
                ))
                .await
                .unwrap(),
            (9, 0, Some(0), Some(2), false)
        );

        let mut closing = Box::pin(owner.close());
        assert!(closing.as_mut().poll(&mut context()).is_pending());
        runtime.run_until_stalled().unwrap();
        assert!(closing.as_mut().poll(&mut context()).is_pending());
        assert_eq!(backend.state.lock().unwrap().loads.len(), 1);

        backend.complete(0, Ok(present(99, 99)));
        runtime.run_until_stalled().unwrap();
        assert!(matches!(
            closing.as_mut().poll(&mut context()),
            Poll::Ready(Ok(()))
        ));
        drop(closing);
        assert_eq!(store.read(|state| *state.value()).await.unwrap(), 9);
        assert_eq!(
            {
                let storage = backend.state.lock().unwrap();
                assert!(storage.closed);
                assert!(storage.loads.is_empty());
                assert!(storage.saves.is_empty());
                (storage.revisioned_load_calls, storage.revisioned_save_calls)
            },
            (2, 0)
        );
        store.shutdown().await.unwrap();
    });
}

// Catches publishing a prepared value after a reentrant close fenced its session.
#[test]
fn close_during_capture_fences_publication_without_replacing_cached_value() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let runtime = DeterministicRuntime::with_step_budget(64);
    let backend = Memory::new();
    let (state, reducer) = SharedDocument::revisioned(7, backend.clone(), Binary);
    let session = reducer.session();
    let store = Store::new(state, reducer, runtime.runtime());
    let closer = document_owner(&store, session.clone());
    let closing_captures = Arc::new(AtomicUsize::new(0));
    let captured = Arc::clone(&closing_captures);
    let owner = SharedDocumentOwner::new(
        store.scope(
            StatePath::new(|state: &SharedDocumentState<u32, u64>| state, |state| state),
            ActionPath::new(
                |action: &SharedDocumentAction<u32, u64>| Some(action),
                |action| action,
            ),
        ),
        move |value: &u32| {
            if *value == 8 {
                assert_eq!(captured.fetch_add(1, Ordering::SeqCst), 0);
                let mut closing = Box::pin(closer.close());
                assert!(
                    closing
                        .as_mut()
                        .poll(&mut Context::from_waker(Waker::noop()))
                        .is_pending()
                );
            }
            *value
        },
        session,
    );
    let key = owner.key(SharedKeyIdentity::new::<Binary>(
        "close-during-capture",
        "binary",
    ));
    run_document_session_contract(&runtime, async {
        let cache = SharedKeyCache::new();
        let mut loading = Box::pin(cache.shared(key.clone(), || 0));
        assert!(
            loading
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending()
        );
        runtime.run_until_stalled().unwrap();
        backend.complete(0, Ok(present(1, 7)));
        runtime.run_until_stalled().unwrap();
        let shared = loading.await.unwrap();
        runtime.run_until_stalled().unwrap();
        assert_eq!(shared.read(|value| *value).unwrap(), 7);
        let statuses = owner.subscription_statuses(&key);
        assert_eq!(statuses.len(), 1);
        assert!(statuses[0].is_active());

        let _ = store.send(SharedDocumentAction::reload()).await.unwrap();
        runtime.run_until_stalled().unwrap();
        assert_eq!(backend.state.lock().unwrap().loads.len(), 1);
        backend.complete(0, Ok(present(2, 8)));
        runtime.run_until_stalled().unwrap();
        assert_eq!(closing_captures.load(Ordering::SeqCst), 1);
        assert_eq!(shared.read(|value| *value).unwrap(), 7);
        assert_eq!(
            store
                .read(|state| (
                    *state.value(),
                    state.revision(),
                    state.acknowledged_revision(),
                    state.backend_revision().copied(),
                    state.is_loading()
                ))
                .await
                .unwrap(),
            (8, 0, Some(0), Some(2), false)
        );
        let statuses = owner.subscription_statuses(&key);
        assert_eq!(statuses.len(), 1);
        let error = statuses[0].error().unwrap();
        assert!(error.is_ended());
        assert!(!error.is_cancelled());
        assert!(error.error().is_none());

        owner.close().await.unwrap();
        assert!(
            owner.subscription_statuses(&key)[0]
                .error()
                .unwrap()
                .is_ended()
        );
        assert_eq!(shared.read(|value| *value).unwrap(), 7);
        store.shutdown().await.unwrap();
    });
    let storage = backend.state.lock().unwrap();
    assert!(storage.closed);
    assert!(storage.loads.is_empty());
    assert!(storage.saves.is_empty());
    assert_eq!(
        (storage.revisioned_load_calls, storage.revisioned_save_calls),
        (2, 0)
    );
}

#[test]
fn superseded_key_load_failure_retains_physical_join() {
    let runtime = DeterministicRuntime::new();
    let backend = Memory::new();
    let (state, reducer) = SharedDocument::revisioned(7, backend.clone(), Binary);
    let session = reducer.session();
    let store = Store::new(state, reducer, runtime.runtime());
    let owner = document_owner(&store, session.clone());
    let key = owner.key(SharedKeyIdentity::new::<Binary>(
        "superseded-load-failure",
        "binary",
    ));
    block_on(runtime.run(async {
        let context = || Context::from_waker(Waker::noop());
        let mut first = Box::pin(key.load(SharedLoadContext::UserInitiated));
        assert!(first.as_mut().poll(&mut context()).is_pending());
        runtime.run_until_stalled().unwrap();
        assert_eq!(backend.state.lock().unwrap().loads.len(), 1);
        let mut second = Box::pin(key.load(SharedLoadContext::UserInitiated));
        assert!(second.as_mut().poll(&mut context()).is_pending());
        runtime.run_until_stalled().unwrap();
        assert_eq!(backend.state.lock().unwrap().loads.len(), 2);
        runtime.run_until_stalled().unwrap();
        let superseded = first.await.unwrap_err();
        assert!(superseded.is_superseded());
        assert!(superseded.error().is_none());
        assert!(superseded.source().is_none());
        assert!(second.as_mut().poll(&mut context()).is_pending());
        backend.complete(1, Err(StorageError(91)));
        runtime.run_until_stalled().unwrap();
        runtime.run_until_stalled().unwrap();
        let error = second.await.unwrap_err();
        let delivered = error.error().unwrap().downcast_ref::<Failure>().unwrap();
        assert!(std::ptr::eq(
            delivered,
            error.source().unwrap().downcast_ref::<Failure>().unwrap()
        ));
        let retained = delivered.clone();
        store
            .read(move |state| {
                let failure = state.load_error().unwrap();
                let source = failure.source().unwrap();
                let retained_source = retained.source().unwrap();
                assert_eq!(failure.operation(), Operation::Load);
                assert_eq!(failure.revision(), None);
                let cause = failure.error().downcast_ref::<StorageError>().unwrap();
                assert_eq!(cause.0, 91);
                assert!(std::ptr::eq(
                    cause,
                    source.downcast_ref::<StorageError>().unwrap()
                ));
                assert_eq!(retained.operation(), Operation::Load);
                assert_eq!(retained.revision(), None);
                assert!(std::ptr::eq(
                    cause,
                    retained.error().downcast_ref::<StorageError>().unwrap()
                ));
                assert!(std::ptr::eq(
                    cause,
                    retained_source.downcast_ref::<StorageError>().unwrap()
                ));
                assert_eq!(*state.value(), 7);
                assert_eq!(state.revision(), 0);
                assert_eq!(state.acknowledged_revision(), None);
                assert!(state.backend_revision().is_none());
                assert!(!state.is_loading());
            })
            .await
            .unwrap();
        assert_eq!(backend.state.lock().unwrap().loads.len(), 1);
        let mut closing = Box::pin(session.close());
        assert!(closing.as_mut().poll(&mut context()).is_pending());
        drop(closing);
        let mut resumed = Box::pin(session.close());
        assert!(resumed.as_mut().poll(&mut context()).is_pending());
        assert_eq!(backend.state.lock().unwrap().loads.len(), 1);
        backend.complete(0, Ok(present(2, 99)));
        resumed.await.unwrap();
        runtime.run_until_stalled().unwrap();
        let delivered = delivered.clone();
        store
            .read(move |state| {
                assert_eq!(*state.value(), 7);
                assert_eq!(state.revision(), 0);
                assert_eq!(state.acknowledged_revision(), None);
                assert!(state.backend_revision().is_none());
                assert!(!state.is_loading());
                let failure = state.load_error().unwrap();
                assert_eq!(failure.operation(), Operation::Load);
                assert_eq!(failure.revision(), None);
                assert!(std::ptr::eq(
                    failure.error().downcast_ref::<StorageError>().unwrap(),
                    delivered.error().downcast_ref::<StorageError>().unwrap(),
                ));
            })
            .await
            .unwrap();
        assert!(backend.state.lock().unwrap().loads.is_empty());
        assert!(backend.state.lock().unwrap().saves.is_empty());
        store.shutdown().await.unwrap();
    }))
    .unwrap();
}

/// Proves a replacement owner can both load and save while its reader stays attached.
async fn fresh_load_and_save(
    runtime: &DeterministicRuntime,
    backend: &Memory,
    owner: &SharedDocumentOwner<u32, u64>,
    name: &'static str,
) -> rca_core::SharedReader<u32> {
    let reader = cached_value(runtime, backend, owner, name, 41).await;
    assert_eq!(reader.read(|value| *value).unwrap(), 41);
    save_owned_value(runtime, backend, owner, name, 42).await;
    assert_eq!(reader.read(|value| *value).unwrap(), 42);
    reader
}

/// Completes one controlled key save after its captured bytes have been admitted.
async fn save_owned_value(
    runtime: &DeterministicRuntime,
    backend: &Memory,
    owner: &SharedDocumentOwner<u32, u64>,
    name: &'static str,
    value: u32,
) {
    use rca_core::SharedKey;
    let key = owner.key(SharedKeyIdentity::new::<Binary>(name, "binary"));
    let mut saving = Box::pin(key.save(&value));
    assert!(
        saving
            .as_mut()
            .poll(&mut std::task::Context::from_waker(std::task::Waker::noop()))
            .is_pending()
    );
    runtime.run_until_stalled().unwrap();
    backend.complete_save();
    saving.await.unwrap();
    runtime.run_until_stalled().unwrap();
}

// Catches close acknowledging quiescence while a reserved registration is still
// outside the lock, then losing the actual producer debt when start returns.
#[test]
fn close_during_start_retains_late_join_and_cancels_outside_control_lock() {
    use std::time::Duration;

    // Only a deadlock guard: channel acknowledgements determine the schedule.
    const DEADLOCK_GUARD: Duration = Duration::from_secs(10);
    let runtime = DeterministicRuntime::new();
    let (state, reducer) = SharedDocument::revisioned(7, Memory::new(), Binary);
    let session = reducer.session();
    let store = Store::new(state, reducer, runtime.runtime());
    let owner = document_owner(&store, session.clone());
    let (started, started_rx) = std::sync::mpsc::channel();
    let (resume, resume_rx) = std::sync::mpsc::channel();
    let (registered, registered_rx) = std::sync::mpsc::channel();
    let (completion, join) = controlled_future();
    let cancellations = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let cancelling = Arc::clone(&cancellations);
    let reentrant = session.clone();
    let registration = std::thread::spawn(move || {
        let result = owner.observe_changes::<StorageError, _, _, _>(|_| {
            started.send(()).unwrap();
            resume_rx.recv_timeout(DEADLOCK_GUARD).unwrap();
            (
                SharedUpdate::Present(()),
                move || {
                    cancelling.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    let mut closing = Box::pin(reentrant.close());
                    assert!(
                        closing
                            .as_mut()
                            .poll(&mut std::task::Context::from_waker(std::task::Waker::noop()))
                            .is_pending()
                    );
                },
                async move { join.await.map_err(|_cause| StorageError(81))? },
            )
        });
        drop(owner);
        registered.send(()).unwrap();
        result
    });
    started_rx.recv_timeout(DEADLOCK_GUARD).unwrap();
    let mut first = Box::pin(session.close());
    assert!(
        first
            .as_mut()
            .poll(&mut std::task::Context::from_waker(std::task::Waker::noop()))
            .is_pending()
    );
    resume.send(()).unwrap();
    registered_rx.recv_timeout(DEADLOCK_GUARD).unwrap();
    let guard = registration.join().unwrap().unwrap();
    assert_eq!(cancellations.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert!(guard.status().is_cancelled());
    runtime.run_until_stalled().unwrap();
    assert!(
        first
            .as_mut()
            .poll(&mut std::task::Context::from_waker(std::task::Waker::noop()))
            .is_pending()
    );
    drop(first);
    let mut second = Box::pin(session.close());
    let mut third = Box::pin(session.close());
    assert!(
        second
            .as_mut()
            .poll(&mut std::task::Context::from_waker(std::task::Waker::noop()))
            .is_pending()
    );
    assert!(
        third
            .as_mut()
            .poll(&mut std::task::Context::from_waker(std::task::Waker::noop()))
            .is_pending()
    );
    completion.complete(Err(StorageError(82))).unwrap();
    for closing in [second, third] {
        let outcome = block_on(closing).unwrap_err();
        assert_eq!(outcome.failures().len(), 1);
        assert_eq!(
            outcome.failures()[0]
                .error()
                .downcast_ref::<StorageError>()
                .unwrap()
                .0,
            82
        );
    }
    assert_eq!(cancellations.load(std::sync::atomic::Ordering::SeqCst), 1);
    block_on(runtime.run(store.shutdown())).unwrap().unwrap();
}

// Catches dropping a key future leaving a live logical load, resurrecting its
// value after physical completion, or treating cancellation as physical join.
#[test]
fn dropped_key_load_clears_loading_but_session_still_joins_physical_operation() {
    let runtime = DeterministicRuntime::new();
    let backend = Memory::new();
    let (state, reducer) = SharedDocument::revisioned(7, backend.clone(), Binary);
    let session = reducer.session();
    let store = Store::new(state, reducer, runtime.runtime());
    let owner = document_owner(&store, session.clone());
    let key = owner.key(SharedKeyIdentity::new::<Binary>("dropped-load", "binary"));
    block_on(runtime.run(async {
        let mut load = Box::pin(async move { key.load(SharedLoadContext::UserInitiated).await });
        assert!(
            load.as_mut()
                .poll(&mut std::task::Context::from_waker(std::task::Waker::noop()))
                .is_pending()
        );
        runtime.run_until_stalled().unwrap();
        assert!(store.read(SharedDocumentState::is_loading).await.unwrap());
        assert_eq!(
            store
                .read(move |state| {
                    assert!(state.is_loading());
                    drop(load);
                    // The queued KeyCancel cannot run during this actor query.
                    assert!(!state.is_loading());
                    (
                        *state.value(),
                        state.revision(),
                        state.acknowledged_revision(),
                    )
                })
                .await
                .unwrap(),
            (7, 0, None)
        );
        assert_eq!(backend.state.lock().unwrap().loads.len(), 1);
        runtime.run_until_stalled().unwrap();
        assert!(!store.read(SharedDocumentState::is_loading).await.unwrap());
        let mut closing = Box::pin(session.close());
        assert!(
            closing
                .as_mut()
                .poll(&mut std::task::Context::from_waker(std::task::Waker::noop()))
                .is_pending()
        );
        backend.complete(0, Ok(present(2, 99)));
        closing.await.unwrap();
        assert_eq!(
            store
                .read(|state| (*state.value(), state.acknowledged_revision()))
                .await
                .unwrap(),
            (7, None)
        );
        store.shutdown().await.unwrap();
    }))
    .unwrap();
}

// A plain backend exposes no revision identity, even while its save is pending.
// Reloading old bytes must finish without overwriting or acknowledging the edit.
#[test]
fn pending_plain_save_does_not_defer_an_unrevisioned_reload() {
    /// Delays plain saves using the fixture queue without exposing revision identity.
    struct QueuedPlainSave(Memory);
    impl Backend for QueuedPlainSave {
        type Error = StorageError;

        async fn load(&self) -> Result<Option<Vec<u8>>, StorageError> {
            self.0.load().await
        }

        async fn save(&self, bytes: Vec<u8>) -> Result<(), StorageError> {
            // Reuse the controlled I/O queue, but expose no revision to RCA.
            let expected = self.0.state.lock().unwrap().revision;
            match self.0.compare_and_save(expected, bytes).await? {
                CompareSave::Saved(_) => Ok(()),
                CompareSave::Conflict(_) => Err(StorageError(91)),
            }
        }
    }
    impl BackendSession for QueuedPlainSave {
        async fn close(&self) -> Result<(), StorageError> {
            self.0.close().await
        }
    }

    let runtime = DeterministicRuntime::new();
    let backend = Memory::new();
    backend.state.lock().unwrap().bytes = Some(7_u32.to_le_bytes().to_vec());
    let (state, reducer) = SharedDocument::new(7, QueuedPlainSave(backend.clone()), Binary);
    let session = reducer.session();
    let store = Store::new(state, reducer, runtime.runtime());
    block_on(runtime.run(async {
        let _ = store.send(SharedDocumentAction::reload()).await.unwrap();
        runtime.run_until_stalled().unwrap();
        let (edit, mut receipt) = SharedDocumentAction::set_and_flush(8);
        let _ = store.send(edit).await.unwrap();
        runtime.run_until_stalled().unwrap();
        assert_eq!(backend.state.lock().unwrap().saves.len(), 1);

        let _ = store.send(SharedDocumentAction::reload()).await.unwrap();
        runtime.run_until_stalled().unwrap();
        store
            .read(|state| {
                assert_eq!(*state.value(), 8);
                assert_eq!(state.revision(), 1);
                assert_eq!(state.acknowledged_revision(), Some(0));
                assert!(!state.is_loading());
                assert!(state.backend_revision().is_none());
                assert!(state.conflict().is_none());
                assert!(state.load_error().is_none());
            })
            .await
            .unwrap();
        assert!(
            Pin::new(&mut receipt)
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending()
        );
        assert_eq!(backend.state.lock().unwrap().saves.len(), 1);
        assert_eq!(
            backend.state.lock().unwrap().bytes,
            Some(7_u32.to_le_bytes().to_vec())
        );

        backend.complete_save();
        assert_eq!(receipt.await.unwrap().revision, 1);
        assert_eq!(
            store
                .read(|state| (*state.value(), state.acknowledged_revision()))
                .await
                .unwrap(),
            (8, Some(1))
        );
        session.close().await.unwrap();
        store.shutdown().await.unwrap();
    }))
    .unwrap();
    let storage = backend.state.lock().unwrap();
    assert!(storage.closed && storage.loads.is_empty() && storage.saves.is_empty());
    assert_eq!(storage.bytes, Some(8_u32.to_le_bytes().to_vec()));
}

// An unfinished rejected load still sends its cancellation on drop. That
// mismatched session must not disturb the destination's valid pending load.
#[test]
fn dropping_mismatched_key_load_preserves_live_destination_load() {
    let runtime = DeterministicRuntime::new();
    let backend_a = Memory::new();
    let backend_b = Memory::new();
    let (state_a, reducer_a) = SharedDocument::revisioned(7, backend_a.clone(), Binary);
    let (state_b, reducer_b) = SharedDocument::revisioned(23, backend_b.clone(), Binary);
    let session_a = reducer_a.session();
    let session_b = reducer_b.session();
    let store_a = Store::new(state_a, reducer_a, runtime.runtime());
    let store_b = Store::new(state_b, reducer_b, runtime.runtime());
    let owner_a = document_owner(&store_a, session_a);
    let owner_b = document_owner(&store_b, session_b.clone());
    let mismatched_owner = document_owner(&store_a, session_b);
    let key_a = owner_a.key(SharedKeyIdentity::new::<Binary>("valid-load", "binary"));
    let wrong_key =
        mismatched_owner.key(SharedKeyIdentity::new::<Binary>("wrong-session", "binary"));

    block_on(runtime.run(async {
        let context = || Context::from_waker(Waker::noop());
        let mut valid_load = Box::pin(key_a.load(SharedLoadContext::UserInitiated));
        assert!(valid_load.as_mut().poll(&mut context()).is_pending());
        runtime.run_until_stalled().unwrap();
        assert_eq!(backend_a.state.lock().unwrap().loads.len(), 1);

        let mut wrong_load = Box::pin(wrong_key.load(SharedLoadContext::UserInitiated));
        assert!(wrong_load.as_mut().poll(&mut context()).is_pending());
        runtime.run_until_stalled().unwrap();
        // KeyLoad has been rejected, but the future has not consumed its reply.
        // Its unfinished cancellation guard sends KeyCancel to A with B's session.
        drop(wrong_load);
        runtime.run_until_stalled().unwrap();

        store_a
            .read(|state| {
                assert_eq!(*state.value(), 7);
                assert!(state.is_loading());
                assert!(state.load_error().is_none());
                assert_eq!(state.acknowledged_revision(), None);
            })
            .await
            .unwrap();
        assert!(valid_load.as_mut().poll(&mut context()).is_pending());
        for (backend, expected_loads) in [(&backend_a, 1), (&backend_b, 0)] {
            let storage = backend.state.lock().unwrap();
            assert_eq!(storage.loads.len(), expected_loads);
            assert_eq!(storage.revisioned_load_calls, expected_loads);
            assert_eq!(storage.revisioned_save_calls, 0);
            assert!(storage.saves.is_empty());
            assert_eq!(storage.revision, 1);
            assert!(storage.bytes.is_none());
            assert!(!storage.closed);
        }

        backend_a.complete(0, Ok(present(2, 99)));
        runtime.run_until_stalled().unwrap();
        assert_eq!(valid_load.await.unwrap(), Some(99));
        for (store, value, acknowledged, backend_revision) in
            [(&store_a, 99, Some(0), Some(2)), (&store_b, 23, None, None)]
        {
            store
                .read(move |state| {
                    assert_eq!(*state.value(), value);
                    assert_eq!(state.revision(), 0);
                    assert_eq!(state.acknowledged_revision(), acknowledged);
                    assert_eq!(state.backend_revision().copied(), backend_revision);
                    assert!(!state.is_loading());
                    assert!(state.load_error().is_none());
                    assert!(state.document().failure().is_none());
                    assert!(state.conflict().is_none());
                    assert_eq!(state.document().pending_barrier_count(), 0);
                })
                .await
                .unwrap();
        }
        assert!(backend_a.state.lock().unwrap().loads.is_empty());
        assert!(backend_b.state.lock().unwrap().loads.is_empty());
        drop(wrong_key);
        // Closing this owner would fence B while routing Abort to A.
        drop(mismatched_owner);
        drop(key_a);
        owner_a.close().await.unwrap();
        owner_b.close().await.unwrap();
        store_a.shutdown().await.unwrap();
        store_b.shutdown().await.unwrap();
    }))
    .unwrap();
}

/// Transfers owned `Cell` values through the same four-byte format without `Sync`.
struct CellCodec;
impl Codec<std::cell::Cell<u32>> for CellCodec {
    type Error = StorageError;
    fn encode(&self, value: &std::cell::Cell<u32>) -> Result<Vec<u8>, StorageError> {
        Binary.encode(&value.get())
    }
    fn decode(&self, bytes: &[u8]) -> Result<std::cell::Cell<u32>, StorageError> {
        Binary.decode(bytes).map(std::cell::Cell::new)
    }
}

// Cell is Send and Clone but not Sync; plain and revisioned Documents and their
// actual typed keys must transfer owned values without requiring shared access.
#[test]
fn shared_document_accepts_send_non_sync_cell_values_through_reload_and_save() {
    use rca_core::SharedKey;
    /// A mutable owned value that is `Send` and `Clone`, but cannot be shared as `Sync`.
    type Value = std::cell::Cell<u32>;
    /// Makes the transfer bound part of compilation without requiring `Sync`.
    fn transferable<T: Send>() {}
    transferable::<SharedDocumentState<Value, u64>>();
    transferable::<SharedDocumentAction<Value, u64>>();
    transferable::<SharedDocument<Value, u64>>();
    transferable::<rca_persistence::DocumentKey<Value, u64>>();
    let _plain = SharedDocument::new(Value::new(3), Memory::new(), CellCodec);
    let runtime = DeterministicRuntime::new();
    let backend = Memory::new();
    let (state, reducer) = SharedDocument::revisioned(Value::new(7), backend.clone(), CellCodec);
    let session = reducer.session();
    let store = Store::new(state, reducer, runtime.runtime());
    let owner = SharedDocumentOwner::new(
        store.scope(
            StatePath::new(
                |state: &SharedDocumentState<Value, u64>| state,
                |state| state,
            ),
            ActionPath::new(
                |action: &SharedDocumentAction<Value, u64>| Some(action),
                |action| action,
            ),
        ),
        |value: &Value| Value::new(value.get()),
        session,
    );
    let key = owner.key(SharedKeyIdentity::new::<CellCodec>("cell", "binary"));
    block_on(runtime.run(async {
        let mut load = Box::pin(key.load(SharedLoadContext::UserInitiated));
        assert!(
            load.as_mut()
                .poll(&mut std::task::Context::from_waker(std::task::Waker::noop()))
                .is_pending()
        );
        runtime.run_until_stalled().unwrap();
        backend.complete(0, Ok(present(1, 11)));
        runtime.run_until_stalled().unwrap();
        assert_eq!(load.await.unwrap().unwrap().get(), 11);
        let value = Value::new(23);
        let mut save = Box::pin(key.save(&value));
        assert!(
            save.as_mut()
                .poll(&mut std::task::Context::from_waker(std::task::Waker::noop()))
                .is_pending()
        );
        runtime.run_until_stalled().unwrap();
        value.set(99);
        backend.complete_save();
        save.await.unwrap();
        assert_eq!(
            store
                .read(|state| (state.value().get(), state.acknowledged_revision()))
                .await
                .unwrap(),
            (23, Some(1))
        );
        owner.close().await.unwrap();
        store.shutdown().await.unwrap();
    }))
    .unwrap();
}

// Catches cancellation authorising a load already in flight, or conflating a
// producer's drained normal terminal state with cancellation/failure.
#[test]
fn final_producer_guard_drop_fences_inflight_value_and_normal_drain_is_distinct() {
    for cancel in [false, true] {
        let runtime = DeterministicRuntime::new();
        let backend = Memory::new();
        let (state, reducer) = SharedDocument::revisioned(7, backend.clone(), Binary);
        let session = reducer.session();
        let store = Store::new(state, reducer, runtime.runtime());
        let owner = document_owner(&store, session);
        let cancellations = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let cancelling = Arc::clone(&cancellations);
        let guard = owner
            .observe_changes::<StorageError, _, _, _>(|producer| {
                assert!(producer.finish());
                (
                    SharedUpdate::Present(()),
                    move || {
                        cancelling.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    },
                    std::future::ready(Ok(())),
                )
            })
            .unwrap();
        block_on(runtime.run(async {
            runtime.run_until_stalled().unwrap();
            assert!(guard.status().is_active());
            let alias = guard.clone();
            drop(guard);
            assert_eq!(cancellations.load(std::sync::atomic::Ordering::SeqCst), 0);
            let retained = if cancel {
                drop(alias);
                None
            } else {
                Some(alias)
            };
            backend.complete(0, Ok(present(2, 99)));
            runtime.run_until_stalled().unwrap();
            if let Some(guard) = retained {
                assert!(guard.status().is_finished());
                assert!(!guard.status().is_cancelled());
                assert!(guard.status().error().is_none());
                assert_eq!(store.read(|state| *state.value()).await.unwrap(), 99);
            } else {
                assert_eq!(
                    store
                        .read(|state| (*state.value(), state.is_loading()))
                        .await
                        .unwrap(),
                    (7, false)
                );
            }
            assert_eq!(cancellations.load(std::sync::atomic::Ordering::SeqCst), 1);
            owner.close().await.unwrap();
            store.shutdown().await.unwrap();
        }))
        .unwrap();
    }
}

/// Captures bytes and revisions when a read starts, not when it completes.
///
/// Its close future delegates to `Memory` and does not wait for captured reads.
/// Tests release captured reads before close. This fixture does not prove that
/// close waits for pending reads.
#[derive(Clone)]
struct CapturedRead {
    memory: Memory,
    reads: Arc<Mutex<VecDeque<CapturedReadCompletion>>>,
}

type CapturedReadCompletion = (BackendSnapshot<u64>, Completion<BackendSnapshot<u64>>);

impl CapturedRead {
    fn new() -> Self {
        let memory = Memory::new();
        memory.state.lock().unwrap().bytes = Some(7_u32.to_le_bytes().to_vec());
        Self {
            memory,
            reads: Arc::new(Mutex::new(VecDeque::new())),
        }
    }
    async fn read(&self) -> BackendSnapshot<u64> {
        let (completion, future) = controlled_future();
        let snapshot = {
            let state = self.memory.state.lock().unwrap();
            BackendSnapshot {
                revision: state.revision,
                bytes: state.bytes.clone(),
            }
        };
        self.reads.lock().unwrap().push_back((snapshot, completion));
        future.await.unwrap()
    }
    fn release(&self) {
        let (snapshot, completion) = self.reads.lock().unwrap().pop_front().unwrap();
        let _ = completion.complete(snapshot);
    }
}
impl Backend for CapturedRead {
    type Error = StorageError;
    async fn load(&self) -> Result<Option<Vec<u8>>, StorageError> {
        Ok(self.read().await.bytes)
    }
    async fn save(&self, bytes: Vec<u8>) -> Result<(), StorageError> {
        self.memory.save(bytes).await
    }
}
impl BackendSession for CapturedRead {
    async fn close(&self) -> Result<(), StorageError> {
        self.memory.close().await
    }
}
impl RevisionBackend for CapturedRead {
    type Revision = u64;
    async fn load_revisioned(&self) -> Result<BackendSnapshot<u64>, StorageError> {
        Ok(self.read().await)
    }
    async fn compare_and_save(
        &self,
        expected: u64,
        bytes: Vec<u8>,
    ) -> Result<CompareSave<u64>, StorageError> {
        self.memory.compare_and_save(expected, bytes).await
    }
}

fn acknowledged_save_beats_captured_read<R: Clone + Eq + Send + 'static>(
    construct: impl FnOnce(CapturedRead) -> (SharedDocumentState<u32, R>, SharedDocument<u32, R>),
    revisioned: bool,
) {
    let runtime = DeterministicRuntime::new();
    let backend = CapturedRead::new();
    let (state, reducer) = construct(backend.clone());
    let session = reducer.session();
    let store = Store::new(state, reducer, runtime.runtime());
    block_on(runtime.run(async {
        let _ = store.send(SharedDocumentAction::reload()).await.unwrap();
        runtime.run_until_stalled().unwrap();
        backend.release();
        runtime.run_until_stalled().unwrap();
        let _ = store.send(SharedDocumentAction::reload()).await.unwrap();
        runtime.run_until_stalled().unwrap();
        let (edit, receipt) = SharedDocumentAction::set_and_flush(8);
        let _ = store.send(edit).await.unwrap();
        runtime.run_until_stalled().unwrap();
        if revisioned {
            backend.memory.complete_save();
        }
        receipt.await.unwrap();
        backend.release();
        runtime.run_until_stalled().unwrap();
        assert_eq!(
            store
                .read(|state| (*state.value(), state.acknowledged_revision()))
                .await
                .unwrap(),
            (8, Some(1))
        );
        let (flush, receipt) = SharedDocumentAction::flush();
        let _ = store.send(flush).await.unwrap();
        receipt.await.unwrap();
        assert_eq!(
            Binary
                .decode(backend.memory.state.lock().unwrap().bytes.as_ref().unwrap())
                .unwrap(),
            8
        );
        store.shutdown().await.unwrap();
    }))
    .unwrap();
    block_on(session.close()).unwrap();
}

#[test]
fn acknowledged_revisioned_save_cannot_be_rolled_back_by_a_pre_save_read() {
    acknowledged_save_beats_captured_read(
        |backend| SharedDocument::revisioned(0, backend, Binary),
        true,
    );
}
#[test]
fn acknowledged_plain_save_cannot_be_rolled_back_by_a_pre_save_read() {
    acknowledged_save_beats_captured_read(|backend| SharedDocument::new(0, backend, Binary), false);
}

fn absence_then_recreation<R: Clone + Eq + Send + 'static>(
    construct: impl FnOnce(CapturedRead) -> (SharedDocumentState<u32, R>, SharedDocument<u32, R>),
    revisioned: bool,
) {
    let runtime = DeterministicRuntime::new();
    let backend = CapturedRead::new();
    let (state, reducer) = construct(backend.clone());
    assert!(state.backend_revision().is_none());
    let session = reducer.session();
    let store = Store::new(state, reducer, runtime.runtime());
    block_on(runtime.run(async {
        let _ = store.send(SharedDocumentAction::reload()).await.unwrap();
        runtime.run_until_stalled().unwrap();
        backend.release();
        runtime.run_until_stalled().unwrap();
        assert_eq!(
            store
                .read(|state| state.backend_revision().is_some())
                .await
                .unwrap(),
            revisioned
        );
        let (edit, receipt) = SharedDocumentAction::set_and_flush(8);
        let _ = store.send(edit).await.unwrap();
        runtime.run_until_stalled().unwrap();
        if revisioned {
            backend.memory.complete_save();
        }
        receipt.await.unwrap();
        assert_eq!(
            store
                .read(|state| state.backend_revision().is_some())
                .await
                .unwrap(),
            revisioned
        );
        {
            let mut storage = backend.memory.state.lock().unwrap();
            storage.revision += 1;
            storage.bytes = None;
        };
        let _ = store.send(SharedDocumentAction::reload()).await.unwrap();
        runtime.run_until_stalled().unwrap();
        backend.release();
        runtime.run_until_stalled().unwrap();
        assert_eq!(
            store
                .read(|state| (*state.value(), state.acknowledged_revision()))
                .await
                .unwrap(),
            (8, None)
        );
        backend.memory.external_write(9);
        let _ = store.send(SharedDocumentAction::reload()).await.unwrap();
        runtime.run_until_stalled().unwrap();
        backend.release();
        runtime.run_until_stalled().unwrap();
        assert_eq!(
            store
                .read(|state| (*state.value(), state.conflict().is_none()))
                .await
                .unwrap(),
            (9, true)
        );
        store.shutdown().await.unwrap();
    }))
    .unwrap();
    block_on(session.close()).unwrap();
}
#[test]
fn revisioned_delete_then_recreate_does_not_invent_a_local_edit() {
    absence_then_recreation(
        |backend| SharedDocument::revisioned(0, backend, Binary),
        true,
    );
}
#[test]
fn plain_delete_then_recreate_does_not_invent_a_local_edit() {
    absence_then_recreation(|backend| SharedDocument::new(0, backend, Binary), false);
}

#[test]
fn producer_own_write_echo_waits_for_matching_acknowledgement() {
    let runtime = DeterministicRuntime::new();
    let backend = Memory::new();
    let (state, reducer) = SharedDocument::revisioned(7, backend.clone(), Binary);
    let session = reducer.session();
    let store = Store::new(state, reducer, runtime.runtime());
    let owner = document_owner(&store, session);
    block_on(runtime.run(async {
        let _ = store.send(SharedDocumentAction::reload()).await.unwrap();
        runtime.run_until_stalled().unwrap();
        backend.complete(0, Ok(present(1, 7)));
        runtime.run_until_stalled().unwrap();
        let (edit, receipt) = SharedDocumentAction::set_and_flush(8);
        let _ = store.send(edit).await.unwrap();
        runtime.run_until_stalled().unwrap();
        let (ack, result) = backend.commit_save();
        let subscription = owner
            .observe_changes::<StorageError, _, _, _>(|_| {
                (SharedUpdate::Present(()), || {}, std::future::ready(Ok(())))
            })
            .unwrap();
        runtime.run_until_stalled().unwrap();
        backend.complete(0, Ok(present(2, 8)));
        runtime.run_until_stalled().unwrap();
        assert!(subscription.status().is_active());
        assert!(subscription.status().error().is_none());
        ack.complete(Ok(result)).unwrap();
        receipt.await.unwrap();
        runtime.run_until_stalled().unwrap();
        assert!(subscription.status().is_active());
        assert_eq!(
            store
                .read(|state| (
                    *state.value(),
                    state.acknowledged_revision(),
                    state.conflict().is_none()
                ))
                .await
                .unwrap(),
            (8, Some(1), true)
        );
        owner.close().await.unwrap();
        store.shutdown().await.unwrap();
    }))
    .unwrap();
}

#[test]
fn captured_snapshot_bridge_delivers_initial_before_synchronous_latest() {
    struct Recording(Arc<Mutex<Vec<u32>>>);
    impl Codec<u32> for Recording {
        type Error = StorageError;
        fn encode(&self, value: &u32) -> Result<Vec<u8>, StorageError> {
            Binary.encode(value)
        }
        fn decode(&self, bytes: &[u8]) -> Result<u32, StorageError> {
            let value = Binary.decode(bytes)?;
            self.0.lock().unwrap().push(value);
            Ok(value)
        }
    }
    let runtime = DeterministicRuntime::new();
    let decoded = Arc::new(Mutex::new(Vec::new()));
    let backend = Memory::new();
    let (state, reducer) =
        SharedDocument::revisioned(0, backend.clone(), Recording(Arc::clone(&decoded)));
    let session = reducer.session();
    let store = Store::new(state, reducer, runtime.runtime());
    let owner = document_owner(&store, session);
    let guard = owner
        .observe_snapshots::<StorageError, _, _, _>(|producer| {
            let initial = BackendSnapshot {
                revision: Some(1),
                bytes: Some(7_u32.to_le_bytes().to_vec()),
            };
            backend.external_write(9);
            producer
                .publish(SharedUpdate::Present(BackendSnapshot {
                    revision: Some(2),
                    bytes: Some(9_u32.to_le_bytes().to_vec()),
                }))
                .unwrap();
            (
                SharedUpdate::Present(initial),
                || {},
                std::future::ready(Ok(())),
            )
        })
        .unwrap();
    block_on(runtime.run(async {
        runtime.run_until_stalled().unwrap();
        assert_eq!(*decoded.lock().unwrap(), vec![7, 9]);
        assert_eq!(
            store
                .read(|state| (*state.value(), state.backend_revision().copied()))
                .await
                .unwrap(),
            (9, Some(2))
        );
        assert!(guard.status().is_active());
        assert!(backend.state.lock().unwrap().loads.is_empty());
        owner.close().await.unwrap();
        store.shutdown().await.unwrap();
    }))
    .unwrap();
}

#[test]
fn child_interpreter_uses_the_explicit_fresh_generations_resource_after_old_join() {
    let runtime = DeterministicRuntime::new();
    let (_, interpreter) = SharedDocument::revisioned(7, Memory::new(), Binary);
    block_on(interpreter.session().close()).unwrap();
    let backend = Memory::new();
    let (state, resource_owner) = SharedDocument::revisioned(40, backend.clone(), Binary);
    let session = resource_owner.session();
    let store = Store::new(state, interpreter, runtime.runtime());
    let owner = document_owner(&store, session);
    let key = owner.key(SharedKeyIdentity::new::<Binary>(
        "fresh-generation",
        "binary",
    ));
    block_on(runtime.run(async {
        let mut load = Box::pin(key.load(SharedLoadContext::UserInitiated));
        assert!(
            load.as_mut()
                .poll(&mut std::task::Context::from_waker(std::task::Waker::noop()))
                .is_pending()
        );
        runtime.run_until_stalled().unwrap();
        assert_eq!(backend.state.lock().unwrap().loads.len(), 1);
        backend.complete(0, Ok(present(1, 41)));
        assert_eq!(load.await.unwrap(), Some(41));
        owner.close().await.unwrap();
        store.shutdown().await.unwrap();
    }))
    .unwrap();
    drop(resource_owner);
}

// Catches committing a prepared value after its Clone closed the owner or
// dropped the waiting key load. Neither cancellation discharges older physical I/O.
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "keeps both final-fence races and retained physical joins in one lifecycle trace"
)]
fn final_load_commit_rechecks_session_and_key_cancellation() {
    type OnClone = Arc<Mutex<Option<Box<dyn FnOnce() + Send>>>>;
    struct Value {
        number: u32,
        on_clone: OnClone,
    }
    impl Clone for Value {
        fn clone(&self) -> Self {
            if self.number == 99 {
                let callback = self.on_clone.lock().unwrap().take();
                if let Some(callback) = callback {
                    callback();
                }
            }
            Self {
                number: self.number,
                on_clone: Arc::clone(&self.on_clone),
            }
        }
    }
    struct Encoding(OnClone);
    impl Codec<Value> for Encoding {
        type Error = StorageError;
        fn encode(&self, value: &Value) -> Result<Vec<u8>, StorageError> {
            Binary.encode(&value.number)
        }
        fn decode(&self, bytes: &[u8]) -> Result<Value, StorageError> {
            Ok(Value {
                number: Binary.decode(bytes)?,
                on_clone: Arc::clone(&self.0),
            })
        }
    }

    for close_owner in [true, false] {
        let runtime = DeterministicRuntime::with_step_budget(64);
        let backend = Memory::new();
        let on_clone: OnClone = Arc::new(Mutex::new(None));
        let (state, reducer) = SharedDocument::revisioned(
            Value {
                number: 7,
                on_clone: Arc::clone(&on_clone),
            },
            backend.clone(),
            Encoding(Arc::clone(&on_clone)),
        );
        let session = reducer.session();
        let store = Store::new(state, reducer, runtime.runtime());
        let owner = Arc::new(SharedDocumentOwner::new(
            store.scope(
                StatePath::new(
                    |state: &SharedDocumentState<Value, u64>| state,
                    |state| state,
                ),
                ActionPath::new(
                    |action: &SharedDocumentAction<Value, u64>| Some(action),
                    |action| action,
                ),
            ),
            Clone::clone,
            session,
        ));
        let key = owner.key(SharedKeyIdentity::new::<Encoding>(
            "final-load-fence",
            "binary",
        ));
        run_document_session_contract(&runtime, async {
            let context = || Context::from_waker(Waker::noop());
            let _ = store.send(SharedDocumentAction::reload()).await.unwrap();
            runtime.run_until_stalled().unwrap();
            let mut loading = Some(Box::pin(async move {
                key.load(SharedLoadContext::UserInitiated).await
            }));
            assert!(
                loading
                    .as_mut()
                    .unwrap()
                    .as_mut()
                    .poll(&mut context())
                    .is_pending()
            );
            runtime.run_until_stalled().unwrap();
            assert_eq!(backend.state.lock().unwrap().loads.len(), 2);

            if close_owner {
                let closing_owner = Arc::clone(&owner);
                *on_clone.lock().unwrap() = Some(Box::new(move || {
                    let mut closing = Box::pin(closing_owner.close());
                    assert!(
                        closing
                            .as_mut()
                            .poll(&mut Context::from_waker(Waker::noop()))
                            .is_pending()
                    );
                }));
            } else {
                let cancelled = loading.take().unwrap();
                *on_clone.lock().unwrap() = Some(Box::new(move || drop(cancelled)));
            }

            backend.complete(1, Ok(present(2, 99)));
            runtime.run_until_stalled().unwrap();
            assert!(on_clone.lock().unwrap().is_none());
            if let Some(loading) = loading {
                let error = loading
                    .await
                    .err()
                    .expect("closed session rejects prepared value");
                // Closing the owner also aborts its inherited reply delivery.
                assert!(error.is_ended());
                assert!(!error.is_cancelled());
                assert!(error.error().is_none() && error.source().is_none());
            }
            assert_eq!(
                store
                    .read(|state| (
                        state.value().number,
                        state.revision(),
                        state.backend_revision().copied(),
                        state.is_loading(),
                        state.document().is_aborted(),
                    ))
                    .await
                    .unwrap(),
                (7, 0, None, false, close_owner),
            );
            assert_eq!(backend.state.lock().unwrap().loads.len(), 1);
            assert!(!backend.state.lock().unwrap().closed);

            let mut closing = Box::pin(owner.close());
            assert!(closing.as_mut().poll(&mut context()).is_pending());
            runtime.run_until_stalled().unwrap();
            assert!(closing.as_mut().poll(&mut context()).is_pending());
            backend.complete(0, Ok(present(1, 41)));
            closing.await.unwrap();
            assert_eq!(store.read(|state| state.value().number).await.unwrap(), 7);
            store.shutdown().await.unwrap();
        });
        let storage = backend.state.lock().unwrap();
        assert!(storage.closed && storage.loads.is_empty() && storage.saves.is_empty());
        assert_eq!(
            (storage.revisioned_load_calls, storage.revisioned_save_calls),
            (2, 0)
        );
    }
}

#[test]
fn cancellation_during_final_value_preparation_cannot_commit_the_prepared_value() {
    struct Value {
        number: u32,
        cancel: Arc<Mutex<Option<rca_persistence::DocumentSubscription>>>,
    }
    impl Clone for Value {
        fn clone(&self) -> Self {
            if self.number == 99
                && let Some(guard) = self.cancel.lock().unwrap().as_ref()
            {
                guard.cancel();
            }
            Self {
                number: self.number,
                cancel: Arc::clone(&self.cancel),
            }
        }
    }
    struct Encoding(Arc<Mutex<Option<rca_persistence::DocumentSubscription>>>);
    impl Codec<Value> for Encoding {
        type Error = StorageError;
        fn encode(&self, value: &Value) -> Result<Vec<u8>, StorageError> {
            Binary.encode(&value.number)
        }
        fn decode(&self, bytes: &[u8]) -> Result<Value, StorageError> {
            Ok(Value {
                number: Binary.decode(bytes)?,
                cancel: Arc::clone(&self.0),
            })
        }
    }
    let runtime = DeterministicRuntime::new();
    let backend = Memory::new();
    let cancellation = Arc::new(Mutex::new(None));
    let (state, reducer) = SharedDocument::revisioned(
        Value {
            number: 7,
            cancel: Arc::clone(&cancellation),
        },
        backend.clone(),
        Encoding(Arc::clone(&cancellation)),
    );
    let session = reducer.session();
    let store = Store::new(state, reducer, runtime.runtime());
    let owner = SharedDocumentOwner::new(
        store.scope(
            StatePath::new(
                |state: &SharedDocumentState<Value, u64>| state,
                |state| state,
            ),
            ActionPath::new(
                |action: &SharedDocumentAction<Value, u64>| Some(action),
                |action| action,
            ),
        ),
        Clone::clone,
        session,
    );
    let guard = owner
        .observe_changes::<StorageError, _, _, _>(|_| {
            (SharedUpdate::Present(()), || {}, std::future::ready(Ok(())))
        })
        .unwrap();
    *cancellation.lock().unwrap() = Some(guard.clone());
    block_on(runtime.run(async {
        runtime.run_until_stalled().unwrap();
        assert!(guard.status().is_active());
        backend.complete(0, Ok(present(2, 99)));
        runtime.run_until_stalled().unwrap();
        assert!(guard.status().is_cancelled());
        assert_eq!(store.read(|state| state.value().number).await.unwrap(), 7);
        owner.close().await.unwrap();
        store.shutdown().await.unwrap();
    }))
    .unwrap();
}

#[test]
fn newest_load_error_survives_an_older_success_literal() {
    let runtime = DeterministicRuntime::new();
    let backend = Memory::new();
    let (state, reducer) = SharedDocument::revisioned(7, backend.clone(), Binary);
    let store = Store::new(state, reducer, runtime.runtime());
    block_on(runtime.run(async {
        for _ in 0..2 {
            let _ = store.send(SharedDocumentAction::reload()).await.unwrap();
            runtime.run_until_stalled().unwrap();
        }
        backend.complete(1, Err(StorageError(91)));
        runtime.run_until_stalled().unwrap();
        backend.complete(0, Ok(present(2, 99)));
        runtime.run_until_stalled().unwrap();
        assert_eq!(
            store
                .read(|state| (
                    *state.value(),
                    state.is_loading(),
                    state
                        .load_error()
                        .unwrap()
                        .error()
                        .downcast_ref::<StorageError>()
                        .unwrap()
                        .0
                ))
                .await
                .unwrap(),
            (7, false, 91)
        );
        store.shutdown().await.unwrap();
    }))
    .unwrap();
}

#[test]
fn full_mailbox_rejects_registration_but_keeps_its_actual_join_debt() {
    let runtime = DeterministicRuntime::new();
    let (state, reducer) = SharedDocument::revisioned(7, Memory::new(), Binary);
    let session = reducer.session();
    let store = Store::builder(state, reducer, runtime.runtime())
        .mailbox_capacity(1)
        .build()
        .unwrap();
    let owner = document_owner(&store, session.clone());
    let _ = store.try_send(SharedDocumentAction::cancel_load()).unwrap();
    let (completion, join) = controlled_future();
    let cancelled = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let cancelling = Arc::clone(&cancelled);
    let error = owner
        .observe_changes::<StorageError, _, _, _>(|_| {
            (
                SharedUpdate::Present(()),
                move || {
                    cancelling.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                },
                async move { join.await.map_err(|_cause| StorageError(92))? },
            )
        })
        .unwrap_err();
    assert!(error.is_full());
    assert_eq!(cancelled.load(std::sync::atomic::Ordering::SeqCst), 1);
    let mut close = Box::pin(session.close());
    assert!(
        close
            .as_mut()
            .poll(&mut std::task::Context::from_waker(std::task::Waker::noop()))
            .is_pending()
    );
    completion.complete(Err(StorageError(93))).unwrap();
    assert_eq!(
        block_on(close).unwrap_err().failures()[0]
            .error()
            .downcast_ref::<StorageError>()
            .unwrap()
            .0,
        93
    );
    block_on(runtime.run(store.shutdown())).unwrap().unwrap();
}

#[test]
fn external_snapshot_survives_captured_ack_and_the_next_coalesced_save() {
    let runtime = DeterministicRuntime::new();
    let backend = Memory::new();
    let (state, reducer) = SharedDocument::revisioned(7, backend.clone(), Binary);
    let store = Store::new(state, reducer, runtime.runtime());
    block_on(runtime.run(async {
        let _ = store.send(SharedDocumentAction::reload()).await.unwrap();
        runtime.run_until_stalled().unwrap();
        backend.complete(0, Ok(present(1, 7)));
        runtime.run_until_stalled().unwrap();
        let (first, first_receipt) = SharedDocumentAction::set_and_flush(8);
        let _ = store.send(first).await.unwrap();
        runtime.run_until_stalled().unwrap();
        let (ack, result) = backend.commit_save();
        let (second, second_receipt) = SharedDocumentAction::set_and_flush(9);
        let _ = store.send(second).await.unwrap();
        runtime.run_until_stalled().unwrap();
        backend.external_write(77);
        let _ = store.send(SharedDocumentAction::reload()).await.unwrap();
        runtime.run_until_stalled().unwrap();
        backend.complete(0, Ok(present(3, 77)));
        runtime.run_until_stalled().unwrap();
        ack.complete(Ok(result)).unwrap();
        first_receipt.await.unwrap();
        runtime.run_until_stalled().unwrap();
        assert_eq!(
            store
                .read(|state| (
                    *state.value(),
                    state.acknowledged_revision(),
                    *state.conflict().unwrap().revision(),
                    *state.conflict().unwrap().value().unwrap()
                ))
                .await
                .unwrap(),
            (9, Some(1), 3, 77)
        );
        backend.complete_save();
        let _ = second_receipt.await.unwrap_err();
        assert_eq!(
            store
                .read(|state| (
                    *state.value(),
                    state.acknowledged_revision(),
                    *state.conflict().unwrap().revision(),
                    *state.conflict().unwrap().value().unwrap()
                ))
                .await
                .unwrap(),
            (9, Some(1), 3, 77)
        );
        assert_eq!(
            Binary
                .decode(backend.state.lock().unwrap().bytes.as_ref().unwrap())
                .unwrap(),
            77
        );
        store.shutdown().await.unwrap();
    }))
    .unwrap();
}

#[test]
fn keyed_fresh_owner_and_surviving_reader_progress_with_distinct_owned_sessions() {
    let runtime = DeterministicRuntime::new();
    let old_backend = Memory::new();
    let sibling_backend = Memory::new();
    let fresh_backend = Memory::new();
    let (old_state, interpreter) = SharedDocument::revisioned(7, old_backend.clone(), Binary);
    let old_session = interpreter.session();
    let (sibling_state, sibling_resource) =
        SharedDocument::revisioned(20, sibling_backend.clone(), Binary);
    let (replacement, fresh_resource) =
        SharedDocument::revisioned(40, fresh_backend.clone(), Binary);
    let store = Store::new(
        [(1, old_state), (2, sibling_state)].into_iter().collect(),
        KeyedDocumentParent {
            replacement: Some(replacement),
        }
        .for_each(keyed_document_state(), keyed_document_action(), interpreter),
        runtime.runtime(),
    );
    block_on(runtime.run(async {
        let old = SharedDocumentOwner::new(
            store
                .scope_keyed(keyed_document_state(), keyed_document_action(), 1)
                .await
                .unwrap(),
            |value: &u32| *value,
            old_session,
        );
        let sibling = SharedDocumentOwner::new(
            store
                .scope_keyed(keyed_document_state(), keyed_document_action(), 2)
                .await
                .unwrap(),
            |value: &u32| *value,
            sibling_resource.session(),
        );
        let original = cached_value(&runtime, &old_backend, &old, "keyed-old-reader", 7).await;
        let surviving = cached_value(
            &runtime,
            &sibling_backend,
            &sibling,
            "keyed-sibling-reader",
            20,
        )
        .await;
        let retired = old.key(SharedKeyIdentity::new::<Binary>("keyed-retired", "binary"));
        let _ = store.send(KeyedDocumentAction::Recreate(1)).await.unwrap();
        old.close().await.unwrap();
        let fresh = SharedDocumentOwner::new(
            store
                .scope_keyed(keyed_document_state(), keyed_document_action(), 1)
                .await
                .unwrap(),
            |value: &u32| *value,
            fresh_resource.session(),
        );
        let current =
            fresh_load_and_save(&runtime, &fresh_backend, &fresh, "keyed-fresh-reader").await;
        save_owned_value(
            &runtime,
            &sibling_backend,
            &sibling,
            "keyed-sibling-save",
            23,
        )
        .await;
        assert_eq!(original.read(|value| *value).unwrap(), 7);
        assert_eq!(surviving.read(|value| *value).unwrap(), 23);
        assert_eq!(current.read(|value| *value).unwrap(), 42);
        assert!(
            retired
                .load(SharedLoadContext::UserInitiated)
                .await
                .unwrap_err()
                .is_session()
        );
        fresh.close().await.unwrap();
        sibling.close().await.unwrap();
        store.shutdown().await.unwrap();
    }))
    .unwrap();
}

#[test]
fn stack_fresh_owner_and_original_prefix_reader_progress_after_retired_tail_join() {
    let runtime = DeterministicRuntime::new();
    let old_backend = Memory::new();
    let prefix_backend = Memory::new();
    let fresh_backend = Memory::new();
    let (prefix_state, interpreter) =
        SharedDocument::revisioned(20, prefix_backend.clone(), Binary);
    let prefix_session = interpreter.session();
    let (old_state, old_resource) = SharedDocument::revisioned(7, old_backend.clone(), Binary);
    let (replacement, fresh_resource) =
        SharedDocument::revisioned(40, fresh_backend.clone(), Binary);
    let mut state = StackState::new();
    let prefix_id = state.push(prefix_state);
    let tail_id = state.push(old_state);
    let reducer = stack(
        StackDocumentParent {
            replacement: Some(replacement),
        },
        stack_document_state(),
        stack_document_action(),
        |_: &u32, _: &mut ReduceContext<'_>| panic!("explicit fixture replacement"),
        interpreter,
    );
    let store = Store::new(state, reducer, runtime.runtime());
    block_on(runtime.run(async {
        let old = SharedDocumentOwner::new(
            store
                .scope_stack(
                    stack_document_state(),
                    stack_document_action(),
                    tail_id.clone(),
                )
                .await
                .unwrap(),
            |value: &u32| *value,
            old_resource.session(),
        );
        let prefix = SharedDocumentOwner::new(
            store
                .scope_stack(
                    stack_document_state(),
                    stack_document_action(),
                    prefix_id.clone(),
                )
                .await
                .unwrap(),
            |value: &u32| *value,
            prefix_session,
        );
        let original = cached_value(&runtime, &old_backend, &old, "stack-old-reader", 7).await;
        let surviving = cached_value(
            &runtime,
            &prefix_backend,
            &prefix,
            "stack-prefix-reader",
            20,
        )
        .await;
        let retired = old.key(SharedKeyIdentity::new::<Binary>("stack-retired", "binary"));
        let _ = store
            .send(StackDocumentAction::Recreate(tail_id.clone()))
            .await
            .unwrap();
        old.close().await.unwrap();
        let fresh = SharedDocumentOwner::new(
            store
                .scope_stack(
                    stack_document_state(),
                    stack_document_action(),
                    tail_id.clone(),
                )
                .await
                .unwrap(),
            |value: &u32| *value,
            fresh_resource.session(),
        );
        let current =
            fresh_load_and_save(&runtime, &fresh_backend, &fresh, "stack-fresh-reader").await;
        save_owned_value(&runtime, &prefix_backend, &prefix, "stack-prefix-save", 23).await;
        assert_eq!(original.read(|value| *value).unwrap(), 7);
        assert_eq!(surviving.read(|value| *value).unwrap(), 23);
        assert_eq!(current.read(|value| *value).unwrap(), 42);
        assert!(
            retired
                .load(SharedLoadContext::UserInitiated)
                .await
                .unwrap_err()
                .is_session()
        );
        fresh.close().await.unwrap();
        prefix.close().await.unwrap();
        store.shutdown().await.unwrap();
    }))
    .unwrap();
}

#[test]
fn materialising_absence_updates_presence_only_after_the_actual_receipt() {
    let runtime = DeterministicRuntime::new();
    let backend = Memory::new();
    let (state, reducer) = SharedDocument::revisioned(7, backend.clone(), Binary);
    let session = reducer.session();
    let store = Store::new(state, reducer, runtime.runtime());
    let owner = document_owner(&store, session);
    let key = owner.key(SharedKeyIdentity::new::<Binary>("presence", "binary"));
    let cache = SharedKeyCache::new();
    block_on(runtime.run(async {
        let mut constructing = Box::pin(cache.shared(key.clone(), || 7));
        assert!(
            constructing
                .as_mut()
                .poll(&mut std::task::Context::from_waker(std::task::Waker::noop()))
                .is_pending()
        );
        runtime.run_until_stalled().unwrap();
        backend.complete(
            0,
            Ok(BackendSnapshot {
                revision: 1,
                bytes: None,
            }),
        );
        runtime.run_until_stalled().unwrap();
        let shared = constructing.await.unwrap();
        runtime.run_until_stalled().unwrap();
        assert!(owner.subscription_statuses(&key)[0].is_absent());
        let (flush, receipt) = SharedDocumentAction::flush();
        let _ = store.send(flush).await.unwrap();
        runtime.run_until_stalled().unwrap();
        assert!(owner.subscription_statuses(&key)[0].is_absent());
        backend.complete_save();
        receipt.await.unwrap();
        runtime.run_until_stalled().unwrap();
        assert!(!owner.subscription_statuses(&key)[0].is_absent());
        assert_eq!(shared.read(|value| *value).unwrap(), 7);
        assert_eq!(
            store
                .read(SharedDocumentState::acknowledged_revision)
                .await
                .unwrap(),
            Some(0)
        );
        owner.close().await.unwrap();
        store.shutdown().await.unwrap();
    }))
    .unwrap();
}

#[test]
fn plain_snapshot_initial_absence_is_observed_before_buffered_recreation() {
    let runtime = DeterministicRuntime::new();
    let backend = Memory::new();
    backend.state.lock().unwrap().bytes = Some(7_u32.to_le_bytes().to_vec());
    let (state, reducer) = SharedDocument::new(7, backend.clone(), Binary);
    let session = reducer.session();
    let store = Store::new(state, reducer, runtime.runtime());
    let route = store.scope(
        StatePath::new(|state: &SharedDocumentState<u32>| state, |state| state),
        ActionPath::new(
            |action: &SharedDocumentAction<u32>| Some(action),
            |action| action,
        ),
    );
    let observed = Arc::new(Mutex::new(Vec::new()));
    let key_slot = Arc::new(Mutex::new(None));
    let owner = Arc::new_cyclic(|weak: &std::sync::Weak<SharedDocumentOwner<u32>>| {
        let weak = std::sync::Weak::clone(weak);
        let slot = Arc::clone(&key_slot);
        let observations = Arc::clone(&observed);
        SharedDocumentOwner::new(
            route,
            move |value: &u32| {
                if *value == 9
                    && let Some(key) = slot.lock().unwrap().as_ref()
                {
                    observations
                        .lock()
                        .unwrap()
                        .push(weak.upgrade().unwrap().subscription_statuses(key)[0].is_absent());
                }
                *value
            },
            session,
        )
    });
    let key = owner.key(SharedKeyIdentity::new::<Binary>(
        "plain-snapshot-absence",
        "binary",
    ));
    let cache = SharedKeyCache::new();
    block_on(runtime.run(async {
        let shared = cache.shared(key.clone(), || 0).await.unwrap();
        runtime.run_until_stalled().unwrap();
        *key_slot.lock().unwrap() = Some(key.clone());
        backend.state.lock().unwrap().bytes = None;
        let guard = owner
            .observe_snapshots::<StorageError, _, _, _>(|producer| {
                let initial = BackendSnapshot {
                    revision: None,
                    bytes: backend.state.lock().unwrap().bytes.clone(),
                };
                backend.external_write(9);
                producer
                    .publish(SharedUpdate::Present(BackendSnapshot {
                        revision: None,
                        bytes: Some(9_u32.to_le_bytes().to_vec()),
                    }))
                    .unwrap();
                (
                    SharedUpdate::Present(initial),
                    || {},
                    std::future::ready(Ok(())),
                )
            })
            .unwrap();
        runtime.run_until_stalled().unwrap();
        assert_eq!(observed.lock().unwrap().first().copied(), Some(true));
        assert_eq!(shared.read(|value| *value).unwrap(), 9);
        assert!(!owner.subscription_statuses(&key)[0].is_absent());
        assert!(guard.status().is_active());
        owner.close().await.unwrap();
        store.shutdown().await.unwrap();
    }))
    .unwrap();
}

#[test]
fn full_key_registration_returns_original_admission_error_without_false_owner_status() {
    let runtime = DeterministicRuntime::new();
    let backend = Memory::new();
    let (state, reducer) = SharedDocument::revisioned(7, backend.clone(), Binary);
    let session = reducer.session();
    let store = Store::builder(state, reducer, runtime.runtime())
        .mailbox_capacity(1)
        .build()
        .unwrap();
    let owner = document_owner(&store, session);
    let key = owner.key(SharedKeyIdentity::new::<Binary>(
        "full-key-registration",
        "binary",
    ));
    let cache = SharedKeyCache::new();
    block_on(runtime.run(async {
        let mut constructing = Box::pin(cache.shared(key.clone(), || 0));
        assert!(
            constructing
                .as_mut()
                .poll(&mut std::task::Context::from_waker(std::task::Waker::noop()))
                .is_pending()
        );
        runtime.run_until_stalled().unwrap();
        backend.complete(0, Ok(present(1, 7)));
        runtime.run_until_stalled().unwrap();
        let _ = store.try_send(SharedDocumentAction::cancel_load()).unwrap();
        assert!(
            constructing
                .await
                .unwrap_err()
                .into_failure()
                .unwrap()
                .is_full()
        );
        assert!(owner.subscription_statuses(&key).is_empty());
        owner.close().await.unwrap();
        store.shutdown().await.unwrap();
    }))
    .unwrap();
}

fn captured_registration_cannot_undo_save<R: Clone + Eq + Send + 'static>(
    construct: impl FnOnce(CapturedRead) -> (SharedDocumentState<u32, R>, SharedDocument<u32, R>),
    revision: Option<R>,
    revisioned: bool,
    buffered: bool,
) {
    use std::time::Duration;

    // Only a deadlock guard: channel acknowledgements determine the schedule.
    const DEADLOCK_GUARD: Duration = Duration::from_secs(10);
    let runtime = DeterministicRuntime::new();
    let backend = CapturedRead::new();
    let (state, reducer) = construct(backend.clone());
    let session = reducer.session();
    let store = Store::new(state, reducer, runtime.runtime());
    let owner = SharedDocumentOwner::new(
        store.scope(
            StatePath::new(|state: &SharedDocumentState<u32, R>| state, |state| state),
            ActionPath::new(
                |action: &SharedDocumentAction<u32, R>| Some(action),
                |action| action,
            ),
        ),
        |value: &u32| *value,
        session.clone(),
    );
    block_on(runtime.run(async {
        let _ = store.send(SharedDocumentAction::reload()).await.unwrap();
        runtime.run_until_stalled().unwrap();
        backend.release();
        runtime.run_until_stalled().unwrap();
        let (captured, captured_rx) = std::sync::mpsc::channel();
        let (resume, resume_rx) = std::sync::mpsc::channel();
        let (registered, registered_rx) = std::sync::mpsc::channel();
        let registration = std::thread::spawn(move || {
            let result = owner.observe_snapshots::<StorageError, _, _, _>(|producer| {
                let snapshot = BackendSnapshot {
                    revision,
                    bytes: Some(7_u32.to_le_bytes().to_vec()),
                };
                if buffered {
                    producer
                        .publish(SharedUpdate::Present(snapshot.clone()))
                        .unwrap_or_else(|_| panic!("live producer accepts buffered snapshot"));
                }
                captured.send(()).unwrap();
                resume_rx.recv_timeout(DEADLOCK_GUARD).unwrap();
                (
                    SharedUpdate::Present(snapshot),
                    || {},
                    std::future::ready(Ok(())),
                )
            });
            drop(owner);
            registered.send(()).unwrap();
            result
        });
        captured_rx.recv_timeout(DEADLOCK_GUARD).unwrap();
        let (edit, receipt) = SharedDocumentAction::set_and_flush(8);
        let _ = store.send(edit).await.unwrap();
        runtime.run_until_stalled().unwrap();
        if revisioned {
            backend.memory.complete_save();
        }
        receipt.await.unwrap();
        resume.send(()).unwrap();
        registered_rx.recv_timeout(DEADLOCK_GUARD).unwrap();
        let guard = registration.join().unwrap().unwrap();
        runtime.run_until_stalled().unwrap();
        assert_eq!(
            store
                .read(|state| (*state.value(), state.acknowledged_revision()))
                .await
                .unwrap(),
            (8, Some(1))
        );
        // An ambiguous capture may require a real current read, including the
        // separately buffered latest slot. Complete only actually started I/O.
        let revalidations = if buffered { 2 } else { 1 };
        for _ in 0..revalidations {
            assert!(!backend.reads.lock().unwrap().is_empty());
            backend.release();
            runtime.run_until_stalled().unwrap();
            assert_eq!(store.read(|state| *state.value()).await.unwrap(), 8);
        }
        assert!(backend.reads.lock().unwrap().is_empty());
        assert!(guard.status().is_active());
        let (flush, receipt) = SharedDocumentAction::flush();
        let _ = store.send(flush).await.unwrap();
        receipt.await.unwrap();
        assert_eq!(
            Binary
                .decode(backend.memory.state.lock().unwrap().bytes.as_ref().unwrap())
                .unwrap(),
            8
        );
        store.shutdown().await.unwrap();
    }))
    .unwrap();
    block_on(session.close()).unwrap();
}

#[test]
fn paused_plain_initial_snapshot_cannot_rollback_acknowledged_save() {
    captured_registration_cannot_undo_save(
        |backend| SharedDocument::new(0, backend, Binary),
        None,
        false,
        false,
    );
}

// Catches save-epoch revalidation bypassing the typed malformed-snapshot failure.
fn malformed_revisioned_snapshot_after_save(initial: SharedUpdate<BackendSnapshot<Option<u64>>>) {
    use std::time::Duration;

    // Only a deadlock guard: channel acknowledgements determine the schedule.
    const DEADLOCK_GUARD: Duration = Duration::from_secs(10);
    let runtime = DeterministicRuntime::new();
    let backend = CapturedRead::new();
    let (state, reducer) = SharedDocument::revisioned(0, backend.clone(), Binary);
    let session = reducer.session();
    let store = Store::new(state, reducer, runtime.runtime());
    let owner = document_owner(&store, session.clone());
    let cancelled = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let cancelling = Arc::clone(&cancelled);
    block_on(runtime.run(async {
        let _ = store.send(SharedDocumentAction::reload()).await.unwrap();
        runtime.run_until_stalled().unwrap();
        backend.release();
        runtime.run_until_stalled().unwrap();
        let (captured, captured_rx) = std::sync::mpsc::channel();
        let (resume, resume_rx) = std::sync::mpsc::channel();
        let (registered, registered_rx) = std::sync::mpsc::channel();
        let registration = std::thread::spawn(move || {
            let result = owner.observe_snapshots::<StorageError, _, _, _>(|producer| {
                captured.send(producer).unwrap();
                resume_rx.recv_timeout(DEADLOCK_GUARD).unwrap();
                (
                    initial,
                    move || {
                        cancelling.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    },
                    std::future::ready(Ok(())),
                )
            });
            drop(owner);
            registered.send(()).unwrap();
            result
        });
        let producer = captured_rx.recv_timeout(DEADLOCK_GUARD).unwrap();
        let (edit, receipt) = SharedDocumentAction::set_and_flush(8);
        let _ = store.send(edit).await.unwrap();
        runtime.run_until_stalled().unwrap();
        backend.memory.complete_save();
        assert_eq!(receipt.await.unwrap().revision, 1);
        resume.send(()).unwrap();
        registered_rx.recv_timeout(DEADLOCK_GUARD).unwrap();
        let guard = registration.join().unwrap().unwrap();
        runtime.run_until_stalled().unwrap();
        assert!(
            backend.reads.lock().unwrap().is_empty(),
            "malformed input must not start revalidation"
        );
        let status = guard.status();
        assert_eq!(
            status.error().unwrap().to_string().lines().next(),
            Some("document key Load failed")
        );
        assert!(!status.is_active());
        assert!(!status.is_finished());
        assert_eq!(cancelled.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert!(
            producer
                .publish(SharedUpdate::Present(BackendSnapshot {
                    revision: Some(99),
                    bytes: Some(99_u32.to_le_bytes().to_vec()),
                }))
                .is_err()
        );
        runtime.run_until_stalled().unwrap();
        assert_eq!(
            store
                .read(|state| (*state.value(), state.acknowledged_revision()))
                .await
                .unwrap(),
            (8, Some(1))
        );
        store.shutdown().await.unwrap();
    }))
    .unwrap();
    block_on(session.close()).unwrap();
}

#[test]
fn post_save_revisioned_none_snapshot_terminates_without_revalidation() {
    malformed_revisioned_snapshot_after_save(SharedUpdate::Present(BackendSnapshot {
        revision: None,
        bytes: Some(7_u32.to_le_bytes().to_vec()),
    }));
}

#[test]
fn post_save_revisioned_absent_snapshot_terminates_without_revalidation() {
    malformed_revisioned_snapshot_after_save(SharedUpdate::Absent);
}

#[test]
fn post_save_valid_revisioned_absence_revalidates_and_keeps_producer_active() {
    let runtime = DeterministicRuntime::new();
    let backend = CapturedRead::new();
    let (state, reducer) = SharedDocument::revisioned(0, backend.clone(), Binary);
    let session = reducer.session();
    let store = Store::new(state, reducer, runtime.runtime());
    let owner = document_owner(&store, session);
    let cancelled = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let cancelling = Arc::clone(&cancelled);
    block_on(runtime.run(async {
        let mut producer = None;
        let guard = owner
            .observe_snapshots::<StorageError, _, _, _>(|updates| {
                producer = Some(updates);
                (
                    SharedUpdate::Present(BackendSnapshot {
                        revision: Some(1),
                        bytes: Some(7_u32.to_le_bytes().to_vec()),
                    }),
                    move || {
                        cancelling.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    },
                    std::future::ready(Ok(())),
                )
            })
            .unwrap();
        runtime.run_until_stalled().unwrap();
        let (edit, receipt) = SharedDocumentAction::set_and_flush(8);
        let _ = store.send(edit).await.unwrap();
        runtime.run_until_stalled().unwrap();
        backend.memory.complete_save();
        assert_eq!(receipt.await.unwrap().revision, 1);
        {
            let mut storage = backend.memory.state.lock().unwrap();
            storage.revision = 3;
            storage.bytes = None;
        };
        let producer = producer.unwrap();
        producer
            .publish(SharedUpdate::Present(BackendSnapshot {
                revision: Some(3),
                bytes: None,
            }))
            .unwrap();
        runtime.run_until_stalled().unwrap();
        assert_eq!(backend.reads.lock().unwrap().len(), 1);
        backend.release();
        runtime.run_until_stalled().unwrap();
        assert!(guard.status().is_absent());
        assert!(guard.status().error().is_none());
        assert_eq!(
            store
                .read(|state| (
                    *state.value(),
                    state.backend_revision().copied(),
                    state.acknowledged_revision()
                ))
                .await
                .unwrap(),
            (8, Some(3), None)
        );
        backend.memory.external_write(9);
        producer
            .publish(SharedUpdate::Present(BackendSnapshot {
                revision: Some(4),
                bytes: Some(9_u32.to_le_bytes().to_vec()),
            }))
            .unwrap();
        runtime.run_until_stalled().unwrap();
        assert_eq!(backend.reads.lock().unwrap().len(), 1);
        backend.release();
        runtime.run_until_stalled().unwrap();
        assert_eq!(store.read(|state| *state.value()).await.unwrap(), 9);
        assert!(guard.status().is_active());
        assert_eq!(cancelled.load(std::sync::atomic::Ordering::SeqCst), 0);
        owner.close().await.unwrap();
        assert!(guard.status().is_cancelled());
        assert_eq!(cancelled.load(std::sync::atomic::Ordering::SeqCst), 1);
        store.shutdown().await.unwrap();
    }))
    .unwrap();
}

#[test]
fn paused_revisioned_initial_snapshot_cannot_rollback_acknowledged_save() {
    captured_registration_cannot_undo_save(
        |backend| SharedDocument::revisioned(0, backend, Binary),
        Some(1),
        true,
        false,
    );
}

#[test]
fn buffered_latest_snapshot_cannot_rollback_acknowledged_save() {
    captured_registration_cannot_undo_save(
        |backend| SharedDocument::revisioned(0, backend, Binary),
        Some(1),
        true,
        true,
    );
}

#[test]
fn fresh_binding_survives_final_external_owner_drop_during_publication() {
    let runtime = DeterministicRuntime::new();
    let (_, interpreter) = SharedDocument::new(0, Memory::new(), Binary);
    block_on(interpreter.session().close()).unwrap();
    let backend = Memory::new();
    backend.external_write(7);
    let (state, resource) = SharedDocument::new(0, backend.clone(), Binary);
    let session = resource.session();
    drop(resource);
    let store = Store::new(state, interpreter, runtime.runtime());
    let last_owner = Arc::new(Mutex::new(None));
    let dropping = Arc::clone(&last_owner);
    let owner = SharedDocumentOwner::new(
        store.scope(
            StatePath::new(|state: &SharedDocumentState<u32>| state, |state| state),
            ActionPath::new(
                |action: &SharedDocumentAction<u32>| Some(action),
                |action| action,
            ),
        ),
        move |value: &u32| {
            if *value == 8 {
                drop(dropping.lock().unwrap().take());
            }
            *value
        },
        session,
    );
    let key = owner.key(SharedKeyIdentity::new::<Binary>(
        "last-fresh-owner",
        "binary",
    ));
    *last_owner.lock().unwrap() = Some(owner);
    block_on(runtime.run(async {
        let cache = SharedKeyCache::new();
        let shared = cache.shared(key.clone(), || 0).await.unwrap();
        runtime.run_until_stalled().unwrap();
        assert_eq!(shared.read(|value| *value).unwrap(), 7);
        let (edit, receipt) = SharedDocumentAction::set_and_flush(8);
        let _ = store.send(edit).await.unwrap();
        receipt.await.unwrap();
        assert!(last_owner.lock().unwrap().is_none());
        assert_eq!(shared.read(|value| *value).unwrap(), 8);
        assert_eq!(
            Binary
                .decode(backend.state.lock().unwrap().bytes.as_ref().unwrap())
                .unwrap(),
            8
        );
        assert!(
            key.load(SharedLoadContext::UserInitiated)
                .await
                .unwrap_err()
                .is_ended()
        );
        let (edit, receipt) = SharedDocumentAction::set_and_flush(99);
        let _ = store.send(edit).await.unwrap();
        let _ = receipt.await.unwrap_err();
        assert_eq!(store.read(|state| *state.value()).await.unwrap(), 8);
        store.shutdown().await.unwrap();
    }))
    .unwrap();
}

// A dormant save must not acquire fresh admission or fabricate a receipt after shutdown.
#[test]
fn dormant_document_key_save_reports_ended_after_store_shutdown() {
    use rca_core::SharedKey;

    let runtime = DeterministicRuntime::with_step_budget(64);
    let backend = Memory::new();
    let (state, reducer) = SharedDocument::revisioned(7, backend.clone(), Binary);
    let session = reducer.session();
    let store = Store::new(state, reducer, runtime.runtime());
    let owner = document_owner(&store, session);
    let key = owner.key(SharedKeyIdentity::new::<Binary>("dormant-save", "binary"));

    run_document_session_contract(&runtime, async {
        assert_eq!(store.read(|state| *state.value()).await.unwrap(), 7);
        // Capture the live route now, but do not poll or admit the save.
        let mut saving = Box::pin(key.save(&99));
        store.shutdown().await.unwrap();

        let Poll::Ready(Err(error)) = saving
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
        else {
            panic!("a dormant save must fail immediately after Store shutdown");
        };
        assert!(error.is_ended());
        assert!(!error.is_session());
        assert!(!error.is_full());
        assert!(!error.is_cancelled());
        assert!(error.error().is_none());
        assert!(error.source().is_none());
        drop(saving);
        owner.close().await.unwrap();
    });

    let storage = backend.state.lock().unwrap();
    assert!(storage.closed);
    assert!(storage.loads.is_empty());
    assert!(storage.saves.is_empty());
    assert_eq!(
        (storage.revisioned_load_calls, storage.revisioned_save_calls),
        (0, 0)
    );
}

#[test]
fn admitted_fresh_load_settles_after_final_external_owner_drop() {
    let runtime = DeterministicRuntime::new();
    let (_, interpreter) = SharedDocument::revisioned(0, Memory::new(), Binary);
    block_on(interpreter.session().close()).unwrap();
    let backend = Memory::new();
    let (state, resource) = SharedDocument::revisioned(0, backend.clone(), Binary);
    let session = resource.session();
    drop(resource);
    let store = Store::new(state, interpreter, runtime.runtime());
    let owner = document_owner(&store, session);
    let key = owner.key(SharedKeyIdentity::new::<Binary>(
        "last-fresh-load",
        "binary",
    ));
    block_on(runtime.run(async {
        let mut loading = Box::pin(key.load(SharedLoadContext::UserInitiated));
        assert!(
            loading
                .as_mut()
                .poll(&mut std::task::Context::from_waker(std::task::Waker::noop()))
                .is_pending()
        );
        runtime.run_until_stalled().unwrap();
        assert_eq!(backend.state.lock().unwrap().loads.len(), 1);
        drop(owner);
        backend.complete(0, Ok(present(1, 8)));
        runtime.run_until_stalled().unwrap();
        assert!(matches!(
            loading
                .as_mut()
                .poll(&mut std::task::Context::from_waker(std::task::Waker::noop())),
            std::task::Poll::Ready(Ok(Some(8)))
        ));
        assert_eq!(store.read(|state| *state.value()).await.unwrap(), 8);
        store.shutdown().await.unwrap();
    }))
    .unwrap();
}

#[test]
fn admitted_document_receipts_are_aborted_after_real_session_close() {
    // Catches flush acknowledgement or a local edit after admission crosses closure.
    for editing in [false, true] {
        let runtime = DeterministicRuntime::new();
        let backend = Memory::new();
        let (state, reducer) = SharedDocument::revisioned(7, backend.clone(), Binary);
        let session = reducer.session();
        let store = Store::builder(state, reducer, runtime.runtime())
            .mailbox_capacity(1)
            .build()
            .unwrap();
        let owner = document_owner(&store, session.clone());
        let (action, mut receipt) = if editing {
            SharedDocumentAction::set_and_flush(99)
        } else {
            SharedDocumentAction::flush()
        };
        let admitted = store.try_send(action).unwrap();
        assert!(
            Pin::new(&mut receipt)
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending()
        );
        let (probe, mut probe_receipt) = SharedDocumentAction::set_and_flush(13);
        let full = store.try_send(probe).unwrap_err();
        assert!(full.is_full());
        assert!(!full.is_store_stopped());
        drop(full.into_action());
        assert!(matches!(
            Pin::new(&mut probe_receipt).poll(&mut Context::from_waker(Waker::noop())),
            Poll::Ready(Err(ReceiptError::Abandoned))
        ));
        let mut close = Box::pin(session.close());
        assert!(matches!(
            close.as_mut().poll(&mut Context::from_waker(Waker::noop())),
            Poll::Ready(Ok(()))
        ));
        drop(close);
        {
            let storage = backend.state.lock().unwrap();
            assert!(storage.closed);
            assert_eq!(
                (storage.revisioned_load_calls, storage.revisioned_save_calls),
                (0, 0)
            );
            assert!(storage.loads.is_empty());
            assert!(storage.saves.is_empty());
        };
        block_on(runtime.run(async {
            admitted.finish().await.unwrap().finish().await;
            assert!(matches!(
                Pin::new(&mut receipt).poll(&mut Context::from_waker(Waker::noop())),
                Poll::Ready(Err(ReceiptError::Aborted))
            ));
            store
                .read(|state| {
                    assert_eq!(
                        (
                            *state.value(),
                            state.revision(),
                            state.acknowledged_revision()
                        ),
                        (7, 0, None)
                    );
                    assert!(state.backend_revision().is_none());
                    assert!(!state.is_loading());
                    assert!(state.load_error().is_none());
                    assert!(state.document().failure().is_none());
                    assert!(state.conflict().is_none());
                    assert_eq!(state.document().pending_barrier_count(), 0);
                })
                .await
                .unwrap();
            // Keep the correctly paired owner alive through the rejected reduction.
            drop(owner);
            store.shutdown().await.unwrap();
        }))
        .unwrap();
        let storage = backend.state.lock().unwrap();
        assert_eq!(
            (storage.revisioned_load_calls, storage.revisioned_save_calls),
            (0, 0)
        );
        assert!(storage.loads.is_empty());
        assert!(storage.saves.is_empty());
        assert_eq!(storage.revision, 1);
        assert!(storage.bytes.is_none());
        drop(storage);
        block_on(runtime.drain_deadlines(0)).unwrap();
    }
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "paired admitted load/save closure cases preserve exact recovery and ownership checks"
)]
fn admitted_document_keys_recover_exact_errors_after_real_session_close() {
    use rca_core::SharedKey;

    // Catches late provider work or loss of the original aborted save receipt cause.
    for saving in [false, true] {
        let runtime = DeterministicRuntime::new();
        let backend = Memory::new();
        let (state, reducer) = SharedDocument::revisioned(7, backend.clone(), Binary);
        let session = reducer.session();
        let store = Store::builder(state, reducer, runtime.runtime())
            .mailbox_capacity(1)
            .build()
            .unwrap();
        let owner = document_owner(&store, session.clone());
        let key = owner.key(SharedKeyIdentity::new::<Binary>(
            "admitted-closed",
            "binary",
        ));
        let mut command = Box::pin(async {
            if saving {
                key.save(&99).await.map(|()| None)
            } else {
                key.load(SharedLoadContext::UserInitiated).await
            }
        });
        assert!(
            command
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending()
        );
        let (probe, mut probe_receipt) = SharedDocumentAction::set_and_flush(13);
        let full = store.try_send(probe).unwrap_err();
        assert!(full.is_full());
        assert!(!full.is_store_stopped());
        drop(full.into_action());
        assert!(matches!(
            Pin::new(&mut probe_receipt).poll(&mut Context::from_waker(Waker::noop())),
            Poll::Ready(Err(ReceiptError::Abandoned))
        ));
        let mut close = Box::pin(session.close());
        assert!(matches!(
            close.as_mut().poll(&mut Context::from_waker(Waker::noop())),
            Poll::Ready(Ok(()))
        ));
        drop(close);
        {
            let storage = backend.state.lock().unwrap();
            assert!(storage.closed);
            assert_eq!(
                (storage.revisioned_load_calls, storage.revisioned_save_calls),
                (0, 0)
            );
            assert!(storage.loads.is_empty());
            assert!(storage.saves.is_empty());
        };
        runtime.run_until_stalled().unwrap();
        let Poll::Ready(Err(error)) = command
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
        else {
            panic!("admitted key work must settle after closed-session reduction");
        };
        assert!(!error.is_ended());
        assert!(!error.is_full());
        assert!(!error.is_cancelled());
        if saving {
            assert!(!error.is_session());
            assert_eq!(format!("{error:?}"), "DocumentKeyError { kind: Save, .. }");
            let cause = error
                .error()
                .unwrap()
                .downcast_ref::<ReceiptError>()
                .unwrap();
            assert!(matches!(cause, ReceiptError::Aborted));
            assert!(std::ptr::eq(
                cause,
                error
                    .source()
                    .unwrap()
                    .downcast_ref::<ReceiptError>()
                    .unwrap()
            ));
        } else {
            assert!(error.is_session());
            assert_eq!(
                format!("{error:?}"),
                "DocumentKeyError { kind: Closed, .. }"
            );
            assert!(error.error().is_none());
            assert!(error.source().is_none());
        }
        drop(command);
        block_on(runtime.run(async {
            store
                .read(|state| {
                    assert_eq!(
                        (
                            *state.value(),
                            state.revision(),
                            state.acknowledged_revision()
                        ),
                        (7, 0, None)
                    );
                    assert!(state.backend_revision().is_none());
                    assert!(!state.is_loading());
                    assert!(state.load_error().is_none());
                    assert!(state.document().failure().is_none());
                    assert!(state.conflict().is_none());
                    assert_eq!(state.document().pending_barrier_count(), 0);
                })
                .await
                .unwrap();
            assert!(owner.subscription_statuses(&key).is_empty());
            drop(owner);
            store.shutdown().await.unwrap();
        }))
        .unwrap();
        let storage = backend.state.lock().unwrap();
        assert_eq!(
            (storage.revisioned_load_calls, storage.revisioned_save_calls),
            (0, 0)
        );
        assert!(storage.loads.is_empty());
        assert!(storage.saves.is_empty());
        assert_eq!(storage.revision, 1);
        assert!(storage.bytes.is_none());
        drop(storage);
        block_on(runtime.drain_deadlines(0)).unwrap();
    }
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "keeps snapshot supersession, continued producer delivery, stale completion and physical join in one schedule"
)]
fn document_admission_snapshot_supersedes_key_load_without_retiring_producer() {
    let runtime = DeterministicRuntime::new();
    let backend = Memory::new();
    let (state, reducer) = SharedDocument::revisioned(7, backend.clone(), Binary);
    let session = reducer.session();
    let store = Store::new(state, reducer, runtime.runtime());
    let owner = document_owner(&store, session.clone());
    let key = owner.key(SharedKeyIdentity::new::<Binary>("snapshot-wins", "binary"));
    let (join_completion, join) = controlled_future();
    let cancellations = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let cancelling = Arc::clone(&cancellations);

    block_on(runtime.run(async {
        let mut load = Box::pin(key.load(SharedLoadContext::UserInitiated));
        assert!(
            load.as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending()
        );
        runtime.run_until_stalled().unwrap();
        assert_eq!(backend.state.lock().unwrap().loads.len(), 1);
        let mut endpoint = None;
        let guard = owner
            .observe_snapshots::<StorageError, _, _, _>(|producer| {
                endpoint = Some(producer);
                (
                    SharedUpdate::Present(BackendSnapshot {
                        revision: Some(2),
                        bytes: Some(10_u32.to_le_bytes().to_vec()),
                    }),
                    move || {
                        cancelling.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    },
                    async move { join.await.map_err(|_cause| StorageError(71))? },
                )
            })
            .unwrap();
        runtime.run_until_stalled().unwrap();
        let error = load.await.unwrap_err();
        assert!(error.is_superseded());
        assert!(!error.is_cancelled());
        assert!(error.error().is_none());
        assert!(error.source().is_none());
        assert_eq!(
            store
                .read(|state| (
                    *state.value(),
                    state.backend_revision().copied(),
                    state.is_loading()
                ))
                .await
                .unwrap(),
            (10, Some(2), false)
        );
        assert!(guard.status().is_active());
        assert!(guard.status().error().is_none());

        let endpoint = endpoint.unwrap();
        endpoint
            .publish(SharedUpdate::Present(BackendSnapshot {
                revision: Some(3),
                bytes: Some(11_u32.to_le_bytes().to_vec()),
            }))
            .unwrap();
        runtime.run_until_stalled().unwrap();
        assert_eq!(
            store
                .read(|state| (*state.value(), state.backend_revision().copied()))
                .await
                .unwrap(),
            (11, Some(3))
        );
        assert!(guard.status().is_active());
        assert_eq!(cancellations.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert_eq!(backend.state.lock().unwrap().revisioned_load_calls, 1);

        // The original physical operation still accepts completion while the session is open.
        backend
            .state
            .lock()
            .unwrap()
            .loads
            .pop_front()
            .unwrap()
            .complete(Ok(present(99, 99)))
            .unwrap();
        runtime.run_until_stalled().unwrap();
        assert_eq!(
            store
                .read(|state| (
                    *state.value(),
                    state.revision(),
                    state.acknowledged_revision(),
                    state.backend_revision().copied(),
                    state.is_loading()
                ))
                .await
                .unwrap(),
            (11, 0, Some(0), Some(3), false)
        );
        assert!(guard.status().is_active());
        assert!(guard.status().error().is_none());
        assert!(endpoint.finish());
        runtime.run_until_stalled().unwrap();
        assert!(guard.status().is_finished());
        assert!(!guard.status().is_cancelled());
        assert_eq!(cancellations.load(std::sync::atomic::Ordering::SeqCst), 1);

        let mut closing = Box::pin(owner.close());
        assert!(
            closing
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending()
        );
        runtime.run_until_stalled().unwrap();
        assert!(
            closing
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending()
        );
        drop(closing);
        let mut resumed = Box::pin(session.close());
        assert!(
            resumed
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending()
        );
        join_completion.complete(Ok(())).unwrap();
        resumed.await.unwrap();
        store.shutdown().await.unwrap();
    }))
    .unwrap();
    let storage = backend.state.lock().unwrap();
    assert!(storage.closed);
    assert!(storage.loads.is_empty());
    assert!(storage.saves.is_empty());
    assert_eq!(
        (storage.revisioned_load_calls, storage.revisioned_save_calls),
        (1, 0)
    );
    assert_eq!(cancellations.load(std::sync::atomic::Ordering::SeqCst), 1);
}

#[test]
fn document_admission_dropped_load_does_not_start_io_before_reduction() {
    let runtime = DeterministicRuntime::new();
    let backend = Memory::new();
    let (state, reducer) = SharedDocument::revisioned(7, backend.clone(), Binary);
    let session = reducer.session();
    let store = Store::builder(state, reducer, runtime.runtime())
        .mailbox_capacity(1)
        .build()
        .unwrap();
    let owner = document_owner(&store, session);
    let key = owner.key(SharedKeyIdentity::new::<Binary>(
        "dropped-before-reduction",
        "binary",
    ));
    let mut load = Box::pin(key.load(SharedLoadContext::UserInitiated));
    assert!(
        load.as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_pending()
    );
    let full = store
        .try_send(SharedDocumentAction::cancel_load())
        .unwrap_err();
    assert!(full.is_full());
    drop(full.into_action());
    // The actor has not run; the full mailbox also prevents the best-effort cancel command.
    drop(load);
    runtime.run_until_stalled().unwrap();
    assert_eq!(backend.state.lock().unwrap().revisioned_load_calls, 0);
    assert!(backend.state.lock().unwrap().loads.is_empty());

    block_on(runtime.run(async {
        assert_eq!(
            store
                .read(|state| (
                    *state.value(),
                    state.revision(),
                    state.acknowledged_revision(),
                    state.is_loading()
                ))
                .await
                .unwrap(),
            (7, 0, None, false)
        );
        let mut fresh = Box::pin(key.load(SharedLoadContext::UserInitiated));
        assert!(
            fresh
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending()
        );
        runtime.run_until_stalled().unwrap();
        assert_eq!(backend.state.lock().unwrap().loads.len(), 1);
        backend.complete(0, Ok(present(2, 9)));
        runtime.run_until_stalled().unwrap();
        assert_eq!(fresh.await.unwrap(), Some(9));
        assert_eq!(
            store
                .read(|state| (
                    *state.value(),
                    state.backend_revision().copied(),
                    state.is_loading()
                ))
                .await
                .unwrap(),
            (9, Some(2), false)
        );
        owner.close().await.unwrap();
        store.shutdown().await.unwrap();
    }))
    .unwrap();
    let storage = backend.state.lock().unwrap();
    assert!(storage.closed);
    assert!(storage.loads.is_empty());
    assert!(storage.saves.is_empty());
    assert_eq!(
        (storage.revisioned_load_calls, storage.revisioned_save_calls),
        (1, 0)
    );
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "keeps post-abort load rejection and admitted producer cancellation distinct from physical join"
)]
fn document_admission_after_abort_rejects_load_and_preserves_producer_join() {
    let runtime = DeterministicRuntime::new();
    let backend = Memory::new();
    let (state, reducer) = SharedDocument::revisioned(7, backend.clone(), Binary);
    let session = reducer.session();
    let store = Store::new(state, reducer, runtime.runtime());
    let owner = document_owner(&store, session.clone());
    let key = owner.key(SharedKeyIdentity::new::<Binary>("after-abort", "binary"));
    let (join_completion, join) = controlled_future();
    let cancellations = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let cancelling = Arc::clone(&cancellations);

    block_on(runtime.run(async {
        store
            .send(SharedDocumentAction::abort())
            .await
            .unwrap()
            .finish()
            .await;
        assert!(!backend.state.lock().unwrap().closed);
        let error = key
            .load(SharedLoadContext::UserInitiated)
            .await
            .unwrap_err();
        assert!(error.is_session());
        assert!(!error.is_cancelled());
        assert!(!error.is_ended());
        assert_eq!(
            error.to_string().lines().next(),
            Some("document key Closed failed")
        );
        assert!(error.error().is_none());
        assert!(error.source().is_none());
        let mut endpoint = None;
        let guard = owner
            .observe_changes::<StorageError, _, _, _>(|producer| {
                endpoint = Some(producer);
                (
                    SharedUpdate::Present(()),
                    move || {
                        cancelling.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    },
                    async move { join.await.map_err(|_cause| StorageError(71))? },
                )
            })
            .unwrap();
        assert!(guard.status().is_pending());
        runtime.run_until_stalled().unwrap();
        assert!(guard.status().is_cancelled());
        assert!(guard.status().error().is_none());
        assert_eq!(cancellations.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert!(
            endpoint
                .unwrap()
                .publish(SharedUpdate::Present(()))
                .is_err()
        );
        assert_eq!(
            store
                .read(|state| (
                    *state.value(),
                    state.revision(),
                    state.acknowledged_revision(),
                    state.is_loading(),
                    state.document().is_aborted()
                ))
                .await
                .unwrap(),
            (7, 0, None, false, true)
        );
        assert!(!backend.state.lock().unwrap().closed);
        assert_eq!(backend.state.lock().unwrap().revisioned_load_calls, 0);
        let mut closing = Box::pin(owner.close());
        assert!(
            closing
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending()
        );
        runtime.run_until_stalled().unwrap();
        assert!(
            closing
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending()
        );
        drop(closing);
        let mut resumed = Box::pin(session.close());
        assert!(
            resumed
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending()
        );
        join_completion.complete(Ok(())).unwrap();
        resumed.await.unwrap();
        assert!(guard.status().is_cancelled());
        store.shutdown().await.unwrap();
    }))
    .unwrap();
    let storage = backend.state.lock().unwrap();
    assert!(storage.closed);
    assert!(storage.loads.is_empty());
    assert!(storage.saves.is_empty());
    assert_eq!(
        (storage.revisioned_load_calls, storage.revisioned_save_calls),
        (0, 0)
    );
    assert_eq!(cancellations.load(std::sync::atomic::Ordering::SeqCst), 1);
}

// Catches admitting queued producer I/O after its registration or session lost
// authority. Neither logical cancellation nor rejection satisfies the real join.
#[test]
fn queued_producer_load_rechecks_revoked_authority() {
    for close_session in [false, true] {
        let runtime = DeterministicRuntime::new();
        let backend = Memory::new();
        let (state, reducer) = SharedDocument::revisioned(7, backend.clone(), Binary);
        let session = reducer.session();
        let gate = rca_core::testing::AssertionGate::new();
        let store = Store::builder(state, reducer, runtime.runtime())
            .assertion_gate(gate.clone())
            .build()
            .unwrap();
        let owner = document_owner(&store, session.clone());
        let (join_completion, join) = controlled_future();
        let guard = owner
            .observe_changes::<StorageError, _, _, _>(|_producer| {
                (SharedUpdate::Present(()), || {}, async move {
                    join.await.map_err(|_cause| StorageError(71))?
                })
            })
            .unwrap();
        block_on(runtime.run(async {
            runtime.run_until_stalled().unwrap();
            assert!(guard.status().is_active());
            assert!(gate.has_pending());
            assert_eq!(backend.state.lock().unwrap().revisioned_load_calls, 0);
            let mut closing = Box::pin(session.close());
            if close_session {
                assert!(
                    closing
                        .as_mut()
                        .poll(&mut Context::from_waker(Waker::noop()))
                        .is_pending()
                );
            } else {
                guard.cancel();
            }
            assert!(guard.status().is_cancelled());
            assert!(gate.release_pending());
            runtime.run_until_stalled().unwrap();
            // The rejected load delivers the producer's terminal action next.
            assert!(gate.release_pending());
            runtime.run_until_stalled().unwrap();
            assert_eq!(
                store
                    .read(|state| (
                        *state.value(),
                        state.is_loading(),
                        state.backend_revision().copied()
                    ))
                    .await
                    .unwrap(),
                (7, false, None)
            );
            assert_eq!(backend.state.lock().unwrap().revisioned_load_calls, 0);
            assert!(guard.status().is_cancelled());
            assert!(guard.status().error().is_none());
            assert!(
                closing
                    .as_mut()
                    .poll(&mut Context::from_waker(Waker::noop()))
                    .is_pending()
            );
            join_completion.complete(Ok(())).unwrap();
            closing.await.unwrap();
            store.shutdown().await.unwrap();
        }))
        .unwrap();
    }
}

/// Returns a seeded reader and its captured publication endpoint without an executor.
fn cached_reference_and_subscriber()
-> (rca_core::SharedReader<u32>, rca_core::SharedSubscriber<u32>) {
    use rca_core::{SharedSubscribeContext, SharedSubscriber};

    /// Captures the endpoint while the cache supplies the initial value.
    struct ReferenceKey(Arc<Mutex<Option<SharedSubscriber<u32>>>>);
    impl SharedReaderKey for ReferenceKey {
        type Value = u32;
        type Error = std::convert::Infallible;
        type Subscription = ();

        fn identity(&self) -> SharedKeyIdentity {
            SharedKeyIdentity::new::<Self>("document-subscriber", "u32")
        }
        fn load(
            &self,
            _: SharedLoadContext<'_, u32>,
        ) -> impl Future<Output = Result<Option<u32>, Self::Error>> + Send {
            std::future::ready(Ok(None))
        }
        fn subscribe(
            &self,
            _: SharedSubscribeContext,
            subscriber: SharedSubscriber<u32>,
        ) -> Result<(), Self::Error> {
            *self.0.lock().unwrap() = Some(subscriber);
            Ok(())
        }
    }

    let subscriber = Arc::new(Mutex::new(None));
    let cache = SharedKeyCache::new();
    let mut loading = Box::pin(cache.reader(ReferenceKey(Arc::clone(&subscriber)), || 3));
    let Poll::Ready(Ok(reader)) = loading
        .as_mut()
        .poll(&mut Context::from_waker(Waker::noop()))
    else {
        panic!("an immediate seed-only provider must construct without a driver");
    };
    let subscriber = subscriber.lock().unwrap().take().unwrap();
    (reader, subscriber)
}

// Catches returning a live registration or misclassifying an ended Store as
// mailbox pressure or session closure while its captured owner still exists.
#[test]
fn retained_owner_rejects_subscription_after_store_shutdown() {
    use rca_core::SharedSubscribeContext;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let runtime = DeterministicRuntime::with_step_budget(64);
    let backend = Memory::new();
    let (state, reducer) = SharedDocument::revisioned(7, backend.clone(), Binary);
    let session = reducer.session();
    let store = Store::new(state, reducer, runtime.runtime());
    let route = store.scope(
        StatePath::new(|state: &SharedDocumentState<u32, u64>| state, |state| state),
        ActionPath::new(
            |action: &SharedDocumentAction<u32, u64>| Some(action),
            |action| action,
        ),
    );
    let weak_route = route.downgrade();
    let captures = Arc::new(AtomicUsize::new(0));
    let capturing = Arc::clone(&captures);
    let owner = SharedDocumentOwner::new(
        route,
        move |value: &u32| {
            capturing.fetch_add(1, Ordering::SeqCst);
            *value
        },
        session,
    );
    let key = owner.key(SharedKeyIdentity::new::<Binary>(
        "stopped-store-subscription",
        "binary",
    ));
    let (reader, subscriber) = cached_reference_and_subscriber();

    run_document_session_contract(&runtime, async {
        store.shutdown().await.unwrap();
        assert!(weak_route.upgrade().is_some());
        assert!(!backend.state.lock().unwrap().closed);

        let error = key
            .subscribe(SharedSubscribeContext::UserInitiated, subscriber)
            .unwrap_err();
        assert!(error.is_ended());
        assert!(!error.is_full());
        assert!(!error.is_session());
        assert_eq!(
            error.to_string().lines().next(),
            Some("document key Ended failed")
        );
        assert!(error.error().is_none());
        assert!(error.source().is_none());
        assert!(owner.subscription_statuses(&key).is_empty());
        assert_eq!(reader.read(|value| *value).unwrap(), 3);
        assert_eq!(captures.load(Ordering::SeqCst), 0);

        owner.close().await.unwrap();
    });
    let storage = backend.state.lock().unwrap();
    assert!(storage.closed);
    assert!(storage.loads.is_empty() && storage.saves.is_empty());
    assert_eq!(
        (storage.revisioned_load_calls, storage.revisioned_save_calls),
        (0, 0)
    );
    assert!(storage.bytes.is_none());
}

// Catches publishing a snapshot after its capture callback revoked this
// registration, both during initial delivery and later actor publication.
#[test]
fn capture_time_subscription_cancellation_keeps_the_last_delivered_value() {
    use rca_core::SharedSubscribeContext;
    use rca_persistence::DocumentSubscription;
    use std::sync::atomic::{AtomicUsize, Ordering};

    for (cancel_at, retained, capture_count) in [(7, 3, 1), (9, 7, 2)] {
        let runtime = DeterministicRuntime::with_step_budget(64);
        let backend = Memory::new();
        let (state, reducer) = SharedDocument::revisioned(7, backend.clone(), Binary);
        let session = reducer.session();
        let store = Store::new(state, reducer, runtime.runtime());
        let cancellation: Arc<Mutex<Option<DocumentSubscription>>> = Arc::new(Mutex::new(None));
        let cancelling = Arc::clone(&cancellation);
        let captures = Arc::new(AtomicUsize::new(0));
        let capturing = Arc::clone(&captures);
        let owner = SharedDocumentOwner::new(
            store.scope(
                StatePath::new(|state: &SharedDocumentState<u32, u64>| state, |state| state),
                ActionPath::new(
                    |action: &SharedDocumentAction<u32, u64>| Some(action),
                    |action| action,
                ),
            ),
            move |value: &u32| {
                capturing.fetch_add(1, Ordering::SeqCst);
                if *value == cancel_at {
                    let guard = cancelling.lock().unwrap().take().unwrap();
                    guard.cancel();
                }
                *value
            },
            session,
        );
        let key = owner.key(SharedKeyIdentity::new::<Binary>("capture-cancel", "binary"));
        let (reader, subscriber) = cached_reference_and_subscriber();

        run_document_session_contract(&runtime, async {
            let _ = store.send(SharedDocumentAction::reload()).await.unwrap();
            runtime.run_until_stalled().unwrap();
            backend.complete(0, Ok(present(1, 7)));
            runtime.run_until_stalled().unwrap();
            let guard = key
                .subscribe(SharedSubscribeContext::UserInitiated, subscriber)
                .unwrap();
            *cancellation.lock().unwrap() = Some(guard.clone());
            assert!(guard.status().is_pending());
            runtime.run_until_stalled().unwrap();
            assert_eq!(reader.read(|value| *value).unwrap(), retained);
            if cancel_at == 7 {
                assert!(guard.status().is_cancelled());
            } else {
                assert!(guard.status().is_active());
            }

            let _ = store.send(SharedDocumentAction::reload()).await.unwrap();
            runtime.run_until_stalled().unwrap();
            backend.complete(0, Ok(present(2, 9)));
            runtime.run_until_stalled().unwrap();
            assert_eq!(reader.read(|value| *value).unwrap(), retained);
            assert_eq!(captures.load(Ordering::SeqCst), capture_count);
            assert!(guard.status().is_cancelled());
            assert!(guard.status().error().is_none());
            assert!(cancellation.lock().unwrap().is_none());
            assert_eq!(
                store
                    .read(|state| {
                        (
                            *state.value(),
                            state.revision(),
                            state.backend_revision().copied(),
                        )
                    })
                    .await
                    .unwrap(),
                (9, 0, Some(2))
            );
            owner.close().await.unwrap();
            assert!(guard.status().is_cancelled());
            store.shutdown().await.unwrap();
        });
        let storage = backend.state.lock().unwrap();
        assert!(storage.closed);
        assert!(storage.loads.is_empty() && storage.saves.is_empty());
        assert_eq!(
            (storage.revisioned_load_calls, storage.revisioned_save_calls),
            (2, 0)
        );
    }
}

// Cancellation from a displaced value must survive the post-publication status update.
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "keeps displaced-value cancellation and later delivery in one public lifecycle trace"
)]
fn displaced_value_cancellation_cannot_be_revived_by_subscription_activation() {
    use rca_core::{SharedSubscribeContext, SharedSubscriber};
    use rca_persistence::DocumentSubscription;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Cancellation {
        guard: Mutex<Option<DocumentSubscription>>,
        drops: AtomicUsize,
    }

    #[derive(Clone)]
    struct Value {
        number: u32,
        cancellation: Option<Arc<Cancellation>>,
    }

    impl Drop for Value {
        fn drop(&mut self) {
            if let Some(cancellation) = &self.cancellation {
                let guard = cancellation.guard.lock().unwrap().take().unwrap();
                assert!(guard.status().is_pending());
                // Reenters the registration fences from the displaced-value destructor.
                guard.cancel();
                assert!(guard.status().is_cancelled());
                cancellation.drops.fetch_add(1, Ordering::SeqCst);
            }
        }
    }

    struct Encoding;
    impl Codec<Value> for Encoding {
        type Error = StorageError;

        fn encode(&self, value: &Value) -> Result<Vec<u8>, StorageError> {
            Binary.encode(&value.number)
        }

        fn decode(&self, bytes: &[u8]) -> Result<Value, StorageError> {
            Binary.decode(bytes).map(|number| Value {
                number,
                cancellation: None,
            })
        }
    }

    struct ReferenceKey(Arc<Mutex<Option<SharedSubscriber<Value>>>>);
    impl SharedReaderKey for ReferenceKey {
        type Value = Value;
        type Error = std::convert::Infallible;
        type Subscription = ();

        fn identity(&self) -> SharedKeyIdentity {
            SharedKeyIdentity::new::<Self>("displaced-cancellation", "value")
        }

        fn load(
            &self,
            _: SharedLoadContext<'_, Value>,
        ) -> impl Future<Output = Result<Option<Value>, Self::Error>> + Send {
            std::future::ready(Ok(None))
        }

        fn subscribe(
            &self,
            _: SharedSubscribeContext,
            subscriber: SharedSubscriber<Value>,
        ) -> Result<(), Self::Error> {
            *self.0.lock().unwrap() = Some(subscriber);
            Ok(())
        }
    }

    let runtime = DeterministicRuntime::with_step_budget(64);
    let backend = Memory::new();
    let (state, reducer) = SharedDocument::revisioned(
        Value {
            number: 7,
            cancellation: None,
        },
        backend.clone(),
        Encoding,
    );
    let session = reducer.session();
    let store = Store::new(state, reducer, runtime.runtime());
    let owner = SharedDocumentOwner::new(
        store.scope(
            StatePath::new(
                |state: &SharedDocumentState<Value, u64>| state,
                |state| state,
            ),
            ActionPath::new(
                |action: &SharedDocumentAction<Value, u64>| Some(action),
                |action| action,
            ),
        ),
        Clone::clone,
        session,
    );
    let key = owner.key(SharedKeyIdentity::new::<Encoding>(
        "displaced-cancellation",
        "binary",
    ));
    let cancellation = Arc::new(Cancellation {
        guard: Mutex::new(None),
        drops: AtomicUsize::new(0),
    });
    let subscriber = Arc::new(Mutex::new(None));
    let cache = SharedKeyCache::new();

    run_document_session_contract(&runtime, async {
        let reader = cache
            .reader(ReferenceKey(Arc::clone(&subscriber)), || Value {
                number: 3,
                cancellation: Some(Arc::clone(&cancellation)),
            })
            .await
            .unwrap();
        let subscriber = subscriber.lock().unwrap().take().unwrap();
        let _ = store.send(SharedDocumentAction::reload()).await.unwrap();
        runtime.run_until_stalled().unwrap();
        backend.complete(0, Ok(present(1, 7)));
        runtime.run_until_stalled().unwrap();

        let guard = key
            .subscribe(SharedSubscribeContext::UserInitiated, subscriber)
            .unwrap();
        *cancellation.guard.lock().unwrap() = Some(guard.clone());
        assert!(guard.status().is_pending());
        assert_eq!(cancellation.drops.load(Ordering::SeqCst), 0);
        runtime.run_until_stalled().unwrap();

        // The replacement committed before its displaced value cancelled the guard.
        assert_eq!(reader.read(|value| value.number).unwrap(), 7);
        assert!(guard.status().is_cancelled());
        assert!(!guard.status().is_active());
        assert!(!guard.status().is_finished());
        assert!(guard.status().error().is_none());
        assert_eq!(cancellation.drops.load(Ordering::SeqCst), 1);
        assert!(cancellation.guard.lock().unwrap().is_none());

        let _ = store.send(SharedDocumentAction::reload()).await.unwrap();
        runtime.run_until_stalled().unwrap();
        backend.complete(0, Ok(present(2, 9)));
        runtime.run_until_stalled().unwrap();
        assert_eq!(store.read(|state| state.value().number).await.unwrap(), 9);
        assert_eq!(reader.read(|value| value.number).unwrap(), 7);
        assert!(guard.status().is_cancelled());
        assert!(guard.status().error().is_none());
        assert_eq!(cancellation.drops.load(Ordering::SeqCst), 1);

        owner.close().await.unwrap();
        assert!(guard.status().is_cancelled());
        store.shutdown().await.unwrap();
        drop(reader);
        assert_eq!(cancellation.drops.load(Ordering::SeqCst), 1);
    });
    let storage = backend.state.lock().unwrap();
    assert!(storage.closed);
    assert!(storage.loads.is_empty() && storage.saves.is_empty());
    assert_eq!(
        (storage.revisioned_load_calls, storage.revisioned_save_calls),
        (2, 0)
    );
}

// Catches reporting Active when initial capture drops the last destination
// reader, or retaining that failed registration for later publication.
#[test]
fn initial_capture_losing_the_final_reader_reports_ended() {
    use rca_core::SharedSubscribeContext;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let runtime = DeterministicRuntime::with_step_budget(64);
    let backend = Memory::new();
    let (state, reducer) = SharedDocument::revisioned(7, backend.clone(), Binary);
    let session = reducer.session();
    let store = Store::new(state, reducer, runtime.runtime());
    let (reader, subscriber) = cached_reference_and_subscriber();
    let destination = Arc::new(Mutex::new(Some(reader)));
    let retiring = Arc::clone(&destination);
    let captures = Arc::new(AtomicUsize::new(0));
    let capturing = Arc::clone(&captures);
    let owner = SharedDocumentOwner::new(
        store.scope(
            StatePath::new(|state: &SharedDocumentState<u32, u64>| state, |state| state),
            ActionPath::new(
                |action: &SharedDocumentAction<u32, u64>| Some(action),
                |action| action,
            ),
        ),
        move |value: &u32| {
            capturing.fetch_add(1, Ordering::SeqCst);
            let reader = retiring.lock().unwrap().take().unwrap();
            drop(reader);
            *value
        },
        session,
    );
    let key = owner.key(SharedKeyIdentity::new::<Binary>(
        "capture-reader-drop",
        "binary",
    ));

    run_document_session_contract(&runtime, async {
        let _ = store.send(SharedDocumentAction::reload()).await.unwrap();
        runtime.run_until_stalled().unwrap();
        backend.complete(0, Ok(present(1, 7)));
        runtime.run_until_stalled().unwrap();
        let guard = key
            .subscribe(SharedSubscribeContext::UserInitiated, subscriber.clone())
            .unwrap();
        assert!(guard.status().is_pending());
        runtime.run_until_stalled().unwrap();
        assert!(destination.lock().unwrap().is_none());
        assert!(subscriber.prepare_publication().is_none());
        let status = guard.status();
        let error = status
            .error()
            .expect("initial delivery has no remaining destination");
        assert!(error.is_ended());
        assert!(!status.is_active());
        assert!(!status.is_cancelled());
        assert!(error.error().is_none());
        assert!(error.source().is_none());

        let _ = store.send(SharedDocumentAction::reload()).await.unwrap();
        runtime.run_until_stalled().unwrap();
        backend.complete(0, Ok(present(2, 9)));
        runtime.run_until_stalled().unwrap();
        assert_eq!(captures.load(Ordering::SeqCst), 1);
        assert_eq!(store.read(|state| *state.value()).await.unwrap(), 9);
        assert!(guard.status().error().unwrap().is_ended());
        owner.close().await.unwrap();
        store.shutdown().await.unwrap();
    });
    let storage = backend.state.lock().unwrap();
    assert!(storage.closed);
    assert!(storage.loads.is_empty() && storage.saves.is_empty());
    assert_eq!(
        (storage.revisioned_load_calls, storage.revisioned_save_calls),
        (2, 0)
    );
}

// Catches accepting a subscriber from another live session or from a session
// that expired after enqueueing, before invoking capture or starting I/O.
#[test]
fn foreign_subscription_admission_preserves_the_destination_and_original_session_error() {
    use rca_core::SharedSubscribeContext;
    use std::sync::atomic::{AtomicUsize, Ordering};

    for (expire_before_reduction, expected_error) in [
        (false, "document key WrongSession failed"),
        (true, "document key Closed failed"),
    ] {
        let runtime = DeterministicRuntime::with_step_budget(64);
        let backend = Memory::new();
        let foreign_backend = Memory::new();
        let (state, reducer) = SharedDocument::revisioned(7, backend.clone(), Binary);
        let session = reducer.session();
        let (_, foreign_reducer) = SharedDocument::revisioned(23, foreign_backend.clone(), Binary);
        let foreign_session = foreign_reducer.session();
        let store = Store::new(state, reducer, runtime.runtime());
        let owner = document_owner(&store, session);
        let captures = Arc::new(AtomicUsize::new(0));
        let capturing = Arc::clone(&captures);
        let mismatched = SharedDocumentOwner::new(
            store.scope(
                StatePath::new(|state: &SharedDocumentState<u32, u64>| state, |state| state),
                ActionPath::new(
                    |action: &SharedDocumentAction<u32, u64>| Some(action),
                    |action| action,
                ),
            ),
            move |value: &u32| {
                capturing.fetch_add(1, Ordering::SeqCst);
                *value
            },
            foreign_session.clone(),
        );
        let key = mismatched.key(SharedKeyIdentity::new::<Binary>(
            "foreign-subscribe",
            "binary",
        ));
        let (reader, subscriber) = cached_reference_and_subscriber();
        let guard = key
            .subscribe(SharedSubscribeContext::UserInitiated, subscriber)
            .unwrap();
        assert!(guard.status().is_pending());
        let mut foreign = Some((mismatched, foreign_reducer, foreign_session));

        run_document_session_contract(&runtime, async {
            if expire_before_reduction {
                let (mismatched, reducer, session) = foreign.take().unwrap();
                let mut closing = Box::pin(session.close());
                assert!(matches!(
                    closing
                        .as_mut()
                        .poll(&mut Context::from_waker(Waker::noop())),
                    Poll::Ready(Ok(()))
                ));
                drop(closing);
                drop((mismatched, reducer, session));
            }
            runtime.run_until_stalled().unwrap();
            let status = guard.status();
            let error = status
                .error()
                .expect("foreign subscription must be rejected");
            assert!(error.is_session());
            assert_eq!(error.to_string().lines().next(), Some(expected_error));
            assert!(!error.is_ended());
            assert!(!status.is_active());
            assert!(!status.is_cancelled());
            assert!(error.error().is_none());
            assert!(error.source().is_none());
            assert_eq!(captures.load(Ordering::SeqCst), 0);
            assert_eq!(reader.read(|value| *value).unwrap(), 3);
            assert_eq!(
                store
                    .read(|state| (*state.value(), state.revision()))
                    .await
                    .unwrap(),
                (7, 0)
            );
            for storage in [&backend, &foreign_backend] {
                let storage = storage.state.lock().unwrap();
                assert_eq!(
                    (storage.revisioned_load_calls, storage.revisioned_save_calls),
                    (0, 0)
                );
                assert!(storage.loads.is_empty() && storage.saves.is_empty());
            }
            if let Some((mismatched, reducer, session)) = foreign.take() {
                session.close().await.unwrap();
                drop((mismatched, reducer, session));
            }
            owner.close().await.unwrap();
            store.shutdown().await.unwrap();
        });
        assert!(backend.state.lock().unwrap().closed);
        assert!(foreign_backend.state.lock().unwrap().closed);
    }
}

/// Loads value seven and retains a separate owner of its active subscription.
///
/// The caller can drop the reader independently of the returned subscription.
async fn cached_reader_and_retained_subscription(
    runtime: &DeterministicRuntime,
    backend: &Memory,
    key: rca_persistence::DocumentKey<u32, u64>,
) -> (
    rca_core::SharedReader<u32>,
    rca_persistence::DocumentSubscription,
) {
    use rca_core::{SharedSubscribeContext, SharedSubscriber};
    use rca_persistence::{DocumentKey, DocumentKeyError, DocumentSubscription};

    /// Delegates storage operations while capturing a second subscription owner.
    struct RetainedKey {
        key: DocumentKey<u32, u64>,
        subscription: Arc<Mutex<Option<DocumentSubscription>>>,
    }
    impl SharedReaderKey for RetainedKey {
        type Value = u32;
        type Error = DocumentKeyError;
        type Subscription = DocumentSubscription;

        fn identity(&self) -> SharedKeyIdentity {
            self.key.identity()
        }
        fn load(
            &self,
            context: SharedLoadContext<'_, u32>,
        ) -> impl Future<Output = Result<Option<u32>, DocumentKeyError>> + Send {
            self.key.load(context)
        }
        fn subscribe(
            &self,
            context: SharedSubscribeContext,
            subscriber: SharedSubscriber<u32>,
        ) -> Result<DocumentSubscription, DocumentKeyError> {
            let guard = self.key.subscribe(context, subscriber)?;
            *self.subscription.lock().unwrap() = Some(guard.clone());
            Ok(guard)
        }
    }

    let subscription = Arc::new(Mutex::new(None));
    let cache = SharedKeyCache::new();
    let mut loading = Box::pin(cache.reader(
        RetainedKey {
            key,
            subscription: Arc::clone(&subscription),
        },
        || 0,
    ));
    assert!(
        loading
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_pending()
    );
    runtime.run_until_stalled().unwrap();
    backend.complete(0, Ok(present(1, 7)));
    runtime.run_until_stalled().unwrap();
    let reader = loading.await.unwrap();
    runtime.run_until_stalled().unwrap();
    let guard = subscription.lock().unwrap().take().unwrap();
    assert!(guard.status().is_active());
    assert_eq!(reader.read(|value| *value).unwrap(), 7);
    (reader, guard)
}

// Retired registrations must release captures without callbacks or stalled progress.
#[test]
fn cancelled_or_dropped_registration_releases_capture_without_later_delivery() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    for retain_cancelled in [false, true] {
        let runtime = DeterministicRuntime::with_step_budget(64);
        let backend = Memory::new();
        let (state, reducer) = SharedDocument::revisioned(7, backend.clone(), Binary);
        let session = reducer.session();
        let store = Store::new(state, reducer, runtime.runtime());
        let captures = Arc::new(AtomicUsize::new(0));
        let capturing = Arc::clone(&captures);
        let owner = SharedDocumentOwner::new(
            store.scope(
                StatePath::new(|state: &SharedDocumentState<u32, u64>| state, |state| state),
                ActionPath::new(
                    |action: &SharedDocumentAction<u32, u64>| Some(action),
                    |action| action,
                ),
            ),
            move |value: &u32| {
                capturing.fetch_add(1, Ordering::SeqCst);
                *value
            },
            session.clone(),
        );
        let key = owner.key(SharedKeyIdentity::new::<Binary>(
            "retired-subscription",
            "binary",
        ));
        run_document_session_contract(&runtime, async {
            let (reader, guard) =
                cached_reader_and_retained_subscription(&runtime, &backend, key.clone()).await;
            assert_eq!(captures.load(Ordering::SeqCst), 2);
            let (reader, guard) = if retain_cancelled {
                guard.cancel();
                assert!(guard.status().is_cancelled());
                (Some(reader), Some(guard))
            } else {
                drop(reader);
                drop(guard);
                (None, None)
            };
            drop(key);
            drop(owner);
            assert_eq!(Arc::strong_count(&captures), 2);

            let _ = store.send(SharedDocumentAction::reload()).await.unwrap();
            runtime.run_until_stalled().unwrap();
            backend.complete(0, Ok(present(2, 9)));
            runtime.run_until_stalled().unwrap();
            assert_eq!(captures.load(Ordering::SeqCst), 2);
            assert_eq!(
                store
                    .read(|state| (
                        *state.value(),
                        state.revision(),
                        state.backend_revision().copied(),
                        state.is_loading()
                    ))
                    .await
                    .unwrap(),
                (9, 0, Some(2), false)
            );
            if let Some(reader) = &reader {
                assert_eq!(reader.read(|value| *value).unwrap(), 7);
            }
            if let Some(guard) = &guard {
                assert!(guard.status().is_cancelled());
                assert!(guard.status().error().is_none());
            }
            drop(reader);
            drop(guard);
            assert_eq!(Arc::strong_count(&captures), 1);
            session.close().await.unwrap();
            store.shutdown().await.unwrap();
        });
        let storage = backend.state.lock().unwrap();
        assert!(storage.closed);
        assert!(storage.loads.is_empty());
        assert!(storage.saves.is_empty());
        assert_eq!(
            (storage.revisioned_load_calls, storage.revisioned_save_calls),
            (2, 0)
        );
    }
}

// Abort must release an expired registration's capture without publishing again.
#[test]
fn abort_releases_an_expired_subscription_without_changing_its_reader() {
    use rca_core::SharedSubscribeContext;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let runtime = DeterministicRuntime::with_step_budget(64);
    let backend = Memory::new();
    let (state, reducer) = SharedDocument::revisioned(7, backend.clone(), Binary);
    let session = reducer.session();
    let store = Store::new(state, reducer, runtime.runtime());
    let captures = Arc::new(AtomicUsize::new(0));
    let capturing = Arc::clone(&captures);
    let owner = SharedDocumentOwner::new(
        store.scope(
            StatePath::new(|state: &SharedDocumentState<u32, u64>| state, |state| state),
            ActionPath::new(
                |action: &SharedDocumentAction<u32, u64>| Some(action),
                |action| action,
            ),
        ),
        move |value: &u32| {
            capturing.fetch_add(1, Ordering::SeqCst);
            *value
        },
        session.clone(),
    );
    let key = owner.key(SharedKeyIdentity::new::<Binary>("expired-abort", "binary"));
    let (reader, subscriber) = cached_reference_and_subscriber();

    run_document_session_contract(&runtime, async {
        let _ = store.send(SharedDocumentAction::reload()).await.unwrap();
        runtime.run_until_stalled().unwrap();
        backend.complete(0, Ok(present(1, 7)));
        runtime.run_until_stalled().unwrap();
        let guard = key
            .subscribe(SharedSubscribeContext::UserInitiated, subscriber)
            .unwrap();
        runtime.run_until_stalled().unwrap();
        assert!(guard.status().is_active());
        assert_eq!(reader.read(|value| *value).unwrap(), 7);
        assert_eq!(captures.load(Ordering::SeqCst), 1);
        drop(guard);
        drop(key);
        drop(owner);
        assert_eq!(Arc::strong_count(&captures), 2);

        // No publication or replacement subscription has pruned the weak entry.
        let _ = store.send(SharedDocumentAction::abort()).await.unwrap();
        assert_eq!(Arc::strong_count(&captures), 1);
        assert_eq!(captures.load(Ordering::SeqCst), 1);
        assert_eq!(reader.read(|value| *value).unwrap(), 7);
        assert_eq!(
            store
                .read(|state| (
                    *state.value(),
                    state.document().is_aborted(),
                    state.is_loading(),
                ))
                .await
                .unwrap(),
            (7, true, false)
        );
        session.close().await.unwrap();
        store.shutdown().await.unwrap();
    });
    let storage = backend.state.lock().unwrap();
    assert!(storage.closed);
    assert!(storage.loads.is_empty() && storage.saves.is_empty());
    assert_eq!(
        (storage.revisioned_load_calls, storage.revisioned_save_calls),
        (1, 0)
    );
}

// Losing the captured reference must end its retained guard, not leave it active.
#[test]
fn retained_subscription_reports_ended_after_final_reader_is_dropped() {
    let runtime = DeterministicRuntime::with_step_budget(64);
    let backend = Memory::new();
    let (state, reducer) = SharedDocument::revisioned(7, backend.clone(), Binary);
    let session = reducer.session();
    let store = Store::new(state, reducer, runtime.runtime());
    let owner = document_owner(&store, session);
    let key = owner.key(SharedKeyIdentity::new::<Binary>(
        "orphaned-subscription",
        "binary",
    ));
    run_document_session_contract(&runtime, async {
        let (reader, guard) =
            cached_reader_and_retained_subscription(&runtime, &backend, key.clone()).await;
        drop(reader);
        assert!(guard.status().is_active());
        let _ = store.send(SharedDocumentAction::reload()).await.unwrap();
        runtime.run_until_stalled().unwrap();
        backend.complete(0, Ok(present(2, 9)));
        runtime.run_until_stalled().unwrap();
        assert_eq!(store.read(|state| *state.value()).await.unwrap(), 9);
        let status = guard.status();
        let error = status.error().unwrap();
        assert!(error.is_ended());
        assert!(!status.is_cancelled());
        assert!(error.error().is_none());
        assert!(error.source().is_none());

        guard.cancel();
        assert!(guard.status().error().unwrap().is_ended());
        drop(guard);
        assert!(owner.subscription_statuses(&key).is_empty());
        owner.close().await.unwrap();
        store.shutdown().await.unwrap();
    });
    let storage = backend.state.lock().unwrap();
    assert!(storage.closed);
    assert!(storage.loads.is_empty());
    assert!(storage.saves.is_empty());
    assert_eq!(
        (storage.revisioned_load_calls, storage.revisioned_save_calls),
        (2, 0)
    );
}

// Catches a cancelled queued subscriber invoking capture or becoming active
// after the last cached reader has already relinquished publication authority.
#[test]
fn dropped_cached_reader_cancels_queued_document_subscription() {
    let runtime = DeterministicRuntime::new();
    let backend = Memory::new();
    let (state, reducer) = SharedDocument::revisioned(7, backend.clone(), Binary);
    let session = reducer.session();
    let store = Store::new(state, reducer, runtime.runtime());
    let captures = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let capturing = Arc::clone(&captures);
    let owner = SharedDocumentOwner::new(
        store.scope(
            StatePath::new(|state: &SharedDocumentState<u32, u64>| state, |state| state),
            ActionPath::new(
                |action: &SharedDocumentAction<u32, u64>| Some(action),
                |action| action,
            ),
        ),
        move |value: &u32| {
            capturing.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            *value
        },
        session,
    );
    let key = owner.key(SharedKeyIdentity::new::<Binary>(
        "queued-subscribe",
        "binary",
    ));
    let cache = SharedKeyCache::new();
    block_on(runtime.run(async {
        let mut loading = Box::pin(cache.reader(key.clone(), || 0));
        assert!(
            loading
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending()
        );
        runtime.run_until_stalled().unwrap();
        backend.complete(0, Ok(present(1, 7)));
        runtime.run_until_stalled().unwrap();
        let reader = loading.await.unwrap();
        assert_eq!(reader.read(|value| *value).unwrap(), 7);
        assert!(owner.subscription_statuses(&key)[0].is_pending());
        assert_eq!(captures.load(std::sync::atomic::Ordering::SeqCst), 1);
        drop(reader);
        assert!(owner.subscription_statuses(&key)[0].is_cancelled());
        runtime.run_until_stalled().unwrap();
        assert!(owner.subscription_statuses(&key).is_empty());
        assert_eq!(captures.load(std::sync::atomic::Ordering::SeqCst), 1);
        let _ = store.send(SharedDocumentAction::reload()).await.unwrap();
        runtime.run_until_stalled().unwrap();
        backend.complete(0, Ok(present(2, 9)));
        runtime.run_until_stalled().unwrap();
        assert_eq!(store.read(|state| *state.value()).await.unwrap(), 9);
        assert_eq!(captures.load(std::sync::atomic::Ordering::SeqCst), 1);
        owner.close().await.unwrap();
        store.shutdown().await.unwrap();
    }))
    .unwrap();
}

// Catches accepting a queued subscription after document abort, or confusing
// its ended registration with closure of the still-open resource session.
#[test]
fn queued_document_subscription_after_abort_reports_ended_without_capture() {
    let runtime = DeterministicRuntime::new();
    let backend = Memory::new();
    let (state, reducer) = SharedDocument::revisioned(7, backend.clone(), Binary);
    let session = reducer.session();
    let store = Store::new(state, reducer, runtime.runtime());
    let captures = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let capturing = Arc::clone(&captures);
    let owner = SharedDocumentOwner::new(
        store.scope(
            StatePath::new(|state: &SharedDocumentState<u32, u64>| state, |state| state),
            ActionPath::new(
                |action: &SharedDocumentAction<u32, u64>| Some(action),
                |action| action,
            ),
        ),
        move |value: &u32| {
            capturing.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            *value
        },
        session,
    );
    let key = owner.key(SharedKeyIdentity::new::<Binary>(
        "aborted-subscribe",
        "binary",
    ));
    let cache = SharedKeyCache::new();
    block_on(runtime.run(async {
        let mut loading = Box::pin(cache.reader(key.clone(), || 0));
        assert!(
            loading
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending()
        );
        runtime.run_until_stalled().unwrap();
        backend.complete(0, Ok(present(1, 7)));
        runtime.run_until_stalled().unwrap();
        let abort = store.try_send(SharedDocumentAction::abort()).unwrap();
        // Consume the ready load reply without driving the actor: Subscribe is
        // admitted behind Abort, while the resource session remains open.
        let Poll::Ready(Ok(reader)) = loading
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
        else {
            panic!("the completed load must admit its queued subscription");
        };
        assert!(owner.subscription_statuses(&key)[0].is_pending());
        assert_eq!(captures.load(std::sync::atomic::Ordering::SeqCst), 1);
        runtime.run_until_stalled().unwrap();
        abort.finish().await.unwrap().finish().await;
        let statuses = owner.subscription_statuses(&key);
        assert_eq!(statuses.len(), 1);
        assert!(!statuses[0].is_active());
        assert!(!statuses[0].is_cancelled());
        let error = statuses[0]
            .error()
            .expect("aborted subscription is rejected");
        assert!(error.is_ended());
        assert!(!error.is_session());
        assert!(error.error().is_none());
        assert_eq!(captures.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(reader.read(|value| *value).unwrap(), 7);
        assert!(
            store
                .read(|state| state.document().is_aborted())
                .await
                .unwrap()
        );
        assert!(!backend.state.lock().unwrap().closed);
        drop(reader);
        assert!(owner.subscription_statuses(&key).is_empty());
        owner.close().await.unwrap();
        store.shutdown().await.unwrap();
    }))
    .unwrap();
    let storage = backend.state.lock().unwrap();
    assert!(storage.closed);
    assert!(storage.loads.is_empty());
    assert!(storage.saves.is_empty());
    assert_eq!(
        (storage.revisioned_load_calls, storage.revisioned_save_calls),
        (1, 0)
    );
}

// Catches a retained queued subscriber becoming active or capturing after the
// resource session has physically closed, rather than reporting terminal Closed.
#[test]
fn retained_queued_document_subscription_reports_closed_without_capture() {
    let runtime = DeterministicRuntime::new();
    let backend = Memory::new();
    let (state, reducer) = SharedDocument::revisioned(7, backend.clone(), Binary);
    let session = reducer.session();
    let store = Store::new(state, reducer, runtime.runtime());
    let captures = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let capturing = Arc::clone(&captures);
    let owner = SharedDocumentOwner::new(
        store.scope(
            StatePath::new(|state: &SharedDocumentState<u32, u64>| state, |state| state),
            ActionPath::new(
                |action: &SharedDocumentAction<u32, u64>| Some(action),
                |action| action,
            ),
        ),
        move |value: &u32| {
            capturing.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            *value
        },
        session.clone(),
    );
    let key = owner.key(SharedKeyIdentity::new::<Binary>(
        "closed-subscribe",
        "binary",
    ));
    let cache = SharedKeyCache::new();
    block_on(runtime.run(async {
        let mut loading = Box::pin(cache.reader(key.clone(), || 0));
        assert!(
            loading
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending()
        );
        runtime.run_until_stalled().unwrap();
        backend.complete(0, Ok(present(1, 7)));
        runtime.run_until_stalled().unwrap();
        // Consume only the completed load reply, leaving Subscribe queued.
        let Poll::Ready(Ok(reader)) = loading
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
        else {
            panic!("the completed load must admit its queued subscription");
        };
        assert!(owner.subscription_statuses(&key)[0].is_pending());
        assert_eq!(captures.load(std::sync::atomic::Ordering::SeqCst), 1);
        let mut closing = Box::pin(session.close());
        assert!(matches!(
            closing
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop())),
            Poll::Ready(Ok(()))
        ));
        drop(closing);
        assert!(backend.state.lock().unwrap().closed);
        assert!(owner.subscription_statuses(&key)[0].is_pending());
        runtime.run_until_stalled().unwrap();
        let statuses = owner.subscription_statuses(&key);
        assert_eq!(statuses.len(), 1);
        assert!(!statuses[0].is_pending());
        assert!(!statuses[0].is_active());
        assert!(!statuses[0].is_cancelled());
        assert!(!statuses[0].is_finished());
        let error = statuses[0]
            .error()
            .expect("closed subscription is rejected");
        assert!(error.is_session());
        assert!(!error.is_ended());
        assert!(!error.is_cancelled());
        assert_eq!(
            error.to_string().lines().next(),
            Some("document key Closed failed")
        );
        assert!(error.error().is_none());
        assert!(error.source().is_none());
        assert_eq!(store.read(|state| *state.value()).await.unwrap(), 7);
        owner.close().await.unwrap();
        assert!(
            owner.subscription_statuses(&key)[0]
                .error()
                .expect("terminal failure survives owner cleanup")
                .is_session()
        );
        assert_eq!(captures.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(reader.read(|value| *value).unwrap(), 7);
        drop(reader);
        assert!(owner.subscription_statuses(&key).is_empty());
        store.shutdown().await.unwrap();
    }))
    .unwrap();
    let storage = backend.state.lock().unwrap();
    assert!(storage.closed);
    assert!(storage.loads.is_empty());
    assert!(storage.saves.is_empty());
    assert_eq!(
        (storage.revisioned_load_calls, storage.revisioned_save_calls),
        (1, 0)
    );
}

// Catches is_loading ignoring producer authority while physical I/O is still
// pending, or session close treating cancellation as physical completion.
#[test]
fn pending_producer_load_reports_revocation_before_physical_completion() {
    let runtime = DeterministicRuntime::new();
    let backend = Memory::new();
    let (state, reducer) = SharedDocument::revisioned(7, backend.clone(), Binary);
    let session = reducer.session();
    let store = Store::new(state, reducer, runtime.runtime());
    let owner = document_owner(&store, session);
    let (join_completion, join) = controlled_future();
    let guard = owner
        .observe_changes::<StorageError, _, _, _>(|_producer| {
            (SharedUpdate::Present(()), || {}, async move {
                join.await.map_err(|_cause| StorageError(71))?
            })
        })
        .unwrap();
    block_on(runtime.run(async {
        runtime.run_until_stalled().unwrap();
        assert!(store.read(SharedDocumentState::is_loading).await.unwrap());
        assert_eq!(backend.state.lock().unwrap().loads.len(), 1);
        guard.cancel();
        assert!(!store.read(SharedDocumentState::is_loading).await.unwrap());
        assert_eq!(backend.state.lock().unwrap().loads.len(), 1);
        let mut closing = Box::pin(owner.close());
        assert!(
            closing
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending()
        );
        backend.complete(0, Ok(present(99, 99)));
        runtime.run_until_stalled().unwrap();
        assert_eq!(
            store
                .read(|state| (
                    *state.value(),
                    state.is_loading(),
                    state.backend_revision().copied()
                ))
                .await
                .unwrap(),
            (7, false, None)
        );
        assert!(
            closing
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending()
        );
        join_completion.complete(Ok(())).unwrap();
        closing.await.unwrap();
        assert!(guard.status().is_cancelled());
        assert_eq!(backend.state.lock().unwrap().revisioned_load_calls, 1);
        store.shutdown().await.unwrap();
    }))
    .unwrap();
}

// Catches replying with captured pre-save bytes even when the actor correctly
// keeps the newly acknowledged value. This is not a latest-started-load error.
#[test]
fn keyed_pre_save_load_receipt_returns_the_acknowledged_current_value() {
    let runtime = DeterministicRuntime::new();
    let backend = CapturedRead::new();
    let (state, reducer) = SharedDocument::revisioned(0, backend.clone(), Binary);
    let session = reducer.session();
    let store = Store::new(state, reducer, runtime.runtime());
    let owner = document_owner(&store, session);
    let key = owner.key(SharedKeyIdentity::new::<Binary>(
        "pre-save-receipt",
        "binary",
    ));
    block_on(runtime.run(async {
        let _ = store.send(SharedDocumentAction::reload()).await.unwrap();
        runtime.run_until_stalled().unwrap();
        backend.release();
        runtime.run_until_stalled().unwrap();
        let mut load = Box::pin(key.load(SharedLoadContext::UserInitiated));
        assert!(
            load.as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending()
        );
        runtime.run_until_stalled().unwrap();
        let (edit, receipt) = SharedDocumentAction::set_and_flush(8);
        let _ = store.send(edit).await.unwrap();
        runtime.run_until_stalled().unwrap();
        backend.memory.complete_save();
        receipt.await.unwrap();
        backend.release();
        runtime.run_until_stalled().unwrap();
        assert_eq!(load.await.unwrap(), Some(8));
        assert_eq!(
            store
                .read(|state| (
                    *state.value(),
                    state.acknowledged_revision(),
                    state.backend_revision().copied(),
                    state.conflict().is_none()
                ))
                .await
                .unwrap(),
            (8, Some(1), Some(2), true)
        );
        owner.close().await.unwrap();
        store.shutdown().await.unwrap();
    }))
    .unwrap();
}

// Catches converting a conflicting keyed load into a successful current-value
// receipt, or publishing external bytes over the local edit after save failure.
#[test]
fn keyed_load_receipt_reports_conflict_after_the_pending_save_conflicts() {
    let runtime = DeterministicRuntime::new();
    let backend = Memory::new();
    let (state, reducer) = SharedDocument::revisioned(7, backend.clone(), Binary);
    let session = reducer.session();
    let store = Store::new(state, reducer, runtime.runtime());
    let owner = document_owner(&store, session);
    let key = owner.key(SharedKeyIdentity::new::<Binary>(
        "conflict-receipt",
        "binary",
    ));
    block_on(runtime.run(async {
        let _ = store.send(SharedDocumentAction::reload()).await.unwrap();
        runtime.run_until_stalled().unwrap();
        backend.complete(0, Ok(present(1, 7)));
        runtime.run_until_stalled().unwrap();
        let (edit, receipt) = SharedDocumentAction::set_and_flush(8);
        let _ = store.send(edit).await.unwrap();
        runtime.run_until_stalled().unwrap();
        let mut load = Box::pin(key.load(SharedLoadContext::UserInitiated));
        assert!(
            load.as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending()
        );
        runtime.run_until_stalled().unwrap();
        backend.external_write(9);
        backend.complete(0, Ok(present(2, 9)));
        runtime.run_until_stalled().unwrap();
        assert!(
            load.as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending()
        );
        backend.complete_save();
        let save_error = receipt.await.unwrap_err();
        let ReceiptError::Persistence(failure) = save_error else {
            panic!("expected compare-save conflict");
        };
        assert!(failure.error().is::<ConflictDetected>());
        let error = load.await.unwrap_err();
        assert!(error.is_conflict());
        assert!(!error.is_superseded());
        assert!(error.error().is_none());
        store
            .read(|state| {
                assert_eq!(*state.value(), 8);
                assert_eq!(state.acknowledged_revision(), Some(0));
                assert_eq!(state.backend_revision(), Some(&1));
                assert!(!state.is_loading());
                let conflict = state.conflict().unwrap();
                assert_eq!(*conflict.revision(), 2);
                assert_eq!(conflict.value(), Some(&9));
                assert_eq!(conflict.bytes(), Some(9_u32.to_le_bytes().as_slice()));
            })
            .await
            .unwrap();
        assert_eq!(
            backend.state.lock().unwrap().bytes,
            Some(9_u32.to_le_bytes().to_vec())
        );
        owner.close().await.unwrap();
        store.shutdown().await.unwrap();
    }))
    .unwrap();
}

// Rejected actor delivery must not masquerade as drainage or physical quiescence.
#[test]
fn rejected_producer_delivery_reports_ended_and_retains_physical_join() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    for reject_terminal in [false, true] {
        let runtime = DeterministicRuntime::with_step_budget(64);
        let backend = Memory::new();
        let (state, reducer) = SharedDocument::revisioned(7, backend.clone(), Binary);
        let session = reducer.session();
        let gate = rca_core::testing::AssertionGate::new();
        let store = Store::builder(state, reducer, runtime.runtime())
            .assertion_gate(gate.clone())
            .build()
            .unwrap();
        let owner = document_owner(&store, session);
        let (join_completion, join) = controlled_future();
        let cancellations = Arc::new(AtomicUsize::new(0));
        let cancelling = Arc::clone(&cancellations);
        let mut endpoint = None;
        let guard = owner
            .observe_snapshots::<StorageError, _, _, _>(|producer| {
                endpoint = Some(producer);
                (
                    SharedUpdate::Present(BackendSnapshot {
                        revision: Some(1),
                        bytes: Some(8_u32.to_le_bytes().to_vec()),
                    }),
                    move || {
                        cancelling.fetch_add(1, Ordering::SeqCst);
                    },
                    async move { join.await.map_err(|_cancelled| StorageError(83))? },
                )
            })
            .unwrap();
        let endpoint = endpoint.unwrap();

        run_document_session_contract(&runtime, async {
            runtime.run_until_stalled().unwrap();
            assert!(guard.status().is_active());
            assert!(gate.has_pending());
            if reject_terminal {
                assert!(gate.release_pending());
                runtime.run_until_stalled().unwrap();
                assert!(!gate.has_pending());
                assert!(guard.status().is_active());
                assert_eq!(cancellations.load(Ordering::SeqCst), 0);
                assert!(endpoint.finish());
                runtime.run_until_stalled().unwrap();
                assert!(gate.has_pending());
            }
            assert!(gate.skip_pending());
            runtime.run_until_stalled().unwrap();
            assert!(!gate.has_pending());
            let status = guard.status();
            assert!(status.error().unwrap().is_ended());
            assert!(!status.is_active() && !status.is_finished());
            assert!(!status.is_cancelled());
            assert_eq!(cancellations.load(Ordering::SeqCst), 1);
            assert_eq!(
                store
                    .read(|state| (
                        *state.value(),
                        state.is_loading(),
                        state.backend_revision().copied()
                    ))
                    .await
                    .unwrap(),
                if reject_terminal {
                    (8, false, Some(1))
                } else {
                    (7, false, None)
                }
            );

            let mut closing = Box::pin(owner.close());
            assert!(
                closing
                    .as_mut()
                    .poll(&mut Context::from_waker(Waker::noop()))
                    .is_pending()
            );
            runtime.run_until_stalled().unwrap();
            assert!(
                closing
                    .as_mut()
                    .poll(&mut Context::from_waker(Waker::noop()))
                    .is_pending()
            );
            assert!(backend.state.lock().unwrap().closed);
            join_completion.complete(Ok(())).unwrap();
            closing.await.unwrap();
            assert!(guard.status().error().unwrap().is_ended());
            assert_eq!(cancellations.load(Ordering::SeqCst), 1);
            store.shutdown().await.unwrap();
        });
        let storage = backend.state.lock().unwrap();
        assert!(storage.loads.is_empty() && storage.saves.is_empty());
        assert_eq!(
            (storage.revisioned_load_calls, storage.revisioned_save_calls),
            (0, 0)
        );
    }
}

mod teardown_host {
    //! Lets tests select task polling order and close admission before discarding work.

    use super::{Arc, Mutex, Poll, VecDeque, fmt};
    use rca_core::{MonotonicInstant, RuntimeDriver, RuntimeFuture};

    // Like document.rs's Stepped host, but explicitly closes admission before
    // discarding accepted work, as required by the public driver contract.
    #[derive(Default)]
    struct HostState {
        /// Rejects later spawns once the test starts teardown.
        tearing_down: bool,
        /// Retains accepted tasks until the test takes each one for manual polling.
        tasks: VecDeque<RuntimeFuture>,
    }
    /// Shares explicit task admission without an executor or advancing clock.
    ///
    /// Tests own every task they take. Future clock deadlines remain pending.
    #[derive(Clone, Default)]
    pub(super) struct Host(Arc<Mutex<HostState>>);
    impl fmt::Debug for Host {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("Host(..)")
        }
    }
    impl RuntimeDriver for Host {
        fn spawn(&self, task: RuntimeFuture) {
            let mut state = self.0.lock().unwrap();
            if state.tearing_down {
                drop(state);
                drop(task);
            } else {
                state.tasks.push_back(task);
            }
        }
        fn now(&self) -> MonotonicInstant {
            MonotonicInstant::ZERO
        }
        fn sleep_until(&self, deadline: MonotonicInstant) -> RuntimeFuture {
            if deadline <= MonotonicInstant::ZERO {
                Box::pin(async {})
            } else {
                Box::pin(std::future::pending())
            }
        }
        fn yield_now(&self) -> RuntimeFuture {
            let mut yielded = false;
            Box::pin(std::future::poll_fn(move |context| {
                if yielded {
                    Poll::Ready(())
                } else {
                    yielded = true;
                    context.waker().wake_by_ref();
                    Poll::Pending
                }
            }))
        }
    }
    impl Host {
        /// Transfers the next accepted task to the test that will poll or drop it.
        pub(super) fn take_task(&self) -> RuntimeFuture {
            self.0.lock().unwrap().tasks.pop_front().unwrap()
        }
        /// Closes admission after the test has taken every accepted task.
        pub(super) fn begin_teardown(&self) {
            let mut state = self.0.lock().unwrap();
            assert!(!state.tearing_down);
            assert!(state.tasks.is_empty());
            state.tearing_down = true;
        }
    }
}

// Losing a committed reply during host teardown must not invent producer success.
#[test]
fn producer_reply_loss_after_commit_reports_ended_and_retains_physical_join() {
    use rca_core::Runtime;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use teardown_host::Host;

    for lose_reply in [false, true] {
        let host = Host::default();
        let backend = Memory::new();
        let (state, reducer) = SharedDocument::revisioned(7, backend.clone(), Binary);
        let session = reducer.session();
        let store = Store::new(state, reducer, Runtime::new(host.clone()));
        let mut actor = host.take_task();
        let owner = document_owner(&store, session);
        let (join_completion, join) = controlled_future();
        let cancellations = Arc::new(AtomicUsize::new(0));
        let cancelling = Arc::clone(&cancellations);
        let mut endpoint = None;
        let guard = owner
            .observe_snapshots::<StorageError, _, _, _>(|producer| {
                endpoint = Some(producer);
                (
                    SharedUpdate::Present(BackendSnapshot {
                        revision: Some(1),
                        bytes: Some(8_u32.to_le_bytes().to_vec()),
                    }),
                    move || {
                        cancelling.fetch_add(1, Ordering::SeqCst);
                    },
                    async move { join.await.map_err(|_cancelled| StorageError(84))? },
                )
            })
            .unwrap();
        let endpoint = endpoint.unwrap();
        let mut context = Context::from_waker(Waker::noop());

        assert!(actor.as_mut().poll(&mut context).is_pending());
        let mut observer = host.take_task();
        assert!(observer.as_mut().poll(&mut context).is_pending());
        assert!(actor.as_mut().poll(&mut context).is_pending());
        let mut reply = host.take_task();
        // The send acknowledgement is ready, but the separate value reply has
        // never been polled. The observer now waits for that value reply.
        assert!(observer.as_mut().poll(&mut context).is_pending());
        let mut read = Box::pin(store.read(|state| {
            (
                *state.value(),
                state.is_loading(),
                state.backend_revision().copied(),
            )
        }));
        assert!(read.as_mut().poll(&mut context).is_pending());
        assert!(actor.as_mut().poll(&mut context).is_pending());
        let Poll::Ready(Ok(value)) = read.as_mut().poll(&mut context) else {
            panic!("the committed snapshot must be readable before its reply runs");
        };
        assert_eq!(value, (8, false, Some(1)));
        drop(read);
        assert!(guard.status().is_active());
        assert_eq!(cancellations.load(Ordering::SeqCst), 0);

        if lose_reply {
            host.begin_teardown();
            drop(reply);
            // Teardown may poll retained tasks in this order. Do not poll the
            // actor first: task-loss shutdown would cancel the observer instead.
            assert!(observer.as_mut().poll(&mut context).is_ready());
            let status = guard.status();
            assert!(status.error().unwrap().is_ended());
            assert!(!status.is_active() && !status.is_finished());
            assert!(!status.is_cancelled());
            assert_eq!(cancellations.load(Ordering::SeqCst), 1);
            assert!(!endpoint.finish());
        } else {
            assert!(reply.as_mut().poll(&mut context).is_ready());
            drop(reply);
            assert!(observer.as_mut().poll(&mut context).is_pending());
            assert!(guard.status().is_active());
            assert!(guard.status().error().is_none());
            assert_eq!(cancellations.load(Ordering::SeqCst), 0);
            host.begin_teardown();
        }
        drop(observer);
        drop(actor);
        assert!(store.is_closed());

        let mut closing = Box::pin(owner.close());
        assert!(closing.as_mut().poll(&mut context).is_pending());
        assert!(backend.state.lock().unwrap().closed);
        assert!(closing.as_mut().poll(&mut context).is_pending());
        join_completion.complete(Ok(())).unwrap();
        assert!(matches!(
            closing.as_mut().poll(&mut context),
            Poll::Ready(Ok(()))
        ));
        assert!(guard.status().error().unwrap().is_ended());
        assert_eq!(cancellations.load(Ordering::SeqCst), 1);
        let storage = backend.state.lock().unwrap();
        assert!(storage.loads.is_empty() && storage.saves.is_empty());
        assert_eq!(
            (storage.revisioned_load_calls, storage.revisioned_save_calls),
            (0, 0)
        );
    }
}

// A document close fences keyed reloads without prematurely closing its session.
#[test]
fn closed_document_rejects_keyed_reload_without_reopening_storage() {
    let runtime = DeterministicRuntime::with_step_budget(64);
    let backend = Memory::new();
    let (state, reducer) = SharedDocument::revisioned(7, backend.clone(), Binary);
    let session = reducer.session();
    let store = Store::new(state, reducer, runtime.runtime());
    let owner = document_owner(&store, session);
    let key = owner.key(SharedKeyIdentity::new::<Binary>(
        "closed-document-reload",
        "binary",
    ));

    run_document_session_contract(&runtime, async {
        let _ = store.send(SharedDocumentAction::reload()).await.unwrap();
        runtime.run_until_stalled().unwrap();
        backend.complete(0, Ok(present(1, 7)));
        runtime.run_until_stalled().unwrap();
        let (close, receipt) = SharedDocumentAction::close();
        let _ = store.send(close).await.unwrap();
        let outcome = receipt.await.unwrap();
        assert_eq!((outcome.revision, outcome.closing), (0, true));
        assert!(!backend.state.lock().unwrap().closed);

        let mut loading = Box::pin(key.load(SharedLoadContext::UserInitiated));
        assert!(
            loading
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending()
        );
        let error = loading.await.unwrap_err();
        assert!(error.is_session());
        assert!(!error.is_ended() && !error.is_full());
        assert_eq!(
            error.to_string().lines().next(),
            Some("document key Closed failed")
        );
        assert!(error.error().is_none() && error.source().is_none());
        assert_eq!(
            store
                .read(|state| (
                    *state.value(),
                    state.revision(),
                    state.acknowledged_revision(),
                    state.backend_revision().copied(),
                    state.document().is_closing(),
                    state.document().is_closed(),
                    state.is_loading(),
                ))
                .await
                .unwrap(),
            (7, 0, Some(0), Some(1), true, true, false)
        );
        {
            let storage = backend.state.lock().unwrap();
            assert!(!storage.closed);
            assert!(storage.loads.is_empty() && storage.saves.is_empty());
            assert_eq!(
                (storage.revisioned_load_calls, storage.revisioned_save_calls),
                (1, 0)
            );
        };

        owner.close().await.unwrap();
        store.shutdown().await.unwrap();
    });
    let storage = backend.state.lock().unwrap();
    assert!(storage.closed);
    assert!(storage.loads.is_empty() && storage.saves.is_empty());
    assert_eq!(
        (storage.revisioned_load_calls, storage.revisioned_save_calls),
        (1, 0)
    );
}

// Cancelling queued observation admission must not launch delivery or discharge join debt.
#[test]
fn cancelling_before_observe_commit_preserves_unstarted_delivery_and_physical_join() {
    use rca_core::Runtime;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use teardown_host::Host;

    let host = Host::default();
    let backend = Memory::new();
    let (state, reducer) = SharedDocument::revisioned(7, backend.clone(), Binary);
    let session = reducer.session();
    let store = Store::new(state, reducer, Runtime::new(host.clone()));
    let mut actor = host.take_task();
    let owner = document_owner(&store, session);
    let (join_completion, join) = controlled_future();
    let join_owner = Arc::new(());
    let weak_join = Arc::downgrade(&join_owner);
    let cancellations = Arc::new(AtomicUsize::new(0));
    let cancelling = Arc::clone(&cancellations);
    let mut endpoint = None;
    let guard = owner
        .observe_changes::<StorageError, _, _, _>(|producer| {
            endpoint = Some(producer);
            (
                SharedUpdate::Present(()),
                move || {
                    cancelling.fetch_add(1, Ordering::SeqCst);
                },
                async move {
                    let result = join.await.map_err(|_cancelled| StorageError(85))?;
                    drop(join_owner);
                    result
                },
            )
        })
        .unwrap();
    let endpoint = endpoint.unwrap();
    assert!(guard.status().is_pending());
    // No actor or delivery future has been polled before this public cancellation.
    guard.cancel();
    assert!(guard.status().is_cancelled());
    assert_eq!(cancellations.load(Ordering::SeqCst), 1);
    assert!(weak_join.upgrade().is_some());

    let mut context = Context::from_waker(Waker::noop());
    assert!(actor.as_mut().poll(&mut context).is_pending());
    let status = guard.status();
    assert!(status.is_cancelled());
    assert!(!status.is_active() && !status.is_finished());
    assert!(status.error().is_none());
    assert!(matches!(
        endpoint.publish(SharedUpdate::Present(())),
        Err(SharedUpdate::Present(()))
    ));
    assert!(!endpoint.finish());
    let mut read = Box::pin(store.read(|state| {
        (
            *state.value(),
            state.is_loading(),
            state.backend_revision().copied(),
        )
    }));
    assert!(read.as_mut().poll(&mut context).is_pending());
    assert!(actor.as_mut().poll(&mut context).is_pending());
    assert!(matches!(
        read.as_mut().poll(&mut context),
        Poll::Ready(Ok((7, false, None)))
    ));
    drop(read);

    let mut closing = Box::pin(owner.close());
    assert!(closing.as_mut().poll(&mut context).is_pending());
    assert!(actor.as_mut().poll(&mut context).is_pending());
    assert!(closing.as_mut().poll(&mut context).is_pending());
    assert!(backend.state.lock().unwrap().closed);
    assert!(weak_join.upgrade().is_some());
    assert!(closing.as_mut().poll(&mut context).is_pending());
    join_completion.complete(Ok(())).unwrap();
    assert!(matches!(
        closing.as_mut().poll(&mut context),
        Poll::Ready(Ok(()))
    ));
    drop(closing);
    assert!(weak_join.upgrade().is_none());
    guard.cancel();
    assert!(guard.status().is_cancelled());
    assert!(guard.status().error().is_none());
    assert_eq!(cancellations.load(Ordering::SeqCst), 1);

    let mut shutdown = Box::pin(store.shutdown());
    assert!(shutdown.as_mut().poll(&mut context).is_pending());
    assert!(actor.as_mut().poll(&mut context).is_ready());
    assert!(matches!(
        shutdown.as_mut().poll(&mut context),
        Poll::Ready(Ok(()))
    ));
    drop(shutdown);
    // This existing host check requires its task queue to be empty. The actor
    // was its only extracted task: even a dormant delivery launch fails this.
    host.begin_teardown();
    let storage = backend.state.lock().unwrap();
    assert!(storage.closed);
    assert!(storage.loads.is_empty() && storage.saves.is_empty());
    assert_eq!(
        (storage.revisioned_load_calls, storage.revisioned_save_calls),
        (0, 0)
    );
    assert!(storage.bytes.is_none());
}

// Idle delivery cancellation must preserve its terminal status and physical join debt.
#[test]
fn cancelling_an_idle_producer_preserves_cancellation_until_physical_join() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let runtime = DeterministicRuntime::with_step_budget(64);
    let backend = Memory::new();
    let (state, reducer) = SharedDocument::revisioned(7, backend.clone(), Binary);
    let session = reducer.session();
    let store = Store::new(state, reducer, runtime.runtime());
    let owner = document_owner(&store, session);
    let (join_completion, join) = controlled_future();
    let cancellations = Arc::new(AtomicUsize::new(0));
    let cancelling = Arc::clone(&cancellations);
    let mut endpoint = None;
    let guard = owner
        .observe_changes::<StorageError, _, _, _>(|producer| {
            endpoint = Some(producer);
            (
                SharedUpdate::Present(()),
                move || {
                    cancelling.fetch_add(1, Ordering::SeqCst);
                },
                async move { join.await.map_err(|_cancelled| StorageError(81))? },
            )
        })
        .unwrap();
    let endpoint = endpoint.unwrap();

    run_document_session_contract(&runtime, async {
        runtime.run_until_stalled().unwrap();
        backend.complete(0, Ok(present(1, 8)));
        runtime.run_until_stalled().unwrap();
        assert!(guard.status().is_active());
        assert_eq!(store.read(|state| *state.value()).await.unwrap(), 8);
        assert!(backend.state.lock().unwrap().loads.is_empty());

        guard.cancel();
        runtime.run_until_stalled().unwrap();
        let status = guard.status();
        assert!(status.is_cancelled());
        assert!(status.error().is_none());
        assert!(!status.is_finished());
        assert_eq!(cancellations.load(Ordering::SeqCst), 1);
        assert!(matches!(
            endpoint.publish(SharedUpdate::Present(())),
            Err(SharedUpdate::Present(()))
        ));
        assert!(!endpoint.finish());

        let mut closing = Box::pin(owner.close());
        assert!(
            closing
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending()
        );
        runtime.run_until_stalled().unwrap();
        assert!(
            closing
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending()
        );
        assert!(backend.state.lock().unwrap().closed);
        join_completion.complete(Ok(())).unwrap();
        closing.await.unwrap();
        guard.cancel();
        assert!(guard.status().is_cancelled());
        assert_eq!(cancellations.load(Ordering::SeqCst), 1);
        store.shutdown().await.unwrap();
    });
    let storage = backend.state.lock().unwrap();
    assert!(storage.loads.is_empty() && storage.saves.is_empty());
    assert_eq!(
        (storage.revisioned_load_calls, storage.revisioned_save_calls),
        (1, 0)
    );
}

// A retained route cannot keep an expired persistence session alive or reacquire I/O.
#[test]
fn retained_route_reports_closed_after_the_final_document_session_is_dropped() {
    use rca_core::{SharedKey, SharedSubscribeContext};

    let runtime = DeterministicRuntime::with_step_budget(64);
    let backend = Memory::new();
    let (state, reducer) = SharedDocument::revisioned(7, backend.clone(), Binary);
    let session = reducer.session();
    let store = Store::new(state, reducer, runtime.runtime());
    let route = store.scope(
        StatePath::new(|state: &SharedDocumentState<u32, u64>| state, |state| state),
        ActionPath::new(
            |action: &SharedDocumentAction<u32, u64>| Some(action),
            |action| action,
        ),
    );
    let owner = SharedDocumentOwner::new(route.clone(), |value: &u32| *value, session);
    let key = owner.key(SharedKeyIdentity::new::<Binary>(
        "expired-session",
        "binary",
    ));
    let (reader, subscriber) = cached_reference_and_subscriber();

    run_document_session_contract(&runtime, async {
        owner.close().await.unwrap();
        store.shutdown().await.unwrap();
        drop(owner);
        runtime.run_until_stalled().unwrap();
        assert!(route.downgrade().upgrade().is_some());

        let mut saving = Box::pin(key.save(&99));
        let Poll::Ready(Err(save_error)) = saving
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
        else {
            panic!("save must reject the expired session before actor admission");
        };
        drop(saving);
        let load_error = key
            .load(SharedLoadContext::UserInitiated)
            .await
            .unwrap_err();
        let subscribe_error = key
            .subscribe(SharedSubscribeContext::UserInitiated, subscriber)
            .unwrap_err();
        for error in [save_error, load_error, subscribe_error] {
            assert!(error.is_session());
            assert_eq!(
                error.to_string().lines().next(),
                Some("document key Closed failed")
            );
            assert!(!error.is_ended());
            assert!(error.error().is_none());
            assert!(error.source().is_none());
        }
        assert_eq!(reader.read(|value| *value).unwrap(), 3);
    });
    let storage = backend.state.lock().unwrap();
    assert!(storage.closed);
    assert!(storage.loads.is_empty() && storage.saves.is_empty());
    assert_eq!(
        (storage.revisioned_load_calls, storage.revisioned_save_calls),
        (0, 0)
    );
    assert!(storage.bytes.is_none());
}

// Reusing one key must retain live peers without reviving its retired subscriber.
#[test]
fn reused_document_key_delivers_to_live_subscribers_after_retiring_a_registration() {
    use rca_core::SharedSubscribeContext;

    let runtime = DeterministicRuntime::with_step_budget(64);
    let backend = Memory::new();
    let (state, reducer) = SharedDocument::revisioned(7, backend.clone(), Binary);
    let session = reducer.session();
    let store = Store::new(state, reducer, runtime.runtime());
    let owner = document_owner(&store, session);
    let key = owner.key(SharedKeyIdentity::new::<Binary>("reused-key", "binary"));
    let (retired, retired_subscriber) = cached_reference_and_subscriber();
    let (first, first_subscriber) = cached_reference_and_subscriber();
    let (second, second_subscriber) = cached_reference_and_subscriber();

    run_document_session_contract(&runtime, async {
        let _ = store.send(SharedDocumentAction::reload()).await.unwrap();
        runtime.run_until_stalled().unwrap();
        backend.complete(0, Ok(present(1, 7)));
        runtime.run_until_stalled().unwrap();
        let retired_guard = key
            .subscribe(SharedSubscribeContext::UserInitiated, retired_subscriber)
            .unwrap();
        runtime.run_until_stalled().unwrap();
        assert!(retired_guard.status().is_active());
        assert_eq!(retired.read(|value| *value).unwrap(), 7);
        drop(retired_guard);

        let first_guard = key
            .subscribe(SharedSubscribeContext::UserInitiated, first_subscriber)
            .unwrap();
        let second_guard = key
            .clone()
            .subscribe(SharedSubscribeContext::UserInitiated, second_subscriber)
            .unwrap();
        runtime.run_until_stalled().unwrap();
        assert!(first_guard.status().is_active());
        assert!(second_guard.status().is_active());
        assert_eq!(owner.subscription_statuses(&key).len(), 2);
        assert_eq!(first.read(|value| *value).unwrap(), 7);
        assert_eq!(second.read(|value| *value).unwrap(), 7);

        let _ = store.send(SharedDocumentAction::reload()).await.unwrap();
        runtime.run_until_stalled().unwrap();
        backend.complete(0, Ok(present(2, 9)));
        runtime.run_until_stalled().unwrap();
        assert_eq!(first.read(|value| *value).unwrap(), 9);
        assert_eq!(second.read(|value| *value).unwrap(), 9);
        assert_eq!(retired.read(|value| *value).unwrap(), 7);
        assert!(first_guard.status().is_active());
        assert!(second_guard.status().is_active());
        owner.close().await.unwrap();
        assert!(first_guard.status().is_cancelled());
        assert!(second_guard.status().is_cancelled());
        store.shutdown().await.unwrap();
    });
    let storage = backend.state.lock().unwrap();
    assert!(storage.closed);
    assert!(storage.loads.is_empty() && storage.saves.is_empty());
    assert_eq!(
        (storage.revisioned_load_calls, storage.revisioned_save_calls),
        (2, 0)
    );
}

/// Polls a different close waiter during the first wake, then only counts.
struct RepollOnFirstWake {
    future: Mutex<Pin<Box<dyn Future<Output = ()> + Send>>>,
    notifications: std::sync::atomic::AtomicUsize,
}

impl std::task::Wake for RepollOnFirstWake {
    fn wake(self: Arc<Self>) {
        if self
            .notifications
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            == 0
        {
            let waker = Waker::from(Arc::clone(&self));
            assert!(
                self.future
                    .lock()
                    .unwrap()
                    .as_mut()
                    .poll(&mut Context::from_waker(&waker))
                    .is_pending()
            );
        }
    }
}

#[test]
fn close_forwards_a_wake_consumed_while_another_waiter_polls() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Wakes during its second close poll and completes on its third poll.
    struct WakeDuringClose {
        memory: Memory,
        polls: Arc<AtomicUsize>,
    }

    impl Backend for WakeDuringClose {
        type Error = StorageError;

        async fn load(&self) -> Result<Option<Vec<u8>>, Self::Error> {
            self.memory.load().await
        }

        async fn save(&self, bytes: Vec<u8>) -> Result<(), Self::Error> {
            self.memory.save(bytes).await
        }
    }

    impl BackendSession for WakeDuringClose {
        async fn close(&self) -> Result<(), Self::Error> {
            std::future::poll_fn(|context| match self.polls.fetch_add(1, Ordering::SeqCst) {
                0 => Poll::Pending,
                1 => {
                    context.waker().wake_by_ref();
                    Poll::Pending
                }
                _ => Poll::Ready(Ok(())),
            })
            .await
        }
    }

    let polls = Arc::new(AtomicUsize::new(0));
    let (_, document) = SharedDocument::new(
        7,
        WakeDuringClose {
            memory: Memory::new(),
            polls: Arc::clone(&polls),
        },
        Binary,
    );
    let session = document.session();
    let second_session = session.clone();
    let second = Arc::new(RepollOnFirstWake {
        future: Mutex::new(Box::pin(async move {
            second_session.close().await.unwrap();
        })),
        notifications: AtomicUsize::new(0),
    });
    let waker = Waker::from(Arc::clone(&second));
    let mut context = Context::from_waker(&waker);
    assert!(
        second
            .future
            .lock()
            .unwrap()
            .as_mut()
            .poll(&mut context)
            .is_pending()
    );
    assert_eq!(second.notifications.load(Ordering::SeqCst), 0);

    let mut first = Box::pin(session.close());
    assert!(
        first
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_pending()
    );
    drop(first);
    let notifications = second.notifications.load(Ordering::SeqCst);
    assert_eq!(polls.load(Ordering::SeqCst), 2);

    // Save the notification count before this final poll. Poll even on a broken
    // implementation so the test releases the stored waiter before it fails.
    assert!(
        second
            .future
            .lock()
            .unwrap()
            .as_mut()
            .poll(&mut context)
            .is_ready()
    );
    assert_eq!(polls.load(Ordering::SeqCst), 3);
    assert_eq!(notifications, 2, "the remaining waiter needs a fresh wake");
}

#[test]
fn close_forwards_an_admission_wake_consumed_during_another_poll() {
    admission_wake_during_close(false);
}

#[test]
fn close_releases_an_abandoned_admission_during_another_poll() {
    admission_wake_during_close(true);
}

#[expect(
    clippy::too_many_lines,
    reason = "keeps admission, reentrant wake delivery and both close waiters in one schedule"
)]
fn admission_wake_during_close(abandon: bool) {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    // Only a deadlock guard: channel acknowledgements determine the schedule.
    const DEADLOCK_GUARD: Duration = Duration::from_secs(10);
    let runtime = DeterministicRuntime::with_step_budget(64);
    let backend = Memory::new();
    let (state, reducer) = SharedDocument::revisioned(7, backend.clone(), Binary);
    let session = reducer.session();
    let store = Store::new(state, reducer, runtime.runtime());
    let owner = document_owner(&store, session.clone());
    let (started, started_rx) = std::sync::mpsc::channel();
    let (resume, resume_rx) = std::sync::mpsc::channel();
    let (finished, finished_rx) = std::sync::mpsc::channel();
    let polls = Arc::new(AtomicUsize::new(0));
    let polling = Arc::clone(&polls);
    let first_guard = owner
        .observe_changes::<StorageError, _, _, _>(|_| {
            (
                SharedUpdate::Present(()),
                || {},
                std::future::poll_fn(move |_| {
                    if polling.fetch_add(1, Ordering::SeqCst) == 0 {
                        return Poll::Pending;
                    }
                    resume.send(()).unwrap();
                    finished_rx.recv_timeout(DEADLOCK_GUARD).unwrap();
                    Poll::Ready(Ok(()))
                }),
            )
        })
        .unwrap();
    let physical = Arc::new(());
    let physical_released = Arc::downgrade(&physical);
    let registration = std::thread::spawn(move || {
        // Catch only the deliberately abandoned start callback. Its reservation
        // must unwind before acknowledgement. No failed producer is restarted.
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            owner.observe_changes::<StorageError, _, _, _>(|_| {
                started.send(()).unwrap();
                resume_rx.recv_timeout(DEADLOCK_GUARD).unwrap();
                assert!(!abandon, "deliberately abandoned start callback");
                (SharedUpdate::Present(()), || {}, async move {
                    drop(physical);
                    Ok(())
                })
            })
        }));
        drop(owner);
        finished.send(()).unwrap();
        result
    });
    started_rx.recv_timeout(DEADLOCK_GUARD).unwrap();

    let second_session = session.clone();
    let second = Arc::new(RepollOnFirstWake {
        future: Mutex::new(Box::pin(async move {
            second_session.close().await.unwrap();
        })),
        notifications: AtomicUsize::new(0),
    });
    let waker = Waker::from(Arc::clone(&second));
    let mut context = Context::from_waker(&waker);
    assert!(
        second
            .future
            .lock()
            .unwrap()
            .as_mut()
            .poll(&mut context)
            .is_pending()
    );
    assert_eq!(second.notifications.load(Ordering::SeqCst), 0);
    assert_eq!(polls.load(Ordering::SeqCst), 1);

    let mut first = Box::pin(session.close());
    let first_poll = first.as_mut().poll(&mut Context::from_waker(Waker::noop()));
    if abandon {
        assert!(matches!(first_poll, Poll::Ready(Ok(()))));
    } else {
        assert!(first_poll.is_pending());
    }
    drop(first);
    let registration = registration.join().unwrap();
    let guard = if abandon {
        let Err(failure) = registration else {
            panic!("the deliberately abandoned start callback must panic");
        };
        assert_eq!(
            failure.downcast_ref::<&str>(),
            Some(&"deliberately abandoned start callback")
        );
        None
    } else {
        Some(registration.unwrap().unwrap())
    };
    assert_eq!(physical_released.upgrade().is_none(), abandon);
    assert_eq!(polls.load(Ordering::SeqCst), 2);
    let notifications = second.notifications.load(Ordering::SeqCst);

    // As in the backend-wake test, finish cleanup before asserting the count
    // captured before this poll. No producer or backend event occurs in between.
    assert!(
        second
            .future
            .lock()
            .unwrap()
            .as_mut()
            .poll(&mut context)
            .is_ready()
    );
    assert!(physical_released.upgrade().is_none());
    assert_eq!(polls.load(Ordering::SeqCst), 2);
    block_on(runtime.run(store.shutdown())).unwrap().unwrap();
    drop((first_guard, guard));
    assert_eq!(notifications, 2, "the remaining waiter needs a fresh wake");
}

// Catches publishing quiescence when a start callback transfers its actual join
// while close is polling the previous batch outside the session control lock.
#[test]
fn close_joins_a_producer_attached_while_existing_debts_are_polled() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    // Only a deadlock guard: channel acknowledgements determine the schedule.
    const DEADLOCK_GUARD: Duration = Duration::from_secs(10);
    let runtime = DeterministicRuntime::with_step_budget(64);
    let backend = Memory::new();
    let (state, reducer) = SharedDocument::revisioned(7, backend.clone(), Binary);
    let session = reducer.session();
    let store = Store::new(state, reducer, runtime.runtime());
    let owner = document_owner(&store, session.clone());
    let cancellations = Arc::new([AtomicUsize::new(0), AtomicUsize::new(0)]);
    let physical_owners = [Arc::new(()), Arc::new(())];
    let physical = physical_owners.each_ref().map(Arc::downgrade);
    let [first_physical, late_physical] = physical_owners;
    let (started, started_rx) = std::sync::mpsc::channel();
    let (resume, resume_rx) = std::sync::mpsc::channel();
    let (attached, attached_rx) = std::sync::mpsc::channel();
    let (completion, join) = controlled_future();
    let cancelling = Arc::clone(&cancellations);
    let first = owner
        .observe_changes::<StorageError, _, _, _>(|_| {
            (
                SharedUpdate::Present(()),
                move || {
                    cancelling[0].fetch_add(1, Ordering::SeqCst);
                },
                async move {
                    resume.send(()).unwrap();
                    attached_rx.recv_timeout(DEADLOCK_GUARD).unwrap();
                    drop(first_physical);
                    Ok(())
                },
            )
        })
        .unwrap();

    std::thread::scope(|threads| {
        let cancelling = Arc::clone(&cancellations);
        let registration = threads.spawn(move || {
            let outcome = owner.observe_changes::<StorageError, _, _, _>(|_| {
                started.send(()).unwrap();
                resume_rx.recv_timeout(DEADLOCK_GUARD).unwrap();
                (
                    SharedUpdate::Present(()),
                    move || {
                        cancelling[1].fetch_add(1, Ordering::SeqCst);
                    },
                    async move {
                        let outcome = join.await.map_err(|_cause| StorageError(81))?;
                        drop(late_physical);
                        outcome
                    },
                )
            });
            attached.send(()).unwrap();
            outcome
        });
        started_rx.recv_timeout(DEADLOCK_GUARD).unwrap();
        run_document_session_contract(&runtime, async {
            store.shutdown().await.unwrap();
        });

        let mut closing = Box::pin(session.close());
        let mut context = Context::from_waker(Waker::noop());
        let first_poll = closing.as_mut().poll(&mut context);
        assert!(registration.join().unwrap().unwrap_err().is_ended());
        assert!(first_poll.is_pending());
        assert!(first.status().is_cancelled());
        assert_eq!(
            cancellations
                .each_ref()
                .map(|count| count.load(Ordering::SeqCst)),
            [1, 1]
        );
        assert!(physical[0].upgrade().is_none());
        assert!(physical[1].upgrade().is_some());

        completion.complete(Err(StorageError(82))).unwrap();
        let Poll::Ready(Err(error)) = closing.as_mut().poll(&mut context) else {
            panic!("close must retain the newly attached producer's failure");
        };
        assert_eq!(error.failures().len(), 1);
        let cause = error.failures()[0].error();
        assert_eq!(cause.downcast_ref::<StorageError>().unwrap().0, 82);
        assert!(physical[1].upgrade().is_none());
    });
    drop(first);
    assert_eq!(
        cancellations
            .each_ref()
            .map(|count| count.load(Ordering::SeqCst)),
        [1, 1]
    );
    let storage = backend.state.lock().unwrap();
    assert!(storage.closed && storage.loads.is_empty() && storage.saves.is_empty());
    assert_eq!(
        (storage.revisioned_load_calls, storage.revisioned_save_calls),
        (0, 0)
    );
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "keeps observation cancellation, stale read completion and both physical joins in one schedule"
)]
fn cancelling_an_observation_load_reports_cancelled_and_retains_physical_joins() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let runtime = DeterministicRuntime::with_step_budget(64);
    let backend = Memory::new();
    let (state, reducer) = SharedDocument::revisioned(7, backend.clone(), Binary);
    let session = reducer.session();
    let store = Store::new(state, reducer, runtime.runtime());
    let owner = document_owner(&store, session);
    let (join_completion, join) = controlled_future();
    let cancellations = Arc::new(AtomicUsize::new(0));
    let cancelling = Arc::clone(&cancellations);
    let physical = Arc::new(());
    let physical_released = Arc::downgrade(&physical);
    let mut endpoint = None;
    let guard = owner
        .observe_changes::<StorageError, _, _, _>(|producer| {
            endpoint = Some(producer);
            (
                SharedUpdate::Present(()),
                move || {
                    cancelling.fetch_add(1, Ordering::SeqCst);
                },
                async move {
                    let result = join.await.map_err(|_cancelled| StorageError(81))?;
                    drop(physical);
                    result
                },
            )
        })
        .unwrap();
    let endpoint = endpoint.unwrap();

    run_document_session_contract(&runtime, async {
        runtime.run_until_stalled().unwrap();
        assert!(guard.status().is_active());
        assert!(store.read(SharedDocumentState::is_loading).await.unwrap());
        assert_eq!(backend.state.lock().unwrap().loads.len(), 1);
        assert_eq!(cancellations.load(Ordering::SeqCst), 0);

        // Cancel the observation's pending document load, not its registration.
        // Its Cancelled reply must travel through the producer terminal action.
        store
            .send(SharedDocumentAction::cancel_load())
            .await
            .unwrap()
            .finish()
            .await;
        runtime.run_until_stalled().unwrap();
        let status = guard.status();
        assert!(status.is_cancelled());
        assert!(!status.is_finished());
        assert!(status.error().is_none());
        assert_eq!(cancellations.load(Ordering::SeqCst), 1);
        assert!(physical_released.upgrade().is_some());
        assert!(endpoint.publish(SharedUpdate::Present(())).is_err());
        assert!(!endpoint.finish());
        assert_eq!(
            store
                .read(|state| (
                    *state.value(),
                    state.revision(),
                    state.acknowledged_revision(),
                    state.is_loading()
                ))
                .await
                .unwrap(),
            (7, 0, None, false)
        );

        let mut closing = Box::pin(owner.close());
        assert!(
            closing
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending()
        );
        runtime.run_until_stalled().unwrap();
        assert!(
            closing
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending()
        );
        assert!(backend.state.lock().unwrap().closed);
        // Logical cancellation neither rolls back the old read nor joins it.
        backend.complete(0, Ok(present(2, 99)));
        assert!(
            closing
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending()
        );
        assert!(physical_released.upgrade().is_some());
        join_completion.complete(Ok(())).unwrap();
        closing.await.unwrap();
        assert!(physical_released.upgrade().is_none());
        runtime.run_until_stalled().unwrap();
        assert_eq!(store.read(|state| *state.value()).await.unwrap(), 7);
        assert!(guard.status().is_cancelled());
        assert_eq!(cancellations.load(Ordering::SeqCst), 1);
        store.shutdown().await.unwrap();
    });
    let storage = backend.state.lock().unwrap();
    assert!(storage.closed && storage.loads.is_empty() && storage.saves.is_empty());
    assert_eq!(
        (storage.revisioned_load_calls, storage.revisioned_save_calls),
        (1, 0)
    );
}
