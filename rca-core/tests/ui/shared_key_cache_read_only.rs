use rca_core::{
    Shared, SharedKeyCache, SharedKeyIdentity, SharedLoadContext, SharedReaderKey,
    SharedSubscribeContext, SharedSubscriber,
};
use std::convert::Infallible;

struct ReadOnly;
impl SharedReaderKey for ReadOnly {
    type Value = u32;
    type Error = Infallible;
    type Subscription = ();
    fn identity(&self) -> SharedKeyIdentity {
        SharedKeyIdentity::new::<u32>("read-only", "v1")
    }
    fn load(
        &self,
        _: SharedLoadContext<'_, u32>,
    ) -> impl Future<Output = Result<Option<u32>, Infallible>> + Send {
        std::future::ready(Ok(None))
    }
    fn subscribe(
        &self,
        _: SharedSubscribeContext,
        _: SharedSubscriber<u32>,
    ) -> Result<(), Infallible> {
        Ok(())
    }
}

fn main() {
    let cache = SharedKeyCache::new();
    let shared = Shared::new(1);
    let _ = cache.shared(ReadOnly, || 0);
    let _ = cache.require(ReadOnly);
    let _ = cache.load(&shared, ReadOnly, |value| *value);
}
