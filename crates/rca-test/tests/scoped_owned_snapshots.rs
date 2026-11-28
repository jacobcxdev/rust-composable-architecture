//! Owned history and declared captures across real mounted-child updates.

use std::convert::Infallible;
use std::fmt::Debug;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

use futures_executor::block_on;
use futures_util::FutureExt;
use rca_core::{
    Effect, Feature, FeatureContext, MountIdentity, OptionalMount, ReduceContext, Reducer, Shared,
    SharedAccessError, Snapshot, Task, TaskAccessReason, TaskContext, TaskProgress,
};
use rca_test::{CaptureKey, TestStore};

/// Keeps private bookkeeping and shared quota in independent generated history.
#[derive(rca_macros::Snapshot)]
struct Child {
    history: Vec<String>,
    bookkeeping: usize,
    #[snapshot(with = capture_quota, value = usize)]
    quota: Shared<usize>,
}

/// Copies the shared value without retaining access to its mutable source.
fn capture_quota(quota: &Shared<usize>) -> usize {
    quota.read(|value| *value).unwrap()
}

/// Stores an optional child and the separately asserted shared quota.
#[derive(rca_macros::Snapshot)]
struct Root {
    child: Option<Child>,
    #[snapshot(omit = "Shared quota is asserted through the child and its declared capture")]
    quota: Shared<usize>,
}

/// Removes or restores the same-valued child under a new lifetime.
enum RootAction {
    Remove,
    Restore,
}

/// Changes child presence while retaining the shared quota.
struct RootReducer;

impl Reducer for RootReducer {
    type State = Root;
    type Action = RootAction;

    fn reduce(
        &mut self,
        state: &mut Root,
        action: &RootAction,
        _: &mut ReduceContext<'_>,
    ) -> Effect<RootAction> {
        match action {
            RootAction::Remove => state.child = None,
            RootAction::Restore => state.child = Some(child(state.quota.clone())),
        }
        Effect::none()
    }
}

/// Leaves changes to the mounted task's explicit update callback.
struct ChildFeature;

impl Feature for ChildFeature {
    type State = Child;
    type Action = ();

    fn update(&mut self, _: &mut Child, (): &(), _: &mut FeatureContext<'_, Child, ()>) {}
}

/// Declares quota separately from the selected root history.
struct Quota;

impl CaptureKey for Quota {
    type Value = usize;
    type Error = SharedAccessError;
}

/// Retains task contexts, completion controls and independent capture counters.
struct Probes {
    quota: Shared<usize>,
    contexts: Arc<Mutex<Vec<TaskContext<Child, ()>>>>,
    completions: Arc<Mutex<Vec<async_channel::Sender<()>>>>,
    snapshots: Arc<AtomicUsize>,
    captures: Arc<AtomicUsize>,
}

/// Creates the same initial history for each distinct child lifetime.
fn child(quota: Shared<usize>) -> Child {
    Child {
        history: vec!["queued".into()],
        bookkeeping: 3,
        quota,
    }
}

/// Builds a mounted child whose initial task remains live until explicitly released.
fn fixture<S>(
    snapshot: impl Fn(&Root) -> S + Send + Sync + 'static,
) -> (TestStore<Root, RootAction, S>, Probes)
where
    S: Clone + Debug + PartialEq + Send + 'static,
{
    let quota = Shared::new(12);
    let contexts = Arc::new(Mutex::new(Vec::new()));
    let initial_contexts = Arc::clone(&contexts);
    let completions = Arc::new(Mutex::new(Vec::new()));
    let initial_completions = Arc::clone(&completions);
    let mount = OptionalMount::new(
        |root: &Root| root.child.as_ref(),
        |root: &mut Root| root.child.as_mut(),
        |root: &Root| {
            root.child.as_ref().map(|_| MountIdentity {
                key: (),
                replacement: 0,
                configuration: 0,
            })
        },
        |_| ChildFeature,
    )
    .on_initial(
        |_| (),
        move |_, _| {
            let contexts = Arc::clone(&initial_contexts);
            let completions = Arc::clone(&initial_completions);
            Task::run(move |context| async move {
                // This is the installed Initial task context, not Store::acquire.
                // Keep its producing task alive until the test completes its work.
                let (complete, completion) = async_channel::bounded(1);
                contexts.lock().unwrap().push(context);
                completions.lock().unwrap().push(complete);
                completion.recv().await.unwrap();
            })
        },
    );
    let snapshots = Arc::new(AtomicUsize::new(0));
    let snapshot_calls = Arc::clone(&snapshots);
    let captures = Arc::new(AtomicUsize::new(0));
    let capture_calls = Arc::clone(&captures);
    let captured_quota = quota.clone();
    let store = TestStore::projected_builder(
        Root {
            child: Some(child(quota.clone())),
            quota: quota.clone(),
        },
        RootReducer,
        move |root| {
            snapshot_calls.fetch_add(1, Ordering::SeqCst);
            snapshot(root)
        },
    )
    .mount(mount)
    .with_capture::<Quota>(move |_| {
        capture_calls.fetch_add(1, Ordering::SeqCst);
        captured_quota.read(Snapshot::snapshot)
    })
    .unwrap()
    .build();
    (
        store,
        Probes {
            quota,
            contexts,
            completions,
            snapshots,
            captures,
        },
    )
}

/// Changes private child history and the shared quota in one scoped update.
fn advance(child: &mut Child) {
    child.history.push("imported".into());
    child.bookkeeping += 1;
    child
        .quota
        .try_update(
            |quota| {
                *quota -= 1;
                Ok::<_, Infallible>(())
            },
            || {},
        )
        .unwrap();
}

/// Extracts an assertion message without accepting unrelated panic payload types.
fn panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
    match payload.downcast::<String>() {
        Ok(message) => *message,
        Err(payload) => payload.downcast_ref::<&str>().unwrap().to_string(),
    }
}

// Catches aliases in generated history and a lost scoped Update frontier.
#[test]
fn mounted_updates_keep_history_private_fields_and_shared_expectations_independent() {
    block_on(async {
        let (mut store, probes) = fixture(Snapshot::snapshot);
        let initial = store.receive_structural(|_| {}).await.unwrap();
        let context = probes.contexts.lock().unwrap()[0].clone();
        let mut read = Box::pin(context.read(Snapshot::snapshot));
        assert!(read.as_mut().now_or_never().is_none());
        store.assert_state(|_| {}).await.unwrap();
        let first = read.await.unwrap();

        let mut update = Box::pin(context.update(advance));
        assert!(update.as_mut().now_or_never().is_none());
        assert!(
            store
                .finish()
                .await
                .unwrap_err()
                .finish_diagnostics()
                .unwrap()
                .has_pending_action(),
            "scoped update needs an expectation"
        );
        store
            .assert_state(|root| {
                let child = root.child.as_ref().unwrap();
                assert_eq!(child.history, ["queued"]);
                assert_eq!(child.bookkeeping, 3);
            })
            .await
            .unwrap();
        let _ = store
            .receive_update_with_captures(
                |expected| {
                    let expected = expected.child.as_mut().unwrap();
                    expected.history.push("imported".into());
                    expected.bookkeeping = 4;
                    expected.quota = 11;
                    assert_eq!(capture_quota(&probes.quota), 12);
                    assert_eq!(first.history, ["queued"]);
                },
                |expected| expected.update::<Quota>(|quota: &mut usize| *quota = 11),
            )
            .await
            .unwrap();
        update.await.unwrap();
        assert_eq!(first.history, ["queued"]);
        assert_eq!((first.bookkeeping, first.quota), (3, 12));
        assert_eq!(capture_quota(&probes.quota), 11);
        store
            .assert_state(|root| {
                let child = root.child.as_ref().unwrap();
                assert_eq!(child.history, ["queued", "imported"]);
                assert_eq!(child.bookkeeping, 4);
            })
            .await
            .unwrap();

        probes.completions.lock().unwrap()[0].try_send(()).unwrap();
        initial.finish().await.unwrap();
        store.finish().await.unwrap();
    });
}

// Catches snapshot omission acknowledging a committed update that changes no values.
#[test]
fn omitted_root_snapshot_retains_noop_scoped_update_frontier() {
    block_on(async {
        let (mut store, probes) = fixture(|_| ());
        let initial = store.receive_structural(|()| {}).await.unwrap();
        let context = probes.contexts.lock().unwrap()[0].clone();
        let mut noop = Box::pin(context.update(|_| ()));
        assert!(noop.as_mut().now_or_never().is_none());
        assert!(
            store
                .finish()
                .await
                .unwrap_err()
                .finish_diagnostics()
                .unwrap()
                .has_pending_action(),
            "equal snapshots do not acknowledge updates"
        );
        let _ = store.receive_update(|()| {}).await.unwrap();
        noop.await.unwrap();
        probes.completions.lock().unwrap()[0].try_send(()).unwrap();
        initial.finish().await.unwrap();
        store.finish().await.unwrap();
    });
}

// Catches root omission erasing capture debt, and wrong expected values being accepted.
#[test]
fn omitted_root_snapshot_cannot_hide_missing_or_wrong_scoped_capture_expectations() {
    for expected_quota in [None, Some(99)] {
        block_on(async {
            let (mut store, probes) = fixture(|_| ());
            let initial = store.receive_structural(|()| {}).await.unwrap();
            let context = probes.contexts.lock().unwrap()[0].clone();
            let mut update = Box::pin(context.update(advance));
            assert!(update.as_mut().now_or_never().is_none());
            let error = store
                .receive_update_with_captures(
                    |()| {},
                    |expected| {
                        if let Some(quota) = expected_quota {
                            expected.set::<Quota>(quota)?;
                        }
                        Ok(())
                    },
                )
                .await
                .unwrap_err();
            assert!(error.is_capture_mismatch());
            update.await.unwrap();
            assert_eq!(capture_quota(&probes.quota), 11);
            assert!(
                store
                    .receive_update(|()| panic!("failed boundary must remain sticky"))
                    .await
                    .unwrap_err()
                    .is_incomplete_assertion()
            );
            probes.completions.lock().unwrap()[0].try_send(()).unwrap();
            initial.finish().await.unwrap();
            assert!(store.finish().await.unwrap_err().is_incomplete_assertion());
            let teardown = catch_unwind(AssertUnwindSafe(|| drop(store))).unwrap_err();
            assert!(panic_message(teardown).starts_with(
                "TestStore teardown failed: TestStore incomplete assertion for receive_update at "
            ));
        });
    }
}

// Catches comparison omitting private child state, or expected edits mutating live state.
#[test]
fn wrong_private_scoped_expectation_keeps_projected_mismatch_debt() {
    block_on(async {
        let (mut store, probes) = fixture(Snapshot::snapshot);
        let initial = store.receive_structural(|_| {}).await.unwrap();
        let context = probes.contexts.lock().unwrap()[0].clone();
        let mut update = Box::pin(context.update(advance));
        assert!(update.as_mut().now_or_never().is_none());
        let failure = AssertUnwindSafe(store.receive_update_with_captures(
            |expected| {
                let expected = expected.child.as_mut().unwrap();
                expected.history.push("imported".into());
                expected.bookkeeping = 99;
                expected.quota = 11;
            },
            |expected| expected.set::<Quota>(11),
        ))
        .catch_unwind()
        .await
        .unwrap_err();
        let message = panic_message(failure);
        assert!(message.starts_with("TestStore state mismatch at "));
        assert!(message.contains(" during receive_update: projected coverage"));
        assert!(!message.contains("full-state coverage"));
        update.await.unwrap();
        store
            .assert_state(|root| assert_eq!(root.child.as_ref().unwrap().bookkeeping, 4))
            .await
            .unwrap();
        probes.completions.lock().unwrap()[0].try_send(()).unwrap();
        initial.finish().await.unwrap();
        assert!(store.finish().await.unwrap_err().is_incomplete_assertion());
        let teardown = catch_unwind(AssertUnwindSafe(|| drop(store))).unwrap_err();
        assert!(panic_message(teardown).starts_with(
            "TestStore teardown failed: TestStore incomplete assertion for receive_update at "
        ));
    });
}

// Catches stale update callbacks/captures reaching a restored equal-valued child.
#[test]
fn restored_child_rejects_old_context_before_update_or_snapshot_capture() {
    block_on(async {
        let (mut store, probes) = fixture(Snapshot::snapshot);
        let initial = store.receive_structural(|_| {}).await.unwrap();
        let old = probes.contexts.lock().unwrap()[0].clone();
        let mut read = Box::pin(old.read(Snapshot::snapshot));
        assert!(read.as_mut().now_or_never().is_none());
        store.assert_state(|_| {}).await.unwrap();
        let first = read.await.unwrap();
        let _ = store
            .send_with(RootAction::Remove, |expected| expected.child = None)
            .await
            .unwrap();
        let restored = first.clone();
        let task = store
            .send_with(RootAction::Restore, |expected| {
                expected.child = Some(restored);
            })
            .await
            .unwrap();
        initial.finish().await.unwrap();
        assert_eq!(probes.contexts.lock().unwrap().len(), 2);
        let snapshots = probes.snapshots.load(Ordering::SeqCst);
        let captures = probes.captures.load(Ordering::SeqCst);
        let calls = Arc::new(AtomicUsize::new(0));
        let stale_calls = Arc::clone(&calls);
        let stale = old.update(move |child| {
            stale_calls.fetch_add(1, Ordering::SeqCst);
            advance(child);
        });
        store
            .assert_state(|root| {
                let child = root.child.as_ref().unwrap();
                assert_eq!(child.history, ["queued"]);
                assert_eq!(child.bookkeeping, 3);
            })
            .await
            .unwrap();
        let failure = stale.await.unwrap_err();
        // Removal cancels this context's producing Initial task; unlike an
        // externally acquired context, its effect provenance is now cancelled.
        assert_eq!(failure.reason(), TaskAccessReason::Cancelled);
        assert_eq!(failure.progress(), TaskProgress::NotStarted);
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(probes.snapshots.load(Ordering::SeqCst), snapshots);
        assert_eq!(probes.captures.load(Ordering::SeqCst), captures);
        assert_eq!(capture_quota(&probes.quota), 12);
        assert_eq!(first.history, ["queued"]);
        assert_eq!((first.bookkeeping, first.quota), (3, 12));
        let fresh = probes.contexts.lock().unwrap()[1].clone();
        let mut update = Box::pin(fresh.update(advance));
        assert!(update.as_mut().now_or_never().is_none());
        let _ = store
            .receive_update_with_captures(
                |expected| {
                    let expected = expected.child.as_mut().unwrap();
                    expected.history.push("imported".into());
                    expected.bookkeeping = 4;
                    expected.quota = 11;
                },
                |expected| expected.set::<Quota>(11),
            )
            .await
            .unwrap();
        update.await.unwrap();
        assert_eq!(capture_quota(&probes.quota), 11);
        assert_eq!(first.history, ["queued"]);
        probes.completions.lock().unwrap()[1].try_send(()).unwrap();
        task.finish().await.unwrap();
        store.finish().await.unwrap();
    });
}
