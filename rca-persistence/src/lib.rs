#![deny(rustdoc::broken_intra_doc_links)]
#![forbid(unsafe_code)]

//! Coordinates document loads, saves and persistence acknowledgements.
//!
//! Default features select no runtime, filesystem or serialisation format.
//! Each integration uses an explicit [`Codec`] and backend.
//!
//! # Choose an integration
//!
//! - [`Document`] owns a value and composes as a reducer with a [`Backend`].
//! - [`SharedDocument`] coordinates a cached value through a [`DocumentKey`] and an owned backend session.
//! - [`BackendKey`] connects a byte backend directly to [`rca_core::SharedKeyCache`], without document coordination.
//! - [`RevisionBackend`] adds revision checks to saves and reports conflicts instead of overwriting newer data.
//!
//! # Document execution
//!
//! Document effects encode values synchronously before awaiting the backend.
//! RCA does not move this codec work to another thread. A document permits one
//! save at a time and retains the latest value instead of queueing every edit.
//! After a failure, automatic saves stop until an explicit flush or close.
//! RCA does not retry automatically.
//!
//! `BackendKey` has a different boundary: its save method encodes before returning
//! the future. That future owns the encoded bytes, not a borrowed value.
//!
//! # Optional file backend
//!
//! The `file` feature adds `FileBackend` with an explicit Tokio runtime handle.
//! One writer owns each target in a trusted, existing directory. Replacement
//! does not preserve permissions, ACLs, ownership or hard-link identity.
//! Symlink and regular-file checks do not protect against concurrent directory changes.
//! The adapter provides no cross-process lock or network-filesystem guarantee.
//! Sync requests do not guarantee survival of power loss or hardware-cache honesty.
//! They do not request macOS `F_FULLFSYNC`.
//!
//! Normal failures remove owned temporary files. Process death can leave temporary
//! files, which later loads neither adopt nor remove. Dropping an operation future
//! does not stop a running blocking job. Backend close waits for earlier jobs to
//! stop, but does not replace a successful document flush.
//!
//! # Lifetime and cancellation
//!
//! Dropping an RCA action task does not cancel persistence. Explicit task cancellation
//! can discard a persistence completion without notifying the reducer.
//! After explicit cancellation, submit [`Action::abort`]. Do not recover with an arbitrary cancel-then-flush sequence.
//! Abort, structural removal and Store shutdown do not undo external I/O or guarantee that an external writer stopped.
//!
//! Before restart, wait for the backend to stop all earlier writes.
//! Then construct a new [`Document`] and [`State`], and load again.
//! A replacement document must not race an earlier writer.
//! If saved state is required, use [`Action::close`] before Store shutdown.
//! Shutdown aborts work instead of flushing it.
//! RCA provides no in-place reset, hidden executor, detached task or cross-document transaction.
//!
//! A receipt succeeds only after the Store publishes the acknowledgement and any requested closed state.
//! Its guarantee is limited to the backend's successful-save contract.
//! Dropping a receipt never cancels persistence. The original action retains a reply sender.
//! If acknowledgement is cancelled, the receipt reports abandonment only after that action and all coordinator and effect senders are dropped.
//!
//! # Examples
//!
//! ```
//! use std::convert::Infallible;
//! use std::sync::{Arc, Mutex};
//!
//! use rca_core::Store;
//! use rca_persistence::{Action, Backend, Codec, Document, LoadStatus};
//! # let host = rca_test::DeterministicRuntime::new();
//! # let runtime = host.runtime();
//!
//! // This backend retains bytes in memory, without filesystem I/O.
//! struct Memory(Arc<Mutex<Vec<u8>>>);
//!
//! impl Backend for Memory {
//!     type Error = Infallible;
//!
//!     async fn load(&self) -> Result<Option<Vec<u8>>, Infallible> {
//!         Ok(Some(self.0.lock().unwrap().clone()))
//!     }
//!
//!     async fn save(&self, bytes: Vec<u8>) -> Result<(), Infallible> {
//!         *self.0.lock().unwrap() = bytes;
//!         Ok(())
//!     }
//! }
//!
//! struct Bytes;
//!
//! impl Codec<Vec<u8>> for Bytes {
//!     type Error = Infallible;
//!
//!     fn encode(&self, value: &Vec<u8>) -> Result<Vec<u8>, Infallible> {
//!         Ok(value.clone())
//!     }
//!
//!     fn decode(&self, bytes: &[u8]) -> Result<Vec<u8>, Infallible> {
//!         Ok(bytes.to_vec())
//!     }
//! }
//!
//! let bytes = Arc::new(Mutex::new(vec![3]));
//! let (state, reducer) = Document::new(vec![7], Memory(Arc::clone(&bytes)), Bytes);
//! assert_eq!(state.load_status(), LoadStatus::Unloaded);
//! # futures_executor::block_on(host.run(async {
//! let store = Store::new(state, reducer, runtime);
//! store.send(Action::load()).await.unwrap().finish().await;
//! assert_eq!(
//!     store.read(|state| (state.load_status(), state.value().clone())).await.unwrap(),
//!     (LoadStatus::Ready, vec![3]),
//! );
//!
//! store.send(Action::set(vec![9])).await.unwrap().finish().await;
//! let (close, receipt) = Action::close();
//! let task = store.send(close).await.unwrap();
//!
//! // The receipt confirms persistence and published closed state, not just task completion.
//! let outcome = receipt.await.unwrap();
//! assert_eq!(outcome.revision, 1);
//! assert!(outcome.closing);
//! task.finish().await;
//!
//! assert_eq!(*bytes.lock().unwrap(), vec![9]);
//! let state = store.shutdown_into_state().await.unwrap();
//! assert!(state.is_closed());
//! # })).unwrap();
//! ```

#![cfg_attr(
    feature = "file",
    doc = r"
# File imports

The file types use root imports, like the other persistence APIs:

```
use rca_persistence::{FileBackend, FileError, FileOptions, FileStage, SyncMode};
```

The implementation module is private:

```compile_fail
use rca_persistence::file::{FileBackend, FileError, FileOptions, FileStage, SyncMode};
```
"
)]

include!("crate_items.rs");
