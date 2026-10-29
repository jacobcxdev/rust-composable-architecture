//! Checks cache identity, provider ordering, captured ownership and reload authority.

use std::cell::Cell;
use std::convert::Infallible;
use std::future::ready;
use std::sync::{Arc, Barrier, Mutex};

use futures_executor::block_on;
use rca_core::{
    Shared, SharedKey, SharedKeyCache, SharedKeyDefault, SharedKeyIdentity, SharedLoadContext,
    SharedReaderKey, SharedSubscribeContext, SharedSubscriber,
};

#[cfg(not(miri))]
#[path = "../src/allocation_test/child.rs"]
mod subprocess;

/// Marks the ordinary codec identity independently of the concrete provider type.
struct NumberCodec;
/// Shares an address with the ordinary codec to expose incompatible codec reuse.
struct OtherCodec;

#[test]
fn default_key_cleanup_panic_drops_the_extracted_key() {
    /// Counts the extracted key even when disposing of its unused seed panics.
    struct KeyDrop(std::rc::Rc<Cell<usize>>);
    impl Drop for KeyDrop {
        fn drop(&mut self) {
            self.0.set(self.0.get() + 1);
        }
    }
    /// Panics when the unused seed capture is released, not when it is called.
    struct SeedDrop;
    impl Drop for SeedDrop {
        fn drop(&mut self) {
            panic!("unused default seed cleanup");
        }
    }

    let drops = std::rc::Rc::new(Cell::new(0));
    let seed = SeedDrop;
    let default = SharedKeyDefault::new(KeyDrop(std::rc::Rc::clone(&drops)), move || {
        std::hint::black_box(&seed);
    });
    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        drop(default.into_key());
    }))
    .expect_err("unused seed cleanup must propagate its panic");
    assert_eq!(
        panic.downcast_ref::<&str>(),
        Some(&"unused default seed cleanup")
    );
    assert_eq!(drops.get(), 1, "extracted key leaked during seed cleanup");
}

#[test]
fn cache_hit_cleanup_panic_releases_the_provisional_reader() {
    /// Forces cleanup after a hit has already selected the retained reader.
    struct SeedDrop;
    impl Drop for SeedDrop {
        fn drop(&mut self) {
            panic!("unused cache seed cleanup");
        }
    }

    let cache = SharedKeyCache::new();
    let drops = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let key = ReentrantKey {
        cache: cache.clone(),
        drops: Arc::clone(&drops),
    };
    let reader = block_on(cache.reader(key.clone(), || 7)).unwrap();
    let seed = SeedDrop;
    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        drop(block_on(cache.reader(key, move || {
            std::hint::black_box(&seed);
            panic!("a cache hit must not evaluate its seed");
        })));
    }))
    .expect_err("unused seed cleanup must propagate its panic");
    assert_eq!(
        panic.downcast_ref::<&str>(),
        Some(&"unused cache seed cleanup")
    );
    assert_eq!(reader.read(|value| *value).unwrap(), 7);
    drop(reader);
    assert_eq!(
        drops.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "the rejected return value retained the subscription"
    );
}

#[test]
fn required_value_cleanup_panic_releases_the_cached_reader() {
    let cache = SharedKeyCache::new();
    let key = ControlledKey::new();
    let drops = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let observed = Arc::clone(&drops);
    let winner = block_on(cache.reader(key.clone(), || ReloadDropValue {
        value: 7,
        on_drop: Some(Box::new(move || {
            observed.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        })),
    }))
    .unwrap();
    let mut require = Box::pin(cache.require_reader(key.clone()));
    start_pending(require.as_mut());
    key.reply(Ok(Some(ReloadDropValue {
        value: 11,
        on_drop: Some(Box::new(|| panic!("discarded required value cleanup"))),
    })));
    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        drop(block_on(require));
    }))
    .expect_err("discarded loaded value cleanup must panic");
    assert_eq!(
        panic.downcast_ref::<&str>(),
        Some(&"discarded required value cleanup")
    );
    assert_eq!(winner.read(|value| value.value).unwrap(), 7);
    drop(winner);
    assert_eq!(drops.load(std::sync::atomic::Ordering::SeqCst), 1);
}

#[test]
fn losing_subscription_cleanup_panic_releases_the_winning_reader() {
    /// Distinguishes the losing subscription from the retained winner.
    struct Key {
        inner: ReentrantKey,
        panic_on_drop: bool,
    }
    /// Runs one selected panic before its underlying subscription is released.
    struct Guard {
        _inner: ReentrantSubscription,
        panic_on_drop: bool,
    }
    impl Drop for Guard {
        fn drop(&mut self) {
            assert!(!self.panic_on_drop, "losing subscription cleanup");
        }
    }
    impl SharedReaderKey for Key {
        type Value = u32;
        type Error = Infallible;
        type Subscription = Guard;

        fn identity(&self) -> SharedKeyIdentity {
            self.inner.identity()
        }

        fn load(
            &self,
            context: SharedLoadContext<'_, u32>,
        ) -> impl Future<Output = Result<Option<u32>, Infallible>> + Send {
            self.inner.load(context)
        }

        fn subscribe(
            &self,
            context: SharedSubscribeContext,
            subscriber: SharedSubscriber<u32>,
        ) -> Result<Guard, Infallible> {
            Ok(Guard {
                _inner: self.inner.subscribe(context, subscriber)?,
                panic_on_drop: self.panic_on_drop,
            })
        }
    }

    let cache = SharedKeyCache::new();
    let drops = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let key = |panic_on_drop| Key {
        inner: ReentrantKey {
            cache: cache.clone(),
            drops: Arc::clone(&drops),
        },
        panic_on_drop,
    };
    let mut winner = None;
    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        drop(block_on(cache.reader(key(true), || {
            let mut inner = std::pin::pin!(cache.reader(key(false), || 7));
            let mut context = std::task::Context::from_waker(std::task::Waker::noop());
            winner = Some(match inner.as_mut().poll(&mut context) {
                std::task::Poll::Ready(result) => result.unwrap(),
                std::task::Poll::Pending => panic!("provider must complete immediately"),
            });
            3
        })));
    }))
    .expect_err("losing subscription cleanup must panic");
    assert_eq!(
        panic.downcast_ref::<&str>(),
        Some(&"losing subscription cleanup")
    );
    assert_eq!(drops.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(winner.as_ref().unwrap().read(|value| *value).unwrap(), 7);
    drop(winner);
    assert_eq!(drops.load(std::sync::atomic::Ordering::SeqCst), 2);
}

#[test]
fn discarded_seed_cleanup_panic_drops_the_loaded_value() {
    /// Moves its stored value out once, before the discarded seed is released.
    struct Key(Mutex<Option<ReloadDropValue>>);
    impl SharedReaderKey for Key {
        type Value = ReloadDropValue;
        type Error = Infallible;
        type Subscription = ();

        fn identity(&self) -> SharedKeyIdentity {
            SharedKeyIdentity::new::<Self>("discarded-seed", "v1")
        }

        fn load(
            &self,
            _: SharedLoadContext<'_, ReloadDropValue>,
        ) -> impl Future<Output = Result<Option<ReloadDropValue>, Infallible>> + Send {
            ready(Ok(self.0.lock().unwrap().take()))
        }

        fn subscribe(
            &self,
            _: SharedSubscribeContext,
            _: SharedSubscriber<ReloadDropValue>,
        ) -> Result<(), Infallible> {
            panic!("seed cleanup must precede subscription");
        }
    }

    let cache = SharedKeyCache::new();
    let drops = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let observed = Arc::clone(&drops);
    let key = Key(Mutex::new(Some(ReloadDropValue {
        value: 7,
        on_drop: Some(Box::new(move || {
            observed.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        })),
    })));
    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        drop(block_on(cache.reader(key, || ReloadDropValue {
            value: 3,
            on_drop: Some(Box::new(|| panic!("discarded seed cleanup"))),
        })));
    }))
    .expect_err("discarded seed cleanup must panic");
    assert_eq!(
        panic.downcast_ref::<&str>(),
        Some(&"discarded seed cleanup")
    );
    assert_eq!(drops.load(std::sync::atomic::Ordering::SeqCst), 1);
}

#[test]
fn replacement_hit_cleanup_panic_releases_the_selected_reader() {
    /// Forces disposal of an unused replacement seed to unwind.
    struct SeedDrop;
    impl Drop for SeedDrop {
        fn drop(&mut self) {
            panic!("unused replacement seed cleanup");
        }
    }

    let mut observed = Vec::new();
    for writable in [false, true] {
        let cache = SharedKeyCache::new();
        let key = MemoryKey::new(None);
        let winner = block_on(cache.reader(key.clone(), key.seed(7))).unwrap();
        let source = Shared::new(3);
        let mut reader = source.reader().unwrap();
        let captured = SeedDrop;
        let seed = move |_: &i32| {
            std::hint::black_box(&captured);
            panic!("a replacement hit must not evaluate its seed");
        };
        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            if writable {
                drop(block_on(cache.load(&source, key.clone(), seed)));
            } else {
                drop(block_on(cache.load_reader(&mut reader, key.clone(), seed)));
            }
        }))
        .expect_err("unused replacement seed cleanup must panic");
        assert_eq!(
            panic.downcast_ref::<&str>(),
            Some(&"unused replacement seed cleanup")
        );
        assert_eq!(source.read(|value| *value).unwrap(), 3);
        assert_eq!(reader.read(|value| *value).unwrap(), 3);
        assert_eq!(winner.read(|value| *value).unwrap(), 7);
        assert_eq!(key.events(), ["seed:7", "initial:7", "subscribe:initial"]);
        drop((reader, source, winner));
        observed.push(
            key.events()
                .iter()
                .filter(|event| *event == "unsubscribe")
                .count(),
        );
    }
    assert_eq!(observed, [1, 1], "replacement cleanup retained the winner");
}

/// Counts the original typed cause even when another owner's cleanup panics.
struct CleanupCause(Arc<std::sync::atomic::AtomicUsize>);

impl Drop for CleanupCause {
    fn drop(&mut self) {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }
}

#[test]
fn provider_error_cleanup_panic_drops_the_original_cause() {
    /// Selects load or subscription failure and the owner whose cleanup panics.
    struct Key {
        drops: Arc<std::sync::atomic::AtomicUsize>,
        calls: Arc<Mutex<Vec<&'static str>>>,
        fail_subscription: bool,
        panic_on_drop: bool,
        loaded: Mutex<Option<ReloadDropValue>>,
    }

    impl Drop for Key {
        fn drop(&mut self) {
            assert!(!self.panic_on_drop, "provider key cleanup");
        }
    }

    impl SharedReaderKey for Key {
        type Value = ReloadDropValue;
        type Error = CleanupCause;
        type Subscription = ();

        fn identity(&self) -> SharedKeyIdentity {
            SharedKeyIdentity::new::<Self>("provider-cleanup", "v1")
        }

        fn load(
            &self,
            _: SharedLoadContext<'_, ReloadDropValue>,
        ) -> impl Future<Output = Result<Option<ReloadDropValue>, CleanupCause>> + Send {
            self.calls.lock().unwrap().push("load");
            ready(if self.fail_subscription {
                Ok(self.loaded.lock().unwrap().take())
            } else {
                Err(CleanupCause(Arc::clone(&self.drops)))
            })
        }

        fn subscribe(
            &self,
            _: SharedSubscribeContext,
            _: SharedSubscriber<ReloadDropValue>,
        ) -> Result<(), CleanupCause> {
            self.calls.lock().unwrap().push("subscribe");
            Err(CleanupCause(Arc::clone(&self.drops)))
        }
    }

    let mut observed = Vec::new();
    for (case, require, fail_subscription, panic_key) in [
        ("reader-load-seed", false, false, false),
        ("require-load-key", true, false, true),
        ("reader-subscription-value", false, true, false),
        ("require-subscription-value", true, true, false),
    ] {
        let cache = SharedKeyCache::new();
        let drops = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let calls = Arc::new(Mutex::new(Vec::new()));
        let value = || ReloadDropValue {
            value: 7,
            on_drop: (!panic_key).then(|| {
                Box::new(|| -> () { panic!("provider value cleanup") }) as Box<dyn FnOnce() + Send>
            }),
        };
        let key = Key {
            drops: Arc::clone(&drops),
            calls: Arc::clone(&calls),
            fail_subscription,
            panic_on_drop: panic_key,
            loaded: Mutex::new(require.then(value)),
        };
        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            if require {
                drop(block_on(cache.require_reader(key)));
            } else {
                drop(block_on(cache.reader(key, value)));
            }
        }))
        .expect_err("the selected provider cleanup must panic");
        let expected = if panic_key {
            "provider key cleanup"
        } else {
            "provider value cleanup"
        };
        assert_eq!(panic.downcast_ref::<&str>(), Some(&expected), "{case}");
        let expected_calls: &[_] = if fail_subscription {
            &["load", "subscribe"]
        } else {
            &["load"]
        };
        assert_eq!(calls.lock().unwrap().as_slice(), expected_calls, "{case}");
        cache.reset().unwrap();
        observed.push((case, drops.load(std::sync::atomic::Ordering::SeqCst)));
    }
    assert_eq!(
        observed,
        [
            ("reader-load-seed", 1),
            ("require-load-key", 1),
            ("reader-subscription-value", 1),
            ("require-subscription-value", 1),
        ],
        "a cleanup panic must not leak the returned typed provider cause"
    );
}

#[test]
fn captured_reload_cleanup_panic_drops_the_original_cause() {
    /// Wraps controlled replies with a separately counted, move-owned error cause.
    #[derive(Clone)]
    struct Key {
        inner: ControlledKey<ReloadDropValue>,
        drops: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl SharedReaderKey for Key {
        type Value = ReloadDropValue;
        type Error = CleanupCause;
        type Subscription = ();

        fn identity(&self) -> SharedKeyIdentity {
            self.inner.identity()
        }

        fn load(
            &self,
            context: SharedLoadContext<'_, ReloadDropValue>,
        ) -> impl Future<Output = Result<Option<ReloadDropValue>, CleanupCause>> + Send {
            let pending = self.inner.load(context);
            let drops = Arc::clone(&self.drops);
            async move { pending.await.map_err(|_error| CleanupCause(drops)) }
        }

        fn subscribe(
            &self,
            _: SharedSubscribeContext,
            _: SharedSubscriber<ReloadDropValue>,
        ) -> Result<(), CleanupCause> {
            Ok(())
        }
    }

    impl SharedKey for Key {
        fn save(
            &self,
            _: &ReloadDropValue,
        ) -> impl Future<Output = Result<(), CleanupCause>> + Send {
            ready(Ok(()))
        }
    }

    let mut observed = Vec::new();
    for replacement in [false, true] {
        let cache = SharedKeyCache::new();
        let drops = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let key = Key {
            inner: ControlledKey::new(),
            drops: Arc::clone(&drops),
        };
        let value = || ReloadDropValue {
            value: 7,
            on_drop: Some(Box::new(|| panic!("captured reload value cleanup"))),
        };
        let source = if replacement {
            Shared::new(ReloadDropValue::plain(3))
        } else {
            block_on(cache.shared(key.clone(), value)).unwrap()
        };
        let mut pending = Box::pin(async {
            if replacement {
                cache.load(&source, key.clone(), |_| value()).await
            } else {
                cache.reload(&source, &key).await
            }
        });
        start_pending(pending.as_mut());
        source
            .rebind(&Shared::new(ReloadDropValue::plain(11)))
            .unwrap();
        key.inner.reply(Err(ProviderFailure(Cell::new(42))));
        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            drop(block_on(pending));
        }))
        .expect_err("last selected reference cleanup must panic");
        assert_eq!(
            panic.downcast_ref::<&str>(),
            Some(&"captured reload value cleanup")
        );
        assert_eq!(source.read(|value| value.value).unwrap(), 11);
        cache.reset().unwrap();
        observed.push(drops.load(std::sync::atomic::Ordering::SeqCst));
    }
    assert_eq!(
        observed,
        [1, 1],
        "selected reference cleanup leaked its error"
    );
}

/// Carries a literal cause without adding `Clone`, `Debug` or `Sync` bounds.
struct ProviderFailure(Cell<u32>);

#[test]
fn replacement_source_cleanup_panic_drops_the_prepared_target() {
    let cache = SharedKeyCache::new();
    let key = ControlledKey::new();
    let drops = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let source = Shared::new(ReloadDropValue {
        value: 3,
        on_drop: Some(Box::new(|| panic!("replacement source cleanup"))),
    });
    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        drop(block_on(cache.load(&source, key, |_| {
            source
                .rebind(&Shared::new(ReloadDropValue::plain(11)))
                .unwrap();
            ReloadDropValue {
                value: 7,
                on_drop: Some(Box::new({
                    let drops = Arc::clone(&drops);
                    move || {
                        drops.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    }
                })),
            }
        })));
    }))
    .expect_err("the last source reference must run its panicking cleanup");
    assert_eq!(
        panic.downcast_ref::<&str>(),
        Some(&"replacement source cleanup")
    );
    assert_eq!(source.read(|value| value.value).unwrap(), 11);
    assert_eq!(drops.load(std::sync::atomic::Ordering::SeqCst), 1);
}

/// Records provider order and independently selects load or subscription failure.
struct FailingKey {
    fail_load: bool,
    fail_subscription: bool,
    stored: Option<i32>,
    /// Reenters a supplied handle during load to observe the already changed binding.
    observed: Mutex<Option<Shared<i32>>>,
    calls: Arc<Mutex<Vec<&'static str>>>,
}

impl SharedReaderKey for FailingKey {
    type Value = i32;
    type Error = ProviderFailure;
    type Subscription = ();

    fn identity(&self) -> SharedKeyIdentity {
        SharedKeyIdentity::new::<NumberCodec>("failing", "v1")
    }

    fn load(
        &self,
        _: SharedLoadContext<'_, i32>,
    ) -> impl Future<Output = Result<Option<i32>, ProviderFailure>> + Send {
        self.calls.lock().unwrap().push("load");
        if let Some(observed) = self.observed.lock().unwrap().take() {
            assert_eq!(observed.read(|value| *value).unwrap(), 17);
        }
        ready(if self.fail_load {
            Err(ProviderFailure(Cell::new(42)))
        } else {
            Ok(self.stored)
        })
    }

    fn subscribe(
        &self,
        _: SharedSubscribeContext,
        _: SharedSubscriber<i32>,
    ) -> Result<(), ProviderFailure> {
        self.calls.lock().unwrap().push("subscribe");
        if self.fail_subscription {
            Err(ProviderFailure(Cell::new(43)))
        } else {
            Ok(())
        }
    }
}

impl SharedKey for FailingKey {
    fn save(&self, _: &i32) -> impl Future<Output = Result<(), ProviderFailure>> + Send {
        ready(Ok(()))
    }
}

// Rebinding after the load would expose the old value inside the provider callback.
#[test]
fn failed_replacement_load_preserves_the_new_binding_and_original_cause() {
    let cache = SharedKeyCache::new();
    let shared = Shared::new(5);
    let captured = shared.reader().unwrap();
    let calls = Arc::new(Mutex::new(Vec::new()));
    let key = FailingKey {
        fail_load: true,
        fail_subscription: false,
        stored: None,
        observed: Mutex::new(Some(shared.clone())),
        calls: Arc::clone(&calls),
    };
    let error = block_on(cache.load(&shared, key, |_| 17)).unwrap_err();
    assert_eq!(error.into_failure().unwrap().0.get(), 42);
    assert_eq!(shared.read(|value| *value).unwrap(), 17);
    assert_eq!(captured.read(|value| *value).unwrap(), 5);
    assert_eq!(*calls.lock().unwrap(), ["subscribe", "load"]);
}

// A failed provisional subscription must leave no reusable cache publication.
#[test]
fn subscription_failure_does_not_publish_and_retry_runs_fresh_initial_work() {
    let cache = SharedKeyCache::new();
    let calls = Arc::new(Mutex::new(Vec::new()));
    let key = |fail_subscription| FailingKey {
        fail_load: false,
        fail_subscription,
        stored: None,
        observed: Mutex::new(None),
        calls: Arc::clone(&calls),
    };
    let error = block_on(cache.reader(key(true), || 1)).unwrap_err();
    assert_eq!(error.into_failure().unwrap().0.get(), 43);
    let reader = block_on(cache.reader(key(false), || 2)).unwrap();
    assert_eq!(reader.read(|value| *value).unwrap(), 2);
    assert_eq!(
        *calls.lock().unwrap(),
        ["load", "subscribe", "load", "subscribe"]
    );
}

// Initial load failure is neither absence nor permission to subscribe.
#[test]
fn initial_load_failure_does_not_subscribe_or_publish() {
    let calls = Arc::new(Mutex::new(Vec::new()));
    let key = FailingKey {
        fail_load: true,
        fail_subscription: false,
        stored: None,
        observed: Mutex::new(None),
        calls: Arc::clone(&calls),
    };
    let error = block_on(SharedKeyCache::new().reader(key, || 1)).unwrap_err();
    assert_eq!(error.into_failure().unwrap().0.get(), 42);
    assert_eq!(*calls.lock().unwrap(), ["load"]);
}

// Require must propagate each provider failure and admit no provisional owner.
#[test]
fn require_failures_preserve_the_original_cause_and_allow_fresh_construction() {
    for (fail_load, fail_subscription, expected_cause, expected_calls) in [
        (true, false, 42, vec!["load", "load", "subscribe"]),
        (
            false,
            true,
            43,
            vec!["load", "subscribe", "load", "subscribe"],
        ),
    ] {
        let cache = SharedKeyCache::new();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let key = |fail_load, fail_subscription| FailingKey {
            fail_load,
            fail_subscription,
            stored: Some(11),
            observed: Mutex::new(None),
            calls: Arc::clone(&calls),
        };
        let error = block_on(cache.require_reader(key(fail_load, fail_subscription))).unwrap_err();
        assert!(!error.is_absent());
        assert!(!error.is_collision());
        assert_eq!(error.into_failure().unwrap().0.get(), expected_cause);
        let seeded = Cell::new(false);
        let reader = block_on(cache.reader(key(false, false), || {
            seeded.set(true);
            3
        }))
        .unwrap();
        assert!(seeded.get());
        assert_eq!(reader.read(|value| *value).unwrap(), 11);
        assert_eq!(*calls.lock().unwrap(), expected_calls);
    }
}

/// Returns a stored optional absence, which differs from missing storage.
struct OptionalKey;
impl SharedReaderKey for OptionalKey {
    type Value = Option<u32>;
    type Error = Infallible;
    type Subscription = ();
    fn identity(&self) -> SharedKeyIdentity {
        SharedKeyIdentity::new::<NumberCodec>("optional", "v1")
    }
    fn load(
        &self,
        _: SharedLoadContext<'_, Option<u32>>,
    ) -> impl Future<Output = Result<Option<Option<u32>>, Infallible>> + Send {
        ready(Ok(Some(None)))
    }
    fn subscribe(
        &self,
        _: SharedSubscribeContext,
        _: SharedSubscriber<Option<u32>>,
    ) -> Result<(), Infallible> {
        Ok(())
    }
}

#[test]
fn required_stored_optional_none_is_present_not_missing() {
    let reader = block_on(SharedKeyCache::new().require_reader(OptionalKey)).unwrap();
    assert_eq!(reader.read(|value| *value).unwrap(), None);
}

/// Stops synchronous subscription while the test resets the same cache namespace.
struct PausedKey {
    /// Reports that construction reached its subscription callback.
    entered: std::sync::mpsc::Sender<()>,
    /// Releases that callback only after the reset has completed.
    resume: Arc<Mutex<std::sync::mpsc::Receiver<()>>>,
}
impl SharedReaderKey for PausedKey {
    type Value = u32;
    type Error = Infallible;
    type Subscription = ();
    fn identity(&self) -> SharedKeyIdentity {
        SharedKeyIdentity::new::<NumberCodec>("paused", "v1")
    }
    fn load(
        &self,
        _: SharedLoadContext<'_, u32>,
    ) -> impl Future<Output = Result<Option<u32>, Infallible>> + Send {
        ready(Ok(None))
    }
    fn subscribe(
        &self,
        _: SharedSubscribeContext,
        _: SharedSubscriber<u32>,
    ) -> Result<(), Infallible> {
        self.entered.send(()).unwrap();
        self.resume.lock().unwrap().recv().unwrap();
        Ok(())
    }
}

// Reset removes current lookup only; a constructor already building may publish afterwards.
#[test]
fn reset_during_construction_allows_post_reset_publication() {
    // Contain blocked subscription/joins without changing the controlled schedule.
    // Miri runs the same scenario directly under the surrounding job's limit.
    #[cfg(not(miri))]
    if subprocess::run_in_child("reset_during_construction_allows_post_reset_publication") {
        return;
    }

    let cache = SharedKeyCache::new();
    let (entered, at_subscription) = std::sync::mpsc::channel();
    let (resume, paused) = std::sync::mpsc::channel();
    let paused = Arc::new(Mutex::new(paused));
    let make_key = || PausedKey {
        entered: entered.clone(),
        resume: Arc::clone(&paused),
    };
    let building_cache = cache.clone();
    let key = make_key();
    let building = std::thread::spawn(move || block_on(building_cache.reader(key, || 7)).unwrap());
    at_subscription.recv().unwrap();
    cache.reset().unwrap();
    resume.send(()).unwrap();
    let owner = building.join().unwrap();
    let reused =
        block_on(cache.reader(make_key(), || panic!("post-reset owner must be found"))).unwrap();
    assert_eq!(owner.read(|value| *value).unwrap(), 7);
    assert_eq!(reused.read(|value| *value).unwrap(), 7);
    assert!(at_subscription.try_recv().is_err());
}

/// Separates backing storage, cache identity and observable provider events.
#[derive(Clone)]
struct MemoryKey {
    id: &'static str,
    configuration: &'static str,
    stored: Arc<Mutex<Option<i32>>>,
    events: Arc<Mutex<Vec<String>>>,
    /// Changes only the codec component of an otherwise matching identity.
    other_codec: bool,
    /// Holds both racing constructors before either can publish its candidate.
    subscribe_barrier: Option<Arc<Barrier>>,
}

impl MemoryKey {
    /// Starts a separate backing store and event log with the ordinary identity.
    fn new(stored: Option<i32>) -> Self {
        Self {
            id: "number",
            configuration: "v1",
            stored: Arc::new(Mutex::new(stored)),
            events: Arc::new(Mutex::new(Vec::new())),
            other_codec: false,
            subscribe_barrier: None,
        }
    }

    /// Records seed evaluation without retaining a borrow of the provider.
    fn seed(&self, value: i32) -> impl FnOnce() -> i32 + use<> {
        let events = Arc::clone(&self.events);
        move || {
            events.lock().unwrap().push(format!("seed:{value}"));
            value
        }
    }

    /// Copies the event sequence so assertions do not hold its recording lock.
    fn events(&self) -> Vec<String> {
        self.events.lock().unwrap().clone()
    }
}

/// Records when the last captured owner releases its provider subscription.
struct Subscription(Arc<Mutex<Vec<String>>>);

impl Drop for Subscription {
    fn drop(&mut self) {
        self.0.lock().unwrap().push("unsubscribe".into());
    }
}

impl SharedReaderKey for MemoryKey {
    type Value = i32;
    type Error = Infallible;
    type Subscription = Subscription;

    fn identity(&self) -> SharedKeyIdentity {
        if self.other_codec {
            SharedKeyIdentity::new::<OtherCodec>(self.id, self.configuration)
        } else {
            SharedKeyIdentity::new::<NumberCodec>(self.id, self.configuration)
        }
    }

    fn load(
        &self,
        context: SharedLoadContext<'_, i32>,
    ) -> impl Future<Output = Result<Option<i32>, Infallible>> + Send {
        self.events.lock().unwrap().push(match context {
            SharedLoadContext::Initial(value) => format!("initial:{value}"),
            SharedLoadContext::UserInitiated => "reload".into(),
        });
        ready(Ok(*self.stored.lock().unwrap()))
    }

    fn subscribe(
        &self,
        context: SharedSubscribeContext,
        _: SharedSubscriber<i32>,
    ) -> Result<Subscription, Infallible> {
        self.events.lock().unwrap().push(match context {
            SharedSubscribeContext::Initial => "subscribe:initial".into(),
            SharedSubscribeContext::UserInitiated => "subscribe:user".into(),
        });
        if let Some(barrier) = &self.subscribe_barrier {
            barrier.wait();
        }
        Ok(Subscription(Arc::clone(&self.events)))
    }
}

impl SharedKey for MemoryKey {
    fn save(&self, value: &i32) -> impl Future<Output = Result<(), Infallible>> + Send {
        *self.stored.lock().unwrap() = Some(*value);
        ready(Ok(()))
    }
}

// Loading first would wrongly suppress the fresh-owner seed when bytes exist.
#[test]
fn fresh_present_owner_evaluates_seed_before_load_and_subscription() {
    let cache = SharedKeyCache::new();
    let key = MemoryKey::new(Some(9));
    let shared = block_on(cache.shared(key.clone(), key.seed(3))).unwrap();
    assert_eq!(shared.read(|value| *value).unwrap(), 9);
    assert_eq!(key.events(), ["seed:3", "initial:3", "subscribe:initial"]);
}

// Reconstructing on a hit would run the second seed and replace live edits.
#[test]
fn live_hit_keeps_first_default_without_new_seed_load_or_subscription() {
    let cache = SharedKeyCache::new();
    let key = MemoryKey::new(None);
    let first = block_on(cache.shared(key.clone(), key.seed(3))).unwrap();
    first
        .try_update(
            |value| {
                *value = 7;
                Ok::<_, ()>(())
            },
            || {},
        )
        .unwrap();
    let second = block_on(cache.shared(key.clone(), key.seed(20))).unwrap();
    assert_eq!(second.read(|value| *value).unwrap(), 7);
    assert_eq!(key.events(), ["seed:3", "initial:3", "subscribe:initial"]);
}

// A require cache fast path would incorrectly succeed when storage is absent.
#[test]
fn require_checks_absence_before_reusing_live_owner() {
    let cache = SharedKeyCache::new();
    let key = MemoryKey::new(None);
    let _existing = block_on(cache.shared(key.clone(), key.seed(3))).unwrap();
    let error = block_on(cache.require(key.clone())).unwrap_err();
    assert!(error.is_absent());
    assert!(!error.is_collision());
    assert!(!error.is_poisoned());
    assert!(error.access_error().is_none());
    assert!(std::error::Error::source(&error).is_none());
    assert!(error.to_string().contains("absent"));
    assert!(error.into_failure().is_none());
    assert_eq!(
        key.events(),
        ["seed:3", "initial:3", "subscribe:initial", "reload"]
    );
}

// Fresh require must not start a second implicit load after its user load.
#[test]
fn fresh_require_uses_loaded_value_without_seed_or_initial_load() {
    let cache = SharedKeyCache::new();
    let key = MemoryKey::new(Some(11));
    let shared = block_on(cache.require(key.clone())).unwrap();
    assert_eq!(shared.read(|value| *value).unwrap(), 11);
    assert_eq!(key.events(), ["reload", "subscribe:user"]);
}

// Tying the resource to a writer/cohort would cancel while captures survive.
#[test]
fn captured_projection_retains_subscription_until_last_reference_drop() {
    let cache = SharedKeyCache::new();
    let key = MemoryKey::new(Some(11));
    let shared = block_on(cache.shared(key.clone(), key.seed(3))).unwrap();
    let reader = shared.reader().unwrap();
    let projection = reader.project(|value| Some(value));
    drop(shared);
    drop(reader);
    assert_eq!(projection.read(|value| *value).unwrap(), 11);
    assert_eq!(key.events(), ["seed:3", "initial:3", "subscribe:initial"]);
    drop(projection);
    assert_eq!(
        key.events(),
        ["seed:3", "initial:3", "subscribe:initial", "unsubscribe"]
    );
    assert_eq!(*key.stored.lock().unwrap(), Some(11));
}

// Reset is lookup invalidation, not data revocation or storage clearing.
#[test]
fn reset_keeps_existing_handle_and_backing_storage() {
    let cache = SharedKeyCache::new();
    let key = MemoryKey::new(Some(11));
    let first = block_on(cache.shared(key.clone(), key.seed(3))).unwrap();
    cache.reset().unwrap();
    let second = block_on(cache.shared(key.clone(), key.seed(4))).unwrap();
    first
        .try_update(
            |value| {
                *value = 20;
                Ok::<_, ()>(())
            },
            || {},
        )
        .unwrap();
    assert_eq!(first.read(|value| *value).unwrap(), 20);
    assert_eq!(second.read(|value| *value).unwrap(), 11);
    assert_eq!(*key.stored.lock().unwrap(), Some(11));
    assert_eq!(
        key.events(),
        [
            "seed:3",
            "initial:3",
            "subscribe:initial",
            "seed:4",
            "initial:4",
            "subscribe:initial",
        ]
    );
    drop(first);
    let third =
        block_on(cache.shared(key, || panic!("old-owner cleanup removed the new slot"))).unwrap();
    second
        .try_update(
            |value| {
                *value = 31;
                Ok::<_, ()>(())
            },
            || {},
        )
        .unwrap();
    assert_eq!(third.read(|value| *value).unwrap(), 31);
}

// Returning the winner's cohort would make separately constructed handles rebind together.
#[test]
fn cache_hits_share_reference_but_not_rebinding_cohorts() {
    let cache = SharedKeyCache::new();
    let key = MemoryKey::new(None);
    let first = block_on(cache.shared(key.clone(), key.seed(3))).unwrap();
    let second = block_on(cache.shared(key.clone(), key.seed(4))).unwrap();
    let clone = first.clone();
    let captured = first.reader().unwrap();
    first.rebind(&Shared::new(20)).unwrap();
    assert_eq!(clone.read(|value| *value).unwrap(), 20);
    assert_eq!(second.read(|value| *value).unwrap(), 3);
    assert_eq!(captured.read(|value| *value).unwrap(), 3);
}

// One ambient registry would let a second namespace reuse the first live value.
#[test]
fn separate_namespaces_do_not_reuse_each_others_live_owners() {
    let first_cache = SharedKeyCache::new();
    let second_cache = SharedKeyCache::new();
    let key = MemoryKey::new(None);
    let first = block_on(first_cache.shared(key.clone(), key.seed(3))).unwrap();
    let second = block_on(second_cache.shared(key.clone(), key.seed(4))).unwrap();
    assert_eq!(first.read(|value| *value).unwrap(), 3);
    assert_eq!(second.read(|value| *value).unwrap(), 4);
}

// Reinterpreting a live reference with a different codec must fail before new work.
#[test]
fn same_key_with_different_codec_fails_before_seed_or_load() {
    let cache = SharedKeyCache::new();
    let key = MemoryKey::new(None);
    let _owner = block_on(cache.shared(key.clone(), key.seed(3))).unwrap();
    let mut incompatible = key.clone();
    incompatible.other_codec = true;
    let error = block_on(cache.shared(incompatible, key.seed(4))).unwrap_err();
    assert!(error.is_collision());
    assert!(!error.is_absent());
    assert!(!error.is_poisoned());
    assert!(error.access_error().is_none());
    assert!(std::error::Error::source(&error).is_none());
    assert!(error.to_string().contains("incompatible"));
    assert!(error.into_failure().is_none());
    assert_eq!(key.events(), ["seed:3", "initial:3", "subscribe:initial"]);
}

// Omitting configuration equality must not reuse an incompatible live owner.
#[test]
fn same_key_with_different_configuration_fails_before_seed_or_load() {
    let cache = SharedKeyCache::new();
    let key = MemoryKey::new(None);
    let owner = block_on(cache.reader(key.clone(), key.seed(3))).unwrap();
    let mut incompatible = key.clone();
    incompatible.configuration = "v2";
    assert!(
        block_on(cache.reader(incompatible, key.seed(4)))
            .unwrap_err()
            .is_collision()
    );
    assert_eq!(owner.read(|value| *value).unwrap(), 3);
    assert_eq!(key.events(), ["seed:3", "initial:3", "subscribe:initial"]);
}

// A compatible address does not permit a differently configured late winner.
#[test]
fn configuration_only_late_collision_preserves_winner_and_drops_loser() {
    let cache = SharedKeyCache::new();
    let key = MemoryKey::new(None);
    let mut winning_key = key.clone();
    winning_key.configuration = "v2";
    let mut winner = None;
    let error = block_on(cache.reader(key.clone(), || {
        let mut inner = std::pin::pin!(cache.reader(winning_key.clone(), key.seed(7)));
        let mut context = std::task::Context::from_waker(std::task::Waker::noop());
        let std::task::Poll::Ready(Ok(owner)) = inner.as_mut().poll(&mut context) else {
            panic!("inner candidate must publish immediately");
        };
        winner = Some(owner);
        key.seed(3)()
    }))
    .unwrap_err();
    assert!(error.is_collision());
    assert_eq!(winner.as_ref().unwrap().read(|value| *value).unwrap(), 7);
    assert_eq!(
        key.events(),
        [
            "seed:7",
            "initial:7",
            "subscribe:initial",
            "seed:3",
            "initial:3",
            "subscribe:initial",
            "unsubscribe",
        ]
    );
    let reused =
        block_on(cache.reader(winning_key, || panic!("winner must remain cached"))).unwrap();
    drop(winner);
    assert_eq!(reused.read(|value| *value).unwrap(), 7);
    assert_eq!(key.events().len(), 7);
    drop(reused);
    assert_eq!(
        key.events(),
        [
            "seed:7",
            "initial:7",
            "subscribe:initial",
            "seed:3",
            "initial:3",
            "subscribe:initial",
            "unsubscribe",
            "unsubscribe",
        ]
    );
}

// Applying the same-address shortcut to reload would load an unrelated resource.
#[test]
fn reload_rejects_different_address_before_provider_work() {
    let cache = SharedKeyCache::new();
    let key = MemoryKey::new(None);
    let reader = block_on(cache.reader(key.clone(), key.seed(3))).unwrap();
    let mut other = key.clone();
    other.id = "other-number";
    *key.stored.lock().unwrap() = Some(20);
    assert!(
        block_on(cache.reload_reader(&reader, &other))
            .unwrap_err()
            .is_collision()
    );
    assert_eq!(reader.read(|value| *value).unwrap(), 3);
    assert_eq!(key.events(), ["seed:3", "initial:3", "subscribe:initial"]);
}

// Require performs its load but must not overwrite an already published winner.
#[test]
fn present_require_retains_existing_cached_value() {
    let cache = SharedKeyCache::new();
    let key = MemoryKey::new(None);
    let _first = block_on(cache.shared(key.clone(), key.seed(3))).unwrap();
    *key.stored.lock().unwrap() = Some(20);
    let required = block_on(cache.require(key.clone())).unwrap();
    assert_eq!(required.read(|value| *value).unwrap(), 3);
    assert_eq!(
        key.events(),
        ["seed:3", "initial:3", "subscribe:initial", "reload"]
    );
}

// A reload missing result must not revert edits to the construction default.
#[test]
fn missing_reload_retains_latest_value() {
    let cache = SharedKeyCache::new();
    let key = MemoryKey::new(None);
    let shared = block_on(cache.shared(key.clone(), key.seed(3))).unwrap();
    shared
        .try_update(
            |value| {
                *value = 17;
                Ok::<_, ()>(())
            },
            || {},
        )
        .unwrap();
    block_on(cache.reload(&shared, &key)).unwrap();
    assert_eq!(shared.read(|value| *value).unwrap(), 17);
    assert_eq!(
        key.events(),
        ["seed:3", "initial:3", "subscribe:initial", "reload"]
    );
}

// Debug must not evaluate a seed; direct access deliberately does each time.
#[test]
fn default_diagnostics_are_inert_but_direct_access_is_not_lazy() {
    let key = MemoryKey::new(None);
    let events = Arc::clone(&key.events);
    let default = SharedKeyDefault::new(key.clone(), move || {
        events.lock().unwrap().push("default".into());
        5
    });
    let _ = format!("{default:?}");
    assert!(key.events().is_empty());
    assert_eq!(default.initial_value(), 5);
    assert_eq!(default.initial_value(), 5);
    assert_eq!(key.events(), ["default", "default"]);
}

// Including a default closure's type in identity would split the base-key owner.
#[test]
fn different_default_closures_reuse_the_base_key_owner() {
    let cache = SharedKeyCache::new();
    let key = MemoryKey::new(None);
    let first = block_on(cache.shared_default(SharedKeyDefault::new(key.clone(), || 3))).unwrap();
    let second = block_on(cache.shared_default(SharedKeyDefault::new(key.clone(), || 4))).unwrap();
    assert_eq!(first.read(|value| *value).unwrap(), 3);
    assert_eq!(second.read(|value| *value).unwrap(), 3);
    assert_eq!(key.events(), ["initial:3", "subscribe:initial"]);
}

#[test]
fn default_readers_reuse_live_values_and_retain_one_subscription() {
    let cache = SharedKeyCache::new();
    let key = MemoryKey::new(None);
    let seeds = Cell::new(0);
    let first = block_on(cache.reader_default(SharedKeyDefault::new(key.clone(), || {
        seeds.set(seeds.get() + 1);
        3
    })))
    .unwrap();
    assert_eq!(seeds.get(), 1);
    assert_eq!(first.read(|value| *value).unwrap(), 3);
    let second = block_on(cache.reader_default(SharedKeyDefault::new(key.clone(), || {
        panic!("a live reader must suppress the second default")
    })))
    .unwrap();
    let writer = block_on(cache.shared(key.clone(), || {
        panic!("a default reader must use the base key's cache identity")
    }))
    .unwrap();
    writer
        .try_update(
            |value| {
                *value = 17;
                Ok::<_, ()>(())
            },
            || {},
        )
        .unwrap();
    assert_eq!(first.read(|value| *value).unwrap(), 17);
    assert_eq!(second.read(|value| *value).unwrap(), 17);
    assert_eq!(seeds.get(), 1);
    assert_eq!(key.events(), ["initial:3", "subscribe:initial"]);
    drop(writer);
    drop(first);
    assert_eq!(second.read(|value| *value).unwrap(), 17);
    assert_eq!(key.events(), ["initial:3", "subscribe:initial"]);
    drop(second);
    assert_eq!(
        key.events(),
        ["initial:3", "subscribe:initial", "unsubscribe"]
    );
    assert_eq!(*key.stored.lock().unwrap(), None);
}

/// Collides by address and codec but exposes a different value type.
struct TextKey;

impl SharedReaderKey for TextKey {
    type Value = String;
    type Error = Infallible;
    type Subscription = ();

    fn identity(&self) -> SharedKeyIdentity {
        SharedKeyIdentity::new::<NumberCodec>("number", "v1")
    }

    async fn load(&self, _: SharedLoadContext<'_, String>) -> Result<Option<String>, Infallible> {
        panic!("colliding value type must not start a load");
    }

    fn subscribe(
        &self,
        _: SharedSubscribeContext,
        _: SharedSubscriber<String>,
    ) -> Result<(), Infallible> {
        panic!("colliding value type must not start a subscription");
    }
}

// Type-indexed maps would hide this collision in a separate slot and run its seed.
#[test]
fn one_namespace_rejects_a_different_value_type_at_the_same_address() {
    let cache = SharedKeyCache::new();
    let key = MemoryKey::new(None);
    let _owner = block_on(cache.shared(key.clone(), key.seed(3))).unwrap();
    assert!(
        block_on(cache.reader(TextKey, || panic!("colliding seed must not run")))
            .unwrap_err()
            .is_collision()
    );
}

/// Preserves identity fields while changing the concrete provider type.
struct OtherKey(MemoryKey);

impl SharedReaderKey for OtherKey {
    type Value = i32;
    type Error = Infallible;
    type Subscription = Subscription;

    fn identity(&self) -> SharedKeyIdentity {
        self.0.identity()
    }

    async fn load(&self, _: SharedLoadContext<'_, i32>) -> Result<Option<i32>, Infallible> {
        panic!("colliding concrete key must not start a load");
    }

    fn subscribe(
        &self,
        _: SharedSubscribeContext,
        _: SharedSubscriber<i32>,
    ) -> Result<Subscription, Infallible> {
        panic!("colliding concrete key must not start a subscription");
    }
}

impl SharedKey for OtherKey {
    async fn save(&self, _: &i32) -> Result<(), Infallible> {
        panic!("colliding concrete key must not start a save");
    }
}

// Value and codec compatibility do not make an unrelated key implementation safe.
#[test]
fn one_namespace_rejects_a_different_concrete_key_at_the_same_address() {
    let cache = SharedKeyCache::new();
    let key = MemoryKey::new(None);
    let _owner = block_on(cache.shared(key.clone(), key.seed(3))).unwrap();
    assert!(
        block_on(cache.reader(OtherKey(key.clone()), key.seed(4)))
            .unwrap_err()
            .is_collision()
    );
}

// Invalid captures must fail before the supplied provider can perform a reload.
#[test]
fn reload_rejects_unkeyed_and_wrong_concrete_key_readers_before_provider_work() {
    let cache = SharedKeyCache::new();
    let key = MemoryKey::new(None);
    let unkeyed = Shared::new(5).reader().unwrap();
    assert!(
        block_on(cache.reload_reader(&unkeyed, &key))
            .unwrap_err()
            .is_collision()
    );
    assert!(key.events().is_empty());
    assert_eq!(unkeyed.read(|value| *value).unwrap(), 5);

    let keyed = block_on(cache.reader(key.clone(), key.seed(3))).unwrap();
    assert!(
        block_on(cache.reload_reader(&keyed, &OtherKey(key.clone())))
            .unwrap_err()
            .is_collision()
    );
    assert_eq!(keyed.read(|value| *value).unwrap(), 3);
    assert_eq!(key.events(), ["seed:3", "initial:3", "subscribe:initial"]);
}

/// Is `Send` without being `Sync`, `Clone` or `Debug`.
struct MoveOnly(Cell<u32>);
/// Supplies no stored value, leaving the caller's move-only seed intact.
struct MoveOnlyKey;

impl SharedReaderKey for MoveOnlyKey {
    type Value = MoveOnly;
    type Error = Infallible;
    type Subscription = ();

    fn identity(&self) -> SharedKeyIdentity {
        SharedKeyIdentity::new::<NumberCodec>("move-only", "v1")
    }

    fn load(
        &self,
        _: SharedLoadContext<'_, MoveOnly>,
    ) -> impl Future<Output = Result<Option<MoveOnly>, Infallible>> + Send {
        ready(Ok(None))
    }

    fn subscribe(
        &self,
        _: SharedSubscribeContext,
        _: SharedSubscriber<MoveOnly>,
    ) -> Result<(), Infallible> {
        Ok(())
    }
}

// Requiring Value: Sync/Clone/Debug would reject this Send-only move-only payload.
#[test]
fn cached_readers_transport_send_only_values_without_domain_clone_or_debug() {
    let cache = SharedKeyCache::new();
    let reader = block_on(cache.reader(MoveOnlyKey, || MoveOnly(Cell::new(9)))).unwrap();
    let result = std::thread::spawn(move || reader.read(|value| value.0.get()).unwrap());
    assert_eq!(result.join().unwrap(), 9);
}

// Publishing candidates without a second check would split simultaneous owners.
#[test]
fn racing_candidates_share_one_winner_and_release_the_losing_subscription() {
    // Contain partial thread launch and stalled barriers with the shared watchdog.
    // Miri runs the same scenario directly under the surrounding job's limit.
    #[cfg(not(miri))]
    if subprocess::run_in_child(
        "racing_candidates_share_one_winner_and_release_the_losing_subscription",
    ) {
        return;
    }

    let cache = SharedKeyCache::new();
    let mut key = MemoryKey::new(None);
    key.subscribe_barrier = Some(Arc::new(Barrier::new(2)));
    let first_cache = cache.clone();
    let first_key = key.clone();
    let first = std::thread::spawn(move || {
        block_on(first_cache.shared(first_key.clone(), first_key.seed(3))).unwrap()
    });
    let second_cache = cache.clone();
    let second_key = key.clone();
    let second = std::thread::spawn(move || {
        block_on(second_cache.shared(second_key.clone(), second_key.seed(4))).unwrap()
    });
    let first = first.join().unwrap();
    let second = second.join().unwrap();
    first
        .try_update(
            |value| {
                *value = 19;
                Ok::<_, ()>(())
            },
            || {},
        )
        .unwrap();
    assert_eq!(second.read(|value| *value).unwrap(), 19);
    let events = key.events();
    assert_eq!(
        events
            .iter()
            .filter(|event| event.starts_with("seed:"))
            .count(),
        2
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| event.starts_with("initial:"))
            .count(),
        2
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| *event == "subscribe:initial")
            .count(),
        2
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| *event == "unsubscribe")
            .count(),
        1
    );
    drop(first);
    drop(second);
    assert_eq!(
        key.events()
            .iter()
            .filter(|event| *event == "unsubscribe")
            .count(),
        2
    );
}

// Replacement must use the current value, skip initial load, then reload explicitly.
#[test]
fn default_key_replacement_uses_current_seed_without_evaluating_its_default() {
    let cache = SharedKeyCache::new();
    let source = MemoryKey::new(None);
    let shared = block_on(cache.shared(source.clone(), source.seed(3))).unwrap();
    let captured = shared.reader().unwrap();
    shared
        .try_update(
            |value| {
                *value = 15;
                Ok::<_, ()>(())
            },
            || {},
        )
        .unwrap();
    let mut target = MemoryKey::new(None);
    target.id = "target";
    let default = SharedKeyDefault::new(target.clone(), || panic!("replacement ignores default"));
    block_on(cache.load(&shared, default.into_key(), |value| *value)).unwrap();
    assert_eq!(shared.read(|value| *value).unwrap(), 15);
    assert_eq!(captured.read(|value| *value).unwrap(), 15);
    assert_eq!(target.events(), ["subscribe:user", "reload"]);
    shared
        .try_update(
            |value| {
                *value = 21;
                Ok::<_, ()>(())
            },
            || {},
        )
        .unwrap();
    assert_eq!(captured.read(|value| *value).unwrap(), 15);
}

// A replacement cache hit must not evaluate even the caller's snapshot closure.
#[test]
fn replacement_hit_reuses_subscription_and_suppresses_current_value_seed() {
    let cache = SharedKeyCache::new();
    let source = Shared::new(15);
    let target = MemoryKey::new(None);
    let existing = block_on(cache.shared(target.clone(), target.seed(4))).unwrap();
    block_on(cache.load(&source, target.clone(), |_| panic!("hit must not seed"))).unwrap();
    assert_eq!(source.read(|value| *value).unwrap(), 4);
    assert_eq!(existing.read(|value| *value).unwrap(), 4);
    assert_eq!(
        target.events(),
        ["seed:4", "initial:4", "subscribe:initial", "reload"]
    );
}

// Reload uses its supplied provider; replacement hits retain the winning provider.
#[test]
fn reload_uses_supplied_provider_but_replacement_hits_use_retained_provider() {
    /// Keeps the real future type while requiring transport across threads.
    fn assert_send<T: Send>(value: T) -> T {
        value
    }

    let cache = SharedKeyCache::new();
    let retained = MemoryKey::new(Some(11));
    let supplied = MemoryKey::new(Some(23));
    let owner = block_on(cache.shared(retained.clone(), retained.seed(1))).unwrap();
    let captured = owner.reader().unwrap();

    block_on(assert_send(cache.reload_reader(&captured, &supplied))).unwrap();
    assert_eq!(owner.read(|value| *value).unwrap(), 23);
    assert_eq!(supplied.events(), ["reload"]);
    assert_eq!(
        retained.events(),
        ["seed:1", "initial:1", "subscribe:initial"]
    );

    let replacement = Shared::new(5);
    block_on(assert_send(cache.load(
        &replacement,
        supplied.clone(),
        |_| panic!("hit must not seed"),
    )))
    .unwrap();
    assert_eq!(replacement.read(|value| *value).unwrap(), 11);
    assert_eq!(owner.read(|value| *value).unwrap(), 11);
    assert_eq!(supplied.events(), ["reload"]);
    assert_eq!(
        retained.events(),
        ["seed:1", "initial:1", "subscribe:initial", "reload"]
    );

    *retained.stored.lock().unwrap() = Some(17);
    let mut reader = Shared::new(7).reader().unwrap();
    block_on(assert_send(cache.load_reader(
        &mut reader,
        supplied.clone(),
        |_| panic!("hit must not seed"),
    )))
    .unwrap();
    assert_eq!(reader.read(|value| *value).unwrap(), 17);
    assert_eq!(owner.read(|value| *value).unwrap(), 17);
    assert_eq!(replacement.read(|value| *value).unwrap(), 17);
    assert_eq!(supplied.events(), ["reload"]);
    assert_eq!(
        retained.events(),
        [
            "seed:1",
            "initial:1",
            "subscribe:initial",
            "reload",
            "reload"
        ]
    );
}

// A replacement race retains the winning provider and tears down the loser first.
#[test]
fn replacement_race_drops_the_losing_subscription_before_reloading_the_winner() {
    for reader_only in [false, true] {
        let cache = SharedKeyCache::new();
        let source = Shared::new(5);
        let retained = MemoryKey::new(Some(11));
        let mut supplied = MemoryKey::new(Some(23));
        supplied.events = Arc::clone(&retained.events);
        let mut winner = None;
        let seed = |value: &i32| {
            assert_eq!(*value, 5);
            let mut inner = std::pin::pin!(cache.reader(retained.clone(), retained.seed(7)));
            let mut context = std::task::Context::from_waker(std::task::Waker::noop());
            winner = Some(match inner.as_mut().poll(&mut context) {
                std::task::Poll::Ready(result) => result.unwrap(),
                std::task::Poll::Pending => panic!("fixture provider must complete immediately"),
            });
            *retained.stored.lock().unwrap() = Some(17);
            29
        };
        let selected = if reader_only {
            let mut reader = source.reader().unwrap();
            block_on(cache.load_reader(&mut reader, supplied, seed)).unwrap();
            reader
        } else {
            block_on(cache.load(&source, supplied, seed)).unwrap();
            source.reader().unwrap()
        };
        assert_eq!(selected.read(|value| *value).unwrap(), 17);
        assert_eq!(winner.as_ref().unwrap().read(|value| *value).unwrap(), 17);
        assert_eq!(
            source.read(|value| *value).unwrap(),
            if reader_only { 5 } else { 17 }
        );
        assert_eq!(
            retained.events(),
            [
                "seed:7",
                "initial:7",
                "subscribe:initial",
                "subscribe:user",
                "unsubscribe",
                "reload"
            ]
        );
    }
}

// Reader replacement must not retarget captured clones or recreate a live target.
#[test]
fn reader_replacement_miss_and_hit_preserve_independent_owners() {
    let cache = SharedKeyCache::new();
    let source = MemoryKey::new(None);
    let mut reader = block_on(cache.reader(source.clone(), source.seed(5))).unwrap();
    let captured = reader.clone();
    let mut target = MemoryKey::new(Some(23));
    target.id = "reader-target";
    block_on(cache.load_reader(&mut reader, target.clone(), |value| {
        assert_eq!(*value, 5);
        target.events.lock().unwrap().push("seed:17".into());
        17
    }))
    .unwrap();
    assert_eq!(reader.read(|value| *value).unwrap(), 23);
    assert_eq!(captured.read(|value| *value).unwrap(), 5);
    assert_eq!(target.events(), ["seed:17", "subscribe:user", "reload"]);
    assert_eq!(
        source.events(),
        ["seed:5", "initial:5", "subscribe:initial"]
    );

    let target_owner = reader.clone();
    let mut hit = captured.clone();
    *target.stored.lock().unwrap() = Some(29);
    block_on(cache.load_reader(&mut hit, target.clone(), |_| panic!("hit must not seed"))).unwrap();
    assert_eq!(hit.read(|value| *value).unwrap(), 29);
    assert_eq!(reader.read(|value| *value).unwrap(), 29);
    assert_eq!(target_owner.read(|value| *value).unwrap(), 29);
    assert_eq!(captured.read(|value| *value).unwrap(), 5);
    assert_eq!(
        target.events(),
        ["seed:17", "subscribe:user", "reload", "reload"]
    );
    drop(captured);
    assert_eq!(source.events().last().unwrap(), "unsubscribe");
    assert_eq!(Arc::strong_count(&source.stored), 1);
    drop(reader);
    drop(hit);
    assert_eq!(target.events().last().unwrap(), "reload");
    drop(target_owner);
    assert_eq!(target.events().last().unwrap(), "unsubscribe");
    assert_eq!(Arc::strong_count(&target.stored), 1);
}

// A failed reload must retain the replacement rather than restore the old reader.
#[test]
fn reader_failed_replacement_preserves_binding_and_original_cause() {
    let cache = SharedKeyCache::new();
    let mut reader = Shared::new(5).reader().unwrap();
    let captured = reader.clone();
    let calls = Arc::new(Mutex::new(Vec::new()));
    let key = FailingKey {
        fail_load: true,
        fail_subscription: false,
        stored: None,
        observed: Mutex::new(None),
        calls: Arc::clone(&calls),
    };
    let error = block_on(cache.load_reader(&mut reader, key, |_| 17)).unwrap_err();
    assert_eq!(error.into_failure().unwrap().0.get(), 42);
    assert_eq!(reader.read(|value| *value).unwrap(), 17);
    assert_eq!(captured.read(|value| *value).unwrap(), 5);
    assert_eq!(*calls.lock().unwrap(), ["subscribe", "load"]);
    drop(reader);
    assert_eq!(Arc::strong_count(&calls), 1);
}

// Failed subscription admission must neither rebind readers nor publish a candidate.
#[test]
fn reader_replacement_subscription_failure_preserves_old_binding() {
    let cache = SharedKeyCache::new();
    let mut reader = Shared::new(5).reader().unwrap();
    let captured = reader.clone();
    let calls = Arc::new(Mutex::new(Vec::new()));
    let key = |fail_subscription| FailingKey {
        fail_load: false,
        fail_subscription,
        stored: None,
        observed: Mutex::new(None),
        calls: Arc::clone(&calls),
    };
    let seeds = Cell::new(0);
    let seed = |value: &i32| {
        assert_eq!(*value, 5);
        seeds.set(seeds.get() + 1);
        17
    };
    let error = block_on(cache.load_reader(&mut reader, key(true), seed)).unwrap_err();
    assert_eq!(error.into_failure().unwrap().0.get(), 43);
    assert_eq!(seeds.get(), 1);
    assert_eq!(reader.read(|value| *value).unwrap(), 5);
    assert_eq!(captured.read(|value| *value).unwrap(), 5);
    assert_eq!(*calls.lock().unwrap(), ["subscribe"]);
    assert_eq!(Arc::strong_count(&calls), 1);

    block_on(cache.load_reader(&mut reader, key(false), seed)).unwrap();
    assert_eq!(seeds.get(), 2);
    assert_eq!(reader.read(|value| *value).unwrap(), 17);
    assert_eq!(captured.read(|value| *value).unwrap(), 5);
    assert_eq!(*calls.lock().unwrap(), ["subscribe", "subscribe", "load"]);
    drop(reader);
    assert_eq!(Arc::strong_count(&calls), 1);
}

// A caller override must not evaluate the default it has explicitly replaced.
#[test]
fn caller_seed_override_uses_base_identity_and_skips_default_factory() {
    let cache = SharedKeyCache::new();
    let key = MemoryKey::new(None);
    let default = SharedKeyDefault::new(key.clone(), || panic!("default was overridden"));
    let shared = block_on(cache.shared(default.into_key(), key.seed(23))).unwrap();
    assert_eq!(shared.read(|value| *value).unwrap(), 23);
    assert_eq!(key.events(), ["seed:23", "initial:23", "subscribe:initial"]);
}

// A cache is not the owner of returned data or its subscription.
#[test]
fn dropping_namespace_keeps_existing_reference_and_subscription_alive() {
    let cache = SharedKeyCache::new();
    let key = MemoryKey::new(None);
    let shared = block_on(cache.shared(key.clone(), key.seed(3))).unwrap();
    drop(cache);
    assert_eq!(shared.read(|value| *value).unwrap(), 3);
    assert_eq!(key.events(), ["seed:3", "initial:3", "subscribe:initial"]);
    drop(shared);
    assert_eq!(key.events().last().unwrap(), "unsubscribe");
}

// Lookup reset must not make the existing owner unusable for explicit reload.
#[test]
fn reset_preserves_reload_capability_but_other_namespaces_cannot_reload() {
    let cache = SharedKeyCache::new();
    let key = MemoryKey::new(None);
    let shared = block_on(cache.shared(key.clone(), key.seed(3))).unwrap();
    let other = SharedKeyCache::new();
    assert!(
        block_on(other.reload(&shared, &key))
            .unwrap_err()
            .is_collision()
    );
    cache.reset().unwrap();
    *key.stored.lock().unwrap() = Some(22);
    block_on(cache.reload(&shared, &key)).unwrap();
    assert_eq!(shared.read(|value| *value).unwrap(), 22);
}

/// Reenters the cache during provider callbacks and retains weak delivery handles.
#[derive(Clone)]
struct DeliveryKey {
    subscribers: Arc<Mutex<Vec<SharedSubscriber<i32>>>>,
    cache: SharedKeyCache,
}

impl SharedReaderKey for DeliveryKey {
    type Value = i32;
    type Error = Infallible;
    type Subscription = ();

    fn identity(&self) -> SharedKeyIdentity {
        SharedKeyIdentity::new::<NumberCodec>("delivery", "v1")
    }

    fn load(
        &self,
        _: SharedLoadContext<'_, i32>,
    ) -> impl Future<Output = Result<Option<i32>, Infallible>> + Send {
        // A provider callback under the cache mutex would deadlock this reentry.
        self.cache.reset().unwrap();
        ready(Ok(None))
    }

    fn subscribe(
        &self,
        _: SharedSubscribeContext,
        subscriber: SharedSubscriber<i32>,
    ) -> Result<(), Infallible> {
        self.cache.reset().unwrap();
        // Synchronous delivery must not inherit a value/cache construction guard.
        assert!(subscriber.yield_value(7).unwrap());
        self.subscribers.lock().unwrap().push(subscriber);
        Ok(())
    }
}

// Strong callback capture would leak the owner and permit stale delivery after drop.
#[test]
fn weak_producer_delivery_reenters_without_locks_and_cannot_revive_old_owners() {
    let cache = SharedKeyCache::new();
    let key = DeliveryKey {
        subscribers: Arc::new(Mutex::new(Vec::new())),
        cache: cache.clone(),
    };
    let reader = block_on(cache.reader(key.clone(), || 3)).unwrap();
    assert_eq!(reader.read(|value| *value).unwrap(), 7);
    let old = key.subscribers.lock().unwrap()[0].clone();
    assert!(old.yield_value(11).unwrap());
    assert_eq!(reader.read(|value| *value).unwrap(), 11);
    drop(reader);
    assert!(!old.yield_value(99).unwrap());
    let replacement = block_on(cache.reader(key, || 5)).unwrap();
    assert!(!old.yield_value(99).unwrap());
    assert_eq!(replacement.read(|value| *value).unwrap(), 7);
}

/// Keeps cache access available to the subscription's destructor.
#[derive(Clone)]
struct ReentrantKey {
    cache: SharedKeyCache,
    drops: Arc<std::sync::atomic::AtomicUsize>,
}

/// Resets the namespace on release to detect cleanup under a cache guard.
struct ReentrantSubscription(ReentrantKey);
impl Drop for ReentrantSubscription {
    fn drop(&mut self) {
        self.0.cache.reset().unwrap();
        self.0
            .drops
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }
}

impl SharedReaderKey for ReentrantKey {
    type Value = u32;
    type Error = Infallible;
    type Subscription = ReentrantSubscription;
    fn identity(&self) -> SharedKeyIdentity {
        SharedKeyIdentity::new::<NumberCodec>("reentrant-drop", "v1")
    }
    fn load(
        &self,
        _: SharedLoadContext<'_, u32>,
    ) -> impl Future<Output = Result<Option<u32>, Infallible>> + Send {
        ready(Ok(None))
    }
    fn subscribe(
        &self,
        _: SharedSubscribeContext,
        _: SharedSubscriber<u32>,
    ) -> Result<Self::Subscription, Infallible> {
        Ok(ReentrantSubscription(self.clone()))
    }
}

// A losing provisional subscription must be destroyed after releasing publication's lock.
#[test]
fn losing_candidate_and_last_owner_destructors_can_reenter_the_cache() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let cache = SharedKeyCache::new();
    let drops = Arc::new(AtomicUsize::new(0));
    let key = ReentrantKey {
        cache: cache.clone(),
        drops: Arc::clone(&drops),
    };
    let mut winner = None;
    let returned = block_on(cache.reader(key.clone(), || {
        // This provider is immediately ready; poll directly rather than
        // nesting an executor inside the outer constructor's executor.
        let mut inner = std::pin::pin!(cache.reader(key.clone(), || 7));
        let mut context = std::task::Context::from_waker(std::task::Waker::noop());
        winner = Some(match inner.as_mut().poll(&mut context) {
            std::task::Poll::Ready(result) => result.unwrap(),
            std::task::Poll::Pending => panic!("fixture provider must complete immediately"),
        });
        3
    }))
    .unwrap();
    assert_eq!(returned.read(|value| *value).unwrap(), 7);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    drop(winner);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    drop(returned);
    assert_eq!(drops.load(Ordering::SeqCst), 2);
}

// A late incompatible publication must preserve its winner and drop outside locks.
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "keeps the two concrete providers and bounded reentry schedule in one fixture"
)]
fn incompatible_provisional_publication_drops_loser_outside_cache_lock() {
    /// Records the retained candidate's real provider and subscription lifetime.
    #[derive(Clone)]
    struct WinnerKey {
        cache: SharedKeyCache,
        events: Arc<Mutex<Vec<&'static str>>>,
    }
    /// Adds the final winner release to the same event sequence.
    struct WinnerSubscription(Arc<Mutex<Vec<&'static str>>>);
    impl Drop for WinnerSubscription {
        fn drop(&mut self) {
            self.0.lock().unwrap().push("winner-drop");
        }
    }
    impl SharedReaderKey for WinnerKey {
        type Value = i32;
        type Error = Infallible;
        type Subscription = WinnerSubscription;

        fn identity(&self) -> SharedKeyIdentity {
            SharedKeyIdentity::new::<NumberCodec>("late-incompatible", "v1")
        }
        fn load(
            &self,
            _: SharedLoadContext<'_, i32>,
        ) -> impl Future<Output = Result<Option<i32>, Infallible>> + Send {
            self.events.lock().unwrap().push("winner-load");
            ready(Ok(None))
        }
        fn subscribe(
            &self,
            _: SharedSubscribeContext,
            _: SharedSubscriber<i32>,
        ) -> Result<WinnerSubscription, Infallible> {
            self.events.lock().unwrap().push("winner-subscribe");
            Ok(WinnerSubscription(Arc::clone(&self.events)))
        }
    }
    /// Keeps the address/codec but deliberately changes the concrete provider type.
    struct LosingKey(WinnerKey);
    /// Looks up the compatible winner while the losing candidate is released.
    struct LosingSubscription(WinnerKey);
    impl Drop for LosingSubscription {
        fn drop(&mut self) {
            self.0.events.lock().unwrap().push("loser-drop");
            // No fixture guard spans the public lookup; its temporary owner dies here.
            let mut lookup = std::pin::pin!(self.0.cache.reader(self.0.clone(), || {
                panic!("retained winner lookup must not rerun seed")
            }));
            let mut context = std::task::Context::from_waker(std::task::Waker::noop());
            let std::task::Poll::Ready(Ok(winner)) = lookup.as_mut().poll(&mut context) else {
                panic!("destructor must find the ready compatible winner");
            };
            assert_eq!(winner.read(|value| *value).unwrap(), 7);
            drop(winner);
            self.0.events.lock().unwrap().push("loser-reentered");
        }
    }
    impl SharedReaderKey for LosingKey {
        type Value = i32;
        type Error = Infallible;
        type Subscription = LosingSubscription;

        fn identity(&self) -> SharedKeyIdentity {
            self.0.identity()
        }
        fn load(
            &self,
            _: SharedLoadContext<'_, i32>,
        ) -> impl Future<Output = Result<Option<i32>, Infallible>> + Send {
            self.0.events.lock().unwrap().push("loser-load");
            ready(Ok(None))
        }
        fn subscribe(
            &self,
            _: SharedSubscribeContext,
            _: SharedSubscriber<i32>,
        ) -> Result<LosingSubscription, Infallible> {
            self.0.events.lock().unwrap().push("loser-subscribe");
            Ok(LosingSubscription(self.0.clone()))
        }
    }
    let cache = SharedKeyCache::new();
    let events = Arc::new(Mutex::new(Vec::new()));
    let key = WinnerKey {
        cache: cache.clone(),
        events: Arc::clone(&events),
    };
    let mut winner = None;
    {
        let mut outer = std::pin::pin!(cache.reader(LosingKey(key.clone()), || {
            events.lock().unwrap().push("loser-seed");
            let mut inner = std::pin::pin!(cache.reader(key.clone(), || {
                events.lock().unwrap().push("winner-seed");
                7
            }));
            let mut context = std::task::Context::from_waker(std::task::Waker::noop());
            let std::task::Poll::Ready(Ok(retained)) = inner.as_mut().poll(&mut context) else {
                panic!("inner candidate must publish immediately");
            };
            winner = Some(retained);
            3
        }));
        let mut context = std::task::Context::from_waker(std::task::Waker::noop());
        let std::task::Poll::Ready(Err(error)) = outer.as_mut().poll(&mut context) else {
            panic!("outer candidate must report an immediate late collision");
        };
        assert!(error.is_collision());
    };
    assert_eq!(winner.as_ref().unwrap().read(|value| *value).unwrap(), 7);
    assert_eq!(
        *events.lock().unwrap(),
        [
            "loser-seed",
            "winner-seed",
            "winner-load",
            "winner-subscribe",
            "loser-load",
            "loser-subscribe",
            "loser-drop",
            "loser-reentered",
        ]
    );
    drop(winner);
    assert_eq!(
        *events.lock().unwrap(),
        [
            "loser-seed",
            "winner-seed",
            "winner-load",
            "winner-subscribe",
            "loser-load",
            "loser-subscribe",
            "loser-drop",
            "loser-reentered",
            "winner-drop",
        ]
    );
}

// A seed panic cannot poison metadata or leave a partially initialised live slot.
#[test]
fn seed_panic_leaves_the_namespace_usable_for_fresh_construction() {
    let cache = SharedKeyCache::new();
    let key = MemoryKey::new(None);
    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = block_on(cache.shared(key.clone(), || panic!("seed failed")));
    }));
    let panic = panic.expect_err("the construction seed panic must propagate");
    assert_eq!(panic.downcast_ref::<&str>(), Some(&"seed failed"));
    assert!(key.events().is_empty());
    let owner = block_on(cache.shared(key.clone(), key.seed(9))).unwrap();
    assert_eq!(owner.read(|value| *value).unwrap(), 9);
    assert_eq!(key.events(), ["seed:9", "initial:9", "subscribe:initial"]);
}

/// Completes one user-requested load with presence, absence or a typed provider failure.
type ReloadReply<Value> = futures_channel::oneshot::Sender<Result<Option<Value>, ProviderFailure>>;

/// Queues reload replies in start order without imposing `Clone` on stored values.
struct ControlledKey<Value> {
    /// Owns only senders; dropping a pending load closes its corresponding receiver.
    replies: Arc<Mutex<std::collections::VecDeque<ReloadReply<Value>>>>,
}

impl<Value> Clone for ControlledKey<Value> {
    fn clone(&self) -> Self {
        Self {
            replies: Arc::clone(&self.replies),
        }
    }
}

impl<Value> ControlledKey<Value> {
    /// Starts with no active user-requested loads.
    fn new() -> Self {
        Self {
            replies: Arc::new(Mutex::new(std::collections::VecDeque::new())),
        }
    }

    /// Completes the earliest load and requires that its receiver still exists.
    fn reply(&self, result: Result<Option<Value>, ProviderFailure>) {
        let reply = self.take_reply();
        assert!(reply.send(result).is_ok());
    }

    /// Removes one sender so tests can complete loads in a different order.
    fn take_reply(&self) -> ReloadReply<Value> {
        self.replies.lock().unwrap().pop_front().unwrap()
    }
}

impl<Value: Send + 'static> SharedReaderKey for ControlledKey<Value> {
    type Value = Value;
    type Error = ProviderFailure;
    type Subscription = ();
    fn identity(&self) -> SharedKeyIdentity {
        SharedKeyIdentity::new::<NumberCodec>("controlled", "v1")
    }
    fn load(
        &self,
        context: SharedLoadContext<'_, Value>,
    ) -> impl Future<Output = Result<Option<Value>, ProviderFailure>> + Send {
        let receiver = matches!(context, SharedLoadContext::UserInitiated).then(|| {
            let (reply, receiver) = futures_channel::oneshot::channel();
            self.replies.lock().unwrap().push_back(reply);
            receiver
        });
        async move {
            match receiver {
                Some(receiver) => receiver.await.unwrap(),
                None => Ok(None),
            }
        }
    }
    fn subscribe(
        &self,
        _: SharedSubscribeContext,
        _: SharedSubscriber<Value>,
    ) -> Result<(), ProviderFailure> {
        Ok(())
    }
}

impl<Value: Send + 'static> SharedKey for ControlledKey<Value> {
    fn save(&self, _: &Value) -> impl Future<Output = Result<(), ProviderFailure>> + Send {
        ready(Ok(()))
    }
}

/// Polls once to establish an unfinished load before the test supplies a reply.
fn start_pending(future: std::pin::Pin<&mut impl Future>) {
    let mut context = std::task::Context::from_waker(std::task::Waker::noop());
    assert!(future.poll(&mut context).is_pending());
}

#[test]
fn controlled_reply_releases_queue_before_waking_the_receiver() {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::task::{Context, Poll, Wake, Waker};

    /// Observes queue access during the real oneshot wake without blocking or panicking.
    struct QueueWake {
        key: ControlledKey<u32>,
        notifications: AtomicUsize,
        unlocked: AtomicBool,
    }

    impl Wake for QueueWake {
        fn wake(self: Arc<Self>) {
            self.notifications.fetch_add(1, Ordering::Relaxed);
            self.unlocked
                .store(self.key.replies.try_lock().is_ok(), Ordering::Relaxed);
        }
    }

    let key = ControlledKey::new();
    let observer = Arc::new(QueueWake {
        key: key.clone(),
        notifications: AtomicUsize::new(0),
        unlocked: AtomicBool::new(false),
    });
    let waker = Waker::from(Arc::clone(&observer));
    let mut context = Context::from_waker(&waker);
    let mut pending = std::pin::pin!(key.load(SharedLoadContext::UserInitiated));
    assert!(pending.as_mut().poll(&mut context).is_pending());

    key.reply(Ok(Some(7)));
    assert_eq!(observer.notifications.load(Ordering::Relaxed), 1);
    assert!(
        observer.unlocked.load(Ordering::Relaxed),
        "reply must release its queue before waking the receiver"
    );
    let Poll::Ready(Ok(Some(value))) = pending.as_mut().poll(&mut context) else {
        panic!("the receiver must retain its completed value");
    };
    assert_eq!(value, 7);
}

// Source contention must not seed, subscribe or replace either public receiver.
#[test]
fn replacement_source_busy_preserves_binding_until_a_successful_retry() {
    for (writable, expected) in [(true, (18, 6)), (false, (6, 18))] {
        let cache = SharedKeyCache::new();
        let key = MemoryKey::new(None);
        let shared = Shared::new(5);
        let mut reader = shared.reader().unwrap();
        let captured = reader.clone();
        let seeds = Cell::new(0);
        let seed = |value: &i32| {
            seeds.set(seeds.get() + 1);
            *value + 12
        };
        let error = captured
            .read(|_| {
                if writable {
                    block_on(cache.load(&shared, key.clone(), seed))
                } else {
                    block_on(cache.load_reader(&mut reader, key.clone(), seed))
                }
            })
            .unwrap()
            .unwrap_err();
        assert!(error.access_error().unwrap().is_busy());
        assert!(!error.is_poisoned());
        assert!(
            std::error::Error::source(&error)
                .unwrap()
                .downcast_ref::<rca_core::SharedAccessError>()
                .unwrap()
                .is_busy()
        );
        assert!(error.to_string().contains("busy"));
        assert!(error.into_failure().is_none());
        assert_eq!(seeds.get(), 0);
        assert!(key.events().is_empty());
        assert_eq!(shared.read(|value| *value).unwrap(), 5);
        assert_eq!(reader.read(|value| *value).unwrap(), 5);

        shared
            .try_update(
                |value| {
                    *value = 6;
                    Ok::<_, ()>(())
                },
                || {},
            )
            .unwrap();
        assert_eq!(reader.read(|value| *value).unwrap(), 6);
        assert_eq!(captured.read(|value| *value).unwrap(), 6);

        if writable {
            block_on(cache.load(&shared, key.clone(), seed)).unwrap();
        } else {
            block_on(cache.load_reader(&mut reader, key.clone(), seed)).unwrap();
        }
        assert_eq!(seeds.get(), 1);
        assert_eq!(key.events(), ["subscribe:user", "reload"]);
        assert_eq!(
            (
                shared.read(|value| *value).unwrap(),
                reader.read(|value| *value).unwrap(),
            ),
            expected
        );
        assert_eq!(captured.read(|value| *value).unwrap(), 6);
    }
}

// A target collision must not seed or replace either receiving binding.
#[test]
fn incompatible_replacement_target_preserves_receivers_before_provider_work() {
    for writable in [true, false] {
        let cache = SharedKeyCache::new();
        let key = MemoryKey::new(None);
        let target = block_on(cache.shared(key.clone(), key.seed(3))).unwrap();
        let mut incompatible = key.clone();
        incompatible.other_codec = true;
        let shared = Shared::new(5);
        let mut reader = shared.reader().unwrap();
        let seed = |_: &i32| panic!("colliding target must not seed");
        let error = if writable {
            block_on(cache.load(&shared, incompatible, seed))
        } else {
            block_on(cache.load_reader(&mut reader, incompatible, seed))
        }
        .unwrap_err();
        assert!(error.is_collision());
        assert_eq!(shared.read(|value| *value).unwrap(), 5);
        assert_eq!(reader.read(|value| *value).unwrap(), 5);
        shared
            .try_update(
                |value| {
                    *value = 6;
                    Ok::<_, ()>(())
                },
                || {},
            )
            .unwrap();
        assert_eq!(reader.read(|value| *value).unwrap(), 6);
        assert_eq!(target.read(|value| *value).unwrap(), 3);
        assert_eq!(key.events(), ["seed:3", "initial:3", "subscribe:initial"]);
    }
}

// Concrete-key rejection must precede rebind, or restore a reader before returning.
#[test]
fn different_concrete_replacement_key_preserves_receivers_before_provider_work() {
    for writable in [true, false] {
        let cache = SharedKeyCache::new();
        let key = MemoryKey::new(None);
        let target = block_on(cache.shared(key.clone(), key.seed(3))).unwrap();
        let shared = Shared::new(5);
        let mut reader = shared.reader().unwrap();
        let seed = |_: &i32| panic!("colliding target must not seed");
        let error = if writable {
            block_on(cache.load(&shared, OtherKey(key.clone()), seed))
        } else {
            block_on(cache.load_reader(&mut reader, OtherKey(key.clone()), seed))
        }
        .unwrap_err();
        assert!(error.is_collision());
        assert_eq!(shared.read(|value| *value).unwrap(), 5);
        assert_eq!(reader.read(|value| *value).unwrap(), 5);
        shared
            .try_update(
                |value| {
                    *value = 6;
                    Ok::<_, ()>(())
                },
                || {},
            )
            .unwrap();
        assert_eq!(reader.read(|value| *value).unwrap(), 6);
        assert_eq!(target.read(|value| *value).unwrap(), 3);
        assert_eq!(key.events(), ["seed:3", "initial:3", "subscribe:initial"]);
    }
}

// Replacement seeds borrow the source, so their panic poisons it, not metadata.
#[test]
fn replacement_seed_panic_poisons_only_the_captured_source_value() {
    let cache = SharedKeyCache::new();
    let key = MemoryKey::new(None);
    let shared = Shared::new(5);
    let captured = shared.reader().unwrap();
    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = block_on(cache.load(&shared, key.clone(), |_| panic!("seed failed")));
    }));
    let panic = panic.expect_err("the replacement seed panic must propagate");
    assert_eq!(panic.downcast_ref::<&str>(), Some(&"seed failed"));
    assert!(key.events().is_empty());
    assert!(shared.read(|_| ()).unwrap_err().is_poisoned());
    assert!(captured.read(|_| ()).unwrap_err().is_poisoned());
    let error = block_on(cache.load(&shared, key.clone(), |_| {
        panic!("poisoned source must not seed")
    }))
    .unwrap_err();
    assert!(error.access_error().unwrap().is_poisoned());
    assert!(!error.is_poisoned());
    assert!(key.events().is_empty());

    let replacement = block_on(cache.shared(key.clone(), key.seed(9))).unwrap();
    shared.rebind(&replacement).unwrap();
    assert_eq!(shared.read(|value| *value).unwrap(), 9);
    assert!(captured.read(|_| ()).unwrap_err().is_poisoned());
    assert_eq!(key.events(), ["seed:9", "initial:9", "subscribe:initial"]);
}

// Deferring rebind until the provider resolves would leave the old value here.
#[test]
fn reader_replacement_binds_before_pending_reload_completes() {
    let cache = SharedKeyCache::new();
    let key = ControlledKey::new();
    let mut reader = Shared::new(5).reader().unwrap();
    let captured = reader.clone();
    let mut load = Box::pin(cache.load_reader(&mut reader, key.clone(), |_| 17));
    start_pending(load.as_mut());
    let reply = key.take_reply();
    drop(load);
    assert_eq!(reader.read(|value| *value).unwrap(), 17);
    assert_eq!(captured.read(|value| *value).unwrap(), 5);
    assert!(reply.send(Ok(Some(99))).is_err());
    drop(reader);
    assert_eq!(Arc::strong_count(&key.replies), 1);
}

// Latest-completed semantics would let A overwrite B's newer result.
#[test]
fn reload_latest_started_wins_when_newer_completes_before_older() {
    let cache = SharedKeyCache::new();
    let key = ControlledKey::new();
    let owner = block_on(cache.shared(key.clone(), || 1)).unwrap();
    let independent = block_on(cache.shared(key.clone(), || 99)).unwrap();
    let mut a = Box::pin(cache.reload(&owner, &key));
    start_pending(a.as_mut());
    let reply_a = key.take_reply();
    let mut b = Box::pin(cache.reload(&independent, &key));
    start_pending(b.as_mut());
    key.reply(Ok(Some(20)));
    block_on(b).unwrap();
    assert!(reply_a.send(Ok(Some(10))).is_ok());
    block_on(a).unwrap();
    assert_eq!(owner.read(|value| *value).unwrap(), 20);
    assert_eq!(independent.read(|value| *value).unwrap(), 20);
}

// An old completion must not commit even temporarily while latest B is pending.
#[test]
fn reload_older_completion_cannot_publish_while_latest_is_pending() {
    let cache = SharedKeyCache::new();
    let key = ControlledKey::new();
    let owner = block_on(cache.shared(key.clone(), || 1)).unwrap();
    let mut a = Box::pin(cache.reload(&owner, &key));
    let mut b = Box::pin(cache.reload(&owner, &key));
    start_pending(a.as_mut());
    start_pending(b.as_mut());
    key.reply(Ok(Some(10)));
    block_on(a).unwrap();
    assert_eq!(owner.read(|value| *value).unwrap(), 1);
    key.reply(Ok(Some(20)));
    block_on(b).unwrap();
    assert_eq!(owner.read(|value| *value).unwrap(), 20);
}

#[test]
fn reload_latest_absence_retains_current_and_does_not_revive_older() {
    let cache = SharedKeyCache::new();
    let key = ControlledKey::new();
    let owner = block_on(cache.shared(key.clone(), || 1)).unwrap();
    let mut a = Box::pin(cache.reload(&owner, &key));
    start_pending(a.as_mut());
    let reply_a = key.take_reply();
    let mut b = Box::pin(cache.reload(&owner, &key));
    start_pending(b.as_mut());
    owner
        .try_update(
            |value| {
                *value = 5;
                Ok::<_, ()>(())
            },
            || {},
        )
        .unwrap();
    key.reply(Ok(None));
    block_on(b).unwrap();
    assert!(reply_a.send(Ok(Some(10))).is_ok());
    block_on(a).unwrap();
    assert_eq!(owner.read(|value| *value).unwrap(), 5);
}

#[test]
fn reload_latest_error_preserves_typed_cause_and_does_not_revive_older() {
    let cache = SharedKeyCache::new();
    let key = ControlledKey::new();
    let owner = block_on(cache.shared(key.clone(), || 1)).unwrap();
    let mut a = Box::pin(cache.reload(&owner, &key));
    start_pending(a.as_mut());
    let reply_a = key.take_reply();
    let mut b = Box::pin(cache.reload(&owner, &key));
    start_pending(b.as_mut());
    key.reply(Err(ProviderFailure(Cell::new(42))));
    assert_eq!(block_on(b).unwrap_err().into_failure().unwrap().0.get(), 42);
    assert!(reply_a.send(Ok(Some(10))).is_ok());
    block_on(a).unwrap();
    assert_eq!(owner.read(|value| *value).unwrap(), 1);
}

#[test]
fn reload_latest_cancellation_does_not_revive_older() {
    let cache = SharedKeyCache::new();
    let key = ControlledKey::new();
    let owner = block_on(cache.shared(key.clone(), || 1)).unwrap();
    let mut a = Box::pin(cache.reload(&owner, &key));
    start_pending(a.as_mut());
    let reply_a = key.take_reply();
    let mut b = Box::pin(cache.reload(&owner, &key));
    start_pending(b.as_mut());
    drop(b);
    assert!(key.take_reply().send(Ok(Some(20))).is_err());
    assert!(reply_a.send(Ok(Some(10))).is_ok());
    block_on(a).unwrap();
    assert_eq!(owner.read(|value| *value).unwrap(), 1);
}

#[test]
fn reload_old_error_and_cancellation_cannot_retire_latest() {
    let cache = SharedKeyCache::new();
    let key = ControlledKey::new();
    let owner = block_on(cache.shared(key.clone(), || 1)).unwrap();
    let mut a = Box::pin(cache.reload(&owner, &key));
    let mut b = Box::pin(cache.reload(&owner, &key));
    let mut c = Box::pin(cache.reload(&owner, &key));
    start_pending(a.as_mut());
    start_pending(b.as_mut());
    start_pending(c.as_mut());
    key.reply(Err(ProviderFailure(Cell::new(41))));
    assert_eq!(block_on(a).unwrap_err().into_failure().unwrap().0.get(), 41);
    drop(b);
    assert!(key.take_reply().send(Ok(Some(20))).is_err());
    key.reply(Ok(Some(30)));
    block_on(c).unwrap();
    assert_eq!(owner.read(|value| *value).unwrap(), 30);
}

#[test]
fn reload_reset_and_other_namespaces_do_not_retarget_captured_references() {
    let cache = SharedKeyCache::new();
    let other_cache = SharedKeyCache::new();
    let key = ControlledKey::new();
    let owner = block_on(cache.shared(key.clone(), || 1)).unwrap();
    let captured = owner.reader().unwrap();
    let other = block_on(other_cache.shared(key.clone(), || 2)).unwrap();
    let mut a = Box::pin(cache.reload(&owner, &key));
    start_pending(a.as_mut());
    let reply_a = key.take_reply();
    let mut b = Box::pin(other_cache.reload(&other, &key));
    start_pending(b.as_mut());
    let reply_b = key.take_reply();
    cache.reset().unwrap();
    let fresh = block_on(cache.shared(key.clone(), || 3)).unwrap();
    owner.rebind(&fresh).unwrap();
    let mut c = Box::pin(cache.reload(&fresh, &key));
    start_pending(c.as_mut());
    key.reply(Ok(Some(30)));
    block_on(c).unwrap();
    assert!(reply_a.send(Ok(Some(10))).is_ok());
    assert!(reply_b.send(Ok(Some(20))).is_ok());
    block_on(a).unwrap();
    block_on(b).unwrap();
    assert_eq!(captured.read(|value| *value).unwrap(), 10);
    assert_eq!(other.read(|value| *value).unwrap(), 20);
    assert_eq!(owner.read(|value| *value).unwrap(), 30);
}

/// Runs one optional destructor callback while retaining an independent literal value.
struct ReloadDropValue {
    value: u32,
    /// Taken before execution so reentrant cleanup cannot call it twice.
    on_drop: Option<Box<dyn FnOnce() + Send>>,
}
impl ReloadDropValue {
    /// Constructs a value without cleanup side effects.
    fn plain(value: u32) -> Self {
        Self {
            value,
            on_drop: None,
        }
    }
}
impl Drop for ReloadDropValue {
    fn drop(&mut self) {
        if let Some(callback) = self.on_drop.take() {
            callback();
        }
    }
}

// A displaced owner's destructor may redirect the cohort, but not this load.
#[test]
fn replacement_reload_keeps_its_selected_owner_after_reentrant_cohort_rebinding() {
    let cache = SharedKeyCache::new();
    let key = ControlledKey::new();
    let winner = block_on(cache.shared(key.clone(), || ReloadDropValue::plain(11))).unwrap();
    let source = Shared::new(ReloadDropValue::plain(5));
    let third = Shared::new(ReloadDropValue::plain(30));
    let displaced = source.clone();
    let third_target = third.clone();
    source
        .try_update(
            |value| {
                value.on_drop = Some(Box::new(move || {
                    displaced.rebind(&third_target).unwrap();
                }));
                Ok::<_, ()>(())
            },
            || {},
        )
        .unwrap();

    let mut load = Box::pin(cache.load(&source, key.clone(), |_| panic!("hit must not seed")));
    start_pending(load.as_mut());
    assert_eq!(source.read(|value| value.value).unwrap(), 30);
    assert_eq!(winner.read(|value| value.value).unwrap(), 11);
    key.reply(Ok(Some(ReloadDropValue::plain(17))));
    block_on(load).unwrap();
    assert_eq!(winner.read(|value| value.value).unwrap(), 17);
    assert_eq!(source.read(|value| value.value).unwrap(), 30);
    assert_eq!(third.read(|value| value.value).unwrap(), 30);
}

// Public reload must expose commit contention and destroy its rejected owned value.
#[test]
fn reload_commit_busy_drops_the_loaded_value_before_a_successful_retry() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let cache = SharedKeyCache::new();
    let key = ControlledKey::new();
    let owner = block_on(cache.shared(key.clone(), || ReloadDropValue::plain(1))).unwrap();
    let reader = owner.reader().unwrap();
    let drops = Arc::new(AtomicUsize::new(0));
    let rejected_drops = Arc::clone(&drops);
    let mut reload = Box::pin(cache.reload(&owner, &key));
    start_pending(reload.as_mut());
    key.reply(Ok(Some(ReloadDropValue {
        value: 2,
        on_drop: Some(Box::new(move || {
            rejected_drops.fetch_add(1, Ordering::SeqCst);
        })),
    })));
    let error = reader
        .read(|value| {
            assert_eq!(value.value, 1);
            let mut context = std::task::Context::from_waker(std::task::Waker::noop());
            let std::task::Poll::Ready(result) = reload.as_mut().poll(&mut context) else {
                panic!("a ready provider result must not wait for busy value access");
            };
            result.unwrap_err()
        })
        .unwrap();
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    drop(reload);
    assert!(error.access_error().unwrap().is_busy());
    assert!(!error.is_poisoned());
    assert_eq!(owner.read(|value| value.value).unwrap(), 1);
    assert_eq!(reader.read(|value| value.value).unwrap(), 1);

    let mut retry = Box::pin(cache.reload(&owner, &key));
    start_pending(retry.as_mut());
    key.reply(Ok(Some(ReloadDropValue::plain(3))));
    block_on(retry).unwrap();
    assert_eq!(owner.read(|value| value.value).unwrap(), 3);
    assert_eq!(reader.read(|value| value.value).unwrap(), 3);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
}

// Dropping either displaced or rejected values under either guard would prevent reentry.
#[test]
fn reload_displaced_and_rejected_value_destructors_reenter_after_all_guards() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let cache = SharedKeyCache::new();
    let key = ControlledKey::new();
    let owner =
        Arc::new(block_on(cache.shared(key.clone(), || ReloadDropValue::plain(1))).unwrap());
    let calls = Arc::new(AtomicUsize::new(0));
    let dropping = |value| {
        let weak = Arc::downgrade(&owner);
        let cache = cache.clone();
        let key = key.clone();
        let calls = Arc::clone(&calls);
        ReloadDropValue {
            value,
            on_drop: Some(Box::new(move || {
                let Some(owner) = weak.upgrade() else {
                    return;
                };
                assert_eq!(owner.read(|current| current.value).unwrap(), 20);
                let mut reload = Box::pin(cache.reload(&owner, &key));
                start_pending(reload.as_mut());
                drop(reload);
                assert!(key.take_reply().send(Ok(None)).is_err());
                calls.fetch_add(1, Ordering::SeqCst);
            })),
        }
    };
    owner
        .try_update(
            |value| {
                *value = dropping(1);
                Ok::<_, ()>(())
            },
            || {},
        )
        .unwrap();
    let mut a = Box::pin(cache.reload(&owner, &key));
    start_pending(a.as_mut());
    let reply_a = key.take_reply();
    let mut b = Box::pin(cache.reload(&owner, &key));
    start_pending(b.as_mut());
    key.reply(Ok(Some(ReloadDropValue::plain(20))));
    block_on(b).unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(reply_a.send(Ok(Some(dropping(10)))).is_ok());
    block_on(a).unwrap();
    assert_eq!(owner.read(|value| value.value).unwrap(), 20);
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

/// Combines controlled reload replies with synchronous and later producer delivery.
#[derive(Clone)]
struct PublishingKey {
    loads: ControlledKey<i32>,
    subscriber: Arc<Mutex<Option<SharedSubscriber<i32>>>>,
}

impl PublishingKey {
    /// Starts without an installed producer or pending reload.
    fn new() -> Self {
        Self {
            loads: ControlledKey::new(),
            subscriber: Arc::new(Mutex::new(None)),
        }
    }

    /// Captures the installed weak producer without holding the fixture lock.
    fn subscriber(&self) -> SharedSubscriber<i32> {
        self.subscriber.lock().unwrap().as_ref().unwrap().clone()
    }
}

impl SharedReaderKey for PublishingKey {
    type Value = i32;
    type Error = ProviderFailure;
    type Subscription = ();

    fn identity(&self) -> SharedKeyIdentity {
        self.loads.identity()
    }

    fn load(
        &self,
        context: SharedLoadContext<'_, i32>,
    ) -> impl Future<Output = Result<Option<i32>, ProviderFailure>> + Send {
        self.loads.load(context)
    }

    fn subscribe(
        &self,
        _: SharedSubscribeContext,
        subscriber: SharedSubscriber<i32>,
    ) -> Result<(), ProviderFailure> {
        // Must succeed before build_reference installs its retained resource.
        assert!(subscriber.yield_value(3).unwrap());
        *self.subscriber.lock().unwrap() = Some(subscriber);
        Ok(())
    }
}

// A ready result is still obsolete when publication wins before its caller polls again.
#[test]
fn subscriber_publication_supersedes_an_unconsumed_ready_reload() {
    let cache = SharedKeyCache::new();
    let key = PublishingKey::new();
    let reader = block_on(cache.reader(key.clone(), || 1)).unwrap();
    assert_eq!(reader.read(|value| *value).unwrap(), 3);
    let mut reload = Box::pin(cache.reload_reader(&reader, &key));
    start_pending(reload.as_mut());
    key.loads.reply(Ok(Some(7)));
    assert!(key.subscriber().yield_value(8).unwrap());
    block_on(reload).unwrap();
    assert_eq!(reader.read(|value| *value).unwrap(), 8);
}

#[test]
fn subscriber_publication_supersedes_pending_reload_but_not_a_later_start() {
    let cache = SharedKeyCache::new();
    let key = PublishingKey::new();
    let reader = block_on(cache.reader(key.clone(), || 1)).unwrap();
    let mut old = Box::pin(cache.reload_reader(&reader, &key));
    start_pending(old.as_mut());
    assert!(key.subscriber().yield_value(8).unwrap());
    let mut fresh = Box::pin(cache.reload_reader(&reader, &key));
    start_pending(fresh.as_mut());
    key.loads.reply(Ok(Some(7)));
    block_on(old).unwrap();
    assert_eq!(reader.read(|value| *value).unwrap(), 8);
    key.loads.reply(Ok(Some(9)));
    block_on(fresh).unwrap();
    assert_eq!(reader.read(|value| *value).unwrap(), 9);
}

// Busy delivery publishes nothing and must not revoke an already authorised load.
#[test]
fn busy_subscriber_publication_preserves_active_reload_authority() {
    let cache = SharedKeyCache::new();
    let key = PublishingKey::new();
    let reader = block_on(cache.reader(key.clone(), || 1)).unwrap();
    let mut reload = Box::pin(cache.reload_reader(&reader, &key));
    start_pending(reload.as_mut());
    let subscriber = key.subscriber();
    reader
        .read(|_| assert!(subscriber.yield_value(8).unwrap_err().is_busy()))
        .unwrap();
    key.loads.reply(Ok(Some(7)));
    block_on(reload).unwrap();
    assert_eq!(reader.read(|value| *value).unwrap(), 7);
}

#[test]
fn require_checks_storage_before_collision_and_preserves_the_live_owner() {
    let cache = SharedKeyCache::new();
    let key = MemoryKey::new(None);
    let owner = block_on(cache.reader(key.clone(), key.seed(3))).unwrap();
    let mut incompatible = key.clone();
    incompatible.configuration = "v2";

    let absent = block_on(cache.require_reader(incompatible.clone())).unwrap_err();
    assert!(absent.is_absent());
    assert!(!absent.is_collision());
    *key.stored.lock().unwrap() = Some(41);
    let collision = block_on(cache.require_reader(incompatible)).unwrap_err();
    assert!(collision.is_collision());
    assert!(!collision.is_absent());
    assert_eq!(owner.read(|value| *value).unwrap(), 3);
    let compatible = block_on(cache.require_reader(key.clone())).unwrap();
    assert_eq!(compatible.read(|value| *value).unwrap(), 3);
    assert_eq!(*key.stored.lock().unwrap(), Some(41));
    assert_eq!(
        key.events(),
        [
            "seed:3",
            "initial:3",
            "subscribe:initial",
            "reload",
            "reload",
            "reload",
        ]
    );
    drop((owner, compatible));
    assert_eq!(key.events().last().unwrap(), "unsubscribe");
    assert_eq!(
        key.events()
            .iter()
            .filter(|event| *event == "unsubscribe")
            .count(),
        1
    );
}

/// Allows the required-storage load while retaining a deliberately different key type.
struct RequiredOtherKey(OtherKey);

impl SharedReaderKey for RequiredOtherKey {
    type Value = i32;
    type Error = Infallible;
    type Subscription = Subscription;

    fn identity(&self) -> SharedKeyIdentity {
        self.0.identity()
    }

    async fn load(&self, context: SharedLoadContext<'_, i32>) -> Result<Option<i32>, Infallible> {
        assert!(matches!(context, SharedLoadContext::UserInitiated));
        self.0.0.load(context).await
    }

    fn subscribe(
        &self,
        context: SharedSubscribeContext,
        subscriber: SharedSubscriber<i32>,
    ) -> Result<Subscription, Infallible> {
        self.0.subscribe(context, subscriber)
    }
}

impl SharedKey for RequiredOtherKey {
    async fn save(&self, value: &i32) -> Result<(), Infallible> {
        self.0.save(value).await
    }
}

// Present storage must not allow a different provider type to reuse the live resource.
#[test]
fn require_rejects_a_different_concrete_key_after_loading_without_replacing_the_owner() {
    let cache = SharedKeyCache::new();
    let key = MemoryKey::new(None);
    let owner = block_on(cache.reader(key.clone(), key.seed(3))).unwrap();
    *key.stored.lock().unwrap() = Some(41);

    let error = block_on(cache.require(RequiredOtherKey(OtherKey(key.clone())))).unwrap_err();
    assert!(error.is_collision());
    assert!(!error.is_absent());
    assert!(error.into_failure().is_none());
    let reused =
        block_on(cache.reader(key.clone(), || panic!("live owner must be reused"))).unwrap();
    assert_eq!(owner.read(|value| *value).unwrap(), 3);
    assert_eq!(reused.read(|value| *value).unwrap(), 3);
    assert_eq!(*key.stored.lock().unwrap(), Some(41));
    assert_eq!(
        key.events(),
        ["seed:3", "initial:3", "subscribe:initial", "reload"]
    );
    drop(owner);
    assert_eq!(key.events().last().unwrap(), "reload");
    drop(reused);
    assert_eq!(
        key.events(),
        [
            "seed:3",
            "initial:3",
            "subscribe:initial",
            "reload",
            "unsubscribe"
        ]
    );
}
