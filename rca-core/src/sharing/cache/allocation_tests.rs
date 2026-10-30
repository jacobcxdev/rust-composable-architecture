//! Measures cache construction, live-hit and error-formatting allocations in isolated tests.
//!
//! The dedicated allocation host runs each case without a concurrent libtest thread.

use std::convert::Infallible;
use std::hint::black_box;

use crate::allocation_test;
use crate::{
    SharedKeyCache, SharedKeyIdentity, SharedLoadContext, SharedReaderKey, SharedSubscribeContext,
    SharedSubscriber,
};

/// Allows initial construction but rejects provider work during a live cache hit.
struct Key {
    /// Distinguishes the first request from the measured repeated lookups.
    constructing: bool,
}

impl SharedReaderKey for Key {
    type Value = u64;
    type Error = Infallible;
    type Subscription = ();

    fn identity(&self) -> SharedKeyIdentity {
        SharedKeyIdentity::new::<u64>("allocation-control", "u64-v1")
    }

    fn load(
        &self,
        _: SharedLoadContext<'_, u64>,
    ) -> impl Future<Output = Result<Option<u64>, Infallible>> + Send {
        assert!(self.constructing, "a live hit must not load");
        std::future::ready(Ok(None))
    }

    fn subscribe(
        &self,
        _: SharedSubscribeContext,
        _: SharedSubscriber<u64>,
    ) -> Result<(), Infallible> {
        assert!(self.constructing, "a live hit must not subscribe");
        Ok(())
    }
}

/// Checks that live hits allocate only the caller's requested identity.
///
/// # Panics
/// Panics if a live hit constructs a provider or exceeds its allocation budget.
pub(crate) fn live_cache_hits_allocate_only_the_requested_identity() {
    if allocation_test::run_in_child("live_cache_hits_allocate_only_the_requested_identity") {
        return;
    }

    let cache = SharedKeyCache::new();
    let owner = futures_executor::block_on(cache.reader(Key { constructing: true }, || 7))
        .expect("initial reader");
    // Initialise platform mutex storage before measuring repeated live hits.
    assert_eq!(owner.read(|value| *value).unwrap(), 7);
    let profiler = dhat::Profiler::builder().testing().build();
    let before = dhat::HeapStats::get();
    for _ in 0..1_024 {
        let reader = futures_executor::block_on(cache.reader(
            Key {
                constructing: false,
            },
            || panic!("a live hit must not seed"),
        ))
        .expect("live cached reader");
        assert_eq!(black_box(reader.read(|value| *value).unwrap()), 7);
    }
    let after = dhat::HeapStats::get();
    drop(profiler);
    let blocks = after.total_blocks - before.total_blocks;
    let bytes = after.total_bytes - before.total_bytes;
    println!("live-cache-hits=1024 blocks={blocks} bytes={bytes} value=7");
    // Each caller-requested identity owns an 18-byte address and 6-byte config.
    // Looking up that identity must not copy the cache's existing metadata.
    assert_eq!(blocks, 2_048);
    assert_eq!(bytes, 24_576);
}

/// Checks that both debug formats write directly into fixed storage.
///
/// # Panics
/// Panics if formatting changes, fails or allocates.
pub(crate) fn shared_key_error_debug_formats_without_allocating() {
    use std::fmt::{self, Write as _};

    /// Holds formatted bytes without adding destination allocations to the measurement.
    struct StackBuffer {
        bytes: [u8; 256],
        /// End of the initialised prefix, checked against capacity on every write.
        len: usize,
    }

    impl fmt::Write for StackBuffer {
        fn write_str(&mut self, s: &str) -> fmt::Result {
            let end = self.len + s.len();
            self.bytes
                .get_mut(self.len..end)
                .ok_or(fmt::Error)?
                .copy_from_slice(s.as_bytes());
            self.len = end;
            Ok(())
        }
    }

    if allocation_test::run_in_child("shared_key_error_debug_formats_without_allocating") {
        return;
    }

    let cache = SharedKeyCache::new();
    let error = futures_executor::block_on(cache.require_reader(Key { constructing: true }))
        .expect_err("the provider has no stored value");
    assert!(error.is_absent());
    let mut compact = StackBuffer {
        bytes: [0; 256],
        len: 0,
    };
    let mut pretty = StackBuffer {
        bytes: [0; 256],
        len: 0,
    };
    let profiler = dhat::Profiler::builder().testing().build();
    let before = dhat::HeapStats::get();
    write!(&mut compact, "{:?}", black_box(&error)).unwrap();
    let compact_after = dhat::HeapStats::get();
    write!(&mut pretty, "{:#?}", black_box(&error)).unwrap();
    let after = dhat::HeapStats::get();
    drop(profiler);

    let compact_blocks = compact_after.total_blocks - before.total_blocks;
    let compact_bytes = compact_after.total_bytes - before.total_bytes;
    let pretty_blocks = after.total_blocks - compact_after.total_blocks;
    let pretty_bytes = after.total_bytes - compact_after.total_bytes;
    println!(
        "shared-key-error-debug compact-blocks={compact_blocks} compact-bytes={compact_bytes} pretty-blocks={pretty_blocks} pretty-bytes={pretty_bytes}"
    );
    assert_eq!(
        std::str::from_utf8(&compact.bytes[..compact.len]).unwrap(),
        "SharedKeyError { kind: \"required shared value is absent from storage\", .. }"
    );
    assert_eq!(
        std::str::from_utf8(&pretty.bytes[..pretty.len]).unwrap(),
        "SharedKeyError {\n    kind: \"required shared value is absent from storage\",\n    ..\n}"
    );
    assert_eq!(
        (compact_blocks, compact_bytes, pretty_blocks, pretty_bytes),
        (0, 0, 0, 0),
        "formatting a shared key error must not allocate temporary strings"
    );
}

/// Counts construction and subscription release independently of allocator statistics.
#[derive(Default)]
struct PublicationProbe {
    loads: std::sync::atomic::AtomicUsize,
    subscriptions: std::sync::atomic::AtomicUsize,
    drops: std::sync::atomic::AtomicUsize,
}

/// Uses one fixed identity for cold and deliberately losing publication scenarios.
#[derive(Clone)]
struct PublicationKey(std::sync::Arc<PublicationProbe>);

/// Records the exact lifetime of one admitted provider subscription.
struct PublicationSubscription(std::sync::Arc<PublicationProbe>);

impl Drop for PublicationSubscription {
    fn drop(&mut self) {
        self.0
            .drops
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }
}

impl SharedReaderKey for PublicationKey {
    type Value = u64;
    type Error = Infallible;
    type Subscription = PublicationSubscription;

    fn identity(&self) -> SharedKeyIdentity {
        SharedKeyIdentity::new::<u64>("shared-profile", "v1")
    }

    fn load(
        &self,
        context: SharedLoadContext<'_, u64>,
    ) -> impl Future<Output = Result<Option<u64>, Infallible>> + Send {
        assert!(matches!(context, SharedLoadContext::Initial(_)));
        self.0
            .loads
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        std::future::ready(Ok(None))
    }

    fn subscribe(
        &self,
        context: SharedSubscribeContext,
        _: SharedSubscriber<u64>,
    ) -> Result<Self::Subscription, Infallible> {
        assert_eq!(context, SharedSubscribeContext::Initial);
        self.0
            .subscriptions
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(PublicationSubscription(std::sync::Arc::clone(&self.0)))
    }
}

/// Requires an immediate provider result without introducing an executor allocation.
fn poll_publication<Output>(future: impl Future<Output = Output>) -> Output {
    let mut future = std::pin::pin!(future);
    let mut context = std::task::Context::from_waker(std::task::Waker::noop());
    match future.as_mut().poll(&mut context) {
        std::task::Poll::Ready(output) => output,
        std::task::Poll::Pending => panic!("publication fixture must complete immediately"),
    }
}

/// Measures outer post-seed construction, then checks release of every measured owner.
///
/// A nested live winner selects the losing-publication path. Optional raw profiles
/// use a fresh caller-selected path and remain available after a failed assertion.
fn publication_allocations(late_winner: bool) -> u64 {
    use std::sync::atomic::Ordering::SeqCst;

    let cache = SharedKeyCache::new();
    let probe = std::sync::Arc::new(PublicationProbe::default());
    let key = PublicationKey(std::sync::Arc::clone(&probe));
    let case = if late_winner { "late-winner" } else { "cold" };
    // Explicit qualification opt-in; ordinary tests never overwrite dhat-heap.json.
    let output = std::env::var_os("RCA_R09_DHAT_DIR")
        .map(|directory| std::path::PathBuf::from(directory).join(format!("{case}.json")));
    let profiler = if let Some(path) = &output {
        // Reserve a new path before DHAT begins; a stale result is an error.
        drop(
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(path)
                .expect("reserve a fresh publication profile"),
        );
        dhat::Profiler::builder()
            .file_name(path)
            .trim_backtraces(None)
            .build()
    } else {
        dhat::Profiler::builder().testing().build()
    };
    let mut winner = None;
    let mut before = None;
    let mut seeds = 0;
    let returned = poll_publication(cache.reader(key.clone(), || {
        seeds += 1;
        if late_winner {
            // The outer lookup already missed. Keep the inner owner alive so
            // the outer constructor must lose at the real publication boundary.
            winner = Some(
                poll_publication(cache.reader(key.clone(), || {
                    seeds += 1;
                    7
                }))
                .expect("nested winner"),
            );
        }
        // Both cases measure the same outer post-seed construction phase.
        before = Some(dhat::HeapStats::get());
        3
    }))
    .expect("outer reader");
    let after = dhat::HeapStats::get();
    let before = before.expect("outer seed must run exactly once");

    let expected_value = if late_winner { 7 } else { 3 };
    let constructors = if late_winner { 2 } else { 1 };
    assert_eq!(returned.read(|value| *value).unwrap(), expected_value);
    if let Some(winner) = &winner {
        assert_eq!(winner.read(|value| *value).unwrap(), 7);
    }
    assert_eq!(seeds, constructors);
    assert_eq!(probe.loads.load(SeqCst), constructors);
    assert_eq!(probe.subscriptions.load(SeqCst), constructors);
    assert_eq!(probe.drops.load(SeqCst), constructors - 1);
    drop(winner);
    assert_eq!(probe.drops.load(SeqCst), constructors - 1);
    assert_eq!(returned.read(|value| *value).unwrap(), expected_value);
    drop((returned, key, cache));
    let released = dhat::HeapStats::get();
    drop(profiler);

    // Keep the full raw attribution even when the final allocation budget fails.
    if let Some(path) = output {
        assert!(std::fs::metadata(path).unwrap().len() > 0);
    }
    assert_eq!(probe.drops.load(SeqCst), constructors);
    assert_eq!(std::sync::Arc::strong_count(&probe), 1);
    assert_eq!(released.curr_blocks, 0);
    assert_eq!(released.curr_bytes, 0);
    println!(
        "publication case={case} seeds={seeds} loads={constructors} subscriptions={constructors} drops={constructors} value={expected_value} before={before:?} after={after:?} released={released:?}"
    );
    after.total_blocks - before.total_blocks
}

/// Checks the construction budget and ownership of a cold publication.
///
/// # Panics
/// Panics if construction, cleanup or the allocation budget changes.
pub(crate) fn cold_publication_keeps_its_required_construction_allocations() {
    if allocation_test::run_in_child("cold_publication_keeps_its_required_construction_allocations")
    {
        return;
    }
    // Five construction blocks below, one map address, one config and one table.
    assert_eq!(publication_allocations(false), 8);
}

/// Checks that a losing publication reuses the winner's existing metadata.
///
/// # Panics
/// Panics if construction, cleanup or the allocation budget changes.
pub(crate) fn losing_publication_does_not_clone_winner_metadata() {
    if allocation_test::run_in_child("losing_publication_does_not_clone_winner_metadata") {
        return;
    }
    // Two retained compatibility strings, reference Arc, load coordinator Arc,
    // and the retained key-resource Box. No copied winner identity is needed.
    assert_eq!(publication_allocations(true), 5);
}
