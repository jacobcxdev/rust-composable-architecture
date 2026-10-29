//! Separates backend work completion from atomic revision comparison.

use std::fmt;

use crate::Backend;

/// Stops new backend work and waits for every already admitted operation.
///
/// This capability says nothing about successful persistence or rollback.
/// Closing is permanent across aliases of the same backend session. Once close
/// stops admission, dropping its future must not reopen admission or lose pending work.
/// A later close must resume waiting for those same operations. No `Clone` is required.
pub trait BackendSession: Backend {
    /// Closes admission and waits for physical backend quiescence.
    ///
    /// The first poll must stop new work before waiting for admitted operations.
    /// Creating this future alone need not stop admission. Success means that all
    /// admitted operations have ended, including work whose delivery future was dropped.
    ///
    /// # Errors
    /// Returns the original backend failure without claiming quiescence unless
    /// the implementation's error contract explicitly guarantees it.
    fn close(&self) -> impl Future<Output = Result<(), Self::Error>> + Send;
}

/// Couples bytes or explicit absence with the backend's actual revision identity.
///
/// The backend captures both fields together. Debug output omits their contents.
pub struct BackendSnapshot<Revision> {
    /// Identity changes after each relevant write, even a return to earlier bytes.
    pub revision: Revision,
    /// Missing storage is distinct from an empty byte payload.
    pub bytes: Option<Vec<u8>>,
}

impl<Revision: Clone> Clone for BackendSnapshot<Revision> {
    fn clone(&self) -> Self {
        Self {
            revision: self.revision.clone(),
            bytes: self.bytes.clone(),
        }
    }
}

impl<Revision> fmt::Debug for BackendSnapshot<Revision> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BackendSnapshot").finish_non_exhaustive()
    }
}

/// Reports either a committed comparison or the actual conflicting storage state.
///
/// Both outcomes retain the backend's original revision value without requiring `Debug`.
pub enum CompareSave<Revision> {
    /// The atomic comparison succeeded and committed this new backend revision.
    Saved(Revision),
    /// Nothing was written; storage no longer matched the expected revision.
    Conflict(BackendSnapshot<Revision>),
}

impl<Revision> fmt::Debug for CompareSave<Revision> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Saved(_) => "CompareSave::Saved(..)",
            Self::Conflict(_) => "CompareSave::Conflict(..)",
        })
    }
}

/// Provides genuine atomic compare-and-save independently of session completion.
///
/// Revision equality must identify the same storage version, not merely equal
/// bytes, timestamps with collisions, or an earlier identical value. Absence
/// also has an identity. Comparison and mutation must form one atomic backend
/// operation with respect to every writer covered by this capability.
///
/// A plain file replacement does not satisfy this protocol for arbitrary
/// external writers. The native file adapter deliberately does not implement it.
pub trait RevisionBackend: Backend {
    /// Opaque backend identity; independent of local revisions and load attempts.
    type Revision: Clone + Eq + Send + 'static;

    /// Reads bytes and their matching backend revision in one consistent snapshot.
    ///
    /// # Errors
    /// Returns the original backend error; absence is a successful snapshot.
    fn load_revisioned(
        &self,
    ) -> impl Future<Output = Result<BackendSnapshot<Self::Revision>, Self::Error>> + Send;

    /// Atomically compares the expected revision and commits replacement bytes.
    ///
    /// # Errors
    /// Returns the original backend failure. A comparison mismatch is the typed
    /// [`CompareSave::Conflict`] outcome and must not perform the proposed write.
    fn compare_and_save(
        &self,
        expected: Self::Revision,
        bytes: Vec<u8>,
    ) -> impl Future<Output = Result<CompareSave<Self::Revision>, Self::Error>> + Send;
}

/// Marks an unversioned interpretation without inventing backend revision values.
///
/// Plain documents use this private marker internally but never report a backend
/// revision. External code cannot construct it or request a default value.
///
/// ```compile_fail,E0423
/// use rca_persistence::NoBackendRevision;
/// let revision = NoBackendRevision(());
/// ```
///
/// ```compile_fail,E0277
/// let revision: rca_persistence::NoBackendRevision = Default::default();
/// ```
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NoBackendRevision(
    /// Keeps construction inside this crate while retaining zero-sized storage.
    pub(crate) (),
);

#[cfg(feature = "file")]
impl BackendSession for crate::FileBackend {
    async fn close(&self) -> Result<(), Self::Error> {
        crate::FileBackend::close(self).await;
        Ok(())
    }
}
