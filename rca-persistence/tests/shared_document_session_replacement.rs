//! Checks that admitted work cannot cross a replaced document's session boundary.
//!
//! The route remains valid while its state changes identity. Controlled completion
//! separates stale action rejection from the original provider's physical join.

use std::io;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

use futures_executor::block_on;
use rca_core::{ActionPath, Effect, Reduce, Reducer, StatePath, Store};
use rca_persistence::{
    Backend, BackendSession, BackendSnapshot, Codec, ReceiptError, SharedDocument,
    SharedDocumentAction, SharedDocumentOwner, SharedDocumentState, SharedUpdate,
};
use rca_test::{ControlledFuture, DeterministicRuntime, controlled_future};

#[derive(Default)]
/// Counts actual backend polls independently of document state and receipts.
struct Calls {
    /// Counts load polls, including forbidden loads caused by snapshot delivery.
    loads: AtomicUsize,
    /// Counts save polls, including forbidden saves caused by snapshot delivery.
    saves: AtomicUsize,
    /// Counts completed close polls for each independent backend session.
    closes: AtomicUsize,
}

/// Rejects unexpected I/O while recording the original or replacement session's close.
struct Storage(
    /// Retains counters that the test reads after document ownership ends.
    Arc<Calls>,
);

impl Backend for Storage {
    type Error = io::Error;

    fn load(&self) -> impl Future<Output = Result<Option<Vec<u8>>, Self::Error>> + Send {
        std::future::poll_fn(move |_| {
            self.0.loads.fetch_add(1, Ordering::SeqCst);
            Poll::Ready(Err(io::Error::other(
                "snapshots must not start a backend load",
            )))
        })
    }

    fn save(&self, _: Vec<u8>) -> impl Future<Output = Result<(), Self::Error>> + Send {
        std::future::poll_fn(move |_| {
            self.0.saves.fetch_add(1, Ordering::SeqCst);
            Poll::Ready(Err(io::Error::other(
                "observations must not start a backend save",
            )))
        })
    }
}

impl BackendSession for Storage {
    fn close(&self) -> impl Future<Output = Result<(), Self::Error>> + Send {
        std::future::poll_fn(move |_| {
            self.0.closes.fetch_add(1, Ordering::SeqCst);
            Poll::Ready(Ok(()))
        })
    }
}

/// Converts fixture values to and from four little-endian bytes.
struct Binary;

impl Codec<u32> for Binary {
    type Error = io::Error;

    fn encode(&self, value: &u32) -> Result<Vec<u8>, Self::Error> {
        Ok(value.to_le_bytes().to_vec())
    }

    fn decode(&self, bytes: &[u8]) -> Result<u32, Self::Error> {
        bytes
            .try_into()
            .map(u32::from_le_bytes)
            .map_err(io::Error::other)
    }
}

/// Routes genuine document actions or replaces only the visible document state.
enum Action {
    /// Preserves the document action's original session identity.
    Document(SharedDocumentAction<u32>),
    /// Leaves the route and reducer live with a different session's state.
    Replace,
}

#[derive(Clone, Copy)]
/// Selects the already-admitted producer event sent after state replacement.
enum Delivery {
    /// Offers a new snapshot from the original producer.
    Update,
    /// Ends the original producer's delivery without completing its physical join.
    Finish,
}

/// Rejects an old producer's snapshot after the route receives replacement state.
#[test]
fn admitted_producer_update_rejects_a_replacement_document_session() {
    replacement_rejects_old_delivery(Delivery::Update);
}

/// Rejects old producer completion without aborting the replacement document.
#[test]
fn admitted_producer_completion_rejects_a_replacement_document_session() {
    replacement_rejects_old_delivery(Delivery::Finish);
}

/// Separates session rejection, old-owner close and explicit provider completion.
#[expect(
    clippy::too_many_lines,
    reason = "keeps admission, replacement, rejection and physical-join ownership in one schedule"
)]
fn replacement_rejects_old_delivery(delivery: Delivery) {
    let runtime = DeterministicRuntime::with_step_budget(128);
    let original_calls = Arc::new(Calls::default());
    let replacement_calls = Arc::new(Calls::default());
    let (state, mut document) =
        SharedDocument::new(7, Storage(Arc::clone(&original_calls)), Binary);
    let (replacement, replacement_document) =
        SharedDocument::new(23, Storage(Arc::clone(&replacement_calls)), Binary);
    let original_session = document.session();
    let replacement_session = replacement_document.session();
    let mut replacement = Some(replacement);
    let reducer = Reduce::new(
        move |state: &mut SharedDocumentState<u32>, action: &Action, context| match action {
            Action::Document(action) => document
                .reduce(state, action, context)
                .map(Action::Document),
            Action::Replace => {
                // Ordinary required routes remain live. The document's own exact
                // session check must reject this already admitted producer.
                *state = replacement.take().expect("one replacement");
                Effect::none()
            }
        },
    );
    let store = Store::new(state, reducer, runtime.runtime());
    let captures = Arc::new(AtomicUsize::new(0));
    let capture_count = Arc::clone(&captures);
    let retained = Arc::new(());
    let released = Arc::downgrade(&retained);
    let owner = SharedDocumentOwner::new(
        store.scope(
            StatePath::new(|state: &SharedDocumentState<u32>| state, |state| state),
            ActionPath::new(
                |action: &Action| match action {
                    Action::Document(action) => Some(action),
                    Action::Replace => None,
                },
                Action::Document,
            ),
        ),
        move |value: &u32| {
            let _ = &retained;
            capture_count.fetch_add(1, Ordering::SeqCst);
            *value
        },
        original_session.clone(),
    );
    let cancellations = Arc::new(AtomicUsize::new(0));
    let cancellation_count = Arc::clone(&cancellations);
    let (joined, join) = controlled_future::<Result<(), io::Error>>();
    let mut endpoint = None;
    let guard = owner
        .observe_snapshots::<io::Error, _, _, _>(|producer| {
            endpoint = Some(producer);
            (
                SharedUpdate::Present(BackendSnapshot {
                    revision: None,
                    bytes: Some(8_u32.to_le_bytes().to_vec()),
                }),
                move || {
                    cancellation_count.fetch_add(1, Ordering::SeqCst);
                },
                async move { join.await.map_err(io::Error::other)? },
            )
        })
        .unwrap();
    let endpoint = endpoint.unwrap();

    block_on(runtime.run(async {
        runtime.run_until_stalled().unwrap();
        assert!(guard.status().is_active());
        assert_eq!(store.read(|state| *state.value()).await.unwrap(), 8);
        assert_eq!(captures.load(Ordering::SeqCst), 1);
        assert_eq!(cancellations.load(Ordering::SeqCst), 0);

        let _ = store.send(Action::Replace).await.unwrap();
        match delivery {
            Delivery::Update => endpoint
                .publish(SharedUpdate::Present(BackendSnapshot {
                    revision: None,
                    bytes: Some(99_u32.to_le_bytes().to_vec()),
                }))
                .unwrap(),
            Delivery::Finish => assert!(endpoint.finish()),
        }
        runtime.run_until_stalled().unwrap();
        let status = guard.status();
        assert!(status.error().unwrap().is_session());
        assert!(!status.is_cancelled());
        assert_eq!(captures.load(Ordering::SeqCst), 1);
        assert_eq!(cancellations.load(Ordering::SeqCst), 1);
        assert_eq!(
            store
                .read(|state| (*state.value(), state.revision(), state.is_loading()))
                .await
                .unwrap(),
            (23, 0, false)
        );
        assert!(endpoint.publish(SharedUpdate::Absent).is_err());

        // Closing the old owner must fence only its original document, even
        // though this required route now reaches a replacement document.
        // Logical rejection must still retain the provider's physical join.
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
        assert_eq!(original_calls.closes.load(Ordering::SeqCst), 1);
        assert_eq!(
            store
                .read(|state| {
                    (
                        *state.value(),
                        state.revision(),
                        state.document().is_aborted(),
                    )
                })
                .await
                .unwrap(),
            (23, 0, false)
        );
        joined.complete(Ok(())).unwrap();
        closing.await.unwrap();
        assert_eq!(
            store
                .read(|state| {
                    (
                        *state.value(),
                        state.revision(),
                        state.document().is_aborted(),
                    )
                })
                .await
                .unwrap(),
            (23, 0, false)
        );
        replacement_session.close().await.unwrap();
        store.shutdown().await.unwrap();
    }))
    .unwrap();

    for calls in [&original_calls, &replacement_calls] {
        assert_eq!(calls.loads.load(Ordering::SeqCst), 0);
        assert_eq!(calls.saves.load(Ordering::SeqCst), 0);
        assert_eq!(calls.closes.load(Ordering::SeqCst), 1);
    }
    drop((guard, endpoint, owner, store, replacement_document));
    assert!(released.upgrade().is_none());
    assert_eq!(cancellations.load(Ordering::SeqCst), 1);
}

/// Holds one save until its independent completion control is released.
///
/// This fixture closes only after the save finishes. Its close method records that
/// final call rather than modelling an independent provider-join implementation.
struct PendingSave {
    /// Counts polled work independently of its effects on the actor.
    calls: Arc<Calls>,
    /// Transfers the single save gate into the pending operation.
    release: Mutex<Option<ControlledFuture<()>>>,
    /// Records persisted bytes even if the resulting actor action is rejected.
    committed: Arc<Mutex<Option<Vec<u8>>>>,
}

impl Backend for PendingSave {
    type Error = io::Error;

    fn load(&self) -> impl Future<Output = Result<Option<Vec<u8>>, Self::Error>> + Send {
        std::future::poll_fn(move |_| {
            self.calls.loads.fetch_add(1, Ordering::SeqCst);
            Poll::Ready(Ok(Some(7_u32.to_le_bytes().to_vec())))
        })
    }

    async fn save(&self, bytes: Vec<u8>) -> Result<(), Self::Error> {
        self.calls.saves.fetch_add(1, Ordering::SeqCst);
        let release = self.release.lock().unwrap().take().expect("one save");
        release.await.map_err(io::Error::other)?;
        *self.committed.lock().unwrap() = Some(bytes);
        Ok(())
    }
}

impl BackendSession for PendingSave {
    fn close(&self) -> impl Future<Output = Result<(), Self::Error>> + Send {
        std::future::poll_fn(move |_| {
            self.calls.closes.fetch_add(1, Ordering::SeqCst);
            Poll::Ready(Ok(()))
        })
    }
}

/// Preserves backend commit without applying its stale acknowledgement to replacement state.
#[test]
fn old_save_completion_does_not_acknowledge_a_replacement_document() {
    let runtime = DeterministicRuntime::with_step_budget(128);
    let original_calls = Arc::new(Calls::default());
    let replacement_calls = Arc::new(Calls::default());
    let (release, pending) = controlled_future();
    let committed = Arc::new(Mutex::new(None));
    let (state, mut document) = SharedDocument::new(
        7,
        PendingSave {
            calls: Arc::clone(&original_calls),
            release: Mutex::new(Some(pending)),
            committed: Arc::clone(&committed),
        },
        Binary,
    );
    let (replacement, replacement_document) =
        SharedDocument::new(23, Storage(Arc::clone(&replacement_calls)), Binary);
    let original_session = document.session();
    let replacement_session = replacement_document.session();
    let mut replacement = Some(replacement);
    let reducer = Reduce::new(
        move |state: &mut SharedDocumentState<u32>, action: &Action, context| match action {
            Action::Document(action) => document
                .reduce(state, action, context)
                .map(Action::Document),
            Action::Replace => {
                *state = replacement.take().expect("one replacement");
                Effect::none()
            }
        },
    );
    let store = Store::new(state, reducer, runtime.runtime());

    block_on(runtime.run(async {
        store
            .send(Action::Document(SharedDocumentAction::reload()))
            .await
            .unwrap()
            .finish()
            .await;
        assert_eq!(
            store
                .read(SharedDocumentState::acknowledged_revision)
                .await
                .unwrap(),
            Some(0)
        );
        let (edit, receipt) = SharedDocumentAction::set_and_flush(9);
        let saving = store.send(Action::Document(edit)).await.unwrap();
        runtime.run_until_stalled().unwrap();
        assert_eq!(original_calls.saves.load(Ordering::SeqCst), 1);
        assert!(committed.lock().unwrap().is_none());

        let _ = store.send(Action::Replace).await.unwrap();
        // Root replacement does not cancel the original effect. Let its actual
        // backend write finish, then wait for the resulting action's reduction.
        release.complete(()).unwrap();
        saving.finish().await;
        assert_eq!(
            *committed.lock().unwrap(),
            Some(9_u32.to_le_bytes().to_vec())
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
            (23, 0, None)
        );
        assert!(matches!(receipt.await, Err(ReceiptError::Abandoned)));

        original_session.close().await.unwrap();
        replacement_session.close().await.unwrap();
        store.shutdown().await.unwrap();
    }))
    .unwrap();
    assert_eq!(original_calls.loads.load(Ordering::SeqCst), 1);
    assert_eq!(original_calls.saves.load(Ordering::SeqCst), 1);
    assert_eq!(original_calls.closes.load(Ordering::SeqCst), 1);
    assert_eq!(replacement_calls.loads.load(Ordering::SeqCst), 0);
    assert_eq!(replacement_calls.saves.load(Ordering::SeqCst), 0);
    assert_eq!(replacement_calls.closes.load(Ordering::SeqCst), 1);
    drop((store, replacement_document));
}
