//! A keyed replacement retires child timers both inside and outside the current due batch.

use futures_executor::block_on;
use rca_core::{KeyedState, MonotonicInstant, MountIdentity, ReduceContext, TaskAccessReason};

/// The first row is replaced; the second row keeps its timers and records their order.
struct State {
    /// Stores the delivered numbers for each live key.
    rows: KeyedState<u8, Vec<u8>>,
    /// Removing this mount retires an unrelated scope without editing the collection.
    sibling: Option<()>,
    /// Captures which equal-deadline deliveries preceded the replacement.
    survivor_at_replacement: Vec<u8>,
}

/// The replacement itself is a timed root action, between two surviving child timers.
enum RootAction {
    /// Commits without changing membership.
    Warm,
    /// Retires the mount before the collection.
    HideSibling,
    /// Puts the replacement in the timer queue.
    ScheduleReplacement(MonotonicInstant),
    /// Creates a new insertion lifetime for key one within the same action.
    Replace,
}

/// Each timer carries a number so that the test can distinguish the old and new children.
enum ChildAction {
    /// Schedules one numbered delivery.
    Schedule(MonotonicInstant, u8),
    /// Records a delivery in its target row.
    Deliver(u8),
}

/// Runs the same history on the shared and owner-local Stores.
macro_rules! scenario {
    ($carrier:path, $runtime:path) => {{
        use $carrier as api;
        type RowChildren<'a> = api::MountChildren<'a, State, RootAction, Vec<u8>, ChildAction>;

        let due = super::instant(10);
        let later = super::instant(20);
        let state = State {
            rows: [(1, Vec::new()), (2, Vec::new())].into_iter().collect(),
            sibling: Some(()),
            survivor_at_replacement: Vec::new(),
        };
        let root = api::Reduce::new(|s: &mut State, a: &RootAction, _: &mut ReduceContext<'_>| {
            match a {
                RootAction::Warm => {}
                RootAction::HideSibling => s.sibling = None,
                RootAction::ScheduleReplacement(at) => {
                    return api::Effect::send_at(*at, RootAction::Replace);
                }
                RootAction::Replace => {
                    s.survivor_at_replacement = s.rows[&2].clone();
                    assert_eq!(s.rows.remove(&1), Some(Vec::new()));
                    s.rows.insert(1, Vec::new());
                }
            }
            api::Effect::none()
        });
        let sibling = api::OptionalMount::new(
            |s: &State| s.sibling.as_ref(),
            |s: &mut State| s.sibling.as_mut(),
            |s: &State| s.sibling.map(|()| MountIdentity {
                key: (),
                replacement: 0,
                configuration: 0,
            }),
            |_| api::Reduce::new(
                |_: &mut (), _: &(), _: &mut ReduceContext<'_>| api::Effect::none(),
            ),
        );
        let host = <$runtime>::new();
        let (offer, sites) = std::sync::mpsc::channel();
        let (builder, sibling_site) = api::Store::builder(state, root, host.runtime())
            .mount_with_site(sibling);
        let store = builder.keyed_with_children(
            |s| &s.rows,
            |s| &mut s.rows,
            move |key, _| {
                let key = *key;
                let offer = offer.clone();
                let entry = api::EntryMount::new(api::Reduce::new(
                    |row: &mut Vec<u8>, action: &ChildAction, _: &mut ReduceContext<'_>| match action {
                        ChildAction::Schedule(at, value) => {
                            api::Effect::send_at(*at, ChildAction::Deliver(*value))
                        }
                        ChildAction::Deliver(value) => {
                            row.push(*value);
                            api::Effect::none()
                        }
                    },
                ));
                (entry, move |_: &Vec<u8>, children: &mut RowChildren<'_>| {
                    offer.send((key, children.site().clone())).unwrap();
                })
            },
        ).build().unwrap();
        let clock = host.clone();
        block_on(host.run(async move {
            store.ready().await.unwrap();
            let (1, first_site) = sites.try_recv().unwrap() else {
                panic!("expected the first keyed row");
            };
            let (2, second_site) = sites.try_recv().unwrap() else {
                panic!("expected the second keyed row");
            };
            let old = store.acquire(&first_site).await.unwrap().unwrap();
            let survivor = store.acquire(&second_site).await.unwrap().unwrap();
            let sibling = store.acquire(&sibling_site).await.unwrap().unwrap();
            for _ in 0..2 {
                store.send(RootAction::Warm).await.unwrap().finish().await;
            }

            // Register the replacement between surviving events at the same deadline.
            let first = survivor.send(ChildAction::Schedule(due, 1)).await.unwrap();
            let replace = store.send(RootAction::ScheduleReplacement(due)).await.unwrap();
            let old_due = old.send(ChildAction::Schedule(due, 90)).await.unwrap();
            let second = survivor.send(ChildAction::Schedule(due, 2)).await.unwrap();
            let old_queued = old.send(ChildAction::Schedule(later, 91)).await.unwrap();
            let last = survivor.send(ChildAction::Schedule(later, 3)).await.unwrap();

            store.send(RootAction::HideSibling).await.unwrap().finish().await;
            assert_eq!(sibling.read(|_| ()).await.unwrap_err().reason(), TaskAccessReason::ScopeEnded);
            assert!(old.read(Vec::is_empty).await.unwrap());
            assert!(sites.try_recv().is_err());
            assert_eq!(
                store.runtime_snapshot().await.unwrap().timer_deadlines,
                [due, due, due, due, later, later]
            );
            assert!(!old_due.is_finished() && !old_queued.is_finished());

            clock.advance_to(due).unwrap();
            first.finish().await;
            replace.finish().await;
            second.finish().await;
            assert!(old_due.is_finished() && old_queued.is_finished());
            assert!(!last.is_finished());
            assert_eq!(store.runtime_snapshot().await.unwrap().timer_deadlines, [later]);
            assert_eq!(store.read(|s| s.survivor_at_replacement.clone()).await.unwrap(), [1]);
            assert_eq!(survivor.read(Clone::clone).await.unwrap(), [1, 2]);
            assert!(matches!(
                old.send(ChildAction::Deliver(92)).await,
                Err(rca_core::TaskSendError::Rejected { action: ChildAction::Deliver(92), error })
                    if error.reason() == TaskAccessReason::ScopeEnded
            ));

            let (1, replacement_site) = sites.try_recv().unwrap() else {
                panic!("expected the replacement");
            };
            assert!(sites.try_recv().is_err());
            let replacement = store.acquire(&replacement_site).await.unwrap().unwrap();
            assert!(replacement.read(Vec::is_empty).await.unwrap());
            replacement.send(ChildAction::Deliver(4)).await.unwrap().finish().await;
            clock.advance_to(later).unwrap();
            last.finish().await;
            assert_eq!(replacement.read(Clone::clone).await.unwrap(), [4]);
            assert_eq!(survivor.read(Clone::clone).await.unwrap(), [1, 2, 3]);
            assert!(store.runtime_snapshot().await.unwrap().timer_deadlines.is_empty());
            store.shutdown().await.unwrap();
        })).unwrap();
    }};
}

#[test]
fn shared_store() {
    scenario!(rca_core, rca_test::DeterministicRuntime);
}

#[cfg(feature = "local")]
#[test]
fn local_store() {
    scenario!(rca_core::local, rca_test::local::DeterministicRuntime);
}
