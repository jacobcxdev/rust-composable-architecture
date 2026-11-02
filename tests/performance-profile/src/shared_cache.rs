//! Direct, immediately-ready cache traces with explicit final-reference accounting.

use std::future::{Future, ready};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::task::{Context, Poll, Waker};

use rca_core::{
    Shared, SharedKey, SharedKeyCache, SharedKeyIdentity, SharedLoadContext, SharedReader,
    SharedReaderKey, SharedSubscribeContext, SharedSubscriber,
};

use crate::measurement::Measurement;

#[path = "shared_contention.rs"]
pub(crate) mod contention;

#[derive(Clone, Copy)]
/// Validated cache mode, work size and independently calculated result totals.
pub(crate) struct Config {
    name: &'static str,
    cohort: bool,
    hit: bool,
    size: u64,
    iterations: u64,
    useful: u64,
    checksum: u64,
}

impl Config {
    /// Rejects unsupported sizes, extra arguments and overflowing work totals.
    pub(crate) fn parse(mut args: impl Iterator<Item = String>) -> Result<Self, &'static str> {
        let (name, cohort, hit) = match args.next().as_deref() {
            Some("shared-reader-hit") => ("shared-reader-hit", false, true),
            Some("shared-cohort-hit") => ("shared-cohort-hit", true, true),
            Some("shared-reader-expired") => ("shared-reader-expired", false, false),
            Some("shared-cohort-expired") => ("shared-cohort-expired", true, false),
            _ => return Err("unknown shared-cache workload"),
        };
        let size = decimal(args.next())?;
        let iterations = decimal(args.next())?;
        if args.next().is_some() || iterations == 0 || !(size == 1 || hit && size == 64) {
            return Err(
                "shared hit SIZE must be 1 or 64; expired SIZE must be 1; ITERATIONS must be positive",
            );
        }
        let useful = size
            .checked_mul(iterations)
            .ok_or("shared count overflow")?;
        useful.checked_add(3).ok_or("shared count overflow")?;
        let checksum = if hit {
            useful.checked_mul(7)
        } else {
            iterations.checked_add(1).and_then(|next| {
                if iterations.is_multiple_of(2) {
                    (iterations / 2).checked_mul(next)
                } else {
                    iterations.checked_mul(next / 2)
                }
            })
        }
        .ok_or("shared checksum overflow")?;
        Ok(Self {
            name,
            cohort,
            hit,
            size,
            iterations,
            useful,
            checksum,
        })
    }
}

/// Accepts unsigned decimal digits only, without signs or surrounding spaces.
fn decimal(value: Option<String>) -> Result<u64, &'static str> {
    let value = value.ok_or("expected WORKLOAD SIZE ITERATIONS")?;
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err("expected unsigned decimal integer");
    }
    value.parse().map_err(|_| "unsigned decimal overflow")
}

// The fixed fields are diagnostic work, not private allocation/Arc-clone probes.
macro_rules! counters {
    ($($field:ident),+ $(,)?) => {
        /// Counts fixture-visible operations; these are not synchronisation flags.
        #[derive(Default)]
        struct AtomicCounts {
            $($field: AtomicU64),+
        }

        /// Copies cumulative operation totals at an acknowledged phase boundary.
        #[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
        struct Counts {
            $($field: u64),+
        }

        impl AtomicCounts {
            /// Samples counters after serial work or the concurrent done barrier.
            fn snapshot(&self) -> Counts {
                Counts {
                    $($field: self.$field.load(Ordering::Relaxed)),+
                }
            }
        }

        impl Counts {
            /// Subtracts an earlier sample from the same monotonically increasing counters.
            fn since(self, before: Self) -> Self {
                Self {
                    $($field: self.$field - before.$field),+
                }
            }

            /// Formats the fixed counter schema outside allocation measurement.
            fn json(self) -> String {
                let fields = [$(format!("\"{}\":{}", stringify!($field), self.$field)),+];
                format!("{{{}}}", fields.join(","))
            }
        }
    }
}

counters!(
    seed_calls,
    initial_loads,
    user_loads,
    initial_subscribes,
    user_subscribes,
    save_calls,
    provider_returned_values,
    values_created,
    values_dropped,
    subscriptions_created,
    subscriptions_dropped,
    successful_lookups,
    read_calls,
    capture_operations,
    reader_captures,
    cohort_constructions
);

#[derive(Default)]
/// Tracks the latest binding weakly; cumulative drop counts cover earlier values.
struct Slots {
    value: Weak<()>,
    subscription: Weak<()>,
    subscriber: Option<SharedSubscriber<Value>>,
}

#[derive(Default)]
/// Shares observations without retaining the value or subscription being checked.
struct Probe {
    counts: AtomicCounts,
    slots: Mutex<Slots>,
    #[cfg(test)]
    provider_tag: AtomicU64,
}

/// A move-owned tag with a separately witnessed lifetime and counted destruction.
struct Value {
    tag: u64,
    probe: Arc<Probe>,
    _witness: Arc<()>,
}

impl Value {
    /// Registers the newest weak witness before returning the owning value.
    fn new(probe: &Arc<Probe>, tag: u64) -> Self {
        let witness = Arc::new(());
        probe.slots.lock().unwrap().value = Arc::downgrade(&witness);
        probe.counts.values_created.fetch_add(1, Ordering::Relaxed);
        Self {
            tag,
            probe: Arc::clone(probe),
            _witness: witness,
        }
    }
}

impl Drop for Value {
    fn drop(&mut self) {
        self.probe
            .counts
            .values_dropped
            .fetch_add(1, Ordering::Relaxed);
    }
}

/// Retains the provider's weak publisher and records subscription destruction.
struct Subscription {
    probe: Arc<Probe>,
    _witness: Arc<()>,
    _subscriber: SharedSubscriber<Value>,
}

impl Drop for Subscription {
    fn drop(&mut self) {
        self.probe
            .counts
            .subscriptions_dropped
            .fetch_add(1, Ordering::Relaxed);
    }
}

/// Identifies the normal provider representation in the cache key.
struct Codec;
#[cfg(test)]
/// Makes the same address incompatible without changing its displayed version.
struct OtherCodec;

/// A move-owned provider cause that deliberately requires neither `Clone` nor `Debug`.
struct Cause(std::cell::Cell<u64>);

#[cfg(test)]
#[derive(Clone, Copy, Default)]
/// Selects real provider outcomes for correctness controls, not profiling rows.
enum Behavior {
    #[default]
    Absent,
    Present(u64),
    LoadFailure,
    SubscribeFailure,
    Pending,
}

/// An absent, immediately ready provider with test-only failure and value modes.
struct Key {
    probe: Arc<Probe>,
    address: &'static str,
    #[cfg(test)]
    behavior: Behavior,
    #[cfg(test)]
    other_codec: bool,
}

impl Key {
    /// Uses one shared address so later lookups must resolve the existing binding.
    fn new(probe: &Arc<Probe>) -> Self {
        Self {
            probe: Arc::clone(probe),
            address: "shared-profile",
            #[cfg(test)]
            behavior: Behavior::Absent,
            #[cfg(test)]
            other_codec: false,
        }
    }
}

impl SharedReaderKey for Key {
    type Value = Value;
    type Error = Cause;
    type Subscription = Subscription;

    fn identity(&self) -> SharedKeyIdentity {
        #[cfg(test)]
        if self.other_codec {
            return SharedKeyIdentity::new::<OtherCodec>(self.address, "v1");
        }
        SharedKeyIdentity::new::<Codec>(self.address, "v1")
    }

    fn load(
        &self,
        context: SharedLoadContext<'_, Value>,
    ) -> impl Future<Output = Result<Option<Value>, Cause>> + Send {
        match context {
            SharedLoadContext::Initial(_) => &self.probe.counts.initial_loads,
            SharedLoadContext::UserInitiated => &self.probe.counts.user_loads,
        }
        .fetch_add(1, Ordering::Relaxed);
        #[cfg(test)]
        let result = match self.behavior {
            Behavior::Present(_) => {
                self.probe
                    .counts
                    .provider_returned_values
                    .fetch_add(1, Ordering::Relaxed);
                Ok(Some(Value::new(
                    &self.probe,
                    self.probe.provider_tag.load(Ordering::Relaxed),
                )))
            }
            Behavior::LoadFailure => Err(Cause(std::cell::Cell::new(42))),
            _ => Ok(None),
        };
        #[cfg(not(test))]
        {
            ready(Ok(None))
        }
        #[cfg(test)]
        {
            let pending = matches!(self.behavior, Behavior::Pending);
            let mut result = Some(result);
            std::future::poll_fn(move |_| {
                if pending {
                    Poll::Pending
                } else {
                    Poll::Ready(result.take().expect("polled once"))
                }
            })
        }
    }

    fn subscribe(
        &self,
        context: SharedSubscribeContext,
        subscriber: SharedSubscriber<Value>,
    ) -> Result<Subscription, Cause> {
        match context {
            SharedSubscribeContext::Initial => &self.probe.counts.initial_subscribes,
            SharedSubscribeContext::UserInitiated => &self.probe.counts.user_subscribes,
        }
        .fetch_add(1, Ordering::Relaxed);
        #[cfg(test)]
        if matches!(self.behavior, Behavior::SubscribeFailure) {
            return Err(Cause(std::cell::Cell::new(43)));
        }
        let witness = Arc::new(());
        let mut slots = self.probe.slots.lock().unwrap();
        slots.subscription = Arc::downgrade(&witness);
        slots.subscriber = Some(subscriber.clone());
        self.probe
            .counts
            .subscriptions_created
            .fetch_add(1, Ordering::Relaxed);
        Ok(Subscription {
            probe: Arc::clone(&self.probe),
            _witness: witness,
            _subscriber: subscriber,
        })
    }
}

impl SharedKey for Key {
    fn save(&self, _: &Value) -> impl Future<Output = Result<(), Cause>> + Send {
        self.probe.counts.save_calls.fetch_add(1, Ordering::Relaxed);
        ready(Ok(()))
    }
}

/// Polls once and drops a pending operation; this is not a general executor.
fn poll_ready<F: Future>(future: F) -> Result<F::Output, &'static str> {
    let mut future = std::pin::pin!(future);
    match future
        .as_mut()
        .poll(&mut Context::from_waker(Waker::noop()))
    {
        Poll::Ready(result) => Ok(result),
        Poll::Pending => Err("direct cache fixture unexpectedly pending"),
    }
}

/// Counts actual fallback creation independently of cache lookup success.
fn seed(probe: &Arc<Probe>, tag: u64) -> Value {
    probe.counts.seed_calls.fetch_add(1, Ordering::Relaxed);
    Value::new(probe, tag)
}

/// Consumes the typed provider cause before returning the fixture diagnostic.
fn provider_error(error: rca_core::SharedKeyError<Cause>) -> &'static str {
    // Recover the original move-owned provider failure before dropping it.
    let _cause_tag = error.into_failure().map(|cause| cause.0.get());
    "shared provider failed"
}

/// Keeps reader ownership distinct from a writable rebinding cohort.
enum Owner {
    Reader(SharedReader<Value>),
    Cohort(Shared<Value>),
}

impl Owner {
    /// Reads through the actual owner and counts only successful accesses.
    fn read(&self, probe: &Probe) -> Result<u64, &'static str> {
        let tag = match self {
            Self::Reader(reader) => reader.read(|value| value.tag),
            Self::Cohort(shared) => shared.read(|value| value.tag),
        }
        .map_err(|_| "shared read failed")?;
        probe.counts.read_calls.fetch_add(1, Ordering::Relaxed);
        Ok(tag)
    }

    /// Retains the current reference independently of a writable cohort's rebinding.
    fn capture(&self, probe: &Probe) -> Result<SharedReader<Value>, &'static str> {
        let reader = match self {
            Self::Reader(reader) => reader.clone(),
            Self::Cohort(shared) => shared.reader().map_err(|_| "shared capture failed")?,
        };
        probe
            .counts
            .capture_operations
            .fetch_add(1, Ordering::Relaxed);
        probe.counts.reader_captures.fetch_add(1, Ordering::Relaxed);
        Ok(reader)
    }
}

/// Resolves one real cache binding and records its returned owner kind.
fn lookup(
    cache: &SharedKeyCache,
    probe: &Arc<Probe>,
    cohort: bool,
    tag: u64,
) -> Result<Owner, &'static str> {
    let owner = if cohort {
        let shared = poll_ready(cache.shared(Key::new(probe), || seed(probe, tag)))?
            .map_err(provider_error)?;
        probe
            .counts
            .cohort_constructions
            .fetch_add(1, Ordering::Relaxed);
        Owner::Cohort(shared)
    } else {
        Owner::Reader(
            poll_ready(cache.reader(Key::new(probe), || seed(probe, tag)))?
                .map_err(provider_error)?,
        )
    };
    probe
        .counts
        .successful_lookups
        .fetch_add(1, Ordering::Relaxed);
    Ok(owner)
}

/// Rejects missing and extra operations across the complete counter set.
fn expect_counts(actual: Counts, expected: Counts) -> Result<(), &'static str> {
    if actual == expected {
        Ok(())
    } else {
        Err("shared cache counter mismatch")
    }
}

/// Defines one absent-provider miss without reading the measured counters.
fn binding_counts(count: u64, cohort: bool) -> Counts {
    Counts {
        seed_calls: count,
        initial_loads: count,
        initial_subscribes: count,
        values_created: count,
        subscriptions_created: count,
        successful_lookups: count,
        cohort_constructions: if cohort { count } else { 0 },
        ..Counts::default()
    }
}

/// Requires balanced destruction, expired weak owners and a closed producer.
fn expect_expired(probe: &Probe) -> Result<(), &'static str> {
    let counts = probe.counts.snapshot();
    let slots = probe.slots.lock().unwrap();
    if counts.values_created != counts.values_dropped
        || counts.subscriptions_created != counts.subscriptions_dropped
        || slots.value.upgrade().is_some()
        || slots.subscription.upgrade().is_some()
        || slots
            .subscriber
            .as_ref()
            .is_some_and(|subscriber| subscriber.prepare_publication().is_some())
    {
        return Err("shared cache retained a value, subscription or producer owner");
    }
    Ok(())
}

/// Requires the latest value and subscription to remain independently reachable.
fn expect_live(probe: &Probe) -> Result<(), &'static str> {
    let slots = probe.slots.lock().unwrap();
    if slots.value.upgrade().is_none() || slots.subscription.upgrade().is_none() {
        return Err("shared cache released a captured owner early");
    }
    Ok(())
}

#[inline(never)]
/// Measures repeated hits while the cold binding remains owned.
fn profile_shared_cache_hits(
    cache: &SharedKeyCache,
    probe: &Arc<Probe>,
    config: Config,
) -> Result<(u64, u64), &'static str> {
    let mut checksum = 0;
    let mut last_tag = 0;
    for _ in 0..config.iterations {
        for _ in 0..config.size {
            let owner = lookup(cache, probe, config.cohort, 99)?;
            let tag = owner.read(probe)?;
            if tag != 7 {
                return Err("shared hit did not retain the cached value");
            }
            checksum += tag;
            last_tag = tag;
            drop(owner);
        }
    }
    Ok((checksum, last_tag))
}

#[inline(never)]
/// Measures fresh bindings, including each cycle's drop and expiry checks.
fn profile_shared_cache_expired(
    cache: &SharedKeyCache,
    probe: &Arc<Probe>,
    config: Config,
) -> Result<(u64, u64), &'static str> {
    let mut checksum = 0;
    let mut last_tag = 0;
    for tag in 1..=config.iterations {
        let owner = lookup(cache, probe, config.cohort, tag)?;
        let observed = owner.read(probe)?;
        if observed != tag {
            return Err("expired lookup reused a live value");
        }
        checksum += observed;
        last_tag = observed;
        drop(owner);
        let mut expected = binding_counts(tag + 1, config.cohort);
        expected.read_calls = tag + 1;
        expected.values_dropped = tag + 1;
        expected.subscriptions_dropped = tag + 1;
        expect_counts(probe.counts.snapshot(), expected)?;
        expect_expired(probe)?;
    }
    Ok((checksum, last_tag))
}

/// Establishes the first hit owner, or proves expiry before the first miss cycle.
fn cold_binding(
    cache: &SharedKeyCache,
    probe: &Arc<Probe>,
    config: Config,
) -> Result<(Option<Owner>, Counts), &'static str> {
    let cold_owner = lookup(cache, probe, config.cohort, if config.hit { 7 } else { 0 })?;
    if cold_owner.read(probe)? != if config.hit { 7 } else { 0 } {
        return Err("wrong cold value");
    }
    let base = if config.hit {
        Some(cold_owner)
    } else {
        drop(cold_owner);
        expect_expired(probe)?;
        None
    };
    let cold = probe.counts.snapshot();
    let mut expected_cold = binding_counts(1, config.cohort);
    expected_cold.read_calls = 1;
    expected_cold.values_dropped = u64::from(!config.hit);
    expected_cold.subscriptions_dropped = u64::from(!config.hit);
    expect_counts(cold, expected_cold)?;
    Ok((base, cold))
}

/// Checks phase totals and final-owner release before serialising a successful run.
pub(crate) fn run(config: Config) -> Result<String, &'static str> {
    let mut measurement = Measurement::new(None);
    let probe = Arc::new(Probe::default());
    let cache = SharedKeyCache::new();
    measurement.construction_finished();
    let (base, cold) = cold_binding(&cache, &probe, config)?;
    measurement.cold_finished();
    let (checksum, last_tag) = if config.hit {
        profile_shared_cache_hits(&cache, &probe, config)?
    } else {
        profile_shared_cache_expired(&cache, &probe, config)?
    };
    let after_work = probe.counts.snapshot();
    let work = after_work.since(cold);
    let mut expected_work = if config.hit {
        Counts {
            successful_lookups: config.useful,
            cohort_constructions: if config.cohort { config.useful } else { 0 },
            ..Counts::default()
        }
    } else {
        let mut counts = binding_counts(config.iterations, config.cohort);
        counts.values_dropped = config.iterations;
        counts.subscriptions_dropped = config.iterations;
        counts
    };
    expected_work.read_calls = config.useful;
    expect_counts(work, expected_work)?;
    measurement.work_finished();
    if checksum != config.checksum || last_tag != if config.hit { 7 } else { config.iterations } {
        return Err("wrong shared checksum or last tag");
    }
    let captured = if let Some(base) = &base {
        if base.read(&probe)? != 7 {
            return Err("wrong validation value");
        }
        let captured = base.capture(&probe)?;
        expect_live(&probe)?;
        Some(captured)
    } else {
        expect_expired(&probe)?;
        None
    };
    let after_validation = probe.counts.snapshot();
    let validation = after_validation.since(after_work);
    expect_counts(
        validation,
        Counts {
            read_calls: u64::from(config.hit),
            capture_operations: u64::from(config.hit),
            reader_captures: u64::from(config.hit),
            ..Counts::default()
        },
    )?;
    measurement.validation_finished();
    drop(base);
    if let Some(captured) = captured {
        expect_live(&probe)?;
        if captured
            .read(|value| value.tag)
            .map_err(|_| "captured read failed")?
            != 7
        {
            return Err("wrong final capture value");
        }
        probe.counts.read_calls.fetch_add(1, Ordering::Relaxed);
        drop(captured);
    }
    expect_expired(&probe)?;
    let totals = probe.counts.snapshot();
    let teardown = totals.since(after_validation);
    expect_counts(
        teardown,
        Counts {
            read_calls: u64::from(config.hit),
            values_dropped: u64::from(config.hit),
            subscriptions_dropped: u64::from(config.hit),
            ..Counts::default()
        },
    )?;
    *probe.slots.lock().unwrap() = Slots::default();
    drop((cache, probe));
    let allocations = measurement.finish()?;
    Ok(Trace {
        checksum,
        last_tag,
        totals,
        phases: [cold, work, validation, teardown],
    }
    .json(config, allocations))
}

/// Holds scalar evidence only, after all cache and provider owners are dropped.
struct Trace {
    checksum: u64,
    last_tag: u64,
    totals: Counts,
    phases: [Counts; 4],
}

impl Trace {
    /// Writes the fixed public diagnostic record after allocation accounting ends.
    fn json(self, config: Config, allocations: Option<crate::measurement::Record>) -> String {
        let [cold, work, validation, teardown] = self.phases;
        let allocations = allocations.map_or_else(|| "null".to_owned(), |record| record.json());
        let fields = self.totals.json();
        format!(
            concat!(
                "{{\"schema\":\"rca-shared-cache-performance-profile-v1\",",
                "\"execution\":\"direct-ready\",\"workload\":\"{}\",\"size\":{},\"iterations\":{},\"cycles\":{},",
                "\"features\":{{\"deterministic\":{},\"allocations\":{}}},\"allocator\":\"{}\",",
                "\"roots_committed\":0,\"responses_committed\":0,\"causal_completed\":0,\"cache_namespaces\":1,",
                "\"domain_clones\":0,\"checksum\":{},\"last_tag\":{},{}",
                ",\"counters\":{{\"cold\":{},\"work\":{},\"validation\":{},\"teardown\":{}}},",
                "\"owners_released\":true,\"value_expired\":true,\"subscription_expired\":true,",
                "\"producer_closed\":true,\"allocations\":{}}}"
            ),
            config.name,
            config.size,
            config.iterations,
            config.iterations,
            cfg!(feature = "deterministic"),
            cfg!(feature = "allocations"),
            if cfg!(feature = "allocations") {
                "dhat-0.3.3"
            } else {
                "system"
            },
            self.checksum,
            self.last_tag,
            &fields[1..fields.len() - 1],
            cold.json(),
            work.json(),
            validation.json(),
            teardown.json(),
            allocations
        )
    }
}

#[cfg(test)]
#[path = "shared_cache_controls.rs"]
mod controls;
