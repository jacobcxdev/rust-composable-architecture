mod document;
mod document_transitions;
mod receipt;
mod revision;
mod shared_document;
mod shared_key;
mod subscription;

#[cfg(feature = "file")]
mod file;

#[cfg(feature = "file")]
#[doc(inline)]
pub use file::{FileBackend, FileError, FileOptions, FileStage, SyncMode};

#[doc(inline)]
pub use document::{
    Action, Attempt, Backend, Codec, Document, Failure, LoadStatus, Operation, State,
};
#[doc(inline)]
pub use receipt::{FlushOutcome, Receipt, ReceiptError};
#[doc(inline)]
pub use revision::{
    BackendSession, BackendSnapshot, CompareSave, NoBackendRevision, RevisionBackend,
};
#[doc(inline)]
pub use shared_document::{
    ConflictDetected, DocumentKey, DocumentKeyError, DocumentSubscription,
    DocumentSubscriptionStatus, SessionCloseError, SessionFailure, SharedConflict, SharedDocument,
    SharedDocumentAction, SharedDocumentOwner, SharedDocumentSession, SharedDocumentState,
};
#[doc(inline)]
pub use shared_key::{BackendKey, BackendKeyError};
#[doc(inline)]
pub use subscription::{
    ProducerJoin, SharedProducer, SharedSubscription, SharedUpdate, SharedUpdates, UpdateError,
};
