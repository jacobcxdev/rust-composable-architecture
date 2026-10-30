//! Caches weak typed references inside an explicitly owned namespace.
//!
//! Fresh ordinary construction evaluates its seed before initial loading, even
//! when storage contains a value. A live compatible hit does no seed, load or
//! subscription work. Require always loads first and rejects storage absence;
//! its loaded value may then lose to an existing live reference.
//!
//! Constructors check, build outside the cache lock, then check again. Racing
//! candidates may both seed, load and subscribe; one live publication wins and
//! the losing subscription is dropped outside the lock. There is no exactly-once
//! provider guarantee. Reset removes lookup entries, not storage, existing
//! handles or in-flight constructors; a later publication can repopulate it.
//!
//! Initial load borrows the seed. Subscription context records initial versus
//! user intent, without duplicating a move-only seed. Unlike Swift Sharing, a
//! subscription does not automatically restore a captured original default.
//! Producer absence, conflict handling, persistence ordering and Store action
//! delivery are separate policies, not supplied by this cache.
//!
//! Loading and subscribing are separate provider calls. A non-replaying provider
//! can miss an update between them; construction does not promise a continuous
//! snapshot or an event history. Providers requiring continuity must supply
//! replay, atomic snapshot/registration or revision reconciliation themselves.
//! Synchronous subscription delivery may replace the initially loaded value or
//! seed before construction returns.
//!
//! Only cache admission requires `Value: Send + 'static`; no `Sync`, `Clone` or
//! `Debug` is required of values. General in-memory [`Shared`] remains usable
//! with local values. Provider identities and callbacks run outside the cache
//! lock. That lock briefly blocks only for internal immutable metadata access.
//! Value access keeps the ordinary non-blocking shared-value access contract.
//!
//! Reloads arbitrate on their captured reference: the latest started request
//! alone may commit, even when it completes absent, fails or is cancelled.
//! Superseded callers still receive their own provider errors, but cannot
//! replace the current value. Successful producer delivery also supersedes
//! earlier reloads; failed delivery leaves their authority unchanged. Producer
//! ordering, absence and conflict rules remain separate policies.

use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::convert::Infallible;
use std::fmt;
use std::sync::{Arc, Mutex, Weak};

use super::{Shared, SharedAccessError, SharedReader, SharedReference, try_lock};

#[cfg(all(test, feature = "allocation-measurement"))]
pub(crate) mod allocation_tests;

/// Identifies a resource and its codec configuration within one cache namespace.
#[derive(Clone, Eq, PartialEq)]
pub struct SharedKeyIdentity {
    /// Names the resource within a cache namespace, including backend identity.
    address: String,
    /// Distinguishes incompatible codec implementations at the same address.
    codec: TypeId,
    /// Distinguishes incompatible settings of the same codec implementation.
    configuration: String,
}

impl SharedKeyIdentity {
    /// Creates a stable address with explicit codec type and configuration.
    ///
    /// Equal addresses must designate the same resource. Include backend/store
    /// identity in the address when one cache serves multiple stores. Codec
    /// configuration must distinguish incompatible instances of the same type.
    #[must_use]
    pub fn new<Codec: 'static>(
        address: impl Into<String>,
        configuration: impl Into<String>,
    ) -> Self {
        Self {
            address: address.into(),
            codec: TypeId::of::<Codec>(),
            configuration: configuration.into(),
        }
    }
}

impl fmt::Debug for SharedKeyIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SharedKeyIdentity").finish_non_exhaustive()
    }
}

/// Distinguishes a borrowed construction seed from an explicit storage load.
pub enum SharedLoadContext<'a, Value> {
    /// Initial construction borrows its already evaluated seed.
    Initial(&'a Value),
    /// Explicit require or reload supplies no default.
    UserInitiated,
}

impl<Value> fmt::Debug for SharedLoadContext<'_, Value> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Initial(_) => "SharedLoadContext::Initial(..)",
            Self::UserInitiated => "SharedLoadContext::UserInitiated",
        })
    }
}

/// Identifies subscription intent without retaining a duplicate seed value.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SharedSubscribeContext {
    /// Ordinary seeded construction follows its initial load.
    Initial,
    /// Require or key replacement skips an implicit initial load.
    UserInitiated,
}

/// Supplies typed loading and an owned subscription for a stable resource.
///
/// Implementations must keep identity stable and represent absence as outer
/// `None`, independently of a stored optional value. Subscription guards must
/// release their provider registration on drop. Retain the supplied weak
/// subscriber, not an independently captured strong handle to its owner.
/// Loading before subscribing does not itself prevent missed updates. Specify
/// the provider's replay, registration and absence rules when stronger continuity
/// is required; successful subscription setup is not necessarily delivery acknowledgement.
pub trait SharedReaderKey: Send + Sync + 'static {
    /// Value admitted to the heterogeneous cache.
    type Value: Send + 'static;
    /// Original provider failure, without a required formatting trait.
    type Error;
    /// Resource retained until the final captured reference is dropped.
    type Subscription: Send + Sync + 'static;

    /// Returns the stable resource and codec compatibility identity.
    fn identity(&self) -> SharedKeyIdentity;

    /// Loads storage, keeping a missing value distinct from a provider failure.
    ///
    /// # Errors
    /// Returns the original provider failure.
    fn load(
        &self,
        context: SharedLoadContext<'_, Self::Value>,
    ) -> impl Future<Output = Result<Option<Self::Value>, Self::Error>> + Send;

    /// Starts owned producer delivery without holding a cache or value lock.
    ///
    /// # Errors
    /// Returns the original subscription failure; construction is not published.
    fn subscribe(
        &self,
        context: SharedSubscribeContext,
        subscriber: SharedSubscriber<Self::Value>,
    ) -> Result<Self::Subscription, Self::Error>;
}

/// Adds explicit saving capability to a typed readable resource.
///
/// Cache-backed mutation uses ordinary [`Shared::try_update`] and its explicit
/// publication callback. This trait does not install automatic saving, flush
/// barriers, coalescing, rollback or a runtime.
pub trait SharedKey: SharedReaderKey {
    /// Saves a borrowed value using the provider's own completion guarantee.
    ///
    /// # Errors
    /// Returns the original encoding or provider failure.
    fn save(&self, value: &Self::Value) -> impl Future<Output = Result<(), Self::Error>> + Send;
}

/// Delivers owned values through a weak captured reference.
pub struct SharedSubscriber<Value> {
    /// Targets only the original captured value; never resolves a new cache hit.
    reference: Weak<SharedReference<Value>>,
    /// Shares reload ordering without keeping its resource alive.
    loads: Weak<Mutex<Option<Weak<()>>>>,
}

impl<Value> SharedSubscriber<Value> {
    /// Replaces a live value, returning false after its final owner has gone.
    ///
    /// This conveys data only, not Store scope or action-delivery authority.
    /// Successful replacement supersedes already started reloads atomically.
    /// Displaced values are dropped after releasing both internal locks.
    ///
    /// # Errors
    /// Returns Busy or Poisoned without applying the value or superseding a reload.
    ///
    /// # Panics
    /// A displaced value's destructor may panic after replacement. It runs
    /// after releasing both internal locks.
    pub fn yield_value(&self, mut value: Value) -> Result<bool, SharedAccessError> {
        self.prepare_publication()
            .map_or(Ok(false), |publication| publication.try_replace(&mut value))
    }

    /// Retains this captured reference for one externally fenced publication.
    ///
    /// Prepare before acquiring a caller-owned cancellation fence. Release that
    /// fence before dropping this owner or the displaced value. Preparation does
    /// not grant Store, session or subscription authority and never finds a new
    /// reference by logical identity. Returns None after the captured resource ends.
    #[must_use]
    pub fn prepare_publication(&self) -> Option<SharedPublication<Value>> {
        Some(SharedPublication {
            reference: self.reference.upgrade()?,
            loads: self.loads.upgrade()?,
        })
    }
}

/// Briefly retains one captured reference and its reload coordinator.
///
/// Drop this prepared owner outside external control fences: it may be the final
/// reference owner, whose value/resource destructors can run arbitrary code.
#[must_use]
pub struct SharedPublication<Value> {
    /// Retains the exact captured owner through an externally fenced swap.
    reference: Arc<SharedReference<Value>>,
    /// Lets successful producer delivery retire earlier reloads atomically.
    loads: Arc<Mutex<Option<Weak<()>>>>,
}

impl<Value> SharedPublication<Value> {
    /// Swaps a live reference with the caller's slot without dropping either value.
    ///
    /// On success the slot contains the displaced value; otherwise it is unchanged.
    /// This lets an external cancellation owner hold its commit fence over the
    /// swap, then release that fence before dropping the displaced value. No user
    /// callback or value destructor runs here. Successful swaps supersede reloads.
    ///
    /// # Errors
    /// Returns Busy or Poisoned without changing the slot or reload authority.
    pub fn try_replace(&self, value: &mut Value) -> Result<bool, SharedAccessError> {
        let retired = {
            let mut latest = self.loads.lock().map_err(|_error| load_poisoned())?;
            let mut current = try_lock(&self.reference.value)?;
            std::mem::swap(&mut *current, value);
            latest.take()
        };
        drop(retired);
        Ok(true)
    }
}

impl<Value> fmt::Debug for SharedPublication<Value> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SharedPublication").finish_non_exhaustive()
    }
}

impl<Value> Clone for SharedSubscriber<Value> {
    fn clone(&self) -> Self {
        Self {
            reference: Weak::clone(&self.reference),
            loads: Weak::clone(&self.loads),
        }
    }
}

impl<Value> fmt::Debug for SharedSubscriber<Value> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SharedSubscriber").finish_non_exhaustive()
    }
}

/// Attaches a repeatable default without changing the base key's identity.
pub struct SharedKeyDefault<Key, Seed> {
    /// Supplies resource identity independently of the default value.
    key: Key,
    /// Evaluates a fresh default only when the requested operation needs one.
    seed: Seed,
}

impl<Key, Seed> SharedKeyDefault<Key, Seed> {
    /// Associates a base key with a seed closure without invoking it.
    #[must_use]
    pub const fn new(key: Key, seed: Seed) -> Self {
        Self { key, seed }
    }

    /// Removes the default for require, key replacement or an overriding seed.
    ///
    /// # Panics
    /// Propagates a destructor panic from the unused seed's captures. The key is
    /// still owned locally and is dropped if that cleanup unwinds.
    #[must_use]
    pub fn into_key(self) -> Key {
        let Self { key, seed } = self;
        drop(seed);
        key
    }
}

impl<Key: SharedReaderKey, Seed: Fn() -> Key::Value> SharedKeyDefault<Key, Seed> {
    /// Evaluates the default on every direct access, independently of cache hits.
    pub fn initial_value(&self) -> Key::Value {
        (self.seed)()
    }
}

impl<Key, Seed> fmt::Debug for SharedKeyDefault<Key, Seed> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SharedKeyDefault").finish_non_exhaustive()
    }
}

/// Keeps cache admission failures distinct from value access and provider causes.
enum Failure<Cause> {
    /// A live address has an incompatible codec, value type or concrete key type.
    Collision,
    /// A require operation found no stored value, independently of cache state.
    Absent,
    /// The namespace's metadata lock was poisoned.
    Poisoned,
    /// Captured-value or reload-coordinator access failed.
    Access(SharedAccessError),
    /// The provider's original owned load or subscription failure.
    Provider(Cause),
}

/// Preserves a cache, value-access or original typed provider failure.
pub struct SharedKeyError<Cause> {
    /// Retains the typed cause while public formatting hides provider details.
    failure: Failure<Cause>,
}

impl<Cause> SharedKeyError<Cause> {
    /// Returns whether a live address has incompatible value, key or codec identity.
    #[must_use]
    pub const fn is_collision(&self) -> bool {
        matches!(self.failure, Failure::Collision)
    }

    /// Returns whether require found no stored value.
    #[must_use]
    pub const fn is_absent(&self) -> bool {
        matches!(self.failure, Failure::Absent)
    }

    /// Returns whether the cache metadata lock was poisoned.
    #[must_use]
    pub const fn is_poisoned(&self) -> bool {
        matches!(self.failure, Failure::Poisoned)
    }

    /// Returns a captured-value access error when one prevented the operation.
    #[must_use]
    pub const fn access_error(&self) -> Option<&SharedAccessError> {
        match &self.failure {
            Failure::Access(error) => Some(error),
            _ => None,
        }
    }

    /// Recovers the original provider cause without requiring domain traits.
    #[must_use]
    pub fn into_failure(self) -> Option<Cause> {
        match self.failure {
            Failure::Provider(cause) => Some(cause),
            _ => None,
        }
    }

    /// Supplies the same redacted text to Display and quoted Debug without allocating.
    const fn message(&self) -> &'static str {
        match &self.failure {
            Failure::Collision => "shared key has an incompatible live reference",
            Failure::Absent => "required shared value is absent from storage",
            Failure::Poisoned => "shared cache metadata is poisoned",
            Failure::Access(error) => error.message(),
            Failure::Provider(_) => "shared key provider failed",
        }
    }
}

impl<Cause> fmt::Debug for SharedKeyError<Cause> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SharedKeyError")
            .field("kind", &self.message())
            .finish_non_exhaustive()
    }
}

impl<Cause> fmt::Display for SharedKeyError<Cause> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.message())
    }
}

impl<Cause: std::error::Error + 'static> std::error::Error for SharedKeyError<Cause> {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match &self.failure {
            Failure::Access(error) => Some(error),
            Failure::Provider(cause) => Some(cause),
            _ => None,
        }
    }
}

impl<Cause> From<SharedAccessError> for SharedKeyError<Cause> {
    fn from(error: SharedAccessError) -> Self {
        Self {
            failure: Failure::Access(error),
        }
    }
}

#[derive(Clone, Eq, PartialEq)]
/// Stores a provider identity for admission and later full reload checks.
struct Compatibility {
    /// Immutable resource address, codec and configuration supplied outside locks.
    identity: SharedKeyIdentity,
}

impl Compatibility {
    /// Calls provider identity code before entering the namespace lock.
    fn for_key<Key: SharedReaderKey>(key: &Key) -> Self {
        Self {
            identity: key.identity(),
        }
    }
}

/// Admits compatible lookups without keeping their values or subscriptions alive.
struct Entry {
    /// Detects incompatible codecs after the map key established address equality.
    codec: TypeId,
    /// Retains compact codec settings; the map key already owns the resource address.
    configuration: Box<str>,
    /// Erases the value type while retaining only weak allocation identity.
    reference: Weak<dyn Any + Send + Sync>,
}

impl Entry {
    /// Compares remaining identity fields after lookup matched the resource address.
    fn matches_at_same_address(&self, other: &Compatibility) -> bool {
        self.codec == other.identity.codec
            && self.configuration.as_ref() == other.identity.configuration.as_str()
    }
}

/// Owns provider state for exactly one cached reference, not its writable cohorts.
struct KeyResource<Key: SharedReaderKey> {
    /// Retains the provider used by explicit reload after key replacement.
    key: Key,
    /// Binds later reloads to the original complete provider identity.
    compatibility: Compatibility,
    /// Rejects reload through a different namespace without retaining this cache.
    namespace: Weak<Mutex<HashMap<Box<str>, Entry>>>,
    /// Unregisters delivery when the final captured reference drops.
    _subscription: Key::Subscription,
    /// Holds only the latest request's weak identity; attempts own their tokens.
    loads: Arc<Mutex<Option<Weak<()>>>>,
}

/// Owns one injected heterogeneous namespace of weak typed value references.
#[derive(Clone, Default)]
pub struct SharedKeyCache {
    /// Shares weak lookup metadata between clones, never strong value ownership.
    entries: Arc<Mutex<HashMap<Box<str>, Entry>>>,
}

impl fmt::Debug for SharedKeyCache {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SharedKeyCache").finish_non_exhaustive()
    }
}

impl SharedKeyCache {
    /// Creates an empty independent namespace without provider work.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates an independent writable cohort over a cached or freshly seeded reference.
    ///
    /// # Errors
    /// Returns collision, metadata poison, or an original load/subscription failure.
    ///
    /// # Panics
    /// Propagates the same callback and destructor panics as [`Self::reader`].
    pub async fn shared<Key: SharedKey>(
        &self,
        key: Key,
        seed: impl FnOnce() -> Key::Value,
    ) -> Result<Shared<Key::Value>, SharedKeyError<Key::Error>> {
        Ok(cohort(self.reader(key, seed).await?))
    }

    /// Captures a cached reference or seeds, loads and subscribes a fresh owner.
    ///
    /// # Errors
    /// Returns collision, metadata poison, or an original load/subscription failure.
    ///
    /// # Panics
    /// Propagates seed, provider and discarded value/resource destructor panics.
    /// These callbacks run without the cache lock; publication may already have
    /// occurred before cleanup panics.
    pub async fn reader<Key: SharedReaderKey>(
        &self,
        key: Key,
        seed: impl FnOnce() -> Key::Value,
    ) -> Result<SharedReader<Key::Value>, SharedKeyError<Key::Error>> {
        let compatibility = Compatibility::for_key(&key);
        if let Some(reference) = self.lookup::<Key>(&compatibility)? {
            let reader = SharedReader { reference };
            resource::<Key>(&reader)?;
            drop(compatibility);
            drop(seed);
            drop(key);
            return Ok(reader);
        }
        let seed = seed();
        let loaded = match key.load(SharedLoadContext::Initial(&seed)).await {
            Ok(loaded) => loaded,
            Err(cause) => {
                let error = provider(cause);
                drop(seed);
                drop(compatibility);
                drop(key);
                return Err(error);
            }
        };
        let value = match loaded {
            Some(value) => {
                drop(seed);
                value
            }
            None => seed,
        };
        let reference = build_reference(
            key,
            compatibility.clone(),
            Arc::downgrade(&self.entries),
            value,
            SharedSubscribeContext::Initial,
        )?;
        self.publish::<Key>(compatibility, reference)
    }

    /// Constructs with a default closure while preserving the base key identity.
    ///
    /// # Errors
    /// Returns the same failures as [`Self::shared`].
    ///
    /// # Panics
    /// Propagates the same callback and destructor panics as [`Self::reader`].
    pub async fn shared_default<Key: SharedKey, Seed: Fn() -> Key::Value>(
        &self,
        default: SharedKeyDefault<Key, Seed>,
    ) -> Result<Shared<Key::Value>, SharedKeyError<Key::Error>> {
        self.shared(default.key, default.seed).await
    }

    /// Constructs a reader without adding the default wrapper to cache identity.
    ///
    /// # Errors
    /// Returns the same failures as [`Self::reader`].
    ///
    /// # Panics
    /// Propagates the same callback and destructor panics as [`Self::reader`].
    pub async fn reader_default<Key: SharedReaderKey, Seed: Fn() -> Key::Value>(
        &self,
        default: SharedKeyDefault<Key, Seed>,
    ) -> Result<SharedReader<Key::Value>, SharedKeyError<Key::Error>> {
        self.reader(default.key, default.seed).await
    }

    /// Requires stored data before creating or reusing a writable reference.
    ///
    /// Use [`SharedKeyDefault::into_key`] to require a default key without using
    /// its seed. A live cached value may win over the newly loaded value.
    ///
    /// # Errors
    /// Returns absence, collision, poison, or original load/subscription failure.
    ///
    /// # Panics
    /// Propagates the same provider and destructor panics as [`Self::require_reader`].
    pub async fn require<Key: SharedKey>(
        &self,
        key: Key,
    ) -> Result<Shared<Key::Value>, SharedKeyError<Key::Error>> {
        Ok(cohort(self.require_reader(key).await?))
    }

    /// Requires stored data before looking up a read-only reference.
    ///
    /// # Errors
    /// Returns absence even with a live cache hit, or collision/poison/provider failure.
    ///
    /// # Panics
    /// Propagates provider and discarded value/resource destructor panics. Cleanup
    /// completes before a selected reader or original provider cause is returned.
    pub async fn require_reader<Key: SharedReaderKey>(
        &self,
        key: Key,
    ) -> Result<SharedReader<Key::Value>, SharedKeyError<Key::Error>> {
        let loaded = match key.load(SharedLoadContext::UserInitiated).await {
            Ok(loaded) => loaded,
            Err(cause) => {
                let error = provider(cause);
                drop(key);
                return Err(error);
            }
        }
        .ok_or(SharedKeyError {
            failure: Failure::Absent,
        })?;
        let compatibility = Compatibility::for_key(&key);
        if let Some(reference) = self.lookup::<Key>(&compatibility)? {
            let reader = SharedReader { reference };
            resource::<Key>(&reader)?;
            drop(compatibility);
            drop(loaded);
            drop(key);
            return Ok(reader);
        }
        let reference = build_reference(
            key,
            compatibility.clone(),
            Arc::downgrade(&self.entries),
            loaded,
            SharedSubscribeContext::UserInitiated,
        )?;
        self.publish::<Key>(compatibility, reference)
    }

    /// Reloads a captured writable reference, retaining its current value on absence.
    ///
    /// Concurrent rebinding does not retarget this operation after capture.
    /// Only the latest started reload on that reference may replace its value;
    /// completion, failure or cancellation never revives an older request.
    ///
    /// # Errors
    /// Returns incompatible key, access failure, or original provider failure.
    ///
    /// # Panics
    /// Propagates provider and displaced-value cleanup panics. Releasing the
    /// captured reference can also run a panicking destructor before return.
    pub async fn reload<Key: SharedReaderKey>(
        &self,
        shared: &Shared<Key::Value>,
        key: &Key,
    ) -> Result<(), SharedKeyError<Key::Error>> {
        let reader = shared.reader()?;
        let result = self.reload_reader(&reader, key).await;
        drop(reader);
        result
    }

    /// Reloads a read-only captured reference without granting mutation access.
    ///
    /// # Errors
    /// Returns incompatible key, value-access failure, or original provider failure.
    ///
    /// # Panics
    /// Propagates provider and displaced or rejected value destructor panics.
    pub async fn reload_reader<Key: SharedReaderKey>(
        &self,
        reader: &SharedReader<Key::Value>,
        key: &Key,
    ) -> Result<(), SharedKeyError<Key::Error>> {
        let resource = resource::<Key>(reader)?;
        if !std::ptr::eq(resource.namespace.as_ptr(), Arc::as_ptr(&self.entries))
            || resource.compatibility != Compatibility::for_key(key)
        {
            return Err(collision());
        }
        reload_reference(reader, key, &resource.loads).await
    }

    /// Rebinds to a key before reloading, seeding fresh owners from current data.
    ///
    /// `seed` runs synchronously only on a target-cache miss and supplies an
    /// owned current-value snapshot without requiring `Clone`. The new owner
    /// skips initial load and subscribes with user intent. Reload failure does
    /// not roll the binding back. Unwrap default keys with `into_key`.
    ///
    /// # Errors
    /// Returns collision, cache/value access, subscription or explicit-load failure.
    ///
    /// # Panics
    /// A seed callback panic poisons the captured source value. Provider or
    /// displaced value/resource destructor panics also propagate. The receiving
    /// cohort may already be rebound and is not rolled back.
    pub async fn load<Key: SharedKey>(
        &self,
        shared: &Shared<Key::Value>,
        key: Key,
        seed: impl FnOnce(&Key::Value) -> Key::Value,
    ) -> Result<(), SharedKeyError<Key::Error>> {
        let reader = self.replacement(&shared.reader()?, key, seed)?;
        let resource = resource::<Key>(&reader)?;
        shared.rebind(&cohort(reader.clone()))?;
        let result = reload_reference(&reader, &resource.key, &resource.loads).await;
        drop(reader);
        result
    }

    /// Replaces this reader's reference before explicitly reloading the selected key.
    ///
    /// Other reader clones retain their previously captured reference.
    ///
    /// # Errors
    /// Returns collision, cache/value access, subscription or explicit-load failure.
    ///
    /// # Panics
    /// A seed callback panic poisons the captured source value. Provider or
    /// displaced value/resource destructor panics also propagate. This reader
    /// may already be replaced and is not rolled back.
    pub async fn load_reader<Key: SharedReaderKey>(
        &self,
        reader: &mut SharedReader<Key::Value>,
        key: Key,
        seed: impl FnOnce(&Key::Value) -> Key::Value,
    ) -> Result<(), SharedKeyError<Key::Error>> {
        let replacement = self.replacement(reader, key, seed)?;
        let previous = std::mem::replace(reader, replacement);
        let resource = match resource::<Key>(reader) {
            Ok(resource) => resource,
            Err(error) => {
                drop(std::mem::replace(reader, previous));
                return Err(error);
            }
        };
        drop(previous);
        reload_reference(reader, &resource.key, &resource.loads).await
    }

    /// Clears weak lookup entries without revoking owners or touching storage.
    ///
    /// In-flight constructors may publish again after this reset.
    ///
    /// # Errors
    /// Returns metadata poison if another operation panicked while holding its lock.
    pub fn reset(&self) -> Result<(), SharedKeyError<Infallible>> {
        let previous = {
            let mut entries = self.entries.lock().map_err(|_error| poisoned())?;
            std::mem::take(&mut *entries)
        };
        drop(previous);
        Ok(())
    }

    /// Captures a replacement without rebinding the caller or performing a load.
    ///
    /// A miss snapshots the current value and subscribes with user intent. The
    /// caller validates the concrete retained resource before changing its handle.
    fn replacement<Key: SharedReaderKey>(
        &self,
        current: &SharedReader<Key::Value>,
        key: Key,
        seed: impl FnOnce(&Key::Value) -> Key::Value,
    ) -> Result<SharedReader<Key::Value>, SharedKeyError<Key::Error>> {
        let compatibility = Compatibility::for_key(&key);
        if let Some(reference) = self.lookup::<Key>(&compatibility)? {
            // The caller validates the concrete resource before committing its binding.
            let reader = SharedReader { reference };
            drop(compatibility);
            drop(seed);
            drop(key);
            return Ok(reader);
        }
        let value = current.read(seed)?;
        let reference = build_reference(
            key,
            compatibility.clone(),
            Arc::downgrade(&self.entries),
            value,
            SharedSubscribeContext::UserInitiated,
        )?;
        self.publish::<Key>(compatibility, reference)
    }

    /// Upgrades a compatible live entry, releasing the metadata lock before drops.
    #[expect(
        clippy::type_complexity,
        reason = "keeps the value type and provider failure coupled at this private boundary"
    )]
    fn lookup<Key: SharedReaderKey>(
        &self,
        compatibility: &Compatibility,
    ) -> Result<Option<Arc<SharedReference<Key::Value>>>, SharedKeyError<Key::Error>> {
        let found = {
            let entries = self.entries.lock().map_err(|_error| poisoned())?;
            entries
                .get(compatibility.identity.address.as_str())
                .map(|entry| {
                    // Compatibility contains only owned strings and type IDs, so
                    // comparing it runs no provider code. Carry the owner out
                    // even on collision: its final drop may reenter this cache.
                    (
                        entry.matches_at_same_address(compatibility),
                        entry.reference.upgrade(),
                    )
                })
        };
        match found {
            Some((compatible, Some(reference))) => {
                if !compatible {
                    return Err(collision());
                }
                reference
                    .downcast()
                    .map(Some)
                    .map_err(|_reference| collision())
            }
            _ => Ok(None),
        }
    }

    /// Selects the live winner after provider construction outside the cache lock.
    ///
    /// Losing candidates and displaced weak entries are destroyed after unlocking.
    fn publish<Key: SharedReaderKey>(
        &self,
        compatibility: Compatibility,
        candidate: Arc<SharedReference<Key::Value>>,
    ) -> Result<SharedReader<Key::Value>, SharedKeyError<Key::Error>> {
        let erased: Arc<dyn Any + Send + Sync> = Arc::clone(&candidate) as Arc<_>;
        let (winner, previous) = {
            let mut entries = self.entries.lock().map_err(|_error| poisoned())?;
            let winner = entries
                .get(compatibility.identity.address.as_str())
                .and_then(|entry| {
                    entry
                        .reference
                        .upgrade()
                        .map(|reference| (entry.matches_at_same_address(&compatibility), reference))
                });
            let previous = if winner.is_none() {
                entries.insert(
                    compatibility.identity.address.into_boxed_str(),
                    Entry {
                        codec: compatibility.identity.codec,
                        configuration: compatibility.identity.configuration.into_boxed_str(),
                        reference: Arc::downgrade(&erased),
                    },
                )
            } else {
                None
            };
            (winner, previous)
        };
        drop(previous);
        drop(erased);
        if let Some((compatible, winner)) = winner {
            if !compatible {
                return Err(collision());
            }
            let reader = SharedReader {
                reference: winner.downcast().map_err(|_reference| collision())?,
            };
            resource::<Key>(&reader)?;
            drop(candidate);
            return Ok(reader);
        }
        Ok(SharedReader {
            reference: candidate,
        })
    }
}

/// Creates an independent rebinding slot over an already captured reference.
fn cohort<Value>(reader: SharedReader<Value>) -> Shared<Value> {
    Shared {
        cohort: Arc::new(Mutex::new(reader)),
    }
}

/// Establishes weak delivery before subscribing, then retains the provider guard.
///
/// Synchronous subscription delivery may replace the seed. The one-time slot
/// keeps the provider's type recoverable without locking during later lookups.
fn build_reference<Key: SharedReaderKey>(
    key: Key,
    compatibility: Compatibility,
    namespace: Weak<Mutex<HashMap<Box<str>, Entry>>>,
    value: Key::Value,
    context: SharedSubscribeContext,
) -> Result<Arc<SharedReference<Key::Value>>, SharedKeyError<Key::Error>> {
    let reference = Arc::new(SharedReference::new(value));
    // Establish delivery arbitration before subscribe can synchronously yield.
    let loads = Arc::new(Mutex::new(None));
    let subscription = match key.subscribe(
        context,
        SharedSubscriber {
            reference: Arc::downgrade(&reference),
            loads: Arc::downgrade(&loads),
        },
    ) {
        Ok(subscription) => subscription,
        Err(cause) => {
            let error = provider(cause);
            drop(loads);
            drop(reference);
            drop(namespace);
            drop(compatibility);
            drop(key);
            return Err(error);
        }
    };
    // Construction owns the only setter; callbacks retain only weak routes.
    let installed = reference.retained.set(Box::new(KeyResource {
        key,
        compatibility,
        namespace,
        _subscription: subscription,
        loads,
    }));
    assert!(
        installed.is_ok(),
        "a new shared reference has no retained resource"
    );
    Ok(reference)
}

/// Checks the concrete provider type as well as the reference's already typed value.
fn resource<Key: SharedReaderKey>(
    reader: &SharedReader<Key::Value>,
) -> Result<&KeyResource<Key>, SharedKeyError<Key::Error>> {
    reader
        .reference
        .retained
        .get()
        .and_then(|resource| resource.downcast_ref::<KeyResource<Key>>())
        .ok_or_else(collision)
}

/// Starts one reload and commits only if no later load or delivery superseded it.
async fn reload_reference<Key: SharedReaderKey>(
    reader: &SharedReader<Key::Value>,
    key: &Key,
    loads: &Mutex<Option<Weak<()>>>,
) -> Result<(), SharedKeyError<Key::Error>> {
    let attempt = ReferenceLoadAttempt::begin(&reader.reference, loads)?;
    let loaded = key
        .load(SharedLoadContext::UserInitiated)
        .await
        .map_err(provider)?;
    attempt.commit(loaded)?;
    Ok(())
}

/// Owns one non-reusable request token and retires it on cancellation or failure.
struct ReferenceLoadAttempt<'a, Value> {
    /// Borrows the captured destination for the complete load attempt.
    reference: &'a SharedReference<Value>,
    /// Serialises token replacement with value commit and producer delivery.
    loads: &'a Mutex<Option<Weak<()>>>,
    /// Keeps this request's allocation identity unique while the attempt exists.
    identity: Arc<()>,
    /// Skips cancellation cleanup once commit has consumed this request's authority.
    retired: bool,
}

impl<'a, Value> ReferenceLoadAttempt<'a, Value> {
    /// Supersedes earlier attempts before starting provider work.
    fn begin(
        reference: &'a SharedReference<Value>,
        loads: &'a Mutex<Option<Weak<()>>>,
    ) -> Result<Self, SharedAccessError> {
        let identity = Arc::new(());
        let previous = {
            let mut latest = loads.lock().map_err(|_error| load_poisoned())?;
            latest.replace(Arc::downgrade(&identity))
        };
        drop(previous);
        Ok(Self {
            reference,
            loads,
            identity,
            retired: false,
        })
    }

    /// Compares request identity without reviving a completed or cancelled attempt.
    fn is_latest(&self, latest: Option<&Weak<()>>) -> bool {
        // Both owners keep their allocation identities unique; comparison needs
        // no temporary Weak increment/decrement or upgrade of a retired request.
        latest.is_some_and(|latest| std::ptr::eq(latest.as_ptr(), Arc::as_ptr(&self.identity)))
    }

    /// Applies only the latest result, dropping displaced or rejected values unlocked.
    ///
    /// Absence retires the latest request without replacing its current value.
    fn commit(mut self, value: Option<Value>) -> Result<bool, SharedAccessError> {
        let mut value = value;
        let (committed, displaced, previous) = {
            let mut latest = self.loads.lock().map_err(|_error| load_poisoned())?;
            if self.is_latest(latest.as_ref()) {
                // Validate and replace under one coordinator guard. Never
                // block on value access or drop user values under either lock.
                let displaced = if value.is_some() {
                    let mut current = try_lock(&self.reference.value)?;
                    value
                        .take()
                        .map(|value| std::mem::replace(&mut *current, value))
                } else {
                    None
                };
                (true, displaced, latest.take())
            } else {
                (false, None, None)
            }
        };
        self.retired = true;
        drop(previous);
        drop(displaced);
        drop(value);
        Ok(committed)
    }
}

impl<Value> Drop for ReferenceLoadAttempt<'_, Value> {
    fn drop(&mut self) {
        if self.retired {
            return;
        }
        // Drop cannot report poison and must not introduce a second panic.
        // A poisoned coordinator remains poisoned for subsequent operations.
        let previous = if let Ok(mut latest) = self.loads.lock() {
            if self.is_latest(latest.as_ref()) {
                latest.take()
            } else {
                None
            }
        } else {
            None
        };
        drop(previous);
    }
}

/// Classifies reload-coordinator poison as captured-reference access failure.
fn load_poisoned() -> SharedAccessError {
    SharedAccessError {
        kind: super::AccessKind::Poisoned,
    }
}

/// Preserves a provider cause without formatting or cloning it.
fn provider<Cause>(cause: Cause) -> SharedKeyError<Cause> {
    SharedKeyError {
        failure: Failure::Provider(cause),
    }
}

/// Reports incompatible live identity without exposing resource details.
fn collision<Cause>() -> SharedKeyError<Cause> {
    SharedKeyError {
        failure: Failure::Collision,
    }
}

/// Distinguishes namespace metadata poison from captured-reference access poison.
fn poisoned<Cause>() -> SharedKeyError<Cause> {
    SharedKeyError {
        failure: Failure::Poisoned,
    }
}

#[cfg(test)]
ordinary_test_items! {
    mod tests {
        use super::*;

        // Diagnostic formatting must neither require Debug on values nor disclose them.
        #[test]
        fn diagnostics_redact_resource_identity_and_non_debug_values() {
            struct Secret(&'static str);

            let identity = SharedKeyIdentity::new::<()>("private-address", "private-configuration");
            let reference = Arc::new(SharedReference::new(Secret("private-value")));
            let loads = Arc::new(Mutex::new(None));
            let subscriber = SharedSubscriber {
                reference: Arc::downgrade(&reference),
                loads: Arc::downgrade(&loads),
            };
            let publication = subscriber.prepare_publication().unwrap();
            let value = reference.value.lock().unwrap();
            let initial = format!("{:?}", SharedLoadContext::Initial(&*value));
            let explicit = format!("{:?}", SharedLoadContext::<Secret>::UserInitiated);
            assert_ne!(initial, explicit);
            for diagnostic in [
                format!("{identity:?}"),
                format!("{subscriber:?}"),
                format!("{publication:?}"),
                format!("{:?}", SharedKeyCache::new()),
                initial,
                explicit,
            ] {
                for secret in ["private-address", "private-configuration", value.0] {
                    assert!(!diagnostic.contains(secret));
                }
            }
        }

        // Public diagnostics redact provider details while the typed error chain preserves them.
        #[test]
        fn provider_error_source_preserves_the_cause_without_disclosing_it() {
            let error = provider(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "private-provider-path",
            ));
            assert!(!error.is_absent());
            assert!(!error.is_collision());
            assert!(!error.is_poisoned());
            assert!(error.access_error().is_none());
            assert!(!format!("{error}: {error:?}").contains("private-provider-path"));
            let source = std::error::Error::source(&error)
                .unwrap()
                .downcast_ref::<std::io::Error>()
                .unwrap();
            assert_eq!(source.kind(), std::io::ErrorKind::PermissionDenied);
            assert_eq!(source.to_string(), "private-provider-path");
            let recovered = error.into_failure().unwrap();
            assert_eq!(recovered.kind(), std::io::ErrorKind::PermissionDenied);
            assert_eq!(recovered.to_string(), "private-provider-path");
        }

        // Poison must reject all metadata operations without revoking captured owners.
        #[test]
        fn poisoned_metadata_rejects_lookup_publication_and_reset_without_revoking_owners() {
            let cache = SharedKeyCache::new();
            let seeds = std::cell::Cell::new(0);
            let seed = || {
                seeds.set(seeds.get() + 1);
                7
            };
            let owner = futures_executor::block_on(cache.reader(LayoutKey, seed)).unwrap();
            let compatibility = Compatibility::for_key(&LayoutKey);
            let candidate = build_reference(
                LayoutKey,
                compatibility.clone(),
                Arc::downgrade(&cache.entries),
                11,
                SharedSubscribeContext::UserInitiated,
            )
            .unwrap();
            let provisional = Arc::downgrade(&candidate);
            let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _guard = cache.entries.lock().unwrap();
                panic!("metadata interrupted");
            }));
            assert!(panic.is_err());
            let errors = [
                futures_executor::block_on(cache.reader(LayoutKey, seed)).unwrap_err(),
                cache
                    .publish::<LayoutKey>(compatibility, candidate)
                    .unwrap_err(),
                cache.reset().unwrap_err(),
            ];
            for error in errors {
                assert!(error.is_poisoned());
                assert!(!error.is_collision());
                assert!(!error.is_absent());
                assert!(error.access_error().is_none());
                assert!(std::error::Error::source(&error).is_none());
                assert!(error.to_string().contains("metadata is poisoned"));
                assert!(error.into_failure().is_none());
            }
            assert_eq!(seeds.get(), 1);
            assert!(provisional.upgrade().is_none());
            assert_eq!(owner.read(|value| *value).unwrap(), 7);
        }

        // Internal fault injection, not a publicly constructible cache state.
        #[test]
        fn mismatched_erased_entry_rejects_reader_without_seed_or_owner_leak() {
            let cache = SharedKeyCache::new();
            let compatibility = Compatibility::for_key(&LayoutKey);
            let owner = Arc::new(SharedReference::new("private-wrong-value"));
            let erased: Arc<dyn Any + Send + Sync> = Arc::clone(&owner) as Arc<_>;
            cache.entries.lock().unwrap().insert(
                compatibility.identity.address.clone().into_boxed_str(),
                Entry {
                    codec: compatibility.identity.codec,
                    configuration: compatibility.identity.configuration.clone().into_boxed_str(),
                    reference: Arc::downgrade(&erased),
                },
            );
            drop(erased);

            let error = futures_executor::block_on(cache.reader(LayoutKey, || {
                panic!("a mismatched live reference must not fall back to its seed")
            }))
            .unwrap_err();
            assert!(error.is_collision());
            assert_eq!(
                error.to_string(),
                "shared key has an incompatible live reference"
            );
            assert!(!format!("{error:?}").contains("private-wrong-value"));
            assert!(!error.is_poisoned());
            assert_eq!(Arc::strong_count(&owner), 1);
            assert_eq!(*owner.value.lock().unwrap(), "private-wrong-value");
            drop(cache);
            assert_eq!(Arc::strong_count(&owner), 1);
        }

        // A forged compatible identity must not turn a failed downcast into publication.
        #[test]
        fn mismatched_erased_winner_rejects_publication_and_drops_candidate_outside_lock() {
            use std::sync::atomic::{AtomicUsize, Ordering};

            struct CandidateDrop(SharedKeyCache, Arc<AtomicUsize>);
            impl Drop for CandidateDrop {
                fn drop(&mut self) {
                    assert!(
                        self.0.entries.try_lock().is_ok(),
                        "candidate drop holds metadata lock"
                    );
                    self.1.fetch_add(1, Ordering::SeqCst);
                }
            }

            let cache = SharedKeyCache::new();
            let compatibility = Compatibility::for_key(&LayoutKey);
            let owner = Arc::new(SharedReference::new("private-wrong-value"));
            let erased: Arc<dyn Any + Send + Sync> = Arc::clone(&owner) as Arc<_>;
            let retained = Arc::downgrade(&erased);
            cache.entries.lock().unwrap().insert(
                compatibility.identity.address.clone().into_boxed_str(),
                Entry {
                    codec: compatibility.identity.codec,
                    configuration: compatibility.identity.configuration.clone().into_boxed_str(),
                    reference: Weak::clone(&retained),
                },
            );
            drop(erased);
            let drops = Arc::new(AtomicUsize::new(0));
            let candidate = Arc::new(SharedReference::new(11_usize));
            candidate
                .retained
                .set(Box::new(CandidateDrop(cache.clone(), Arc::clone(&drops))))
                .unwrap_or_else(|_| panic!("fresh candidate has no retained resource"));
            let provisional = Arc::downgrade(&candidate);

            let error = cache
                .publish::<LayoutKey>(compatibility.clone(), candidate)
                .unwrap_err();
            assert!(error.is_collision());
            assert_eq!(
                error.to_string(),
                "shared key has an incompatible live reference"
            );
            assert!(!format!("{error:?}").contains("private-wrong-value"));
            assert_eq!(drops.load(Ordering::SeqCst), 1);
            assert!(provisional.upgrade().is_none());
            assert_eq!(Arc::strong_count(&owner), 1);
            let entries = cache.entries.lock().unwrap();
            assert_eq!(entries.len(), 1);
            assert!(Weak::ptr_eq(
                &entries[compatibility.identity.address.as_str()].reference,
                &retained
            ));
            drop(entries);
            assert_eq!(*owner.value.lock().unwrap(), "private-wrong-value");
            drop(cache);
            assert_eq!(drops.load(Ordering::SeqCst), 1);
            assert_eq!(Arc::strong_count(&drops), 1);
        }

        // A poisoned coordinator cannot accept a new load, commit or producer value.
        #[test]
        fn poisoned_reload_coordinator_preserves_values_and_does_not_panic_during_retirement() {
            let reference = Arc::new(SharedReference::new(7));
            let loads = Arc::new(Mutex::new(None));
            let subscriber = SharedSubscriber {
                reference: Arc::downgrade(&reference),
                loads: Arc::downgrade(&loads),
            };
            let publication = subscriber.prepare_publication().unwrap();
            let attempt = ReferenceLoadAttempt::begin(&reference, &loads).unwrap();
            let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _guard = loads.lock().unwrap();
                panic!("reload coordination interrupted");
            }));
            assert!(panic.is_err());
            assert!(attempt.commit(Some(11)).unwrap_err().is_poisoned());
            let error = ReferenceLoadAttempt::begin(&reference, &loads)
                .err()
                .unwrap();
            assert!(error.is_poisoned());
            let mut next = 13;
            assert!(
                publication
                    .try_replace(&mut next)
                    .unwrap_err()
                    .is_poisoned()
            );
            assert_eq!(next, 13);
            assert_eq!(*reference.value.lock().unwrap(), 7);
            assert!(loads.is_poisoned());
        }

        // Coordinator poison must reach callers before provider I/O, without leaking owners.
        #[test]
        fn reload_propagates_coordinator_poison_before_invoking_provider() {
            use std::sync::atomic::{AtomicUsize, Ordering};

            #[derive(Clone)]
            struct Key {
                loads: Arc<AtomicUsize>,
                retirements: Arc<AtomicUsize>,
            }
            struct Subscription(Arc<AtomicUsize>);

            impl Drop for Subscription {
                fn drop(&mut self) {
                    self.0.fetch_add(1, Ordering::SeqCst);
                }
            }

            impl SharedReaderKey for Key {
                type Value = usize;
                type Error = Infallible;
                type Subscription = Subscription;

                fn identity(&self) -> SharedKeyIdentity {
                    SharedKeyIdentity::new::<Self>("poisoned-reload", "")
                }

                fn load(
                    &self,
                    _: SharedLoadContext<'_, usize>,
                ) -> impl Future<Output = Result<Option<usize>, Infallible>> + Send {
                    self.loads.fetch_add(1, Ordering::SeqCst);
                    std::future::ready(Ok(None))
                }

                fn subscribe(
                    &self,
                    _: SharedSubscribeContext,
                    _: SharedSubscriber<usize>,
                ) -> Result<Subscription, Infallible> {
                    Ok(Subscription(Arc::clone(&self.retirements)))
                }
            }

            let key = Key {
                loads: Arc::new(AtomicUsize::new(0)),
                retirements: Arc::new(AtomicUsize::new(0)),
            };
            let cache = SharedKeyCache::new();
            let reader = futures_executor::block_on(cache.reader(key.clone(), || 7)).unwrap();
            let owner = Arc::downgrade(&reader.reference);
            let loads = Arc::clone(&resource::<Key>(&reader).unwrap().loads);
            assert_eq!(key.loads.load(Ordering::SeqCst), 1);
            let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _guard = loads.lock().unwrap();
                panic!("reload coordination interrupted");
            }));
            assert!(panic.is_err());

            let error = futures_executor::block_on(cache.reload_reader(&reader, &key)).unwrap_err();
            assert!(error.access_error().unwrap().is_poisoned());
            assert!(!error.is_poisoned(), "not a metadata-lock failure");
            assert!(!error.is_collision());
            assert!(
                std::error::Error::source(&error)
                    .unwrap()
                    .downcast_ref::<SharedAccessError>()
                    .unwrap()
                    .is_poisoned()
            );
            assert_eq!(key.loads.load(Ordering::SeqCst), 1);
            assert_eq!(reader.read(|value| *value).unwrap(), 7);
            assert!(loads.is_poisoned());
            assert_eq!(key.retirements.load(Ordering::SeqCst), 0);
            drop(reader);
            assert!(owner.upgrade().is_none());
            assert_eq!(key.retirements.load(Ordering::SeqCst), 1);
        }

        // Capture contention must fail before a reload reaches the provider.
        #[test]
        fn reload_rejects_a_busy_cohort_without_changing_its_capture() {
            use std::sync::atomic::{AtomicUsize, Ordering};

            #[derive(Clone)]
            struct CountingKey(Arc<AtomicUsize>);

            impl SharedReaderKey for CountingKey {
                type Value = usize;
                type Error = Infallible;
                type Subscription = ();

                fn identity(&self) -> SharedKeyIdentity {
                    SharedKeyIdentity::new::<Self>("busy-cohort", "")
                }

                fn load(
                    &self,
                    _: SharedLoadContext<'_, usize>,
                ) -> impl Future<Output = Result<Option<usize>, Infallible>> + Send {
                    self.0.fetch_add(1, Ordering::SeqCst);
                    std::future::ready(Ok(None))
                }

                fn subscribe(
                    &self,
                    _: SharedSubscribeContext,
                    _: SharedSubscriber<usize>,
                ) -> Result<(), Infallible> {
                    Ok(())
                }
            }

            let key = CountingKey(Arc::new(AtomicUsize::new(0)));
            let cache = SharedKeyCache::new();
            let reader = futures_executor::block_on(cache.reader(key.clone(), || 7)).unwrap();
            assert_eq!(key.0.load(Ordering::SeqCst), 1);
            let shared = cohort(reader.clone());
            let guard = shared.cohort.lock().unwrap();
            let error = futures_executor::block_on(cache.reload(&shared, &key)).unwrap_err();
            assert!(error.access_error().unwrap().is_busy());
            assert_eq!(key.0.load(Ordering::SeqCst), 1);
            assert_eq!(reader.read(|value| *value).unwrap(), 7);
            drop(guard);
            futures_executor::block_on(cache.reload(&shared, &key)).unwrap();
            assert_eq!(key.0.load(Ordering::SeqCst), 2);
            assert_eq!(shared.read(|value| *value).unwrap(), 7);
        }

        // Private cohort guards stop capture or rebind without fabricating cache state.
        #[test]
        fn load_contention_retires_provisional_subscription_before_retry() {
            #[derive(Clone)]
            struct Key(Arc<Mutex<Vec<&'static str>>>);
            struct Subscription(Arc<Mutex<Vec<&'static str>>>);
            impl Drop for Subscription {
                fn drop(&mut self) {
                    self.0.lock().unwrap().push("retire");
                }
            }
            impl SharedReaderKey for Key {
                type Value = String;
                type Error = Infallible;
                type Subscription = Subscription;

                fn identity(&self) -> SharedKeyIdentity {
                    SharedKeyIdentity::new::<Self>("contended-load", "")
                }
                fn load(
                    &self,
                    context: SharedLoadContext<'_, String>,
                ) -> impl Future<Output = Result<Option<String>, Infallible>> + Send {
                    assert!(matches!(context, SharedLoadContext::UserInitiated));
                    self.0.lock().unwrap().push("reload");
                    std::future::ready(Ok(None))
                }
                fn subscribe(
                    &self,
                    context: SharedSubscribeContext,
                    _: SharedSubscriber<String>,
                ) -> Result<Subscription, Infallible> {
                    assert_eq!(context, SharedSubscribeContext::UserInitiated);
                    self.0.lock().unwrap().push("subscribe");
                    Ok(Subscription(Arc::clone(&self.0)))
                }
            }
            impl SharedKey for Key {
                fn save(&self, _: &String) -> impl Future<Output = Result<(), Infallible>> + Send {
                    self.0.lock().unwrap().push("save");
                    std::future::ready(Ok(()))
                }
            }

            let cache = SharedKeyCache::new();
            let key = Key(Arc::new(Mutex::new(Vec::new())));
            let shared = Shared::new(String::from("original"));
            let original = shared.reader().unwrap();
            let guard = shared.cohort.lock().unwrap();
            let error = futures_executor::block_on(cache.load(&shared, key.clone(), |_| {
                panic!("capture contention must not seed")
            }))
            .unwrap_err();
            assert!(error.access_error().unwrap().is_busy());
            assert!(error.into_failure().is_none());
            assert!(key.0.lock().unwrap().is_empty());
            drop(guard);

            let mut retained_guard = None;
            let error = futures_executor::block_on(cache.load(&shared, key.clone(), |value| {
                assert_eq!(value, "original");
                retained_guard = Some(shared.cohort.lock().unwrap());
                String::from("provisional")
            }))
            .unwrap_err();
            assert!(error.access_error().unwrap().is_busy());
            assert!(error.into_failure().is_none());
            assert_eq!(*key.0.lock().unwrap(), ["subscribe", "retire"]);
            drop(retained_guard);
            assert!(shared.read(|value| value == "original").unwrap());
            assert!(original.read(|value| value == "original").unwrap());

            futures_executor::block_on(cache.load(&shared, key.clone(), |value| {
                assert_eq!(value, "original");
                String::from("replacement")
            }))
            .unwrap();
            assert_eq!(
                *key.0.lock().unwrap(),
                ["subscribe", "retire", "subscribe", "reload"]
            );
            assert!(shared.read(|value| value == "replacement").unwrap());
            assert!(original.read(|value| value == "original").unwrap());
            drop(shared);
            assert_eq!(
                *key.0.lock().unwrap(),
                ["subscribe", "retire", "subscribe", "reload", "retire"]
            );
        }

        #[test]
        fn failed_subscription_orphan_cannot_prepare_without_its_coordinator() {
            struct Value(u8, Arc<Mutex<Vec<u8>>>);
            impl Drop for Value {
                fn drop(&mut self) {
                    self.1.lock().unwrap().push(self.0);
                }
            }
            struct Captured {
                subscriber: SharedSubscriber<Value>,
                publication: SharedPublication<Value>,
            }
            struct FailingSubscription(Arc<Mutex<Option<Captured>>>);
            impl SharedReaderKey for FailingSubscription {
                type Value = Value;
                type Error = &'static str;
                type Subscription = ();

                fn identity(&self) -> SharedKeyIdentity {
                    SharedKeyIdentity::new::<Self>("orphan", "")
                }

                fn load(
                    &self,
                    _: SharedLoadContext<'_, Value>,
                ) -> impl Future<Output = Result<Option<Value>, &'static str>> + Send {
                    std::future::ready(Ok(None))
                }

                fn subscribe(
                    &self,
                    context: SharedSubscribeContext,
                    subscriber: SharedSubscriber<Value>,
                ) -> Result<(), &'static str> {
                    assert!(matches!(context, SharedSubscribeContext::Initial));
                    let publication = subscriber.prepare_publication().unwrap();
                    assert!(
                        self.0
                            .lock()
                            .unwrap()
                            .replace(Captured {
                                subscriber,
                                publication,
                            })
                            .is_none()
                    );
                    Err("subscription failed")
                }
            }

            let cache = SharedKeyCache::new();
            let captured = Arc::new(Mutex::new(None));
            let drops = Arc::new(Mutex::new(Vec::new()));
            let error = futures_executor::block_on(
                cache.reader(FailingSubscription(Arc::clone(&captured)), || {
                    Value(1, Arc::clone(&drops))
                }),
            )
            .unwrap_err();
            assert_eq!(error.into_failure(), Some("subscription failed"));
            assert!(cache.entries.lock().unwrap().is_empty());
            assert!(drops.lock().unwrap().is_empty());
            let Captured {
                subscriber,
                publication,
            } = captured.lock().unwrap().take().unwrap();

            // Split a real provisional owner only at this private protocol boundary.
            // This is not a public scheduling proof or a fabricated weak identity.
            let SharedPublication { reference, loads } = publication;
            assert!(reference.retained.get().is_none());
            assert_eq!(Arc::strong_count(&reference), 1);
            assert_eq!(Arc::strong_count(&loads), 1);
            drop(loads);
            assert!(subscriber.loads.upgrade().is_none());
            assert_eq!(subscriber.reference.strong_count(), 1);

            assert!(subscriber.prepare_publication().is_none());
            assert_eq!(Arc::strong_count(&reference), 1);
            assert_eq!(reference.value.lock().unwrap().0, 1);
            assert!(drops.lock().unwrap().is_empty());
            assert!(
                !subscriber
                    .yield_value(Value(2, Arc::clone(&drops)))
                    .unwrap()
            );
            assert_eq!(*drops.lock().unwrap(), [2]);
            assert_eq!(reference.value.lock().unwrap().0, 1);
            assert_eq!(Arc::strong_count(&reference), 1);
            drop(reference);
            assert_eq!(*drops.lock().unwrap(), [2, 1]);
            assert!(subscriber.reference.upgrade().is_none());
            assert!(subscriber.prepare_publication().is_none());
            drop(subscriber);
            drop(cache);
            assert_eq!(*drops.lock().unwrap(), [2, 1]);
            assert_eq!(Arc::strong_count(&drops), 1);
        }

        #[test]
        fn subscriber_swap_retains_displaced_value_until_external_fence_is_released() {
            struct Reentrant(Arc<Mutex<bool>>, Arc<std::sync::atomic::AtomicUsize>);
            impl Drop for Reentrant {
                fn drop(&mut self) {
                    *self
                        .0
                        .try_lock()
                        .expect("destructor can reenter cancellation") = false;
                    self.1.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                }
            }
            let control = Arc::new(Mutex::new(true));
            let drops = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let reference = Arc::new(SharedReference::new(Reentrant(
                Arc::clone(&control),
                Arc::clone(&drops),
            )));
            let loads = Arc::new(Mutex::new(None));
            let subscriber = SharedSubscriber {
                reference: Arc::downgrade(&reference),
                loads: Arc::downgrade(&loads),
            };
            let mut next = Reentrant(Arc::clone(&control), Arc::clone(&drops));
            let publication = subscriber.prepare_publication().unwrap();
            drop(reference);
            let guard = control.lock().unwrap();
            assert!(publication.try_replace(&mut next).unwrap());
            drop(guard);
            drop(next);
            assert_eq!(drops.load(std::sync::atomic::Ordering::SeqCst), 1);
            drop(publication);
            assert_eq!(drops.load(std::sync::atomic::Ordering::SeqCst), 2);
            assert!(!*control.lock().unwrap());
            assert!(subscriber.prepare_publication().is_none());
        }

        #[test]
        fn prepared_publication_and_cancellation_have_one_final_commit_order() {
            let reference = Arc::new(SharedReference::new(7));
            let loads = Arc::new(Mutex::new(None));
            let subscriber = SharedSubscriber {
                reference: Arc::downgrade(&reference),
                loads: Arc::downgrade(&loads),
            };
            let publication = subscriber.prepare_publication().unwrap();
            let control = Arc::new(Mutex::new(true));
            let publishing = Arc::clone(&control);
            let cancelling = Arc::clone(&control);
            let (checked, checked_rx) = std::sync::mpsc::channel();
            let (resume, resume_rx) = std::sync::mpsc::channel();
            let (attempted, attempted_rx) = std::sync::mpsc::channel();
            let (cancelled, cancelled_rx) = std::sync::mpsc::channel();
            let worker = std::thread::spawn(move || {
                let mut next = 8;
                let fence = publishing.lock().unwrap();
                assert!(*fence);
                checked.send(()).unwrap();
                resume_rx.recv().unwrap(); // controlled pause immediately before swap
                assert!(publication.try_replace(&mut next).unwrap());
                drop(fence);
                assert_eq!(next, 7);
            });
            checked_rx.recv().unwrap();
            let cancellation_worker = std::thread::spawn(move || {
                attempted.send(()).unwrap();
                *cancelling.lock().unwrap() = false;
                cancelled.send(()).unwrap();
            });
            attempted_rx.recv().unwrap();
            assert_eq!(*reference.value.lock().unwrap(), 7);
            assert!(matches!(
                cancelled_rx.try_recv(),
                Err(std::sync::mpsc::TryRecvError::Empty)
            ));
            resume.send(()).unwrap();
            worker.join().unwrap();
            cancellation_worker.join().unwrap();
            cancelled_rx.recv().unwrap();
            assert_eq!(*reference.value.lock().unwrap(), 8);
            let publication = subscriber.prepare_publication().unwrap();
            let mut next = 99;
            {
                let fence = control.lock().unwrap();
                if *fence {
                    publication.try_replace(&mut next).unwrap();
                }
            }
            assert_eq!(next, 99);
            assert_eq!(*reference.value.lock().unwrap(), 8);
        }

        #[test]
        fn obsolete_attempts_cannot_clear_the_latest_loading_token() {
            let reference = SharedReference::new(1);
            let loads = Mutex::new(None);
            let old = ReferenceLoadAttempt::begin(&reference, &loads).unwrap();
            let latest = ReferenceLoadAttempt::begin(&reference, &loads).unwrap();
            assert!(!old.commit(Some(10)).unwrap());
            assert!(latest.is_latest(loads.lock().unwrap().as_ref()));
            assert!(latest.commit(Some(20)).unwrap());
            assert!(loads.lock().unwrap().is_none());
            assert_eq!(*reference.value.lock().unwrap(), 20);

            let old = ReferenceLoadAttempt::begin(&reference, &loads).unwrap();
            let latest = ReferenceLoadAttempt::begin(&reference, &loads).unwrap();
            drop(old);
            assert!(latest.is_latest(loads.lock().unwrap().as_ref()));
            drop(latest);
            assert!(loads.lock().unwrap().is_none());
        }

        #[test]
        fn latest_busy_commit_retires_without_replacing_or_poisoning() {
            let reference = SharedReference::new(1);
            let loads = Mutex::new(None);
            let attempt = ReferenceLoadAttempt::begin(&reference, &loads).unwrap();
            let current = reference.value.lock().unwrap();
            assert!(attempt.commit(Some(2)).unwrap_err().is_busy());
            assert!(loads.lock().unwrap().is_none());
            assert_eq!(*current, 1);
            drop(current);
            assert!(
                ReferenceLoadAttempt::begin(&reference, &loads)
                    .unwrap()
                    .commit(Some(3))
                    .unwrap()
            );
        }

        struct LayoutKey;

        impl SharedReaderKey for LayoutKey {
            type Value = usize;
            type Error = Infallible;
            type Subscription = ();

            fn identity(&self) -> SharedKeyIdentity {
                SharedKeyIdentity::new::<()>("layout", "")
            }

            fn load(
                &self,
                _: SharedLoadContext<'_, usize>,
            ) -> impl Future<Output = Result<Option<usize>, Infallible>> + Send {
                std::future::ready(Ok(None))
            }

            fn subscribe(
                &self,
                _: SharedSubscribeContext,
                _: SharedSubscriber<usize>,
            ) -> Result<(), Infallible> {
                Ok(())
            }
        }

        #[test]
        fn reports_retained_reference_layout() {
            // Adding keyed metadata to public handles would increase every capture.
            assert_eq!(size_of::<Shared<usize>>(), size_of::<Arc<()>>());
            assert_eq!(size_of::<SharedReader<usize>>(), size_of::<Arc<()>>());
            eprintln!(
                "Shared<usize>={} SharedReader<usize>={} old-value-mutex={} retained-reference={} keyed-resource={} coordinator={} attempt={} strong-token={} weak-token={} subscriber={}",
                size_of::<Shared<usize>>(),
                size_of::<SharedReader<usize>>(),
                size_of::<Mutex<usize>>(),
                size_of::<SharedReference<usize>>(),
                size_of::<KeyResource<LayoutKey>>(),
                size_of::<Mutex<Option<Weak<()>>>>(),
                size_of::<ReferenceLoadAttempt<'_, usize>>(),
                size_of::<Arc<()>>(),
                size_of::<Weak<()>>(),
                size_of::<SharedSubscriber<usize>>(),
            );
        }
    }
}
