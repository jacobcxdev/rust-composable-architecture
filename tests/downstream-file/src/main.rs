//! Checks packaged file persistence, backend retirement and restart from saved bytes.

use std::convert::Infallible;
use std::error::Error;

use architecture::{Store, TokioRuntime};
use documents::{Action, Codec, Document, FileBackend, FileOptions, SyncMode};

/// Copies owned bytes across the persistence codec boundary.
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

/// Saves, closes and reloads a document inside a temporary directory.
///
/// # Errors
/// Returns a runtime, store, backend or filesystem error.
fn main() -> Result<(), Box<dyn Error>> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("document");
    let host = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()?;
    host.block_on(async {
        let options = FileOptions {
            maximum_bytes: 8,
            sync: SyncMode::File,
        };
        let backend = FileBackend::new(&path, options, host.handle().clone())?;
        let (state, document) = Document::new(vec![7], backend.clone(), Bytes);
        let store = Store::new(state, document, TokioRuntime::current()?);
        store.send(Action::load()).await?.finish().await;
        store.send(Action::set(vec![11, 13])).await?.finish().await;

        let (close, receipt) = Action::close();
        let _ = store.send(close).await?;
        assert!(receipt.await?.closing);
        // Actor acknowledgement and backend-specific retirement are distinct.
        backend.close().await;
        assert!(store.shutdown_into_state().await?.is_closed());
        assert_eq!(std::fs::read(&path)?, vec![11, 13]);

        let restarted = FileBackend::new(&path, options, host.handle().clone())?;
        let (state, document) = Document::new(vec![0], restarted.clone(), Bytes);
        let store = Store::new(state, document, TokioRuntime::current()?);
        store.send(Action::load()).await?.finish().await;
        assert_eq!(
            store.read(|state| state.value().clone()).await?,
            vec![11, 13]
        );
        restarted.close().await;
        store.shutdown().await?;
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn registry_file_backend_closes_and_restarts_from_saved_bytes() {
        super::main().unwrap();
    }
}
