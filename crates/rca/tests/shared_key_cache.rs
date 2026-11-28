//! Checks cache namespace sharing and subscription lifetime through default exports.

use std::convert::Infallible;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use futures_executor::block_on;
use rca::{
    SharedKeyCache, SharedKeyDefault, SharedKeyIdentity, SharedLoadContext, SharedReaderKey,
    SharedSubscribeContext, SharedSubscriber,
};

/// Returns absence so the cache must use the caller's explicit default.
struct Key {
    /// Counts real load calls independently of the number of key handles.
    loads: Arc<AtomicUsize>,
    /// Counts final subscription release, not key or cache-handle release.
    drops: Arc<AtomicUsize>,
}
/// Records when the cache's actual subscription resource is released.
struct Subscription(Arc<AtomicUsize>);
impl Drop for Subscription {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}
impl SharedReaderKey for Key {
    type Value = u32;
    type Error = Infallible;
    type Subscription = Subscription;
    fn identity(&self) -> SharedKeyIdentity {
        SharedKeyIdentity::new::<u32>("facade", "v1")
    }
    fn load(
        &self,
        _: SharedLoadContext<'_, u32>,
    ) -> impl Future<Output = Result<Option<u32>, Infallible>> + Send {
        self.loads.fetch_add(1, Ordering::SeqCst);
        std::future::ready(Ok(None))
    }
    fn subscribe(
        &self,
        _: SharedSubscribeContext,
        _: SharedSubscriber<u32>,
    ) -> Result<Subscription, Infallible> {
        Ok(Subscription(Arc::clone(&self.drops)))
    }
}

// Downstream default-feature callers must reach the real owned cache, not a parallel facade cache.
#[test]
fn facade_defaults_share_only_the_injected_namespace_and_release_the_final_resource() {
    let cache = SharedKeyCache::new();
    let loads = Arc::new(AtomicUsize::new(0));
    let drops = Arc::new(AtomicUsize::new(0));
    let key = || Key {
        loads: Arc::clone(&loads),
        drops: Arc::clone(&drops),
    };
    let first = block_on(cache.reader_default(SharedKeyDefault::new(key(), || 7))).unwrap();
    let hit = block_on(
        cache
            .clone()
            .reader_default(SharedKeyDefault::new(key(), || panic!("hit cannot seed"))),
    )
    .unwrap();
    assert_eq!(hit.read(|value| *value).unwrap(), 7);
    assert_eq!(loads.load(Ordering::SeqCst), 1);
    let independent = block_on(SharedKeyCache::new().reader(key(), || 9)).unwrap();
    assert_eq!(independent.read(|value| *value).unwrap(), 9);
    assert_eq!(loads.load(Ordering::SeqCst), 2);
    drop(independent);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    drop(cache);
    drop(first);
    assert_eq!(hit.read(|value| *value).unwrap(), 7);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    drop(hit);
    assert_eq!(drops.load(Ordering::SeqCst), 2);
}
