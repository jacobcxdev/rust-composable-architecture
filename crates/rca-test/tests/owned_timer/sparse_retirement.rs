//! Removes one mounted timer from a Store that owns thousands of live root timers.

use std::ops::Range;

use futures_executor::block_on;
use rca_core::{MonotonicInstant, MountIdentity, ReduceContext};

/// Together with the mounted timer, this fills a 4,096-entry timer queue.
const ROOT_TIMERS: usize = 4_095;

/// The root records each delivered timer. The optional child counts its own deliveries.
#[derive(Default)]
struct State {
    /// Changes presence only through an ordinary root action.
    child: Option<usize>,
    /// Contains the sequence numbers of root timers in delivery order.
    delivered: Vec<usize>,
}

/// Splits root admission across both sides of the child's queue position.
enum RootAction {
    /// Schedules one timer for each sequence number.
    Schedule(Range<usize>),
    /// Records one timer delivery.
    Deliver(usize),
    /// Removes the old child or installs a replacement.
    Child(Option<usize>),
}

/// Shared and owner-local carriers have distinct types but the same public operations.
macro_rules! scenario {
    ($carrier:path, $host:path) => {{
        use $carrier as api;

        let deadline = MonotonicInstant::from_elapsed(std::time::Duration::from_secs(1));
        let root = api::Reduce::new(
            move |s: &mut State, action: &RootAction, _: &mut ReduceContext<'_>| {
                match action {
                    RootAction::Schedule(ids) => {
                        return api::Effect::merge(
                            ids.clone()
                                .map(|id| api::Effect::send_at(deadline, RootAction::Deliver(id))),
                        );
                    }
                    RootAction::Deliver(id) => s.delivered.push(*id),
                    RootAction::Child(child) => s.child = *child,
                }
                api::Effect::none()
            },
        );
        let mount = api::OptionalMount::new(
            |s: &State| s.child.as_ref(),
            |s: &mut State| s.child.as_mut(),
            |s: &State| {
                s.child.map(|_| MountIdentity {
                    key: (),
                    replacement: 0,
                    configuration: 0,
                })
            },
            move |_| {
                api::Reduce::new(
                    move |state: &mut usize, fire: &bool, _: &mut ReduceContext<'_>| {
                        if *fire {
                            *state += 1;
                            api::Effect::none()
                        } else {
                            api::Effect::send_at(deadline, true)
                        }
                    },
                )
            },
        );

        let host = <$host>::new();
        let state = State {
            child: Some(0),
            ..State::default()
        };
        let (builder, site) =
            api::Store::builder(state, root, host.runtime()).mount_with_site(mount);
        let store = builder.build().unwrap();
        let clock = host.clone();
        block_on(host.run(async move {
            store.ready().await.unwrap();
            let child = store.acquire(&site).await.unwrap().unwrap();
            let middle = ROOT_TIMERS / 2;
            let before = store.send(RootAction::Schedule(0..middle)).await.unwrap();
            let retired = child.send(false).await.unwrap();
            let after = store
                .send(RootAction::Schedule(middle..ROOT_TIMERS))
                .await
                .unwrap();
            let snapshot = store.runtime_snapshot().await.unwrap();
            assert_eq!(snapshot.timer_deadlines, vec![deadline; ROOT_TIMERS + 1]);
            assert!(!retired.is_finished());

            store
                .send(RootAction::Child(None))
                .await
                .unwrap()
                .finish()
                .await;
            clock.run_until_stalled().unwrap();
            assert!(
                retired.is_finished(),
                "removed child's receipt must finish before time advances"
            );
            assert!(!before.is_finished() && !after.is_finished());
            let snapshot = store.runtime_snapshot().await.unwrap();
            assert_eq!(snapshot.timer_deadlines, vec![deadline; ROOT_TIMERS]);

            store
                .send(RootAction::Child(Some(0)))
                .await
                .unwrap()
                .finish()
                .await;
            clock.advance_to(deadline).unwrap();
            before.finish().await;
            after.finish().await;
            let (delivered, replacement) = store
                .read(|s| (s.delivered.clone(), s.child))
                .await
                .unwrap();
            assert_eq!(delivered, (0..ROOT_TIMERS).collect::<Vec<_>>());
            assert_eq!(
                replacement,
                Some(0),
                "retired timer reached the replacement"
            );
            assert!(
                store
                    .runtime_snapshot()
                    .await
                    .unwrap()
                    .timer_deadlines
                    .is_empty()
            );
            store.shutdown().await.unwrap();
        }))
        .unwrap();
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
