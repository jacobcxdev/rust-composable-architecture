//! Overlapping stores keep equal shared keys in separate namespaces.

use std::convert::Infallible;
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use futures_executor::block_on;
use rca_core::{
    Effect, ReduceContext, Reducer, SharedAccessError, SharedKey, SharedKeyCache,
    SharedKeyIdentity, SharedLoadContext, SharedReaderKey, SharedSubscribeContext,
    SharedSubscriber,
};
use rca_test::{CaptureKey, TestStore};

/// Tracks each store's shared value independently of its empty reducer state.
struct ValueCapture;
impl CaptureKey for ValueCapture {
    type Value = u64;
    type Error = SharedAccessError;
}

/// Leaves all observable work to shared values, captures and the test clock.
struct Feature;
impl Reducer for Feature {
    type State = ();
    type Action = ();

    fn reduce(&mut self, (): &mut (), (): &(), _: &mut ReduceContext<'_>) -> Effect<()> {
        Effect::none()
    }
}

/// Uses one key identity for independent backends in different caches.
struct Key(Arc<Mutex<u64>>);
impl SharedReaderKey for Key {
    type Value = u64;
    type Error = Infallible;
    type Subscription = ();

    fn identity(&self) -> SharedKeyIdentity {
        SharedKeyIdentity::new::<u64>("same-key", "same-codec")
    }

    fn load(
        &self,
        _: SharedLoadContext<'_, u64>,
    ) -> impl Future<Output = Result<Option<u64>, Infallible>> + Send {
        std::future::ready(Ok(Some(*self.0.lock().unwrap())))
    }

    fn subscribe(
        &self,
        _: SharedSubscribeContext,
        _: SharedSubscriber<u64>,
    ) -> Result<(), Infallible> {
        Ok(())
    }
}
impl SharedKey for Key {
    fn save(&self, value: &u64) -> impl Future<Output = Result<(), Infallible>> + Send {
        *self.0.lock().unwrap() = *value;
        std::future::ready(Ok(()))
    }
}

/// Meets the other worker without keeping it waiting after this worker unwinds.
struct Rendezvous {
    ready: mpsc::Sender<()>,
    peer: mpsc::Receiver<()>,
}

impl Rendezvous {
    /// Gives each worker the only sender and receiver for its side.
    fn pair() -> [Self; 2] {
        let (left_ready, left_peer) = mpsc::channel();
        let (right_ready, right_peer) = mpsc::channel();
        [
            Self {
                ready: left_ready,
                peer: right_peer,
            },
            Self {
                ready: right_ready,
                peer: left_peer,
            },
        ]
    }

    /// Exchanges arrival signals; the timeout is a watchdog, not a clock oracle.
    fn wait(&self) {
        self.ready
            .send(())
            .expect("namespace peer exited before rendezvous");
        self.peer
            .recv_timeout(Duration::from_secs(30))
            .expect("namespace peer did not reach rendezvous");
    }
}

/// Keeps setup, mutation and cache reset overlapping with an independent worker.
fn parameter(initial: u64, changed: u64, advance: Duration, rendezvous: Rendezvous) {
    let cache = SharedKeyCache::new();
    let backend = Arc::new(Mutex::new(initial));
    let shared = block_on(cache.shared(Key(Arc::clone(&backend)), || 0)).unwrap();
    let alias = block_on(cache.shared(Key(Arc::clone(&backend)), || 99)).unwrap();
    let before = shared.read(|value| *value).unwrap();
    let captured = shared.clone();
    let mut store = TestStore::builder((), Feature)
        .with_capture::<ValueCapture>(move |()| captured.read(|value| *value))
        .unwrap()
        .build();
    let initial_elapsed = store.now().elapsed();

    rendezvous.wait();
    let advanced = block_on(store.advance(advance));
    shared
        .try_update(
            |value| {
                *value = changed;
                Ok::<_, Infallible>(())
            },
            || {},
        )
        .unwrap();

    rendezvous.wait();
    let advanced_elapsed = store.now().elapsed();
    let aliased = alias.read(|value| *value).unwrap();
    cache.reset().unwrap();

    rendezvous.wait();
    drop(rendezvous);

    // Both invocations have met at every overlap point before clock failures unwind.
    assert_eq!(initial_elapsed, Duration::ZERO);
    advanced.unwrap();
    assert_eq!(advanced_elapsed, advance);
    assert_eq!(before, initial);
    assert_eq!(aliased, changed);
    assert_eq!(shared.read(|value| *value).unwrap(), changed);
    assert_eq!(*backend.lock().unwrap(), initial);
    block_on(store.check_captures(move |expected| expected.set::<ValueCapture>(changed))).unwrap();
    block_on(store.finish()).unwrap();
}

#[test]
fn same_key_parameter_invocations_overlap_without_aliasing() {
    let [left, right] = Rendezvous::pair();
    std::thread::scope(|threads| {
        threads.spawn(move || parameter(11, 13, Duration::from_millis(11), left));
        threads.spawn(move || parameter(22, 24, Duration::from_millis(22), right));
    });
}

#[test]
fn parameter_stops_when_its_peer_exits() {
    let [worker, peer] = Rendezvous::pair();
    drop(peer);

    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        parameter(11, 13, Duration::from_millis(11), worker);
    }))
    .expect_err("the actual worker must report its peer's exit");
    let message = panic
        .downcast_ref::<String>()
        .expect("channel failure supplies a diagnostic message");
    assert!(
        message.contains("namespace peer exited before rendezvous"),
        "{message}"
    );
}

#[test]
fn cache_reset_cannot_erase_an_admitted_capture_mismatch() {
    let cache = SharedKeyCache::new();
    let shared = block_on(cache.shared(Key(Arc::new(Mutex::new(3))), || 0)).unwrap();
    let captured = shared.clone();
    let mut store = TestStore::builder((), Feature)
        .with_capture::<ValueCapture>(move |()| captured.read(|value| *value))
        .unwrap()
        .build();
    shared
        .try_update(
            |value| {
                *value = 4;
                Ok::<_, Infallible>(())
            },
            || {},
        )
        .unwrap();
    assert!(
        block_on(store.check_captures(|_| Ok(())))
            .unwrap_err()
            .is_capture_mismatch()
    );
    cache.reset().unwrap();
    shared
        .try_update(
            |value| {
                *value = 3;
                Ok::<_, Infallible>(())
            },
            || {},
        )
        .unwrap();
    assert!(
        block_on(store.check_captures(|_| Ok(())))
            .unwrap_err()
            .is_incomplete_assertion()
    );
    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(store)))
        .expect_err("an admitted capture mismatch must remain unfinished at teardown");
    let message = panic
        .downcast_ref::<String>()
        .expect("teardown supplies a diagnostic message");
    assert!(message.contains("incomplete assertion"), "{message}");
}
