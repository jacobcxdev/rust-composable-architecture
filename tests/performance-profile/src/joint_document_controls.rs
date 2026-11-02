//! Independent `TestStore` transcript and observation-oracle rejection checks.
//!
//! Private editor state and shared document values have separate capture keys.
//! Advancing the editor's clock does not drive the separate document runtime.
use std::convert::Infallible;

use rca_test::{CaptureKey, TestStore};

use super::*;

/// Captures owned editor fields so later edits cannot rewrite earlier expectations.
struct PrivateValues;
impl CaptureKey for PrivateValues {
    type Value = Vec<(u8, u32, u64, Option<u64>)>;
    type Error = Infallible;
}
/// Captures current reader values separately from the editor's private revision.
struct SharedValues;
impl CaptureKey for SharedValues {
    type Value = Vec<Option<u32>>;
    type Error = rca_core::SharedAccessError;
}

// Removing on-change, delivering the timer twice, acknowledging before the
// backend, or treating a cloned shared handle as history breaks this witness.
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one causal transcript keeps independently asserted private and shared boundaries together"
)]
fn ordinary_save_preserves_independent_private_and_shared_obligations() {
    let (host, runtime, host_owner) = Host::new(false, SCHEDULER_BUDGET).unwrap();
    let probes = Arc::new(Probes::default());
    let mut documents = Documents::new(runtime, Arc::clone(&probes));
    host.run_ready_bounded(
        async {
            let client = documents.client().await.unwrap();
            let mut store = TestStore::projected_builder(
                editor_state(),
                editor_reducer(Arc::clone(&probes)),
                |_| (),
            )
            .with_dependency::<DocumentsKey>(client)
            .with_capture::<PrivateValues>(|state| Ok(private_snapshot(state)))
            .unwrap()
            .with_capture::<SharedValues>(shared_snapshot)
            .unwrap()
            .build();

            for (id, expected) in [(1, vec![Some(7), None]), (2, vec![Some(7), Some(20)])] {
                let loading = store.send(row(id, ChildAction::Open)).await.unwrap();
                settle(&host).unwrap();
                let _ = store
                    .receive_with_captures(
                        move |action| {
                            matches!(
                                action,
                                Action::Row(child)
                                    if *child.id() == id
                                        && matches!(child.action(), ChildAction::Loaded(_))
                            )
                        },
                        |()| {},
                        move |captures| captures.set::<SharedValues>(expected),
                    )
                    .await
                    .unwrap();
                loading.finish().await.unwrap();
            }

            // Editor completion does not drive the separate document actor's
            // queued subscription registration.
            settle(&host).unwrap();
            assert_eq!(count(&probes.document_reductions), 6);

            let editing = store
                .send_with_captures(
                    row(1, ChildAction::Edit(8)),
                    |()| {},
                    |captures| {
                        captures.set::<PrivateValues>(vec![(1, 8, 1, None), (2, 20, 0, None)])
                    },
                )
                .await
                .unwrap();
            let _ = store.send(row(1, ChildAction::Edit(8))).await.unwrap();
            let _ = store.send(Action::Unrelated).await.unwrap();
            assert_eq!(count(&probes.reactions), 1);
            assert_eq!(count(&probes.saves), 0);

            store.advance(DELAY).await.unwrap();
            let _ = store
                .receive(|action| {
                    matches!(
                        action,
                        Action::Row(child)
                            if *child.id() == 1
                                && matches!(child.action(), ChildAction::Flush)
                    )
                })
                .await
                .unwrap();
            settle(&host).unwrap();
            assert_eq!(count(&probes.saves), 1);
            assert_eq!(
                documents.snapshot().await.unwrap(),
                [(8, 1, Some(0)), (20, 0, Some(0))]
            );

            // Local publication precedes physical persistence and acknowledgement.
            store
                .check_captures(|captures| captures.set::<SharedValues>(vec![Some(8), Some(20)]))
                .await
                .unwrap();
            assert_eq!(documents.backends[0].state.lock().unwrap().value, 7);
            assert!(!editing.is_finished());

            documents.backends[0].complete_save().unwrap();
            settle(&host).unwrap();
            store.check_captures(|_| Ok(())).await.unwrap();
            let _ = store
                .receive_with_captures(
                    |action| {
                        matches!(
                            action,
                            Action::Row(child)
                                if *child.id() == 1
                                    && matches!(child.action(), ChildAction::Saved)
                        )
                    },
                    |()| {},
                    |captures| {
                        captures.set::<PrivateValues>(vec![(1, 8, 1, Some(1)), (2, 20, 0, None)])
                    },
                )
                .await
                .unwrap();
            let issue = store
                .expect_issue(|issue| issue.code == "joint.document.saved")
                .await
                .unwrap();
            assert_eq!(issue.severity, IssueSeverity::Note);
            assert_eq!(issue.message, "primary revision 1 persisted");
            assert!(issue.cause.is_none());
            assert_eq!(
                documents.snapshot().await.unwrap(),
                [(8, 1, Some(1)), (20, 0, Some(0))]
            );
            assert_eq!(count(&probes.document_reductions), 8);

            editing.finish().await.unwrap();
            store.finish().await.unwrap();
            drop(store);
            documents.cleanup().await.unwrap();
        },
        DEADLINE,
        OUTER_BUDGET,
    )
    .unwrap();
    assert_eq!(count(&probes.loads), 2);
    assert_eq!(count(&probes.writes), 1);
    assert_eq!(count(&probes.reactions), 1);
    assert_eq!(count(&probes.timers), 1);
    assert_eq!(count(&probes.saved), 1);
    assert_eq!(count(&probes.issues), 1);
    assert_eq!(count(&probes.parent_reductions), 9);
    assert_eq!(count(&probes.child_reductions), 8);
    assert_eq!(count(&probes.document_reductions), 10);
    assert_eq!(count(&probes.created), 3);
    assert_eq!(count(&probes.completed), 3);
    assert_eq!(count(&probes.dropped), 3);
    assert_eq!(count(&probes.closes), 2);
    assert!(documents.owners_released());
    drop(documents);
    drop(host);
    assert_eq!(host_owner.strong_count(), 0);
}

// One Pending poll alone must not accept an unread actor-owned value.
#[test]
fn quiet_observation_oracle_rejects_queued_delivery_and_closure() {
    let (host, runtime, host_owner) = Host::new(false, SCHEDULER_BUDGET).unwrap();
    host.run_ready_bounded(
        async {
            let store = Store::new(
                (),
                rca_core::Reduce::new(|(): &mut (), (): &(), _: &mut ReduceContext<'_>| {
                    Effect::none()
                }),
                runtime,
            );
            let mut observation = store.observe(|()| [7, 20]).await.unwrap();
            assert_eq!(observation.next().await, Some([7, 20]));
            store.send(()).await.unwrap().finish().await;
            assert_eq!(
                check_observation_quiet(&host, &mut observation),
                Err("joint unexpected observation or closure"),
            );
            store.shutdown_into_state().await.unwrap();
            assert_eq!(
                check_observation_quiet(&host, &mut observation),
                Err("joint unexpected observation or closure"),
            );
        },
        DEADLINE,
        OUTER_BUDGET,
    )
    .unwrap();
    drop(host);
    assert_eq!(host_owner.strong_count(), 0);
}
