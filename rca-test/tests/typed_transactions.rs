//! Checks typed-update acknowledgement, independent expectations and retained failure boundaries.

use futures_executor::block_on;
use futures_util::FutureExt;
use rca_core::{Feature, FeatureContext, Task};
use rca_test::TestStore;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

/// Queues a no-op followed by a mutation, each requiring its own expectation.
struct Updates;

impl Feature for Updates {
    type State = usize;
    type Action = ();
    fn update(&mut self, _: &mut usize, (): &(), context: &mut FeatureContext<'_, usize, ()>) {
        context.task(Task::<usize, ()>::run(|context| async move {
            let _ = context.update(|_| ()).await.unwrap();
            let _ = context.update(|state| *state += 1).await.unwrap();
        }));
    }
}

#[test]
fn no_op_update_requires_its_own_expectation_and_diagnostic_does_not_acknowledge() {
    block_on(async {
        let mut store = TestStore::builder(0, Updates).build();
        let task = store.send(()).await.unwrap();

        let pending = store
            .finish()
            .await
            .expect_err("omitted no-op update is debt");
        assert!(pending.finish_diagnostics().unwrap().has_pending_action());
        store
            .assert_state(|state| assert_eq!(*state, 0))
            .await
            .unwrap();
        let pending = store
            .finish()
            .await
            .expect_err("diagnostic is not an expectation");
        assert!(pending.finish_diagnostics().unwrap().has_pending_action());

        let calls = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&calls);
        let wrong_kind = store
            .receive(move |()| {
                observed.fetch_add(1, Ordering::Relaxed);
                true
            })
            .await
            .unwrap_err();
        assert!(wrong_kind.is_no_received_action());
        assert_eq!(
            calls.load(Ordering::Relaxed),
            0,
            "wrong kind does not run matcher"
        );

        let _ = store.receive_update(|_| {}).await.unwrap();
        store
            .assert_state(|state| assert_eq!(*state, 0))
            .await
            .unwrap();
        let pending = store
            .finish()
            .await
            .expect_err("second update cannot overwrite the first expectation");
        assert!(pending.finish_diagnostics().unwrap().has_pending_action());
        let _ = store.receive_update(|state| *state = 1).await.unwrap();
        task.finish().await.unwrap();
        store.finish().await.unwrap();
    });
}

#[test]
fn failed_update_comparison_keeps_first_boundary_and_independent_live_state() {
    block_on(async {
        let mut store = TestStore::builder(0, Updates).build();
        let task = store.send(()).await.unwrap();
        let panic = std::panic::AssertUnwindSafe(store.receive_update(|expected| *expected = 99))
            .catch_unwind()
            .await
            .expect_err("the incorrect update expectation must fail comparison");
        let diagnostic = panic.downcast_ref::<String>().unwrap();
        assert!(diagnostic.contains("TestStore state mismatch"));
        assert!(diagnostic.contains("during receive_update: full-state coverage"));
        assert!(diagnostic.contains("expected:\n- 1 | 99\nactual:\n+ 1 | 0"));
        store
            .assert_state(|live| assert_eq!(*live, 0))
            .await
            .unwrap();
        assert!(store.finish().await.unwrap_err().is_incomplete_assertion());
        assert!(
            store
                .receive_update(|_| {})
                .await
                .unwrap_err()
                .is_incomplete_assertion(),
            "failed first comparison cannot become a new expectation"
        );
        store
            .assert_state(|live| assert_eq!(*live, 0))
            .await
            .unwrap();
        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(store)))
            .expect_err("unacknowledged mismatch remains an error at final destruction");
        assert!(panic.downcast_ref::<String>().unwrap().starts_with(
            "TestStore teardown failed: TestStore incomplete assertion for receive_update at "
        ));
        assert!(task.is_finished(), "teardown releases the update producer");
    });
}

// Cancellation from an expectation can remove a typed update before release.
// Neither its speculative snapshot nor declared capture is an admitted debt.
#[test]
fn cancelled_update_expectation_leaves_snapshot_and_capture_baselines_unchanged() {
    /// Names the independent count expectation checked beside the root snapshot.
    struct Count;
    impl rca_test::CaptureKey for Count {
        type Value = usize;
        type Error = ();
    }

    block_on(async {
        let mut store = TestStore::builder(0, Updates)
            .with_capture::<Count>(|state| Ok(*state))
            .unwrap()
            .build();
        let task = store.send(()).await.unwrap();
        let mut callbacks = 0;
        let error = store
            .receive_update_with_captures(
                |expected| {
                    *expected = 99;
                    task.cancel()
                        .now_or_never()
                        .expect("finite cancellation")
                        .unwrap();
                },
                |captures| {
                    callbacks += 1;
                    captures.set::<Count>(99)
                },
            )
            .await
            .unwrap_err();
        assert!(error.is_no_received_action());
        assert!(!error.is_incomplete_assertion());
        assert_eq!(callbacks, 1);
        store
            .assert_state(|state| assert_eq!(*state, 0))
            .await
            .unwrap();
        let boundary = store.pending_boundary().unwrap();
        assert!(store.finish().await.unwrap_err().is_incomplete_assertion());
        store
            .receive_terminal(boundary, |error| {
                error.reason() == rca_core::TaskAccessReason::Cancelled
            })
            .await
            .unwrap();
        let next = store.send(()).await.unwrap();
        let _ = store.receive_update(|_| {}).await.unwrap();
        let _ = store
            .receive_update_with_captures(|state| *state = 1, |captures| captures.set::<Count>(1))
            .await
            .unwrap();
        next.finish().await.unwrap();
        store.finish().await.unwrap();
    });
}

#[test]
fn invalid_declared_update_expectation_keeps_the_typed_frontier_for_retry() {
    /// Names the count capture registered by this store.
    struct Count;
    impl rca_test::CaptureKey for Count {
        type Value = usize;
        type Error = ();
    }

    /// Deliberately lacks a registered capture so validation must reject its expectation.
    struct Undeclared;
    impl rca_test::CaptureKey for Undeclared {
        type Value = usize;
        type Error = ();
    }

    block_on(async {
        let mut store = TestStore::builder(0, Updates)
            .with_capture::<Count>(|state| Ok(*state))
            .unwrap()
            .build();
        let parent = store.send(()).await.unwrap();
        let (mut snapshots, mut declarations) = (0, 0);
        let error = store
            .receive_update_with_captures(
                |_| snapshots += 1,
                |captures| {
                    declarations += 1;
                    captures.set::<Undeclared>(0)
                },
            )
            .await
            .expect_err("an undeclared capture cannot release the selected update");
        assert!(error.to_string().contains("UnknownExpectation"));
        assert_eq!((snapshots, declarations), (1, 1));
        assert!(store.pending_boundary().is_some());
        store
            .assert_state(|state| assert_eq!(*state, 0))
            .await
            .unwrap();

        let first = store
            .receive_update_with_captures(|_| {}, |captures| captures.set::<Count>(0))
            .await
            .expect("the same no-op update remains receivable");
        let second = store
            .receive_update_with_captures(|state| *state = 1, |captures| captures.set::<Count>(1))
            .await
            .unwrap();
        first.finish().await.unwrap();
        second.finish().await.unwrap();
        parent.finish().await.unwrap();
        store.finish().await.unwrap();
    });
}

#[test]
fn changed_external_capture_blocks_update_before_expectations_and_keeps_preflight_debt() {
    /// Names state outside the store that can change without a reducer commit.
    struct External;
    impl rca_test::CaptureKey for External {
        type Value = usize;
        type Error = ();
    }

    block_on(async {
        let external = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&external);
        let mut store = TestStore::builder(0, Updates)
            .with_capture::<External>(move |_| Ok(observed.load(Ordering::SeqCst)))
            .unwrap()
            .build();
        let task = store.send(()).await.unwrap();
        external.store(1, Ordering::SeqCst);
        let error = store
            .receive_update_with_captures(
                |_| panic!("preflight must precede snapshot expectations"),
                |_| panic!("preflight must precede declared expectations"),
            )
            .await
            .expect_err("external capture drift rejects before update release");
        assert!(error.is_capture_mismatch());
        assert!(store.pending_boundary().is_some());
        store
            .assert_state(|state| assert_eq!(*state, 0))
            .await
            .unwrap();

        external.store(0, Ordering::SeqCst);
        assert!(
            store
                .receive_update(|_| {})
                .await
                .unwrap_err()
                .is_incomplete_assertion()
        );
        assert!(store.finish().await.unwrap_err().is_incomplete_assertion());
        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(store)))
            .expect_err("failed preflight remains teardown debt");
        assert!(panic.downcast_ref::<String>().unwrap().starts_with(
            "TestStore teardown failed: TestStore incomplete assertion for preflight at "
        ));
        assert!(task.is_finished(), "teardown releases the update producer");
    });
}
