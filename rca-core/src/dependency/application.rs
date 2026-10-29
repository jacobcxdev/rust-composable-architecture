//! Supplies explicitly owned application values, independent of runtime scheduling.
//!
//! Inject these clients through an application-defined dependency key. Each constructor
//! owns its endpoint; cloning a client shares that endpoint, never restarts it.
//! Construct another client to choose an independent stream. Application IDs may
//! repeat and contain no runtime lifecycle generation. Calendar and locale data
//! remain caller-defined; this module performs no detection, parsing or ambient I/O.
//!
//! # Examples
//!
//! ```
//! use rca_core::{ApplicationIds, DependencyKey, DependencyValues};
//! struct Ids;
//! impl DependencyKey for Ids {
//!     type Value = ApplicationIds<u64>;
//! }
//! let mut dependencies = DependencyValues::new();
//! dependencies.set::<Ids>(ApplicationIds::sequence([10, 10, 20]));
//! assert_eq!(dependencies.get_or_insert::<Ids>().next(), Some(10));
//! assert_eq!(dependencies.get_or_insert::<Ids>().next(), Some(10));
//! ```

use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

/// Produces caller-defined application IDs without lifecycle-generation requirements.
pub struct ApplicationIds<Id> {
    /// Shares one caller-defined source without requiring IDs themselves to be shared.
    next: Arc<dyn Fn() -> Option<Id> + Send + Sync>,
}

impl<Id> ApplicationIds<Id> {
    /// Owns an explicitly supplied ID endpoint.
    ///
    /// The endpoint decides repetition and exhaustion. Captures must be `Send + Sync`
    /// for dependency injection; emitted IDs need not implement those traits.
    #[must_use]
    pub fn new(next: impl Fn() -> Option<Id> + Send + Sync + 'static) -> Self {
        Self {
            next: Arc::new(next),
        }
    }

    /// Returns the next application ID from the shared endpoint.
    ///
    /// # Panics
    /// Propagates endpoint panics.
    #[must_use]
    pub fn next(&self) -> Option<Id> {
        (self.next)()
    }

    /// Repeats one explicitly supplied ID indefinitely.
    ///
    /// Only this recipe requires ID cloning. Repetition is intentional and does not
    /// weaken the store's independent lifecycle fencing.
    #[must_use]
    pub fn constant(id: Id) -> Self
    where
        Id: Clone + Send + Sync + 'static,
    {
        Self::new(move || Some(id.clone()))
    }

    /// Moves IDs from an owned sequence, permanently stopping at its first exhaustion.
    ///
    /// Clones share consumption. Reconstruct the sequence to restart it. IDs need
    /// not implement `Clone` or `Debug`; the captured iterator must be `Send`.
    /// The client owns the iterator and releases any retained state with its final
    /// alias. Exhaustion can release iterator state earlier. Iterator calls execute
    /// under a non-reentrant mutex. Concurrent callers must coordinate any required
    /// order. Recursively invoking this endpoint from its iterator can deadlock.
    ///
    /// # Panics
    /// Construction propagates [`IntoIterator::into_iter`] panics. Calls to
    /// [`Self::next`] propagate iterator panics and shared iterator lock poisoning.
    #[must_use]
    pub fn sequence<I: IntoIterator<Item = Id>>(ids: I) -> Self
    where
        I::IntoIter: Send + 'static,
    {
        let ids = Mutex::new(ids.into_iter().fuse());
        Self::new(move || ids.lock().expect("ID sequence lock poisoned").next())
    }
}

impl<Id> Clone for ApplicationIds<Id> {
    fn clone(&self) -> Self {
        Self {
            next: Arc::clone(&self.next),
        }
    }
}

impl<Id> fmt::Debug for ApplicationIds<Id> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ApplicationIds").finish_non_exhaustive()
    }
}

/// Erases the byte endpoint while retaining its concrete error type.
type Fill<E> = dyn Fn(&mut [u8]) -> Result<(), E> + Send + Sync;

/// Fills random bytes through an explicitly supplied, typed fallible endpoint.
pub struct RandomBytes<E> {
    /// Shares the endpoint, not a precomputed byte stream or an error value.
    fill: Arc<Fill<E>>,
}

impl<E> RandomBytes<E> {
    /// Owns a caller-supplied byte endpoint without acquiring ambient entropy.
    #[must_use]
    pub fn new(fill: impl Fn(&mut [u8]) -> Result<(), E> + Send + Sync + 'static) -> Self {
        Self {
            fill: Arc::new(fill),
        }
    }

    /// Fills the supplied slice using the shared endpoint.
    ///
    /// # Errors
    /// Returns the endpoint's error unchanged. The endpoint may partially modify bytes.
    ///
    /// # Panics
    /// Propagates endpoint panics.
    pub fn fill(&self, bytes: &mut [u8]) -> Result<(), E> {
        (self.fill)(bytes)
    }

    /// Interprets eight filled bytes as a little-endian unsigned number.
    ///
    /// # Errors
    /// Returns the endpoint's error unchanged, without producing a number.
    ///
    /// # Panics
    /// Propagates endpoint panics.
    pub fn next_u64(&self) -> Result<u64, E> {
        let mut bytes = [0; 8];
        self.fill(&mut bytes)?;
        Ok(u64::from_le_bytes(bytes))
    }

    /// Owns explicit seed state and a caller-defined deterministic test generator.
    ///
    /// Clones share this state and generator. Reconstruct with the original seed to
    /// restart; replay requires a deterministic generator and the same call sequence.
    /// No algorithm, cryptographic quality or entropy acquisition is promised.
    /// Failed fills retain any generator state changes and partial byte writes.
    /// Generator calls execute under a non-reentrant mutex. Concurrent callers
    /// must coordinate any required order. Recursively invoking this endpoint
    /// from its generator can deadlock.
    ///
    /// # Panics
    /// Construction stores the generator without invoking it. Calls through the
    /// returned endpoint propagate generator panics and shared state lock poisoning.
    #[cfg(feature = "test-util")]
    #[must_use]
    pub fn seeded<State: Send + 'static>(
        seed: State,
        generate: impl FnMut(&mut State, &mut [u8]) -> Result<(), E> + Send + 'static,
    ) -> Self {
        let endpoint = Mutex::new((seed, generate));
        Self::new(move |bytes| {
            let mut endpoint = endpoint.lock().expect("random endpoint lock poisoned");
            let (state, generate) = &mut *endpoint;
            generate(state, bytes)
        })
    }
}

impl<E> Clone for RandomBytes<E> {
    fn clone(&self) -> Self {
        Self {
            fill: Arc::clone(&self.fill),
        }
    }
}

impl<E> fmt::Debug for RandomBytes<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RandomBytes").finish_non_exhaustive()
    }
}

/// Reads civil wall time without providing a monotonic scheduling clock.
///
/// Wall time can move backwards and must not schedule runtime timers.
///
/// ```compile_fail
/// use rca_core::{MonotonicInstant, WallTime};
/// use std::time::SystemTime;
/// let deadline: MonotonicInstant = WallTime::constant(SystemTime::UNIX_EPOCH).now();
/// ```
#[derive(Clone)]
pub struct WallTime {
    /// Shares an explicit wall-time source without granting runtime scheduling authority.
    now: Arc<dyn Fn() -> SystemTime + Send + Sync>,
}

impl WallTime {
    /// Owns a caller-supplied wall-time endpoint.
    #[must_use]
    pub fn new(now: impl Fn() -> SystemTime + Send + Sync + 'static) -> Self {
        Self { now: Arc::new(now) }
    }

    /// Repeats one explicitly supplied wall-time value.
    #[must_use]
    pub fn constant(time: SystemTime) -> Self {
        Self::new(move || time)
    }

    /// Reads the shared wall-time endpoint, independent of runtime deadlines.
    ///
    /// # Panics
    /// Propagates endpoint panics.
    #[must_use]
    pub fn now(&self) -> SystemTime {
        (self.now)()
    }
}

impl fmt::Debug for WallTime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WallTime").finish_non_exhaustive()
    }
}

/// Owns explicit caller-defined calendar and locale data without ambient detection.
pub struct CivilData<Calendar, Locale> {
    /// Owns the caller's calendar representation without interpreting it.
    calendar: Calendar,
    /// Owns the caller's locale representation without ambient detection.
    locale: Locale,
}

impl<Calendar, Locale> CivilData<Calendar, Locale> {
    /// Takes ownership of explicit calendar and locale data.
    #[must_use]
    pub fn new(calendar: Calendar, locale: Locale) -> Self {
        Self { calendar, locale }
    }

    /// Borrows the caller-defined calendar data.
    #[must_use]
    pub fn calendar(&self) -> &Calendar {
        &self.calendar
    }

    /// Borrows the caller-defined locale data.
    #[must_use]
    pub fn locale(&self) -> &Locale {
        &self.locale
    }
}

impl<Calendar, Locale> fmt::Debug for CivilData<Calendar, Locale> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CivilData").finish_non_exhaustive()
    }
}
