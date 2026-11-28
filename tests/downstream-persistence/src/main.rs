//! Checks packaged document receipts, shared sessions and saved-state restoration.

use std::convert::Infallible;
use std::sync::{Arc, Mutex};

use architecture::{
    ActionPath, DeterministicRuntime, SharedKey, SharedKeyCache, SharedKeyIdentity,
    SharedLoadContext, SharedReaderKey, StatePath, Store,
};
use documents::{
    Action, Backend, BackendSession, Codec, Document, FlushOutcome, ReceiptError, SharedDocument,
    SharedDocumentAction, SharedDocumentOwner, SharedDocumentState,
};

/// Shares saved bytes across independent document sessions.
#[derive(Clone, Default)]
struct Memory(Arc<Mutex<Option<Vec<u8>>>>);
impl Backend for Memory {
    type Error = Infallible;
    async fn load(&self) -> Result<Option<Vec<u8>>, Infallible> {
        Ok(self.0.lock().unwrap().clone())
    }
    async fn save(&self, bytes: Vec<u8>) -> Result<(), Infallible> {
        *self.0.lock().unwrap() = Some(bytes);
        Ok(())
    }
}

/// Copies bytes without a fallible serialisation format.
struct Bytes;
impl Codec<Vec<u8>> for Bytes {
    type Error = Infallible;
    fn encode(&self, value: &Vec<u8>) -> Result<Vec<u8>, Infallible> {
        Ok(value.clone())
    }
    fn decode(&self, bytes: &[u8]) -> Result<Vec<u8>, Infallible> {
        Ok(bytes.to_vec())
    }
}

/// Couples shared bytes with a close flag shared by session aliases.
#[derive(Clone)]
struct SessionMemory {
    /// Persists across new sessions so restart can reload earlier writes.
    bytes: Memory,
    /// Serialises admission and immediate work against session close.
    closed: Arc<Mutex<bool>>,
}
impl SessionMemory {
    /// Starts an open session over the supplied saved bytes.
    fn new(bytes: Memory) -> Self {
        Self {
            bytes,
            closed: Arc::new(Mutex::new(false)),
        }
    }
}
impl Backend for SessionMemory {
    type Error = std::io::Error;
    async fn load(&self) -> Result<Option<Vec<u8>>, Self::Error> {
        let closed = self.closed.lock().unwrap();
        if *closed {
            return Err(std::io::ErrorKind::BrokenPipe.into());
        }
        Ok(self.bytes.0.lock().unwrap().clone())
    }
    async fn save(&self, bytes: Vec<u8>) -> Result<(), Self::Error> {
        let closed = self.closed.lock().unwrap();
        if *closed {
            return Err(std::io::ErrorKind::BrokenPipe.into());
        }
        *self.bytes.0.lock().unwrap() = Some(bytes);
        Ok(())
    }
}
impl BackendSession for SessionMemory {
    async fn close(&self) -> Result<(), Self::Error> {
        // Admission and immediate physical work share this lock across aliases.
        // There is no detached work and no lock crosses an await.
        *self.closed.lock().unwrap() = true;
        Ok(())
    }
}

/// Checks shared visibility, alias retirement and a fresh session over saved bytes.
fn shared_document_roundtrip() {
    let bytes = Memory::default();
    let backend = SessionMemory::new(bytes.clone());
    let alias = backend.clone();
    let host = DeterministicRuntime::new();
    futures_executor::block_on(host.run(async {
        let (state, reducer) = SharedDocument::new(vec![7], backend, Bytes);
        let session = reducer.session();
        let store = Store::new(state, reducer, host.runtime());
        let route = store.scope(
            StatePath::new(|s: &SharedDocumentState<Vec<u8>>| s, |s| s),
            ActionPath::new(|a: &SharedDocumentAction<Vec<u8>>| Some(a), |a| a),
        );
        let owner = SharedDocumentOwner::new(route, |value: &Vec<u8>| value.clone(), session);
        let key = owner.key(SharedKeyIdentity::new::<Bytes>(
            "packaged/settings",
            "bytes-v1",
        ));
        let cache = SharedKeyCache::new();
        let shared = cache.shared(key.clone(), || vec![7]).await.unwrap();
        let reader = cache
            .reader(key.clone(), || panic!("cache hit must not reseed"))
            .await
            .unwrap();
        assert_eq!(shared.read(Clone::clone).unwrap(), vec![7]);
        assert_eq!(reader.read(Clone::clone).unwrap(), vec![7]);
        key.save(&vec![11]).await.unwrap();
        assert_eq!(store.read(|s| s.value().clone()).await.unwrap(), vec![11]);
        assert_eq!(shared.read(Clone::clone).unwrap(), vec![11]);
        assert_eq!(reader.read(Clone::clone).unwrap(), vec![11]);
        assert_eq!(*bytes.0.lock().unwrap(), Some(vec![11]));

        owner.close().await.unwrap();
        assert_eq!(
            alias.load().await.unwrap_err().kind(),
            std::io::ErrorKind::BrokenPipe
        );
        assert_eq!(
            alias.save(vec![99]).await.unwrap_err().kind(),
            std::io::ErrorKind::BrokenPipe
        );
        assert!(
            key.load(SharedLoadContext::UserInitiated)
                .await
                .unwrap_err()
                .is_session()
        );

        drop(owner);
        assert!(
            key.load(SharedLoadContext::UserInitiated)
                .await
                .unwrap_err()
                .is_ended()
        );
        store.shutdown().await.unwrap();
    }))
    .unwrap();

    let host = DeterministicRuntime::new();
    futures_executor::block_on(host.run(async {
        let (state, reducer) = SharedDocument::new(vec![0], SessionMemory::new(bytes), Bytes);
        let session = reducer.session();
        let store = Store::new(state, reducer, host.runtime());
        let route = store.scope(
            StatePath::new(|s: &SharedDocumentState<Vec<u8>>| s, |s| s),
            ActionPath::new(|a: &SharedDocumentAction<Vec<u8>>| Some(a), |a| a),
        );
        let owner = SharedDocumentOwner::new(route, |value: &Vec<u8>| value.clone(), session);
        let key = owner.key(SharedKeyIdentity::new::<Bytes>(
            "packaged/settings",
            "bytes-v1",
        ));
        let cache = SharedKeyCache::new();
        let shared = cache.shared(key, || vec![0]).await.unwrap();
        assert_eq!(shared.read(Clone::clone).unwrap(), vec![11]);
        assert_eq!(store.read(|s| s.value().clone()).await.unwrap(), vec![11]);
        owner.close().await.unwrap();
        store.shutdown().await.unwrap();
    }))
    .unwrap();
}

/// Checks shared sessions and the load-before-flush document contract.
fn main() {
    shared_document_roundtrip();
    let host = DeterministicRuntime::new();
    let backend = Memory::default();
    futures_executor::block_on(host.run(async {
        let (state, reducer) = Document::new(vec![7], backend.clone(), Bytes);
        let store = Store::new(state, reducer, host.runtime());
        let (flush, receipt) = Action::flush();
        store.send(flush).await.unwrap().finish().await;
        assert!(matches!(receipt.await, Err(ReceiptError::NotLoaded)));
        store.send(Action::load()).await.unwrap().finish().await;
        store
            .send(Action::set(vec![11]))
            .await
            .unwrap()
            .finish()
            .await;

        let (close, receipt) = Action::close();
        let _ = store.send(close).await.unwrap();
        assert_eq!(
            receipt.await.unwrap(),
            FlushOutcome {
                revision: 1,
                closing: true
            }
        );
        let state = store.shutdown_into_state().await.unwrap();
        assert!(state.is_closed());
        assert_eq!(state.value(), &vec![11]);
        assert_eq!(*backend.0.lock().unwrap(), Some(vec![11]));

        // This backend performs no detached work; the generic receipt makes no
        // stronger quiescence promise for other Backend implementations.
        let (state, reducer) = Document::new(vec![0], backend, Bytes);
        let store = Store::new(state, reducer, host.runtime());
        store.send(Action::load()).await.unwrap().finish().await;
        assert_eq!(
            store.read(|state| state.value().clone()).await.unwrap(),
            vec![11]
        );
        store.shutdown().await.unwrap();
    }))
    .unwrap();
}

#[cfg(test)]
mod tests {
    #[test]
    fn registry_persistence_requires_load_and_closes_before_restart() {
        super::main();
    }
}
