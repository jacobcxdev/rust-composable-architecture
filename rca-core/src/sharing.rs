//! Owns shared values, attenuated readers, and reference-capturing projections.
//!
//! [`Shared`] clones belong to one rebindable cohort. A reader or projection
//! captures the cohort's current value reference, not its future binding.
//! Independently created cohorts can share a reference after [`Shared::rebind`]
//! without sharing subsequent rebindings. These handles own data, not Store
//! membership or output authority. Retaining them cannot revive a retired scope.
//!
//! Access is synchronous and non-blocking. No mutex guard or borrowed value can
//! escape an access callback. Overlapping access (including synchronous reentry)
//! returns Busy; a panicking access callback poisons that value reference.
//! Readers with interior-mutable values retain the capabilities inherent in `&T`;
//! attenuation means no `&mut T` or rebinding API, not deep immutability.
//!
//! Mutation callbacks operate in place: returning `Err` retains earlier writes
//! and the original typed cause. Every normally returning mutation invokes its
//! explicit publication callback after unlocking, even on `Err` or an unchanged
//! value. A panic does not publish. Publication may reenter and panic without
//! poisoning the value. Publication means invoking the caller's callback;
//! acceptance by an external destination remains the caller's responsibility.
//! No observer registry, automatic equality test, task,
//! persistence, or subscription is installed by this module.
//!
//! Partial projections deliberately have **no last-value cache**. Reads and writes
//! return Absent while the selected optional, case, or key is missing. They never
//! recreate that payload. If it reappears in the same reference, the projection
//! sees the current payload; this is data membership, not Store scope authority.
//!
//! # Ordering and lock boundaries
//!
//! A cohort access first captures its reference under the cohort mutex, releases
//! that mutex, then tries the value mutex. A concurrent rebind after capture does
//! not retarget the operation. Rebinding captures the target reference first and
//! linearises at the receiving cohort's pointer swap; it never locks a value.
//! A mutation's retained result becomes readable when its value lock is released.
//! Serialised commits do not imply ordered publication callbacks across threads:
//! another mutation may commit before an earlier callback reads the latest value.
//! Capture an owned snapshot in the mutation result when exact snapshots matter.
//!
//! Error precedence is cohort capture, value lock, projection presence, then user
//! mutation result. Rebinding checks target capture before receiver access. Busy
//! and Poisoned reflect `Mutex::try_lock`; no waiting or poison recovery occurs.
//! Failed access/absence skips both mutation and publication. Library-displaced
//! references and returned results/errors are dropped outside library locks.
//! Read/path/mutation callbacks and any destructors they themselves execute run
//! under the value lock: keep them short and synchronous. Publication runs with
//! no library lock held. Auto-traits follow stored ownership, without unsafe
//! promotion or blanket value `Clone`, `Debug`, `Send`, or `Sync` requirements.
//!
//! # Examples
//!
//! ```
//! use rca_core::Shared;
//! let value = Shared::new(Some(1));
//! let reader = value.reader().unwrap();
//! let child = value.project(Option::as_ref, Option::as_mut).unwrap();
//! child.update(|value| *value += 1, || {}).unwrap();
//! value.rebind(&Shared::new(Some(10))).unwrap();
//! assert_eq!(reader.read(|value| *value).unwrap(), Some(2));
//! assert_eq!(value.read(|value| *value).unwrap(), Some(10));
//! ```

pub mod cache;

use std::any::Any;
use std::fmt;
use std::marker::PhantomData;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, TryLockError};

/// Owns one value independently of any cohort that currently points to it.
struct SharedReference<Value> {
    /// Serialises synchronous value access without exposing its guard.
    value: Mutex<Value>,
    /// Keeps an optional cache subscription alive until the final reader drops.
    retained: OnceLock<Box<dyn Any + Send + Sync>>,
}

impl<Value> SharedReference<Value> {
    /// Creates an uncached reference; cache construction may install one resource.
    fn new(value: Value) -> Self {
        Self {
            value: Mutex::new(value),
            retained: OnceLock::new(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
/// Distinguishes failed admission from a missing projected payload.
enum AccessKind {
    /// Another access currently owns the required mutex.
    Busy,
    /// A previous access unwound while holding the required mutex.
    Poisoned,
    /// A partial projection selected no current value.
    Absent,
}

/// Reports unavailable synchronous access without exposing a lock guard.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SharedAccessError {
    /// The first failed step in capture, locking or projection.
    kind: AccessKind,
}

impl SharedAccessError {
    /// Returns whether another access currently holds the required lock.
    #[must_use]
    pub const fn is_busy(&self) -> bool {
        matches!(self.kind, AccessKind::Busy)
    }

    /// Returns whether a prior access panicked while holding the value lock.
    #[must_use]
    pub const fn is_poisoned(&self) -> bool {
        matches!(self.kind, AccessKind::Poisoned)
    }

    /// Returns whether the selected partial projection is currently missing.
    #[must_use]
    pub const fn is_absent(&self) -> bool {
        matches!(self.kind, AccessKind::Absent)
    }

    /// Shares a fixed, value-free description with cache error diagnostics.
    const fn message(self) -> &'static str {
        match self.kind {
            AccessKind::Busy => "shared value access is busy",
            AccessKind::Poisoned => "shared value access is poisoned",
            AccessKind::Absent => "shared projection is absent",
        }
    }
}

impl fmt::Display for SharedAccessError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.message())
    }
}

impl std::error::Error for SharedAccessError {}

/// Separates skipped mutation from a callback failure after retained writes.
enum MutationFailure<Failure> {
    /// Capture, locking or projection failed before mutation ran.
    Access(SharedAccessError),
    /// Mutation returned this cause and its publication callback returned normally.
    Committed(Failure),
}

/// Preserves an access failure or a typed cause after retained mutation.
pub struct SharedMutationError<Failure> {
    /// Preserves either access classification or the caller's owned cause.
    failure: MutationFailure<Failure>,
}

impl<Failure> SharedMutationError<Failure> {
    /// Returns whether mutation retained its writes and the publication callback returned.
    #[must_use]
    pub const fn is_committed(&self) -> bool {
        matches!(self.failure, MutationFailure::Committed(_))
    }

    /// Returns the access failure when the mutation never ran.
    #[must_use]
    pub const fn access_error(&self) -> Option<&SharedAccessError> {
        match &self.failure {
            MutationFailure::Access(error) => Some(error),
            MutationFailure::Committed(_) => None,
        }
    }

    /// Returns the original owned mutation cause, without requiring domain traits.
    #[must_use]
    pub fn into_failure(self) -> Option<Failure> {
        match self.failure {
            MutationFailure::Access(_) => None,
            MutationFailure::Committed(failure) => Some(failure),
        }
    }
}

impl<Failure> From<SharedAccessError> for SharedMutationError<Failure> {
    fn from(error: SharedAccessError) -> Self {
        Self {
            failure: MutationFailure::Access(error),
        }
    }
}

impl<Failure> fmt::Debug for SharedMutationError<Failure> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SharedMutationError")
            .field("committed", &self.is_committed())
            .field("access", &self.access_error())
            .finish_non_exhaustive()
    }
}

impl<Failure> fmt::Display for SharedMutationError<Failure> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.failure {
            MutationFailure::Access(error) => error.fmt(f),
            MutationFailure::Committed(_) => {
                f.write_str("shared mutation failed after retaining its writes")
            }
        }
    }
}

impl<Failure: std::error::Error + 'static> std::error::Error for SharedMutationError<Failure> {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(match &self.failure {
            MutationFailure::Access(error) => error,
            MutationFailure::Committed(failure) => failure,
        })
    }
}

/// Owns a writable shared value through a rebindable alias cohort.
pub struct Shared<Value> {
    /// Shared rebinding slot; ordinary clones follow every successful pointer swap.
    cohort: Arc<Mutex<SharedReader<Value>>>,
}

impl<Value> Shared<Value> {
    /// Creates a new value reference and an independent cohort.
    #[must_use]
    pub fn new(value: Value) -> Self {
        Self {
            cohort: Arc::new(Mutex::new(SharedReader {
                reference: Arc::new(SharedReference::new(value)),
            })),
        }
    }

    /// Captures the current reference as a reader, independent of later rebinding.
    ///
    /// # Errors
    ///
    /// Returns Busy or Poisoned if the cohort cannot be accessed.
    pub fn reader(&self) -> Result<SharedReader<Value>, SharedAccessError> {
        Ok(try_lock(&self.cohort)?.clone())
    }

    /// Reads the currently captured value with a synchronous callback.
    ///
    /// # Errors
    ///
    /// Returns Busy or Poisoned, checking the cohort before the value reference.
    ///
    /// # Panics
    ///
    /// Propagates a read callback panic, poisoning the value reference. If the
    /// callback rebinds the cohort, releasing the captured reference may also
    /// run a panicking value or resource destructor before this method returns.
    pub fn read<ResultValue>(
        &self,
        read: impl FnOnce(&Value) -> ResultValue,
    ) -> Result<ResultValue, SharedAccessError> {
        let reference = self.reader()?;
        let result = reference.read(read);
        drop(reference);
        result
    }

    /// Mutates the captured value and explicitly publishes after releasing its lock.
    ///
    /// The owned result cannot borrow the value. Publication runs exactly once
    /// after a normally returning mutation, even when the value is unchanged.
    ///
    /// # Examples
    ///
    /// ```
    /// use rca_core::Shared;
    /// let value = Shared::new(1);
    /// let previous = value.update(|value| std::mem::replace(value, 2), || {}).unwrap();
    /// assert_eq!(previous, 1);
    /// ```
    ///
    /// # Errors
    ///
    /// Returns Busy or Poisoned before invoking either callback.
    ///
    /// # Panics
    ///
    /// A mutation panic poisons the reference and skips publication. A publication
    /// panic propagates after commit, without poisoning that reference. Releasing
    /// a reference displaced by publication may also panic before returning.
    pub fn update<ResultValue>(
        &self,
        update: impl FnOnce(&mut Value) -> ResultValue,
        publish: impl FnOnce(),
    ) -> Result<ResultValue, SharedAccessError> {
        let reference = self.reader()?;
        let result = update_reference(&reference, |value| Some(value), update, publish);
        drop(reference);
        result
    }

    /// Mutates the captured value and explicitly publishes after releasing its lock.
    ///
    /// Returning `Err` retains and publishes earlier writes; this is not rollback.
    /// The returned result and typed cause are owned and cannot borrow the value.
    ///
    /// # Errors
    ///
    /// Returns an uncommitted access failure or the committed original user cause.
    ///
    /// # Panics
    ///
    /// A mutation panic poisons the reference and skips publication. A publication
    /// panic propagates after commit, without poisoning that reference. Releasing
    /// a reference displaced by publication may also panic before returning.
    pub fn try_update<ResultValue, Failure>(
        &self,
        update: impl FnOnce(&mut Value) -> Result<ResultValue, Failure>,
        publish: impl FnOnce(),
    ) -> Result<ResultValue, SharedMutationError<Failure>> {
        let reference = self.reader()?;
        let result = match update_reference(&reference, |value| Some(value), update, publish) {
            Ok(result) => result.map_err(|failure| SharedMutationError {
                failure: MutationFailure::Committed(failure),
            }),
            Err(error) => Err(error.into()),
        };
        drop(reference);
        result
    }

    /// Rebinds this cohort to the target's currently captured value reference.
    ///
    /// All ordinary clones follow the swap; captured readers/projections do not.
    /// The target keeps its independent cohort. Displaced references are dropped
    /// after unlocking, so their destructors may reenter the receiving cohort.
    ///
    /// # Errors
    ///
    /// Checks target cohort access first, then receiver access for Busy/Poisoned.
    /// On failure the receiving binding is unchanged.
    ///
    /// # Panics
    ///
    /// A displaced value's destructor may panic after the completed swap.
    pub fn rebind(&self, target: &Self) -> Result<(), SharedAccessError> {
        let replacement = target.reader()?;
        let previous = {
            let mut cohort = try_lock(&self.cohort)?;
            std::mem::replace(&mut *cohort, replacement)
        };
        drop(previous);
        Ok(())
    }

    /// Captures a writable partial projection without caching missing values.
    ///
    /// The getters must select the same logical field. Presence is evaluated on
    /// each access, not when constructing the projection. Total fields use `Some`.
    ///
    /// # Errors
    ///
    /// Returns Busy or Poisoned when capturing the cohort's reference fails.
    pub fn project<Child, Get, GetMut>(
        &self,
        get: Get,
        get_mut: GetMut,
    ) -> Result<SharedProjection<Value, Child, Get, GetMut>, SharedAccessError>
    where
        Get: for<'a> Fn(&'a Value) -> Option<&'a Child>,
        GetMut: for<'a> Fn(&'a mut Value) -> Option<&'a mut Child>,
    {
        Ok(SharedProjection {
            reader: self.reader()?.project(get),
            get_mut,
        })
    }
}

impl<Value> Clone for Shared<Value> {
    fn clone(&self) -> Self {
        Self {
            cohort: Arc::clone(&self.cohort),
        }
    }
}

impl<Value> fmt::Debug for Shared<Value> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Shared").finish_non_exhaustive()
    }
}

/// Reads an owned captured reference without mutation or rebinding authority.
pub struct SharedReader<Value> {
    /// Keeps this exact value and any cache subscription alive across rebinding.
    reference: Arc<SharedReference<Value>>,
}

impl<Value> SharedReader<Value> {
    /// Reads the captured value through a non-escaping synchronous borrow.
    ///
    /// # Errors
    ///
    /// Returns Busy for overlapping access or Poisoned after a callback panic.
    ///
    /// # Panics
    ///
    /// Propagates a read callback panic and poisons the captured reference.
    pub fn read<ResultValue>(
        &self,
        read: impl FnOnce(&Value) -> ResultValue,
    ) -> Result<ResultValue, SharedAccessError> {
        let value = try_lock(&self.reference.value)?;
        let result = read(&value);
        drop(value);
        Ok(result)
    }

    /// Captures a partial read projection with no last-value cache.
    #[must_use]
    pub fn project<Child, Get>(&self, get: Get) -> SharedReadProjection<Value, Child, Get>
    where
        Get: for<'a> Fn(&'a Value) -> Option<&'a Child>,
    {
        SharedReadProjection {
            root: self.clone(),
            get,
            marker: PhantomData,
        }
    }
}

impl<Value> Clone for SharedReader<Value> {
    fn clone(&self) -> Self {
        Self {
            reference: Arc::clone(&self.reference),
        }
    }
}

impl<Value> fmt::Debug for SharedReader<Value> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SharedReader").finish_non_exhaustive()
    }
}

/// Reads a partial field of a captured owned reference.
pub struct SharedReadProjection<Root, Value, Get> {
    /// Owns the original root, not the cohort's current replacement.
    root: SharedReader<Root>,
    /// Selects current membership on every read under the root's value lock.
    get: Get,
    /// Names the borrowed output without owning it or imposing its auto traits.
    marker: PhantomData<fn() -> Value>,
}

impl<Root, Value, Get> SharedReadProjection<Root, Value, Get>
where
    Get: for<'a> Fn(&'a Root) -> Option<&'a Value>,
{
    /// Reads the currently present payload without retaining an absent value.
    ///
    /// # Errors
    ///
    /// Checks Busy/Poisoned before evaluating the path; missing paths return Absent.
    ///
    /// # Panics
    ///
    /// A path/read callback panic propagates and poisons the captured reference.
    pub fn read<ResultValue>(
        &self,
        read: impl FnOnce(&Value) -> ResultValue,
    ) -> Result<ResultValue, SharedAccessError> {
        let root = try_lock(&self.root.reference.value)?;
        let value = (self.get)(&root).ok_or(SharedAccessError {
            kind: AccessKind::Absent,
        })?;
        let result = read(value);
        drop(root);
        Ok(result)
    }
}

impl<Root, Value, Get: Clone> Clone for SharedReadProjection<Root, Value, Get> {
    fn clone(&self) -> Self {
        Self {
            root: self.root.clone(),
            get: self.get.clone(),
            marker: PhantomData,
        }
    }
}

impl<Root, Value, Get> fmt::Debug for SharedReadProjection<Root, Value, Get> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SharedReadProjection")
            .finish_non_exhaustive()
    }
}

/// Mutates a partial field while retaining its originally captured reference.
pub struct SharedProjection<Root, Value, Get, GetMut> {
    /// Provides the shared captured root and immutable selection path.
    reader: SharedReadProjection<Root, Value, Get>,
    /// Selects the same logical field for mutation under that root's value lock.
    get_mut: GetMut,
}

impl<Root, Value, Get, GetMut> SharedProjection<Root, Value, Get, GetMut>
where
    Get: for<'a> Fn(&'a Root) -> Option<&'a Value>,
    GetMut: for<'a> Fn(&'a mut Root) -> Option<&'a mut Value>,
{
    /// Reads the selected payload from the captured reference.
    ///
    /// # Errors
    ///
    /// Returns Busy, Poisoned, or Absent, in access-before-presence order.
    ///
    /// # Panics
    ///
    /// A path/read callback panic propagates and poisons the reference.
    pub fn read<ResultValue>(
        &self,
        read: impl FnOnce(&Value) -> ResultValue,
    ) -> Result<ResultValue, SharedAccessError> {
        self.reader.read(read)
    }

    /// Attenuates this projection without retargeting its captured reference.
    #[must_use]
    pub fn reader(&self) -> SharedReadProjection<Root, Value, Get>
    where
        Get: Clone,
    {
        self.reader.clone()
    }

    /// Mutates a present payload and explicitly publishes after unlocking.
    ///
    /// This never inserts a missing payload or retargets the captured reference.
    /// Publication runs exactly once after a normally returning mutation, even
    /// when unchanged. The owned result cannot borrow the payload.
    ///
    /// # Errors
    ///
    /// Returns Busy, Poisoned, or Absent before invoking either callback.
    ///
    /// # Panics
    ///
    /// A path/mutation panic poisons the reference and skips publication.
    /// A publication panic propagates after commit, without poisoning it.
    pub fn update<ResultValue>(
        &self,
        update: impl FnOnce(&mut Value) -> ResultValue,
        publish: impl FnOnce(),
    ) -> Result<ResultValue, SharedAccessError> {
        update_reference(&self.reader.root, &self.get_mut, update, publish)
    }

    /// Mutates a present payload and explicitly publishes after unlocking.
    ///
    /// This never inserts a missing payload. A user `Err` retains its partial
    /// writes and publishes, just like [`Shared::try_update`].
    ///
    /// # Errors
    ///
    /// Returns uncommitted Busy/Poisoned/Absent or the committed original cause.
    ///
    /// # Panics
    ///
    /// A path/mutation panic poisons the reference and skips publication.
    /// A publication panic propagates after commit, without poisoning it.
    pub fn try_update<ResultValue, Failure>(
        &self,
        update: impl FnOnce(&mut Value) -> Result<ResultValue, Failure>,
        publish: impl FnOnce(),
    ) -> Result<ResultValue, SharedMutationError<Failure>> {
        update_reference(&self.reader.root, &self.get_mut, update, publish)?.map_err(|failure| {
            SharedMutationError {
                failure: MutationFailure::Committed(failure),
            }
        })
    }
}

impl<Root, Value, Get: Clone, GetMut: Clone> Clone for SharedProjection<Root, Value, Get, GetMut> {
    fn clone(&self) -> Self {
        Self {
            reader: self.reader.clone(),
            get_mut: self.get_mut.clone(),
        }
    }
}

impl<Root, Value, Get, GetMut> fmt::Debug for SharedProjection<Root, Value, Get, GetMut> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SharedProjection").finish_non_exhaustive()
    }
}

/// Classifies non-blocking access without waiting or recovering poisoned state.
fn try_lock<Value>(mutex: &Mutex<Value>) -> Result<MutexGuard<'_, Value>, SharedAccessError> {
    mutex.try_lock().map_err(|error| SharedAccessError {
        kind: match error {
            TryLockError::WouldBlock => AccessKind::Busy,
            TryLockError::Poisoned(_) => AccessKind::Poisoned,
        },
    })
}

/// Retains the result locally through unlocked publication, including its panic path.
///
/// Failed access skips both callbacks. A normal mutation publishes even when its
/// result is a user error; mutation panic unlocks without publishing.
fn update_reference<Root, Value, ResultValue>(
    reference: &SharedReader<Root>,
    get_mut: impl for<'a> FnOnce(&'a mut Root) -> Option<&'a mut Value>,
    update: impl FnOnce(&mut Value) -> ResultValue,
    publish: impl FnOnce(),
) -> Result<ResultValue, SharedAccessError> {
    let result = {
        let mut root = try_lock(&reference.reference.value)?;
        let value = get_mut(&mut root).ok_or(SharedAccessError {
            kind: AccessKind::Absent,
        })?;
        update(value)
    };
    publish();
    Ok(result)
}

#[cfg(test)]
ordinary_test_items! {
    mod update_tests {
        use std::cell::Cell;

        use super::Shared;

        // Private lock fixtures make cohort contention and poison deterministic.
        #[test]
        fn cohort_capture_errors_preserve_rebind_ownership_and_precedence() {
            let shared = Shared::new(String::from("original"));
            let target = Shared::new(String::from("target"));
            let original = shared.reader().unwrap();
            let destination = target.reader().unwrap();
            let cohort = shared.cohort.lock().unwrap();
            assert!(shared.read(|_| panic!("busy read")).unwrap_err().is_busy());
            assert!(
                shared
                    .project(|value| Some(value), |value| Some(value))
                    .unwrap_err()
                    .is_busy()
            );
            assert!(shared.rebind(&target).unwrap_err().is_busy());
            assert!(target.rebind(&shared).unwrap_err().is_busy());
            drop(cohort);
            assert!(shared.read(|value| value == "original").unwrap());
            assert!(original.read(|value| value == "original").unwrap());
            assert!(destination.read(|value| value == "target").unwrap());
            shared.rebind(&target).unwrap();
            assert!(shared.read(|value| value == "target").unwrap());

            assert!(
                std::panic::catch_unwind(|| {
                    let _cohort = target.cohort.lock().unwrap();
                    panic!("interrupted target capture");
                })
                .is_err()
            );
            let cohort = shared.cohort.lock().unwrap();
            // Target poison wins over receiver contention; neither binding changes.
            assert!(shared.rebind(&target).unwrap_err().is_poisoned());
            drop(cohort);
            assert!(target.rebind(&shared).unwrap_err().is_poisoned());
            assert!(
                target
                    .read(|_| panic!("poisoned read"))
                    .unwrap_err()
                    .is_poisoned()
            );
            assert!(
                target
                    .project(|value| Some(value), |value| Some(value))
                    .unwrap_err()
                    .is_poisoned()
            );
            assert!(original.read(|value| value == "original").unwrap());
            assert!(destination.read(|value| value == "target").unwrap());
            assert!(shared.read(|value| value == "target").unwrap());
            shared
                .rebind(&Shared::new(String::from("replacement")))
                .unwrap();
            assert!(shared.read(|value| value == "replacement").unwrap());
            assert!(destination.read(|value| value == "target").unwrap());
        }

        #[test]
        fn cohort_busy_and_poisoned_skip_both_callbacks() {
            let shared = Shared::new(0);
            let reader = shared.reader().unwrap();
            let mutations = Cell::new(0);
            let publications = Cell::new(0);
            let update = |value: &mut i32| {
                mutations.set(mutations.get() + 1);
                *value += 1;
                Ok::<_, ()>(*value)
            };
            let publish = || publications.set(publications.get() + 1);
            let cohort = shared.cohort.lock().unwrap();
            assert!(
                shared
                    .update(|_| panic!("busy mutation"), || panic!("busy publication"))
                    .unwrap_err()
                    .is_busy()
            );
            let error = shared.try_update(update, publish).unwrap_err();
            assert!(!error.is_committed());
            assert!(error.access_error().unwrap().is_busy());
            assert_eq!(error.into_failure(), None);
            assert_eq!((mutations.get(), publications.get()), (0, 0));
            assert_eq!(reader.read(|value| *value).unwrap(), 0);
            drop(cohort);
            let panic = std::panic::catch_unwind(|| {
                let _cohort = shared.cohort.lock().unwrap();
                panic!("cohort");
            });
            panic.unwrap_err();
            assert!(
                shared
                    .update(
                        |_| panic!("poisoned mutation"),
                        || panic!("poisoned publication"),
                    )
                    .unwrap_err()
                    .is_poisoned()
            );
            let error = shared.try_update(update, publish).unwrap_err();
            assert!(!error.is_committed());
            assert!(error.access_error().unwrap().is_poisoned());
            assert_eq!(error.into_failure(), None);
            assert_eq!((mutations.get(), publications.get()), (0, 0));
            assert_eq!(reader.read(|value| *value).unwrap(), 0);

            let healthy = Shared::new(7);
            assert_eq!(healthy.try_update(update, publish).unwrap(), 8);
            assert_eq!((mutations.get(), publications.get()), (1, 1));
            assert_eq!(healthy.read(|value| *value).unwrap(), 8);
        }
    }
}
