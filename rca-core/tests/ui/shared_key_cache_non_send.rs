use rca_core::{
    SharedKeyIdentity, SharedLoadContext, SharedReaderKey, SharedSubscribeContext, SharedSubscriber,
};
use std::convert::Infallible;
use std::rc::Rc;

struct LocalKey;
impl SharedReaderKey for LocalKey {
    type Value = Rc<u32>;
    type Error = Infallible;
    type Subscription = ();
    fn identity(&self) -> SharedKeyIdentity {
        SharedKeyIdentity::new::<u32>("local", "v1")
    }
    fn load(
        &self,
        _: SharedLoadContext<'_, Rc<u32>>,
    ) -> impl Future<Output = Result<Option<Rc<u32>>, Infallible>> + Send {
        async { Ok(None) }
    }
    fn subscribe(
        &self,
        _: SharedSubscribeContext,
        _: SharedSubscriber<Rc<u32>>,
    ) -> Result<(), Infallible> {
        Ok(())
    }
}

fn main() {}
