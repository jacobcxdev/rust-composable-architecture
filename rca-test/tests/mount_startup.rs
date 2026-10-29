//! Structural and initial frontiers stay separate during mounted startup.

use futures_executor::block_on;
use futures_util::FutureExt;
use rca_core::{Feature, FeatureContext, MountIdentity, OptionalMount, Store, Task};
use rca_test::{CaptureKey, Exhaustivity, TestStore, TestStoreBuilder};
use std::cell::Cell;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

struct RootFeature;
impl Feature for RootFeature {
    type State = Option<usize>;
    type Action = ();
    fn reduce(
        &mut self,
        _: &mut Self::State,
        (): &(),
        _: &mut FeatureContext<'_, Self::State, ()>,
    ) {
    }
}

struct ChildFeature;
impl Feature for ChildFeature {
    type State = usize;
    type Action = u8;
    fn reduce(&mut self, _: &mut usize, _: &u8, _: &mut FeatureContext<'_, usize, u8>) {}
}
/// Exposes construction count at the structural assertion frontier.
struct Constructions;
impl CaptureKey for Constructions {
    type Value = usize;
    type Error = ();
}

/// Counts child construction before the initial reaction starts its update.
fn descriptor(constructions: Arc<AtomicUsize>) -> OptionalMount<Option<usize>, ChildFeature, ()> {
    let counter = Cell::new(0);
    OptionalMount::new(
        Option::as_ref,
        Option::as_mut,
        |state: &Option<usize>| {
            state.map(|_| MountIdentity {
                key: (),
                replacement: 0,
                configuration: 0,
            })
        },
        move |_| {
            counter.set(counter.get() + 1);
            constructions.fetch_add(1, Ordering::Relaxed);
            ChildFeature
        },
    )
    .on_initial(
        |value| *value,
        |change, _| {
            let rca_core::ReactionChange::Initial { current } = change else {
                panic!("initial only")
            };
            Task::run(move |context| async move {
                context
                    .update(move |state| *state += current)
                    .await
                    .unwrap();
            })
        },
    )
}

// Catches implicit startup acknowledgement, wrong-kind callbacks and skip preflight.
#[test]
fn structural_frontier_captures_private_construction_before_initial_update() {
    block_on(async {
        let constructions = Arc::new(AtomicUsize::new(0));
        let capture_calls = Arc::new(AtomicUsize::new(0));
        let captured = Arc::clone(&constructions);
        let calls = Arc::clone(&capture_calls);
        let mut store = TestStore::feature_builder(Some(2), RootFeature)
            .mount(descriptor(Arc::clone(&constructions)))
            .with_capture::<Constructions>(move |_| {
                calls.fetch_add(1, Ordering::Relaxed);
                Ok(captured.load(Ordering::Relaxed))
            })
            .unwrap()
            .with_exhaustivity(Exhaustivity::NonExhaustive)
            .build();
        assert_eq!(constructions.load(Ordering::Relaxed), 0);
        assert_eq!(capture_calls.load(Ordering::Relaxed), 1);
        assert!(store.finish().await.is_err());
        store
            .assert_state(|state| assert_eq!(*state, Some(2)))
            .await
            .unwrap();
        store
            .receive(|()| panic!("wrong-kind matcher"))
            .await
            .expect_err("Structural frontier rejects an ordinary receive");
        store
            .receive_update_with_captures(
                |_| panic!("wrong-kind snapshot"),
                |_| panic!("wrong-kind capture expectation"),
            )
            .await
            .expect_err("Structural frontier rejects an update receive");
        assert!(store.skip_received().await.is_err());
        assert_eq!(
            capture_calls.load(Ordering::Relaxed),
            1,
            "Structural skip must not capture"
        );
        assert_eq!(constructions.load(Ordering::Relaxed), 0);
        let initial = store
            .receive_structural_with_captures(|_| {}, |expected| expected.set::<Constructions>(1))
            .await
            .unwrap();
        assert_eq!(constructions.load(Ordering::Relaxed), 1);
        assert!(
            store.finish().await.is_err(),
            "Initial update owns the next frontier"
        );
        let _ = store
            .receive_update(|state| *state = Some(4))
            .await
            .unwrap();
        initial.finish().await.unwrap();
        store.finish().await.unwrap();
    });
}

// Catches overwriting the failed Structural assertion with another expectation.
#[test]
fn structural_mismatch_retains_original_debt() {
    block_on(async {
        let mut store = TestStore::feature_builder(Some(2), RootFeature)
            .mount(descriptor(Arc::new(AtomicUsize::new(0))))
            .build();
        let failed = std::panic::AssertUnwindSafe(store.receive_structural(|state| *state = None))
            .catch_unwind()
            .await;
        let panic = failed.expect_err("Structural snapshot mismatch must panic");
        let message = panic
            .downcast_ref::<String>()
            .expect("snapshot mismatch has a String diagnostic");
        assert!(
            message.starts_with("TestStore state mismatch at "),
            "{message}"
        );
        assert!(message.contains(" during receive_structural:"), "{message}");
        store
            .assert_state(|state| assert_eq!(*state, Some(2)))
            .await
            .unwrap();
        store
            .receive_structural(|_| panic!("retained debt"))
            .await
            .expect_err("Original Structural assertion debt stays selected");
        assert!(store.finish().await.unwrap_err().is_incomplete_assertion());
        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(store)))
            .expect_err("Drop must retain the failed Structural comparison");
        let message = panic
            .downcast_ref::<String>()
            .expect("teardown failure has a String diagnostic");
        assert!(
            message.starts_with(
                "TestStore teardown failed: TestStore incomplete assertion for receive_structural at "
            ),
            "{message}"
        );
    });
}

#[test]
fn shared_store_and_builder_transfer_contract() {
    fn send_sync<T: Send + Sync>() {}
    fn send<T: Send>() {}
    send_sync::<Store<Option<usize>, ()>>();
    send_sync::<TestStore<Option<usize>, ()>>();
    send::<TestStoreBuilder<Option<usize>, (), RootFeature>>();
    eprintln!(
        "builder_bytes={}",
        std::mem::size_of::<TestStoreBuilder<Option<usize>, (), RootFeature>>()
    );
}
