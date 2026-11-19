//! Checks cache identity, byte persistence and owned error delivery through public APIs.

use std::convert::Infallible;
use std::error::Error;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use futures_executor::block_on;
use rca_core::{SharedKey, SharedKeyCache, SharedKeyDefault, SharedLoadContext, SharedReaderKey};
use rca_persistence::{Backend, BackendKey, Codec};

#[derive(Clone)]
/// Shares stored bytes across independently retained backend aliases.
struct Memory(
    /// Distinguishes missing storage from an encoded value.
    Arc<Mutex<Option<Vec<u8>>>>,
);

impl Backend for Memory {
    type Error = Infallible;

    fn load(&self) -> impl Future<Output = Result<Option<Vec<u8>>, Infallible>> + Send {
        std::future::ready(Ok(self.0.lock().unwrap().clone()))
    }

    fn save(&self, bytes: Vec<u8>) -> impl Future<Output = Result<(), Infallible>> + Send {
        *self.0.lock().unwrap() = Some(bytes);
        std::future::ready(Ok(()))
    }
}

#[derive(Clone)]
/// Converts the decimal fixture values without adding backend behaviour.
struct Integer;

impl Codec<i32> for Integer {
    type Error = std::num::ParseIntError;

    fn encode(&self, value: &i32) -> Result<Vec<u8>, Self::Error> {
        Ok(value.to_string().into_bytes())
    }

    fn decode(&self, bytes: &[u8]) -> Result<i32, Self::Error> {
        String::from_utf8_lossy(bytes).parse()
    }
}

// A convenience cache disconnected from Backend/Codec would not observe saved bytes.
#[test]
fn real_backend_codec_round_trip_survives_last_owner_release() {
    let storage = Memory(Arc::new(Mutex::new(None)));
    let key = BackendKey::new("integer", "decimal-v1", storage.clone(), Integer);
    assert_eq!(format!("{key:?}"), "BackendKey { .. }");
    let cache = SharedKeyCache::new();
    let owner = block_on(cache.shared(key.clone(), || 7)).unwrap();
    assert_eq!(owner.read(|value| *value).unwrap(), 7);
    assert!(storage.0.lock().unwrap().is_none());
    block_on(key.save(&17)).unwrap();
    assert_eq!(owner.read(|value| *value).unwrap(), 7);
    drop(owner);
    let required = block_on(cache.require(key)).unwrap();
    assert_eq!(required.read(|value| *value).unwrap(), 17);
    assert_eq!(*storage.0.lock().unwrap(), Some(b"17".to_vec()));
}

// Requiring missing storage must not materialise a default or report success.
#[test]
fn requiring_a_default_base_does_not_write_missing_storage() {
    let storage = Memory(Arc::new(Mutex::new(None)));
    let key = BackendKey::new("integer", "decimal-v1", storage.clone(), Integer);
    let default = SharedKeyDefault::new(key, || panic!("require must not evaluate default"));
    let cache = SharedKeyCache::new();
    assert!(
        block_on(cache.require(default.into_key()))
            .unwrap_err()
            .is_absent()
    );
    assert!(storage.0.lock().unwrap().is_none());
}

// Treating decode failure as absence would silently select the seed instead.
#[test]
fn malformed_storage_preserves_the_original_codec_error() {
    let storage = Memory(Arc::new(Mutex::new(Some(b"invalid".to_vec()))));
    let key = BackendKey::new("integer", "decimal-v1", storage, Integer);
    let cache = SharedKeyCache::new();
    let error = block_on(cache.shared(key, || 7)).unwrap_err();
    let cause = error.into_failure().unwrap();
    assert_eq!(
        cause.codec_error().unwrap().kind(),
        &std::num::IntErrorKind::InvalidDigit
    );
    assert!(cause.backend_error().is_none());
}

// Same codec type with incompatible configuration must not reuse an owner.
#[test]
fn codec_configuration_collision_fails_without_changing_stored_bytes() {
    let storage = Memory(Arc::new(Mutex::new(Some(b"17".to_vec()))));
    let cache = SharedKeyCache::new();
    let first = BackendKey::new("integer", "decimal-v1", storage.clone(), Integer);
    let other = BackendKey::new("integer", "decimal-v2", storage.clone(), Integer);
    let _owner = block_on(cache.shared(first, || 7)).unwrap();
    assert!(
        block_on(cache.shared(other, || 9))
            .unwrap_err()
            .is_collision()
    );
    assert_eq!(*storage.0.lock().unwrap(), Some(b"17".to_vec()));
}

/// Proves that values need neither cloning nor shared-reference thread safety.
struct MoveOnly(
    /// Keeps the fixture `Send` without making it `Sync`.
    std::cell::Cell<u32>,
);

#[derive(Clone)]
/// Encodes the known four-byte move-only fixture.
struct MoveOnlyCodec;

impl Codec<MoveOnly> for MoveOnlyCodec {
    type Error = Infallible;

    fn encode(&self, value: &MoveOnly) -> Result<Vec<u8>, Infallible> {
        Ok(value.0.get().to_le_bytes().to_vec())
    }

    fn decode(&self, bytes: &[u8]) -> Result<MoveOnly, Infallible> {
        Ok(MoveOnly(std::cell::Cell::new(u32::from_le_bytes(
            bytes.try_into().unwrap(),
        ))))
    }
}

// Neither adapter cloning nor a Send save future may impose Clone/Sync/Debug on values.
#[test]
fn adapter_and_save_future_accept_send_only_move_only_values() {
    /// Requires the constructed future to support transfer between threads.
    fn assert_send(_: &impl Send) {}
    let storage = Memory(Arc::new(Mutex::new(None)));
    let key = BackendKey::new("move-only", "v1", storage, MoveOnlyCodec);
    let cloned = key.clone();
    let value = MoveOnly(std::cell::Cell::new(19));
    let load = cloned.load(SharedLoadContext::Initial(&value));
    assert_send(&load);
    assert!(block_on(load).unwrap().is_none());
    let save = cloned.save(&value);
    assert_send(&save);
    block_on(save).unwrap();
    let owner = block_on(SharedKeyCache::new().require(key)).unwrap();
    assert_eq!(owner.read(|value| value.0.get()).unwrap(), 19);
}

#[derive(Debug)]
/// Supplies recognisable private text for the diagnostic-redaction test.
struct BackendFailure;

impl std::fmt::Display for BackendFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("private backend detail")
    }
}

impl std::error::Error for BackendFailure {}

/// Rejects reads and writes with the same typed backend failure.
struct FailingBackend;

impl Backend for FailingBackend {
    type Error = BackendFailure;

    fn load(&self) -> impl Future<Output = Result<Option<Vec<u8>>, BackendFailure>> + Send {
        std::future::ready(Err(BackendFailure))
    }

    fn save(&self, _: Vec<u8>) -> impl Future<Output = Result<(), BackendFailure>> + Send {
        std::future::ready(Err(BackendFailure))
    }
}

#[test]
fn backend_failure_stays_typed_without_leaking_through_diagnostics() {
    let key = BackendKey::new("failure", "v1", FailingBackend, Integer);
    let error = block_on(SharedKeyCache::new().reader(key, || 7)).unwrap_err();
    assert!(!format!("{error:?} {error}").contains("private backend detail"));
    let cause = error.into_failure().unwrap();
    assert!(cause.backend_error().is_some());
    assert!(cause.codec_error().is_none());
    assert!(!format!("{cause:?} {cause}").contains("private backend detail"));
}

/// Keeps eager encoding and value destruction independently observable.
struct SaveValue {
    /// Can change after save construction to expose deferred encoding.
    value: std::cell::Cell<u32>,
    /// Counts destruction without retaining the value itself.
    drops: Arc<AtomicUsize>,
}

impl Drop for SaveValue {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}

/// Tracks the original owned error across encoding, delivery and destruction.
struct SaveFailure {
    /// Retains a stable address while the enclosing error moves.
    identity: Box<u8>,
    /// Counts destruction independently of the error's owner.
    drops: Arc<AtomicUsize>,
}

impl std::fmt::Debug for SaveFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("private save failure")
    }
}

impl std::fmt::Display for SaveFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("private save failure")
    }
}

impl Error for SaveFailure {}

impl Drop for SaveFailure {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}

/// Produces one owned encoding failure or encodes the current value.
struct SaveCodec(
    /// Transfers the configured failure exactly once.
    Mutex<Option<SaveFailure>>,
);

impl Codec<SaveValue> for SaveCodec {
    type Error = SaveFailure;

    fn encode(&self, value: &SaveValue) -> Result<Vec<u8>, Self::Error> {
        match self.0.lock().unwrap().take() {
            Some(error) => Err(error),
            None => Ok(value.value.get().to_le_bytes().to_vec()),
        }
    }

    fn decode(&self, _: &[u8]) -> Result<SaveValue, Self::Error> {
        panic!("save must not decode")
    }
}

/// Records admitted bytes before returning one owned backend failure.
struct SaveBackend {
    /// Transfers the configured failure when save work begins.
    error: Mutex<Option<SaveFailure>>,
    /// Records bytes independently of the codec and returned error.
    admitted: Arc<Mutex<Vec<Vec<u8>>>>,
}

impl Backend for SaveBackend {
    type Error = SaveFailure;

    fn load(&self) -> impl Future<Output = Result<Option<Vec<u8>>, Self::Error>> + Send {
        std::future::ready(Ok(None))
    }

    fn save(&self, bytes: Vec<u8>) -> impl Future<Output = Result<(), Self::Error>> + Send {
        self.admitted.lock().unwrap().push(bytes);
        std::future::ready(Err(self
            .error
            .lock()
            .unwrap()
            .take()
            .expect("save failure")))
    }
}

// Encoding failure must bypass backend admission and preserve the original owned cause.
#[test]
fn save_encode_failure_preserves_cause_without_admitting_backend_work() {
    let cause_drops = Arc::new(AtomicUsize::new(0));
    let value_drops = Arc::new(AtomicUsize::new(0));
    let cause = SaveFailure {
        identity: Box::new(71),
        drops: Arc::clone(&cause_drops),
    };
    let identity = std::ptr::from_ref(cause.identity.as_ref());
    let admitted = Arc::new(Mutex::new(Vec::new()));
    let key = BackendKey::new(
        "encode-failure",
        "v1",
        SaveBackend {
            error: Mutex::new(None),
            admitted: Arc::clone(&admitted),
        },
        SaveCodec(Mutex::new(Some(cause))),
    );
    let value = SaveValue {
        value: std::cell::Cell::new(0x0403_0201),
        drops: Arc::clone(&value_drops),
    };
    let error = block_on(key.save(&value)).unwrap_err();
    assert!(admitted.lock().unwrap().is_empty());
    assert!(error.backend_error().is_none());
    let cause = error.codec_error().unwrap();
    assert_eq!(std::ptr::from_ref(cause.identity.as_ref()), identity);
    assert!(std::ptr::eq(
        error
            .source()
            .unwrap()
            .downcast_ref::<SaveFailure>()
            .unwrap(),
        cause
    ));
    assert!(!format!("{error:?} {error}").contains("private save failure"));
    assert_eq!(cause_drops.load(Ordering::SeqCst), 0);
    assert_eq!(value_drops.load(Ordering::SeqCst), 0);
    drop(error);
    drop(value);
    drop(key);
    assert_eq!(cause_drops.load(Ordering::SeqCst), 1);
    assert_eq!(value_drops.load(Ordering::SeqCst), 1);
}

// Dropping an unpolled save must release its eagerly produced error without backend work.
#[test]
fn dropping_unpolled_save_releases_encode_failure_without_backend_admission() {
    let cause_drops = Arc::new(AtomicUsize::new(0));
    let value_drops = Arc::new(AtomicUsize::new(0));
    let admitted = Arc::new(Mutex::new(Vec::new()));
    let key = BackendKey::new(
        "unpolled-encode-failure",
        "v1",
        SaveBackend {
            error: Mutex::new(None),
            admitted: Arc::clone(&admitted),
        },
        SaveCodec(Mutex::new(Some(SaveFailure {
            identity: Box::new(71),
            drops: Arc::clone(&cause_drops),
        }))),
    );
    let value = SaveValue {
        value: std::cell::Cell::new(19),
        drops: Arc::clone(&value_drops),
    };
    let save = key.save(&value);
    assert_eq!(cause_drops.load(Ordering::SeqCst), 0);
    assert!(admitted.lock().unwrap().is_empty());

    drop(save);
    // Deferred encoding would leave the original error inside the still-live key.
    assert_eq!(cause_drops.load(Ordering::SeqCst), 1);
    assert_eq!(value_drops.load(Ordering::SeqCst), 0);
    assert!(admitted.lock().unwrap().is_empty());
    drop(key);
    drop(value);
    assert_eq!(cause_drops.load(Ordering::SeqCst), 1);
    assert_eq!(value_drops.load(Ordering::SeqCst), 1);
}

// Backend failure must retain the encoded payload and cannot be relabelled as codec failure.
#[test]
fn save_backend_failure_preserves_cause_and_exact_encoded_bytes() {
    /// Requires the constructed future to support transfer between threads.
    fn assert_send(_: &impl Send) {}
    let cause_drops = Arc::new(AtomicUsize::new(0));
    let value_drops = Arc::new(AtomicUsize::new(0));
    let cause = SaveFailure {
        identity: Box::new(73),
        drops: Arc::clone(&cause_drops),
    };
    let identity = std::ptr::from_ref(cause.identity.as_ref());
    let admitted = Arc::new(Mutex::new(Vec::new()));
    let key = BackendKey::new(
        "backend-failure",
        "v1",
        SaveBackend {
            error: Mutex::new(Some(cause)),
            admitted: Arc::clone(&admitted),
        },
        SaveCodec(Mutex::new(None)),
    );
    let value = SaveValue {
        value: std::cell::Cell::new(0x0403_0201),
        drops: Arc::clone(&value_drops),
    };
    let save = key.save(&value);
    assert_send(&save);
    value.value.set(0x0807_0605);
    assert!(admitted.lock().unwrap().is_empty());
    let error = block_on(save).unwrap_err();
    assert_eq!(*admitted.lock().unwrap(), [vec![1, 2, 3, 4]]);
    assert!(error.codec_error().is_none());
    let cause = error.backend_error().unwrap();
    assert_eq!(std::ptr::from_ref(cause.identity.as_ref()), identity);
    assert!(std::ptr::eq(
        error
            .source()
            .unwrap()
            .downcast_ref::<SaveFailure>()
            .unwrap(),
        cause
    ));
    assert!(!format!("{error:?} {error}").contains("private save failure"));
    assert_eq!(cause_drops.load(Ordering::SeqCst), 0);
    assert_eq!(value_drops.load(Ordering::SeqCst), 0);
    drop(error);
    drop(value);
    drop(key);
    assert_eq!(cause_drops.load(Ordering::SeqCst), 1);
    assert_eq!(value_drops.load(Ordering::SeqCst), 1);
}
