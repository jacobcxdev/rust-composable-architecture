//! Adapts raw backend bytes and a codec to explicitly cached shared values.

use std::backtrace::{Backtrace, BacktraceStatus};
use std::error::Error;
use std::fmt;
use std::marker::PhantomData;

use rca_core::{
    SharedKey, SharedKeyIdentity, SharedLoadContext, SharedReaderKey, SharedSubscribeContext,
    SharedSubscriber,
};

use crate::{Backend, Codec};

/// A typed raw persistence key with caller-supplied resource identity.
///
/// The resource identifier must distinguish backing stores within a cache;
/// codec configuration must distinguish incompatible instances of a codec.
/// Clones preserve both identities. Missing storage remains absent and never
/// materialises a seed. Saving is explicit and uses the backend's guarantee.
/// This adapter has no watcher, document revision, flush barrier or runtime.
/// A save does not update existing cached values; reload them explicitly.
///
/// Saving encodes the borrowed value when [`SharedKey::save`] is called.
/// Backend saving starts only when the returned future is polled. Dropping an
/// unpolled future discards the encoded result without calling [`Backend::save`].
/// Once backend work starts, cancellation follows that backend's contract.
/// Loading starts when its future is polled and never writes a borrowed seed.
pub struct BackendKey<Value, Storage, Encoding> {
    /// Stable cache address and codec compatibility metadata.
    identity: SharedKeyIdentity,
    /// Owned provider borrowed by each load or save future.
    backend: Storage,
    /// Owned codec used before saving and after loading backend bytes.
    codec: Encoding,
    /// Records the value type without owning a value or imposing its auto traits.
    value: PhantomData<fn() -> Value>,
}

impl<Value, Storage, Encoding: 'static> BackendKey<Value, Storage, Encoding> {
    /// Binds backend and codec instances to explicit stable compatibility metadata.
    ///
    /// Cloned backend and codec instances must remain compatible with this metadata.
    #[must_use]
    pub fn new(
        resource: impl Into<String>,
        codec_configuration: impl Into<String>,
        backend: Storage,
        codec: Encoding,
    ) -> Self {
        Self {
            identity: SharedKeyIdentity::new::<Encoding>(resource, codec_configuration),
            backend,
            codec,
            value: PhantomData,
        }
    }
}

impl<Value, Storage: Clone, Encoding: Clone> Clone for BackendKey<Value, Storage, Encoding> {
    fn clone(&self) -> Self {
        Self {
            identity: self.identity.clone(),
            backend: self.backend.clone(),
            codec: self.codec.clone(),
            value: PhantomData,
        }
    }
}

impl<Value, Storage, Encoding> fmt::Debug for BackendKey<Value, Storage, Encoding> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BackendKey").finish_non_exhaustive()
    }
}

/// Retains exactly one original cause without a public exhaustive error enum.
enum Failure<Storage, Encoding> {
    /// Failure returned by backend loading or saving.
    Backend(
        /// Original backend cause, owned without another allocation.
        Storage,
    ),
    /// Failure returned by synchronous encoding or decoding.
    Codec(
        /// Original codec cause, owned without another allocation.
        Encoding,
    ),
}

/// Preserves an original typed backend or codec cause with opaque diagnostics.
///
/// Display includes a stack trace when Rust's backtrace settings enable capture.
/// Traces can contain function names and source paths, but never format the cause.
pub struct BackendKeyError<Storage, Encoding> {
    /// Original cause and the boundary that produced it.
    failure: Failure<Storage, Encoding>,
    /// Optional stack captured when the adapter reports the original cause.
    backtrace: Backtrace,
}

impl<Storage, Encoding> BackendKeyError<Storage, Encoding> {
    /// Captures a backend failure without boxing or replacing its concrete type.
    fn backend(error: Storage) -> Self {
        Self {
            failure: Failure::Backend(error),
            backtrace: Backtrace::capture(),
        }
    }

    /// Captures a codec failure without formatting potentially sensitive data.
    fn codec(error: Encoding) -> Self {
        Self {
            failure: Failure::Codec(error),
            backtrace: Backtrace::capture(),
        }
    }

    /// Returns the original backend cause, if the operation reached that boundary.
    ///
    /// Formatting this cause explicitly can reveal private backend details.
    #[must_use]
    pub const fn backend_error(&self) -> Option<&Storage> {
        match &self.failure {
            Failure::Backend(error) => Some(error),
            Failure::Codec(_) => None,
        }
    }

    /// Returns the original encoding or decoding cause.
    ///
    /// Formatting this cause explicitly can reveal document contents.
    #[must_use]
    pub const fn codec_error(&self) -> Option<&Encoding> {
        match &self.failure {
            Failure::Codec(error) => Some(error),
            Failure::Backend(_) => None,
        }
    }
}

impl<Storage, Encoding> fmt::Debug for BackendKeyError<Storage, Encoding> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BackendKeyError")
            .field(
                "boundary",
                &match self.failure {
                    Failure::Backend(_) => "backend",
                    Failure::Codec(_) => "codec",
                },
            )
            .finish_non_exhaustive()
    }
}

impl<Storage, Encoding> fmt::Display for BackendKeyError<Storage, Encoding> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self.failure {
            Failure::Backend(_) => "shared key backend failed",
            Failure::Codec(_) => "shared key codec failed",
        })?;
        if self.backtrace.status() == BacktraceStatus::Captured {
            write!(f, "\n{}", self.backtrace)?;
        }
        Ok(())
    }
}

impl<Storage: Error + 'static, Encoding: Error + 'static> Error
    for BackendKeyError<Storage, Encoding>
{
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match &self.failure {
            Failure::Backend(error) => Some(error),
            Failure::Codec(error) => Some(error),
        }
    }
}

impl<Value: Send + 'static, Storage: Backend, Encoding: Codec<Value>> SharedReaderKey
    for BackendKey<Value, Storage, Encoding>
{
    type Value = Value;
    type Error = BackendKeyError<Storage::Error, Encoding::Error>;
    type Subscription = ();

    fn identity(&self) -> SharedKeyIdentity {
        self.identity.clone()
    }

    #[expect(
        clippy::manual_async_fn,
        reason = "discard the borrowed seed before creating a Send future without requiring Value: Sync"
    )]
    fn load(
        &self,
        _: SharedLoadContext<'_, Value>,
    ) -> impl Future<Output = Result<Option<Value>, Self::Error>> + Send {
        async move {
            self.backend
                .load()
                .await
                .map_err(BackendKeyError::backend)?
                .map(|bytes| self.codec.decode(&bytes))
                .transpose()
                .map_err(BackendKeyError::codec)
        }
    }

    fn subscribe(
        &self,
        _: SharedSubscribeContext,
        _: SharedSubscriber<Value>,
    ) -> Result<(), Self::Error> {
        Ok(())
    }
}

impl<Value: Send + 'static, Storage: Backend, Encoding: Codec<Value>> SharedKey
    for BackendKey<Value, Storage, Encoding>
{
    fn save(&self, value: &Value) -> impl Future<Output = Result<(), Self::Error>> + Send {
        // Encode before constructing the future: it owns bytes, not an &Value
        // crossing suspension, so a Send-only value does not acquire Sync.
        let encoded = self.codec.encode(value);
        async move {
            let bytes = encoded.map_err(BackendKeyError::codec)?;
            self.backend
                .save(bytes)
                .await
                .map_err(BackendKeyError::backend)
        }
    }
}

#[cfg(all(test, feature = "file"))]
pub(crate) mod tests {
    use super::{
        Backend, BackendKey, BackendKeyError, Codec, Error, SharedKey, SharedLoadContext,
        SharedReaderKey, fmt,
    };

    /// Selects a backend or codec failure without allocating fixture payloads.
    struct Probe(bool);

    impl Backend for Probe {
        type Error = fmt::Error;

        fn load(&self) -> impl Future<Output = Result<Option<Vec<u8>>, Self::Error>> + Send {
            std::future::ready(if self.0 {
                Err(fmt::Error)
            } else {
                Ok(Some(Vec::new()))
            })
        }

        fn save(&self, _: Vec<u8>) -> impl Future<Output = Result<(), Self::Error>> + Send {
            std::future::ready(if self.0 { Err(fmt::Error) } else { Ok(()) })
        }
    }

    impl Codec<i32> for Probe {
        type Error = fmt::Error;

        fn encode(&self, _: &i32) -> Result<Vec<u8>, Self::Error> {
            if self.0 {
                Err(fmt::Error)
            } else {
                Ok(Vec::new())
            }
        }

        fn decode(&self, _: &[u8]) -> Result<i32, Self::Error> {
            if self.0 { Err(fmt::Error) } else { Ok(7) }
        }
    }

    /// Creates independently failing boundaries with the same zero-sized error type.
    fn key(backend_failure: bool, codec_failure: bool) -> BackendKey<i32, Probe, Probe> {
        BackendKey::new(
            "shared-key-probe",
            "v1",
            Probe(backend_failure),
            Probe(codec_failure),
        )
    }

    /// Measures real public futures inside the existing isolated allocation process.
    pub(crate) fn measure_error_allocations() {
        type Error = BackendKeyError<fmt::Error, fmt::Error>;

        for (backend_failure, codec_failure) in [(true, false), (false, true)] {
            let key = key(backend_failure, codec_failure);
            // Warm the executor and optional environment lookup outside the window.
            drop(futures_executor::block_on(
                key.load(SharedLoadContext::UserInitiated),
            ));
            let before = dhat::HeapStats::get();
            let load = key.load(SharedLoadContext::UserInitiated);
            let load_size = std::mem::size_of_val(&load);
            let loaded = futures_executor::block_on(load).unwrap_err();
            let save = key.save(&7);
            let save_size = std::mem::size_of_val(&save);
            let saved = futures_executor::block_on(save).unwrap_err();
            let retained = dhat::HeapStats::get();
            assert_eq!(retained.total_blocks, before.total_blocks);
            drop(loaded);
            drop(saved);
            let released = dhat::HeapStats::get();
            assert_eq!(released.curr_blocks, before.curr_blocks);
            assert_eq!(released.curr_bytes, before.curr_bytes);
            println!(
                "shared key backend_failure={backend_failure} error_size={} load_result_size={} save_result_size={} load_future_size={load_size} save_future_size={save_size} operation_allocations={}",
                std::mem::size_of::<Error>(),
                std::mem::size_of::<Result<Option<i32>, Error>>(),
                std::mem::size_of::<Result<(), Error>>(),
                retained.total_blocks - before.total_blocks,
            );
        }
    }

    /// Checks both public operations under fresh-process backtrace settings.
    pub(crate) fn check_error_backtraces(enabled: bool) {
        for (backend_failure, codec_failure) in [(true, false), (false, true)] {
            let key = key(backend_failure, codec_failure);
            let errors = [
                futures_executor::block_on(key.load(SharedLoadContext::UserInitiated)).unwrap_err(),
                futures_executor::block_on(key.save(&7)).unwrap_err(),
            ];
            for error in errors {
                let summary = if backend_failure {
                    "shared key backend failed"
                } else {
                    "shared key codec failed"
                };
                let displayed = error.to_string();
                crate::document::tests::assert_display_rejects_writes(&error, summary, enabled);
                assert!(displayed.starts_with(summary));
                let cause = error
                    .backend_error()
                    .or_else(|| error.codec_error())
                    .unwrap();
                assert_eq!(error.backend_error().is_some(), backend_failure);
                assert_eq!(error.codec_error().is_some(), codec_failure);
                assert!(std::ptr::eq(
                    error
                        .source()
                        .unwrap()
                        .downcast_ref::<fmt::Error>()
                        .unwrap(),
                    cause
                ));
                assert!(!displayed.contains(&cause.to_string()));
                assert!(!format!("{error:?}").contains(&cause.to_string()));
                if enabled {
                    assert!(
                        displayed.lines().count() > 1,
                        "enabled shared key diagnostics must include the captured stack"
                    );
                } else {
                    assert_eq!(displayed, summary);
                }
            }
        }
    }
}
