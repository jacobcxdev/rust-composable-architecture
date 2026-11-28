//! Checks absolute deadlines, owned payload destruction and cancellation registration order.

use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::Duration,
};

use futures_executor::block_on;
use rca_core::{
    ActionPath, Effect, MonotonicInstant, OptionalStatePath, ReduceContext, Reducer, ReducerExt,
};
use rca_test::{DeterministicRuntime, TestStore};

/// Starts one prepared effect and records its output ID.
#[derive(Clone, Debug, Eq, PartialEq)]
enum Action {
    Start,
    Output(u8),
}

/// Consumes one prepared effect, leaving its deadline and action trace observable.
struct Feature(Option<Effect<Action>>);

impl Reducer for Feature {
    type State = Vec<Action>;
    type Action = Action;

    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &Action,
        _: &mut ReduceContext<'_>,
    ) -> Effect<Action> {
        state.push(action.clone());
        match action {
            Action::Start => self.0.take().unwrap(),
            Action::Output(_) => Effect::none(),
        }
    }
}

#[test]
fn deferred_start_does_not_rebase_deadline() {
    block_on(async {
        let effect = Effect::send_at(
            MonotonicInstant::from_elapsed(Duration::from_millis(10)),
            Action::Output(1),
        )
        .delay(Duration::from_millis(4));
        let mut store = TestStore::new(Vec::new(), Feature(Some(effect)));
        let root = store
            .send_with(Action::Start, |s| s.push(Action::Start))
            .await
            .unwrap();
        store.advance(Duration::from_millis(9)).await.unwrap();
        store
            .assert_state(|s| assert_eq!(s, &[Action::Start]))
            .await
            .unwrap();
        assert!(!root.is_finished());
        store.advance(Duration::from_millis(1)).await.unwrap();
        let _ = store
            .receive_with(|a| *a == Action::Output(1), |s| s.push(Action::Output(1)))
            .await
            .unwrap();
        root.finish().await.unwrap();
        store.finish().await.unwrap();
    });
}

#[test]
fn elapsed_deadlines_need_no_further_advance() {
    for deadline in [
        MonotonicInstant::ZERO,
        MonotonicInstant::from_elapsed(Duration::from_millis(3)),
    ] {
        block_on(async {
            let effect =
                Effect::send_at(deadline, Action::Output(1)).delay(Duration::from_millis(4));
            let mut store = TestStore::new(Vec::new(), Feature(Some(effect)));
            let root = store
                .send_with(Action::Start, |s| s.push(Action::Start))
                .await
                .unwrap();
            store.advance(Duration::from_millis(4)).await.unwrap();
            let _ = store
                .receive_with(|a| *a == Action::Output(1), |s| s.push(Action::Output(1)))
                .await
                .unwrap();
            root.finish().await.unwrap();
            store.finish().await.unwrap();
        });
    }
}

#[test]
fn equal_deadlines_preserve_one_shot_insertion_order() {
    block_on(async {
        let effect = Effect::merge(
            [2, 1].map(|id| Effect::send_at(MonotonicInstant::ZERO, Action::Output(id))),
        );
        let mut store = TestStore::new(Vec::new(), Feature(Some(effect)));
        let root = store
            .send_with(Action::Start, |s| s.push(Action::Start))
            .await
            .unwrap();
        for id in [2, 1] {
            let _ = store
                .receive_with(
                    move |a| *a == Action::Output(id),
                    move |s| s.push(Action::Output(id)),
                )
                .await
                .unwrap();
        }
        root.finish().await.unwrap();
        store.finish().await.unwrap();
    });
}

/// A failed drop-log comparison must leave the actual payload destructor usable.
#[test]
fn failed_drop_assertion_does_not_poison_payload_cleanup() {
    let dropped = Arc::new(Mutex::new(Vec::new()));
    let payload = Payload {
        id: 7,
        dropped: Arc::clone(&dropped),
    };
    let mismatch = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        assert_dropped(&dropped, &[99]);
    }));
    assert!(mismatch.is_err(), "the deliberately wrong log must fail");

    // A separate unwind keeps the regression from aborting during double panic.
    let cleanup = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(payload)));
    assert!(
        cleanup.is_ok(),
        "the mismatch must not break payload cleanup"
    );
    assert_dropped(&dropped, &[7]);
}

/// Copies recorded destruction before comparison, leaving the payload cleanup lock unheld.
fn assert_dropped(dropped: &Mutex<Vec<u8>>, expected: &[u8]) {
    let actual = dropped.lock().unwrap().clone();
    assert_eq!(actual, expected);
}

/// Records actual destruction of a non-cloneable scheduled payload.
#[derive(Debug)]
struct Payload {
    id: u8,
    dropped: Arc<Mutex<Vec<u8>>>,
}

impl Drop for Payload {
    fn drop(&mut self) {
        self.dropped.lock().unwrap().push(self.id);
    }
}

/// Carries owned timer output without requiring the payload to clone.
#[derive(Debug)]
enum OwnedAction {
    Start,
    Cancel,
    Output(Payload),
}

/// Consumes a finite sequence of prepared timers and records their payload IDs.
struct OwnedFeature(VecDeque<Effect<OwnedAction>>);

impl Reducer for OwnedFeature {
    type State = Vec<u8>;
    type Action = OwnedAction;

    fn reduce(
        &mut self,
        state: &mut Vec<u8>,
        action: &OwnedAction,
        _: &mut ReduceContext<'_>,
    ) -> Effect<OwnedAction> {
        match action {
            OwnedAction::Start => self.0.pop_front().unwrap(),
            OwnedAction::Cancel => Effect::cancel("request"),
            OwnedAction::Output(payload) => {
                state.push(payload.id);
                Effect::none()
            }
        }
    }
}

/// Maps an owned payload without changing its absolute deadline.
fn owned_effect(
    id: u8,
    deadline: MonotonicInstant,
    dropped: &Arc<Mutex<Vec<u8>>>,
) -> Effect<OwnedAction> {
    Effect::send_at(
        deadline,
        Payload {
            id,
            dropped: Arc::clone(dropped),
        },
    )
    .map(OwnedAction::Output)
}

#[test]
fn mapped_non_clone_payload_arrives_at_original_deadline() {
    block_on(async {
        let dropped = Arc::new(Mutex::new(Vec::new()));
        let deadline = MonotonicInstant::from_elapsed(Duration::from_millis(10));
        let effect = owned_effect(7, deadline, &dropped).delay(Duration::from_millis(4));
        let mut store = TestStore::new(Vec::new(), OwnedFeature([effect].into()));
        let root = store.send_with(OwnedAction::Start, |_| {}).await.unwrap();
        store.advance(Duration::from_millis(9)).await.unwrap();
        assert_dropped(&dropped, &[]);
        store.advance(Duration::from_millis(1)).await.unwrap();
        let _ = store
            .receive_with(
                |a| matches!(a, OwnedAction::Output(p) if p.id == 7),
                |s| s.push(7),
            )
            .await
            .unwrap();
        root.finish().await.unwrap();
        store.finish().await.unwrap();
        assert_dropped(&dropped, &[7]);
    });
}

#[test]
fn maximum_deadline_has_no_sleep_and_root_cancel_drops_payload() {
    block_on(async {
        let runtime = DeterministicRuntime::new();
        let dropped = Arc::new(Mutex::new(Vec::new()));
        let effect = owned_effect(1, MonotonicInstant::MAX, &dropped);
        let store =
            rca_core::Store::new(Vec::new(), OwnedFeature([effect].into()), runtime.runtime());
        let root = runtime
            .run(store.send(OwnedAction::Start))
            .await
            .unwrap()
            .unwrap();
        runtime.run_until_stalled().unwrap();
        assert_eq!(runtime.pending_sleeps(), [] as [rca_test::PendingSleep; 0]);
        assert_dropped(&dropped, &[]);
        assert!(!root.is_finished());
        root.cancel();
        runtime.run(root.finish()).await.unwrap();
        assert_dropped(&dropped, &[1]);
        assert_eq!(
            runtime
                .run(store.read(Clone::clone))
                .await
                .unwrap()
                .unwrap(),
            [] as [u8; 0]
        );
        runtime.run(store.shutdown()).await.unwrap().unwrap();
    });
}

#[test]
fn replacement_releases_old_payload_and_preserves_new_deadline() {
    block_on(async {
        let dropped = Arc::new(Mutex::new(Vec::new()));
        let deadline = MonotonicInstant::from_elapsed(Duration::from_millis(10));
        let effects =
            [1, 2].map(|id| owned_effect(id, deadline, &dropped).cancellable_in_flight("request"));
        let mut store = TestStore::new(Vec::new(), OwnedFeature(effects.into()));
        let first = store.send_with(OwnedAction::Start, |_| {}).await.unwrap();
        store.advance(Duration::from_millis(4)).await.unwrap();
        let second = store.send_with(OwnedAction::Start, |_| {}).await.unwrap();
        first.finish().await.unwrap();
        assert_dropped(&dropped, &[1]);
        store.advance(Duration::from_millis(6)).await.unwrap();
        let _ = store
            .receive_with(
                |a| matches!(a, OwnedAction::Output(p) if p.id == 2),
                |s| s.push(2),
            )
            .await
            .unwrap();
        second.finish().await.unwrap();
        store.finish().await.unwrap();
        assert_dropped(&dropped, &[1, 2]);
    });
}

#[test]
fn shutdown_releases_pending_absolute_payload() {
    block_on(async {
        let runtime = DeterministicRuntime::new();
        let dropped = Arc::new(Mutex::new(Vec::new()));
        let effect = owned_effect(1, MonotonicInstant::MAX, &dropped);
        let store =
            rca_core::Store::new(Vec::new(), OwnedFeature([effect].into()), runtime.runtime());
        let root = runtime
            .run(store.send(OwnedAction::Start))
            .await
            .unwrap()
            .unwrap();
        assert_dropped(&dropped, &[]);
        runtime.run(store.shutdown()).await.unwrap().unwrap();
        runtime.run(root.finish()).await.unwrap();
        assert_dropped(&dropped, &[1]);
    });
}

#[test]
fn cancellation_wrapper_registration_remains_deferred() {
    for outer in [false, true] {
        block_on(async {
            let dropped = Arc::new(Mutex::new(Vec::new()));
            let effect = owned_effect(
                1,
                MonotonicInstant::from_elapsed(Duration::from_millis(10)),
                &dropped,
            );
            let effect = if outer {
                effect
                    .delay(Duration::from_millis(4))
                    .cancellable("request")
            } else {
                effect
                    .cancellable("request")
                    .delay(Duration::from_millis(4))
            };
            let mut store = TestStore::new(Vec::new(), OwnedFeature([effect].into()));
            let root = store.send_with(OwnedAction::Start, |_| {}).await.unwrap();
            let _ = store.send_with(OwnedAction::Cancel, |_| {}).await.unwrap();
            store
                .assert_state(|s| assert_eq!(s.as_slice(), []))
                .await
                .unwrap();
            assert_dropped(&dropped, if outer { &[1] } else { &[] });
            store.advance(Duration::from_millis(10)).await.unwrap();
            if !outer {
                let _ = store
                    .receive_with(
                        |a| matches!(a, OwnedAction::Output(p) if p.id == 1),
                        |s| s.push(1),
                    )
                    .await
                    .unwrap();
            }
            root.finish().await.unwrap();
            store.finish().await.unwrap();
            assert_dropped(&dropped, &[1]);
        });
    }
}

/// Ends the optional child lifetime while leaving removal outside child routing.
struct OptionalParent;

impl Reducer for OptionalParent {
    type State = Option<Vec<u8>>;
    type Action = OwnedAction;

    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &OwnedAction,
        _: &mut ReduceContext<'_>,
    ) -> Effect<OwnedAction> {
        if matches!(action, OwnedAction::Cancel) {
            *state = None;
        }
        Effect::none()
    }
}

#[test]
fn optional_removal_drops_registered_and_deferred_payloads() {
    for deferred in [false, true] {
        block_on(async {
            let dropped = Arc::new(Mutex::new(Vec::new()));
            let effect = owned_effect(1, MonotonicInstant::MAX, &dropped);
            let effect = if deferred {
                effect.delay(Duration::from_millis(4))
            } else {
                effect
            };
            let reducer = OptionalParent.if_some(
                OptionalStatePath::new(
                    |s: &Option<Vec<u8>>| s.as_ref(),
                    |s: &mut Option<Vec<u8>>| s.as_mut(),
                ),
                ActionPath::new(
                    |a: &OwnedAction| {
                        if matches!(a, OwnedAction::Cancel) {
                            None
                        } else {
                            Some(a)
                        }
                    },
                    |a| a,
                ),
                OwnedFeature([effect].into()),
            );
            let mut store = TestStore::new(Some(Vec::new()), reducer);
            let root = store.send_with(OwnedAction::Start, |_| {}).await.unwrap();
            assert_dropped(&dropped, &[]);
            let _ = store
                .send_with(OwnedAction::Cancel, |s| *s = None)
                .await
                .unwrap();
            root.finish().await.unwrap();
            assert_dropped(&dropped, &[1]);
            store.advance(Duration::from_millis(10)).await.unwrap();
            store.finish().await.unwrap();
        });
    }
}
