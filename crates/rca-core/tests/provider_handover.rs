//! Checks weak provider handover and dependency capture across later overrides.

use std::convert::Infallible;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

use futures_executor::block_on;
use rca_core::{
    DependencyDefaults, DependencyKey, DependencyValues, SharedKeyCache, SharedKeyIdentity,
    SharedLoadContext, SharedReaderKey, SharedSubscribeContext, SharedSubscriber,
};

/// Captures a load value before waiting and does not replay changes on subscribe.
#[derive(Clone)]
struct NonReplayingKey {
    value: Arc<AtomicU32>,
    /// Delays one load result without retaining the slot lock across the wait.
    loaded: Arc<Mutex<Option<futures_channel::oneshot::Receiver<()>>>>,
    /// Retains the weak callback until the reader's registration ends.
    subscriber: Arc<Mutex<Option<SharedSubscriber<u32>>>>,
}

/// Removes the provider callback when its captured reader loses its last owner.
struct Registration(Arc<Mutex<Option<SharedSubscriber<u32>>>>);

impl Drop for Registration {
    fn drop(&mut self) {
        self.0.lock().unwrap().take();
    }
}

impl SharedReaderKey for NonReplayingKey {
    type Value = u32;
    type Error = Infallible;
    type Subscription = Registration;

    fn identity(&self) -> SharedKeyIdentity {
        SharedKeyIdentity::new::<u32>("non-replaying", "v1")
    }

    fn load(
        &self,
        _: SharedLoadContext<'_, u32>,
    ) -> impl Future<Output = Result<Option<u32>, Infallible>> + Send {
        let value = self.value.load(Ordering::SeqCst);
        let loaded = self.loaded.lock().unwrap().take();
        async move {
            if let Some(loaded) = loaded {
                loaded.await.unwrap();
            }
            Ok(Some(value))
        }
    }

    fn subscribe(
        &self,
        _: SharedSubscribeContext,
        subscriber: SharedSubscriber<u32>,
    ) -> Result<Registration, Infallible> {
        *self.subscriber.lock().unwrap() = Some(subscriber);
        Ok(Registration(Arc::clone(&self.subscriber)))
    }
}

// Reordering subscribe before load or silently reloading would change the weak
// contract; retaining a strong callback owner would break final-owner cleanup.
#[test]
fn non_replaying_provider_does_not_reconcile_an_update_between_load_and_subscribe() {
    let (release, loaded) = futures_channel::oneshot::channel();
    let key = NonReplayingKey {
        value: Arc::new(AtomicU32::new(3)),
        loaded: Arc::new(Mutex::new(Some(loaded))),
        subscriber: Arc::new(Mutex::new(None)),
    };
    let cache = SharedKeyCache::new();
    let mut constructing = std::pin::pin!(cache.reader(key.clone(), || 0));
    assert!(matches!(
        constructing
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop())),
        Poll::Pending
    ));
    assert!(key.subscriber.lock().unwrap().is_none());
    key.value.store(7, Ordering::SeqCst);
    release.send(()).unwrap();
    let reader = block_on(constructing).unwrap();
    assert_eq!(reader.read(|value| *value).unwrap(), 3);
    let subscriber = key.subscriber.lock().unwrap().clone();
    let subscriber = subscriber.expect("completed load registers its provider callback");
    key.value.store(11, Ordering::SeqCst);
    assert!(subscriber.yield_value(11).unwrap());
    assert_eq!(reader.read(|value| *value).unwrap(), 11);
    drop(reader);
    assert!(key.subscriber.lock().unwrap().is_none());
    assert!(!subscriber.yield_value(99).unwrap());
}

/// Identifies the mutable backend whose handle may outlive later overrides.
struct Backend;
impl DependencyKey for Backend {
    type Value = Arc<AtomicU32>;
}

/// Prepares a fresh backend factory without eagerly creating its value.
fn fixture(initial: u32) -> DependencyValues {
    let mut values = DependencyValues::new();
    values
        .prepare(|values| {
            values.register_defaults(
                DependencyDefaults::<Backend>::new()
                    .with_live(move || Arc::new(AtomicU32::new(initial))),
            )
        })
        .unwrap()
        .unwrap();
    values
}

// Cloning a populated dependency container must not be sold as a fresh backend.
// A fresh factory, unlike an intentional clone, cannot inherit previous mutations.
#[test]
fn fresh_fixture_backends_are_independent_but_cloned_containers_share_captured_handles() {
    let mut first = fixture(3);
    let captured = Arc::clone(first.get_or_insert::<Backend>());
    let mut shared = first.clone();
    let mut fresh = fixture(3);
    shared.get_or_insert::<Backend>().store(7, Ordering::SeqCst);
    assert_eq!(captured.load(Ordering::SeqCst), 7);
    assert_eq!(first.get_or_insert::<Backend>().load(Ordering::SeqCst), 7);
    assert_eq!(fresh.get_or_insert::<Backend>().load(Ordering::SeqCst), 3);
    assert!(!Arc::ptr_eq(&captured, fresh.get_or_insert::<Backend>()));
}

// Moving preparation after the state factory or refreshing a captured provider
// from a later environment would alter the original construction result.
#[test]
fn preparation_precedes_state_factory_and_later_overrides_do_not_reconstruct_state() {
    let mut values = fixture(3);
    let construct = |values: &mut DependencyValues| Arc::clone(values.get_or_insert::<Backend>());
    let original_state = construct(&mut values);
    let prepared_state = values.scope(
        |prepared| prepared.set::<Backend>(Arc::new(AtomicU32::new(7))),
        construct,
    );
    values.set::<Backend>(Arc::new(AtomicU32::new(7)));
    let (release, released) = futures_channel::oneshot::channel::<()>();
    let captured = Arc::clone(&original_state);
    let operation = values.scope_async(
        |_| {},
        |mut admitted| async move {
            released.await.unwrap();
            (
                captured.load(Ordering::SeqCst),
                admitted.get_or_insert::<Backend>().load(Ordering::SeqCst),
            )
        },
    );
    let mut operation = std::pin::pin!(operation);
    assert!(
        operation
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_pending()
    );
    values.set::<Backend>(Arc::new(AtomicU32::new(11)));
    release.send(()).unwrap();
    assert_eq!(block_on(operation), (3, 7));
    assert_eq!(original_state.load(Ordering::SeqCst), 3);
    assert_eq!(prepared_state.load(Ordering::SeqCst), 7);
    assert_eq!(construct(&mut values).load(Ordering::SeqCst), 11);
}

// Live lookup must never select a registered test provider when live is absent.
#[test]
fn missing_live_default_does_not_invoke_registered_test_provider() {
    let calls = Arc::new(AtomicU32::new(0));
    let test_calls = Arc::clone(&calls);
    let mut values = DependencyValues::new();
    values
        .register_defaults(DependencyDefaults::<Backend>::new().with_test(move || {
            test_calls.fetch_add(1, Ordering::SeqCst);
            Arc::new(AtomicU32::new(99))
        }))
        .unwrap();
    let missing = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        values.get_or_insert::<Backend>();
    }))
    .expect_err("missing live default rejects lookup");
    let message = missing
        .downcast_ref::<String>()
        .expect("missing live default has a formatted diagnostic");
    assert_eq!(
        message,
        &format!(
            "missing live dependency `{}`; inject a value or register live defaults",
            std::any::type_name::<Backend>()
        )
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}
