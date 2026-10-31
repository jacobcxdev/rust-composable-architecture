//! Delayed work preserves cancellation, child lifetimes and capture cleanup.

use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
    time::Duration,
};

use async_channel::{Receiver, Sender};
use futures_executor::block_on;
use rca_core::{ActionPath, Effect, OptionalStatePath, ReduceContext, Reducer, ReducerExt};
use rca_test::TestStore;

/// Virtual delay shared by the independently asserted timing boundaries.
const DELAY: Duration = Duration::from_millis(10);

/// Consumes a prepared effect on the single start and records every action.
struct ProgramFeature(Option<Effect<Action>>);

impl Reducer for ProgramFeature {
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
            Action::Start(_) => self.0.take().unwrap(),
            Action::Cancel => Effect::cancel("request"),
            _ => Effect::none(),
        }
    }
}

#[test]
fn inner_cancellation_registration_waits_until_expiry() {
    for outer in [false, true] {
        block_on(async {
            let effect = Effect::send(Action::Done);
            let effect = if outer {
                effect.delay(DELAY).cancellable("request")
            } else {
                effect.cancellable("request").delay(DELAY)
            };
            let mut store = TestStore::new(Vec::new(), ProgramFeature(Some(effect)));
            let root = store
                .send_with(Action::Start(1), |s| s.push(Action::Start(1)))
                .await
                .unwrap();
            let _ = store
                .send_with(Action::Cancel, |s| s.push(Action::Cancel))
                .await
                .unwrap();
            store.advance(DELAY).await.unwrap();
            if !outer {
                let _ = store
                    .receive_with(|a| *a == Action::Done, |s| s.push(Action::Done))
                    .await
                    .unwrap();
            }
            root.finish().await.unwrap();
            store.finish().await.unwrap();
        });
    }
}

#[test]
fn interval_factory_cancellation_rejects_its_action_and_does_not_reschedule() {
    use futures_util::FutureExt as _;
    use std::sync::atomic::{AtomicUsize, Ordering};

    for cancel in [false, true] {
        let runtime = rca_test::DeterministicRuntime::with_step_budget(64);
        let root_slot = Arc::new(Mutex::new(None::<rca_core::ActionTask>));
        let callback_root = Arc::clone(&root_slot);
        let calls = Arc::new(AtomicUsize::new(0));
        let callback_calls = Arc::clone(&calls);
        let capture = Arc::new(());
        let capture_lifetime = Arc::downgrade(&capture);
        let interval = Effect::interval(DELAY, move || {
            assert_eq!(Arc::strong_count(&capture), 1);
            callback_calls.fetch_add(1, Ordering::SeqCst);
            if cancel {
                callback_root.lock().unwrap().as_ref().unwrap().cancel();
            }
            Action::Done
        });
        let store = rca_core::Store::new(
            Vec::new(),
            ProgramFeature(Some(interval)),
            runtime.runtime(),
        );
        let root = runtime
            .run(store.send(Action::Start(1)))
            .now_or_never()
            .expect("ready actor commits the initial action")
            .unwrap()
            .unwrap();
        *root_slot.lock().unwrap() = Some(root.clone());

        runtime.advance(DELAY).unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(root.is_cancelled(), cancel);
        assert_eq!(root.is_finished(), cancel);
        let state = runtime
            .run(store.read(Clone::clone))
            .now_or_never()
            .expect("ready actor answers the state read")
            .unwrap()
            .unwrap();
        if cancel {
            assert_eq!(state, [Action::Start(1)]);
            assert!(capture_lifetime.upgrade().is_none());
            runtime.advance(DELAY).unwrap();
            assert_eq!(calls.load(Ordering::SeqCst), 1);
        } else {
            assert_eq!(state, [Action::Start(1), Action::Done]);
            assert!(capture_lifetime.upgrade().is_some());
            assert_eq!(runtime.pending_sleeps().len(), 1);
            root.cancel();
            runtime.run_until_stalled().unwrap();
        }
        root.finish()
            .now_or_never()
            .expect("cancelled interval has released its causal work");
        let snapshot = runtime
            .run(store.runtime_snapshot())
            .now_or_never()
            .expect("ready actor answers the runtime snapshot")
            .unwrap()
            .unwrap();
        assert_eq!(snapshot.live_task_ids, [] as [u64; 0]);
        assert_eq!(
            snapshot.timer_deadlines,
            [] as [rca_core::MonotonicInstant; 0]
        );
        assert_eq!(runtime.pending_sleeps(), [] as [rca_test::PendingSleep; 0]);
        assert!(capture_lifetime.upgrade().is_none());
        runtime
            .run(store.shutdown())
            .now_or_never()
            .expect("ready actor acknowledges shutdown")
            .unwrap()
            .unwrap();
    }
}

#[test]
fn nested_delay_starts_when_sequence_element_begins() {
    block_on(async {
        let effect = Effect::send(Action::Done).delay(DELAY).delay(DELAY);
        let mut store = TestStore::new(Vec::new(), ProgramFeature(Some(effect)));
        let root = store
            .send_with(Action::Start(1), |s| s.push(Action::Start(1)))
            .await
            .unwrap();
        store.advance(Duration::from_millis(19)).await.unwrap();
        store
            .assert_state(|s| assert_eq!(s, &[Action::Start(1)]))
            .await
            .unwrap();
        assert!(!root.is_finished());
        store.advance(Duration::from_millis(1)).await.unwrap();
        let _ = store
            .receive_with(|a| *a == Action::Done, |s| s.push(Action::Done))
            .await
            .unwrap();
        root.finish().await.unwrap();
        store.finish().await.unwrap();
    });
}

#[test]
fn zero_and_empty_delays_need_no_clock_advance() {
    block_on(async {
        let effect = Effect::concatenate([
            Effect::none().delay(Duration::MAX),
            Effect::send(Action::Done).delay(Duration::ZERO),
        ]);
        let mut store = TestStore::new(Vec::new(), ProgramFeature(Some(effect)));
        let root = store
            .send_with(Action::Start(1), |s| s.push(Action::Start(1)))
            .await
            .unwrap();
        let _ = store
            .receive_with(|a| *a == Action::Done, |s| s.push(Action::Done))
            .await
            .unwrap();
        root.finish().await.unwrap();
        store.finish().await.unwrap();
    });
}

#[test]
fn saturated_delay_stays_pending_without_sleep_and_can_be_cancelled() {
    block_on(async {
        let runtime = rca_test::DeterministicRuntime::new();
        runtime.advance(Duration::from_secs(1)).unwrap();
        let feature = ProgramFeature(Some(Effect::send(Action::Done).delay(Duration::MAX)));
        let store = rca_core::Store::new(Vec::new(), feature, runtime.runtime());
        let root = runtime
            .run(store.send(Action::Start(1)))
            .await
            .unwrap()
            .unwrap();
        runtime.run_until_stalled().unwrap();
        assert_eq!(runtime.pending_sleeps(), [] as [rca_test::PendingSleep; 0]);
        assert!(!root.is_finished());
        root.cancel();
        runtime.run(root.finish()).await.unwrap();
        assert_eq!(
            runtime
                .run(store.read(Clone::clone))
                .await
                .unwrap()
                .unwrap(),
            [Action::Start(1)]
        );
        runtime.run(store.shutdown()).await.unwrap().unwrap();
    });
}

#[test]
fn equal_deadlines_complete_all_children_without_launch_order_contract() {
    block_on(async {
        let started = Arc::new(Mutex::new(Vec::new()));
        let effect = Effect::merge((0..3).map(|id| {
            let started = Arc::clone(&started);
            Effect::run(move |_| async move {
                started.lock().unwrap().push(id);
            })
            .delay(DELAY)
        }));
        let mut store = TestStore::new(Vec::new(), ProgramFeature(Some(effect)));
        let root = store
            .send_with(Action::Start(1), |s| s.push(Action::Start(1)))
            .await
            .unwrap();
        store.advance(Duration::from_millis(9)).await.unwrap();
        assert!(started.lock().unwrap().is_empty());
        store.advance(Duration::from_millis(1)).await.unwrap();
        let mut actual = started.lock().unwrap().clone();
        actual.sort_unstable();
        assert_eq!(actual, [0, 1, 2]);
        root.finish().await.unwrap();
        store.finish().await.unwrap();
    });
}

/// Identifies a request, its output, cancellation and sequence completion.
#[derive(Clone, Debug, Eq, PartialEq)]
enum Action {
    Start(u8),
    Output(u8),
    Cancel,
    Done,
}

/// Logs the release of a captured request resource, even before it starts.
struct Probe {
    /// Request whose capture was released.
    id: u8,
    /// Shared release trace, kept separate from the operation-start trace.
    dropped: Arc<Mutex<Vec<u8>>>,
}

impl Drop for Probe {
    fn drop(&mut self) {
        self.dropped.lock().unwrap().push(self.id);
    }
}

/// Releases the log lock before an assertion can unwind into capture cleanup.
fn assert_dropped(dropped: &Mutex<Vec<u8>>, expected: &[u8]) {
    let actual = dropped.lock().unwrap().clone();
    assert_eq!(actual, expected);
}

#[test]
fn failed_drop_assertion_does_not_poison_cleanup() {
    let dropped = Arc::new(Mutex::new(Vec::new()));
    let probe = Probe {
        id: 1,
        dropped: Arc::clone(&dropped),
    };

    let mismatch = std::panic::catch_unwind(|| assert_dropped(&dropped, &[1]));
    assert!(mismatch.is_err(), "the deliberately wrong trace must fail");

    let cleanup = std::panic::catch_unwind(|| drop(probe));
    assert!(cleanup.is_ok(), "failed assertions must not poison cleanup");
    assert_dropped(&dropped, &[1]);
}

/// Delays manually released requests and optionally appends a sequence marker.
struct Feature {
    /// Appends `Done` only after the delayed operation has completed.
    sequence: bool,
    /// One bounded response channel per start, consumed in order.
    responses: VecDeque<Receiver<()>>,
    /// Records entry into the operation body, not effect construction.
    started: Arc<Mutex<Vec<u8>>>,
    /// Records capture release independently of operation entry.
    dropped: Arc<Mutex<Vec<u8>>>,
}

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
        match *action {
            Action::Start(id) => {
                let response = self.responses.pop_front().unwrap();
                let started = Arc::clone(&self.started);
                let probe = Probe {
                    id,
                    dropped: Arc::clone(&self.dropped),
                };
                let effect = Effect::run(move |sender| async move {
                    started.lock().unwrap().push(id);
                    response.recv().await.unwrap();
                    let _ = sender.send(Action::Output(id)).await;
                    drop(probe);
                })
                .delay(Duration::from_millis(10))
                .cancellable_in_flight("request");
                if self.sequence {
                    Effect::concatenate([effect, Effect::send(Action::Done)])
                } else {
                    effect
                }
            }
            Action::Cancel => Effect::cancel("request"),
            Action::Output(_) | Action::Done => Effect::none(),
        }
    }
}

/// Creates exactly `count` responses; tests decide when each operation completes.
fn setup(count: usize) -> (Feature, Vec<Sender<()>>) {
    let (senders, responses) = (0..count).map(|_| async_channel::bounded(1)).unzip();
    (
        Feature {
            sequence: false,
            responses,
            started: Arc::default(),
            dropped: Arc::default(),
        },
        senders,
    )
}

#[test]
fn operation_starts_only_after_delay() {
    block_on(async {
        let (feature, responses) = setup(1);
        let started = Arc::clone(&feature.started);
        let dropped = Arc::clone(&feature.dropped);
        let mut store = TestStore::new(Vec::new(), feature);
        let root = store
            .send_with(Action::Start(1), |s| s.push(Action::Start(1)))
            .await
            .unwrap();
        store
            .assert_state(|s| assert_eq!(s, &[Action::Start(1)]))
            .await
            .unwrap();
        assert!(
            started.lock().unwrap().is_empty(),
            "operation started before delay"
        );
        assert_dropped(&dropped, &[]);
        store.advance(Duration::from_millis(9)).await.unwrap();
        assert!(started.lock().unwrap().is_empty());
        store.advance(Duration::from_millis(1)).await.unwrap();
        assert_eq!(*started.lock().unwrap(), [1]);
        assert!(!root.is_finished());
        responses[0].try_send(()).unwrap();
        let _ = store
            .receive_with(|a| *a == Action::Output(1), |s| s.push(Action::Output(1)))
            .await
            .unwrap();
        root.finish().await.unwrap();
        assert_dropped(&dropped, &[1]);
        store.finish().await.unwrap();
    });
}

#[test]
fn debounce_replaces_waiting_work_and_cancels_running_work() {
    block_on(async {
        let (feature, responses) = setup(2);
        let started = Arc::clone(&feature.started);
        let dropped = Arc::clone(&feature.dropped);
        let mut store = TestStore::new(Vec::new(), feature);
        let old = store
            .send_with(Action::Start(1), |s| s.push(Action::Start(1)))
            .await
            .unwrap();
        store.advance(Duration::from_millis(9)).await.unwrap();
        let fresh = store
            .send_with(Action::Start(2), |s| s.push(Action::Start(2)))
            .await
            .unwrap();
        store
            .assert_state(|s| assert_eq!(s.len(), 2))
            .await
            .unwrap();
        assert!(old.is_finished());
        assert!(responses[0].is_closed());
        assert_dropped(&dropped, &[1]);
        store.advance(Duration::from_millis(1)).await.unwrap();
        assert!(started.lock().unwrap().is_empty());
        store.advance(Duration::from_millis(9)).await.unwrap();
        assert_eq!(*started.lock().unwrap(), [2]);
        let _ = store
            .send_with(Action::Cancel, |s| s.push(Action::Cancel))
            .await
            .unwrap();
        store
            .assert_state(|s| assert_eq!(s.len(), 3))
            .await
            .unwrap();
        assert!(responses[1].is_closed());
        assert_dropped(&dropped, &[1, 2]);
        assert!(fresh.is_finished());
        store.finish().await.unwrap();
    });
}

#[test]
fn root_cancellation_drops_waiting_and_running_captures() {
    for elapsed in [0, 10] {
        block_on(async {
            let (feature, responses) = setup(1);
            let dropped = Arc::clone(&feature.dropped);
            let mut store = TestStore::new(Vec::new(), feature);
            let root = store
                .send_with(Action::Start(1), |s| s.push(Action::Start(1)))
                .await
                .unwrap();
            store.advance(Duration::from_millis(elapsed)).await.unwrap();
            root.cancel().await.unwrap();
            store.advance(Duration::ZERO).await.unwrap();
            assert!(root.is_finished());
            assert!(responses[0].is_closed());
            assert_dropped(&dropped, &[1]);
            store.finish().await.unwrap();
        });
    }
}

#[test]
fn sequence_waits_for_child_not_only_delay() {
    block_on(async {
        let (mut feature, responses) = setup(1);
        feature.sequence = true;
        let mut store = TestStore::new(Vec::new(), feature);
        let root = store
            .send_with(Action::Start(1), |s| s.push(Action::Start(1)))
            .await
            .unwrap();
        store.advance(Duration::from_millis(10)).await.unwrap();
        assert!(!root.is_finished());
        store
            .assert_state(|s| assert_eq!(s, &[Action::Start(1)]))
            .await
            .unwrap();
        responses[0].try_send(()).unwrap();
        let _ = store
            .receive_with(|a| *a == Action::Output(1), |s| s.push(Action::Output(1)))
            .await
            .unwrap();
        let _ = store
            .receive_with(|a| *a == Action::Done, |s| s.push(Action::Done))
            .await
            .unwrap();
        root.finish().await.unwrap();
        store.finish().await.unwrap();
    });
}

#[test]
fn shutdown_drops_delayed_capture_without_running_it() {
    block_on(async {
        let (feature, responses) = setup(1);
        let started = Arc::clone(&feature.started);
        let dropped = Arc::clone(&feature.dropped);
        let runtime = rca_test::DeterministicRuntime::new();
        let store = rca_core::Store::new(Vec::new(), feature, runtime.runtime());
        let root = runtime
            .run(store.send(Action::Start(1)))
            .await
            .unwrap()
            .unwrap();
        assert!(started.lock().unwrap().is_empty());
        runtime.run(store.shutdown()).await.unwrap().unwrap();
        assert!(root.is_finished());
        assert!(responses[0].is_closed());
        assert_dropped(&dropped, &[1]);
    });
}

/// Varies where inherited cancellation ends relative to a delayed continuation.
struct BoundaryFeature {
    /// Ends inheritance outside the delay, rather than inside its operation.
    outer: bool,
    /// Adds an explicit cancellation ID that the inheritance boundary must keep.
    explicit: bool,
}

impl Reducer for BoundaryFeature {
    type State = Vec<Action>;
    type Action = Action;
    fn reduce(
        &mut self,
        state: &mut Vec<Action>,
        action: &Action,
        _: &mut ReduceContext<'_>,
    ) -> Effect<Action> {
        state.push(action.clone());
        match action {
            Action::Start(_) => Effect::send(Action::Output(1)).cancellable("request"),
            Action::Output(_) => {
                let effect = Effect::send(Action::Done);
                let effect = if self.outer {
                    effect.delay(DELAY).without_inherited_cancellation()
                } else {
                    effect.without_inherited_cancellation().delay(DELAY)
                };
                if self.explicit {
                    effect.cancellable("request")
                } else {
                    effect
                }
            }
            Action::Cancel => Effect::cancel("request"),
            Action::Done => Effect::none(),
        }
    }
}

#[test]
fn boundary_order_controls_wait_without_removing_explicit_ids() {
    for (outer, explicit, completes) in [
        (false, false, false),
        (true, false, true),
        (true, true, false),
    ] {
        block_on(async {
            let mut store = TestStore::new(Vec::new(), BoundaryFeature { outer, explicit });
            let root = store
                .send_with(Action::Start(1), |s| s.push(Action::Start(1)))
                .await
                .unwrap();
            let _ = store
                .receive_with(|a| *a == Action::Output(1), |s| s.push(Action::Output(1)))
                .await
                .unwrap();
            let _ = store
                .send_with(Action::Cancel, |s| s.push(Action::Cancel))
                .await
                .unwrap();
            store.advance(DELAY).await.unwrap();
            if completes {
                let _ = store
                    .receive_with(|a| *a == Action::Done, |s| s.push(Action::Done))
                    .await
                    .unwrap();
            }
            root.finish().await.unwrap();
            store.finish().await.unwrap();
        });
    }
}

/// Ends an optional child's lifetime on `Cancel` without forwarding that action.
struct OptionalParent;

impl Reducer for OptionalParent {
    type State = Option<Vec<Action>>;
    type Action = Action;
    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &Action,
        _: &mut ReduceContext<'_>,
    ) -> Effect<Action> {
        if *action == Action::Cancel {
            *state = None;
        }
        Effect::none()
    }
}

#[test]
fn mapped_optional_removal_drops_waiting_and_running_work() {
    for elapsed in [0, 10] {
        block_on(async {
            let (feature, responses) = setup(1);
            let started = Arc::clone(&feature.started);
            let dropped = Arc::clone(&feature.dropped);
            let reducer = OptionalParent.if_some(
                OptionalStatePath::new(
                    |s: &Option<Vec<Action>>| s.as_ref(),
                    |s: &mut Option<Vec<Action>>| s.as_mut(),
                ),
                ActionPath::new(
                    |a: &Action| if *a == Action::Cancel { None } else { Some(a) },
                    |a| a,
                ),
                feature,
            );
            let mut store = TestStore::new(Some(Vec::new()), reducer);
            let root = store
                .send_with(Action::Start(1), |s| {
                    s.as_mut().unwrap().push(Action::Start(1));
                })
                .await
                .unwrap();
            store.advance(Duration::from_millis(elapsed)).await.unwrap();
            let _ = store
                .send_with(Action::Cancel, |s| *s = None)
                .await
                .unwrap();
            store.assert_state(|s| assert!(s.is_none())).await.unwrap();
            assert!(root.is_finished());
            assert!(responses[0].is_closed());
            assert_dropped(&dropped, &[1]);
            assert_eq!(started.lock().unwrap().len(), usize::from(elapsed != 0));
            store.advance(DELAY).await.unwrap();
            store.finish().await.unwrap();
        });
    }
}

/// Routes child work, removes one child or starts both children in one reduction.
#[derive(Debug, Eq, PartialEq)]
enum KeyedAction {
    Row(rca_core::KeyedAction<u8, Action>),
    Remove(u8),
    StartAll,
}

/// Removes keyed state without returning additional work.
struct KeyedParent;

impl Reducer for KeyedParent {
    type State = rca_core::KeyedState<u8, Vec<Action>>;
    type Action = KeyedAction;
    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &KeyedAction,
        _: &mut ReduceContext<'_>,
    ) -> Effect<KeyedAction> {
        if let KeyedAction::Remove(id) = action {
            state.remove(id);
        }
        Effect::none()
    }
}

/// Addresses a request in one keyed child.
fn row(id: u8, action: Action) -> KeyedAction {
    KeyedAction::Row(rca_core::KeyedAction::new(id, action))
}

/// Delays the wrapped reducer's complete output by the supplied duration.
struct DelayedReducer<R>(R, Duration);

impl<R: Reducer> Reducer for DelayedReducer<R>
where
    R::Action: Send + 'static,
{
    type State = R::State;
    type Action = R::Action;
    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &Self::Action,
        context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        self.0.reduce(state, action, context).delay(self.1)
    }
}

/// Schedules child removal in the same due batch as the wrapped delayed work.
struct RemoveAtDeadline<R> {
    /// Composed reducer whose child work must not survive removal.
    inner: R,
    /// Registers child work before removal, rather than adding an outer delay.
    wait_before_removal: bool,
}

impl<R: Reducer<State = Option<Vec<Action>>, Action = Action>> Reducer for RemoveAtDeadline<R> {
    type State = R::State;
    type Action = Action;
    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &Action,
        context: &mut ReduceContext<'_>,
    ) -> Effect<Action> {
        let effect = self.inner.reduce(state, action, context);
        if matches!(action, Action::Start(_)) {
            let removal = Effect::send_after(DELAY, Action::Cancel);
            if self.wait_before_removal {
                Effect::merge([effect, removal])
            } else {
                Effect::merge([removal, effect.delay(DELAY)])
            }
        } else {
            effect
        }
    }
}

#[test]
fn scope_removal_in_same_due_batch_prevents_child_continuation() {
    // Removal must suppress both child and outer composed continuations.
    // Cover removal-first, child-wait-first, and completed outer-wait-first.
    for (wait_before_removal, outer_delay) in [(false, false), (true, false), (true, true)] {
        block_on(async {
            let (feature, responses) = setup(1);
            let dropped = Arc::clone(&feature.dropped);
            let started = Arc::clone(&feature.started);
            let reducer = OptionalParent.if_some(
                OptionalStatePath::new(
                    |s: &Option<Vec<Action>>| s.as_ref(),
                    |s: &mut Option<Vec<Action>>| s.as_mut(),
                ),
                ActionPath::new(
                    |a: &Action| if *a == Action::Cancel { None } else { Some(a) },
                    |a| a,
                ),
                feature,
            );
            let mut store = TestStore::new(
                Some(Vec::new()),
                RemoveAtDeadline {
                    inner: DelayedReducer(
                        reducer,
                        if outer_delay { DELAY } else { Duration::ZERO },
                    ),
                    wait_before_removal,
                },
            );
            let root = store
                .send_with(Action::Start(1), |s| {
                    s.as_mut().unwrap().push(Action::Start(1));
                })
                .await
                .unwrap();
            store.advance(DELAY).await.unwrap();
            let _ = store
                .receive_with(|a| *a == Action::Cancel, |s| *s = None)
                .await
                .unwrap();
            assert_dropped(&dropped, &[1]);
            assert!(started.lock().unwrap().is_empty());
            assert!(responses[0].is_closed());
            root.finish().await.unwrap();
            store.finish().await.unwrap();
        });
    }
}

/// Starts two children together, with optional removal before effects launch.
struct Batch<R> {
    /// Keyed composition that attaches each child's lifetime to its work.
    inner: R,
    /// Removes the first child during the same reduction as the two starts.
    remove_during_reduction: bool,
}

impl<R: Reducer<State = rca_core::KeyedState<u8, Vec<Action>>, Action = KeyedAction>> Reducer
    for Batch<R>
{
    type State = R::State;
    type Action = KeyedAction;
    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &KeyedAction,
        context: &mut ReduceContext<'_>,
    ) -> Effect<KeyedAction> {
        if *action != KeyedAction::StartAll {
            return self.inner.reduce(state, action, context);
        }
        let effects = [1, 2].map(|id| {
            self.inner
                .reduce(state, &row(id, Action::Start(id)), context)
        });
        if self.remove_during_reduction {
            drop(self.inner.reduce(state, &KeyedAction::Remove(1), context));
        }
        Effect::merge(effects)
            .without_inherited_cancellation()
            .cancellable_in_flight("batch")
    }
}

#[test]
fn delay_after_keyed_composition_prunes_one_branch_and_preserves_sibling() {
    for remove_during_reduction in [false, true] {
        block_on(async {
            let (feature, responses) = setup(2);
            let started = Arc::clone(&feature.started);
            let dropped = Arc::clone(&feature.dropped);
            let reducer = rca_core::for_each(
                KeyedParent,
                rca_core::KeyedStatePath::new(|s| s, |s| s),
                rca_core::KeyedActionPath::new(
                    |a| match a {
                        KeyedAction::Row(a) => Some(a),
                        KeyedAction::Remove(_) | KeyedAction::StartAll => None,
                    },
                    KeyedAction::Row,
                ),
                feature,
            );
            let reducer = DelayedReducer(
                Batch {
                    inner: reducer,
                    remove_during_reduction,
                },
                DELAY,
            );
            let initial = [(1, Vec::new()), (2, Vec::new())].into_iter().collect();
            let mut store = TestStore::new(initial, reducer);
            let root = store
                .send_with(KeyedAction::StartAll, move |s| {
                    for id in [1, 2] {
                        s.get_mut(&id).unwrap().push(Action::Start(id));
                    }
                    if remove_during_reduction {
                        s.remove(&1);
                    }
                })
                .await
                .unwrap();
            if !remove_during_reduction {
                let _ = store
                    .send_with(KeyedAction::Remove(1), |s| {
                        s.remove(&1);
                    })
                    .await
                    .unwrap();
            }
            store
                .assert_state(|s| assert_eq!(s.len(), 1))
                .await
                .unwrap();
            let released = dropped.lock().unwrap().clone();
            assert_eq!(released, [1]);
            assert!(responses[0].is_closed());
            assert!(!root.is_finished());
            store.advance(DELAY).await.unwrap();
            assert!(started.lock().unwrap().is_empty());
            store.advance(DELAY).await.unwrap();
            assert_eq!(*started.lock().unwrap(), [2]);
            responses[1].try_send(()).unwrap();
            let _ = store
                .receive_with(
                    |a| *a == row(2, Action::Output(2)),
                    |s| {
                        s.get_mut(&2).unwrap().push(Action::Output(2));
                    },
                )
                .await
                .unwrap();
            root.finish().await.unwrap();
            assert_dropped(&dropped, &[1, 2]);
            store.finish().await.unwrap();
        });
    }
}

#[test]
fn delay_after_optional_composition_releases_removed_child() {
    block_on(async {
        let (feature, responses) = setup(1);
        let dropped = Arc::clone(&feature.dropped);
        let reducer = OptionalParent.if_some(
            OptionalStatePath::new(
                |s: &Option<Vec<Action>>| s.as_ref(),
                |s: &mut Option<Vec<Action>>| s.as_mut(),
            ),
            ActionPath::new(
                |a: &Action| if *a == Action::Cancel { None } else { Some(a) },
                |a| a,
            ),
            feature,
        );
        let mut store = TestStore::new(Some(Vec::new()), DelayedReducer(reducer, Duration::MAX));
        let root = store
            .send_with(Action::Start(1), |s| {
                s.as_mut().unwrap().push(Action::Start(1));
            })
            .await
            .unwrap();
        let _ = store
            .send_with(Action::Cancel, |s| *s = None)
            .await
            .unwrap();
        store.assert_state(|s| assert!(s.is_none())).await.unwrap();
        let released = dropped.lock().unwrap().clone();
        assert_eq!(
            released,
            [1],
            "removed child capture retained behind composed delay"
        );
        assert!(responses[0].is_closed());
        root.finish().await.unwrap();
        store.finish().await.unwrap();
    });
}

#[test]
fn mapped_keyed_removal_drops_waiting_work_without_cancelling_sibling() {
    block_on(async {
        let (feature, responses) = setup(2);
        let started = Arc::clone(&feature.started);
        let dropped = Arc::clone(&feature.dropped);
        let reducer = rca_core::for_each(
            KeyedParent,
            rca_core::KeyedStatePath::new(|s| s, |s| s),
            rca_core::KeyedActionPath::new(
                |a| match a {
                    KeyedAction::Row(a) => Some(a),
                    KeyedAction::Remove(_) | KeyedAction::StartAll => None,
                },
                KeyedAction::Row,
            ),
            feature,
        );
        let initial = [(1, Vec::new()), (2, Vec::new())].into_iter().collect();
        let mut store = TestStore::new(initial, reducer);
        let old = store
            .send_with(row(1, Action::Start(1)), |s| {
                s.get_mut(&1).unwrap().push(Action::Start(1));
            })
            .await
            .unwrap();
        let sibling = store
            .send_with(row(2, Action::Start(2)), |s| {
                s.get_mut(&2).unwrap().push(Action::Start(2));
            })
            .await
            .unwrap();
        let _ = store
            .send_with(KeyedAction::Remove(1), |s| {
                s.remove(&1);
            })
            .await
            .unwrap();
        store
            .assert_state(|s| assert_eq!(s.len(), 1))
            .await
            .unwrap();
        assert!(old.is_finished());
        assert!(responses[0].is_closed());
        assert_dropped(&dropped, &[1]);
        assert!(!sibling.is_finished());
        store.advance(DELAY).await.unwrap();
        assert_eq!(*started.lock().unwrap(), [2]);
        responses[1].try_send(()).unwrap();
        let _ = store
            .receive_with(
                |a| *a == row(2, Action::Output(2)),
                |s| s.get_mut(&2).unwrap().push(Action::Output(2)),
            )
            .await
            .unwrap();
        sibling.finish().await.unwrap();
        assert_dropped(&dropped, &[1, 2]);
        store.finish().await.unwrap();
    });
}

#[test]
fn cancelling_lineage_mid_timer_batch_rejects_siblings_and_new_timers() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Cancels a shared root from the first timer callback in a due batch.
    struct CancellingTimers {
        /// Enables cancellation; false is the independent positive control.
        cancel: bool,
        /// Receives the root handle before any virtual deadline is advanced.
        root: Arc<Mutex<Option<rca_core::ActionTask>>>,
        /// Counts interval factory calls, including calls with rejected output.
        interval_calls: Arc<AtomicUsize>,
    }

    impl Reducer for CancellingTimers {
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
                Action::Start(_) => {
                    let interval_calls = Arc::clone(&self.interval_calls);
                    Effect::merge([
                        Effect::send_after(DELAY, Action::Cancel),
                        Effect::send(Action::Output(1)).delay(DELAY),
                        Effect::send_after(DELAY, Action::Output(2)),
                        Effect::interval(DELAY, move || {
                            interval_calls.fetch_add(1, Ordering::Relaxed);
                            Action::Output(3)
                        }),
                    ])
                }
                Action::Cancel => {
                    if self.cancel {
                        self.root.lock().unwrap().as_ref().unwrap().cancel();
                    }
                    Effect::send_after(DELAY, Action::Done)
                }
                _ => Effect::none(),
            }
        }
    }

    for cancel in [false, true] {
        block_on(async {
            let runtime = rca_test::DeterministicRuntime::new();
            let root_slot = Arc::new(Mutex::new(None));
            let interval_calls = Arc::new(AtomicUsize::new(0));
            let store = rca_core::Store::new(
                Vec::new(),
                CancellingTimers {
                    cancel,
                    root: Arc::clone(&root_slot),
                    interval_calls: Arc::clone(&interval_calls),
                },
                runtime.runtime(),
            );
            let root = runtime
                .run(store.send(Action::Start(1)))
                .await
                .unwrap()
                .unwrap();
            *root_slot.lock().unwrap() = Some(root.clone());

            // Cancellation happens inside the first callback, after its siblings became due.
            runtime.advance(DELAY).unwrap();
            assert_eq!(interval_calls.load(Ordering::Relaxed), usize::from(!cancel));
            assert_eq!(root.is_finished(), cancel);
            runtime.advance(DELAY).unwrap();
            let state = runtime
                .run(store.read(Clone::clone))
                .await
                .unwrap()
                .unwrap();
            if cancel {
                assert_eq!(state, [Action::Start(1), Action::Cancel]);
                assert_eq!(interval_calls.load(Ordering::Relaxed), 0);
            } else {
                // Positive control: every timer family and the newly returned timer can run.
                assert_eq!(state.len(), 7);
                for (action, count) in [
                    (Action::Start(1), 1),
                    (Action::Cancel, 1),
                    (Action::Output(1), 1),
                    (Action::Output(2), 1),
                    (Action::Output(3), 2),
                    (Action::Done, 1),
                ] {
                    assert_eq!(state.iter().filter(|item| **item == action).count(), count);
                }
                assert_eq!(interval_calls.load(Ordering::Relaxed), 2);
                root.cancel();
            }
            runtime.run_until_stalled().unwrap();
            futures_util::FutureExt::now_or_never(root.finish())
                .expect("all cancelled or committed work must be complete");
            let snapshot = runtime
                .run(store.runtime_snapshot())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(snapshot.live_task_ids, [] as [u64; 0]);
            assert_eq!(
                snapshot.timer_deadlines,
                [] as [rca_core::MonotonicInstant; 0]
            );
            assert_eq!(runtime.pending_sleeps(), [] as [rca_test::PendingSleep; 0]);
            runtime.run(store.shutdown()).await.unwrap().unwrap();
        });
    }
}

#[test]
fn timer_release_rechecks_authority_after_state_assertion() {
    for cancel in [false, true] {
        block_on(async {
            let runtime = rca_test::DeterministicRuntime::new();
            let gate = rca_core::testing::AssertionGate::new();
            let feature = ProgramFeature(Some(Effect::send_after(DELAY, Action::Done)));
            let store = rca_core::Store::builder(Vec::new(), feature, runtime.runtime())
                .assertion_gate(gate.clone())
                .build()
                .unwrap();
            let root = runtime
                .run(store.send(Action::Start(1)))
                .await
                .unwrap()
                .unwrap();
            runtime.advance(DELAY).unwrap();
            assert_eq!(gate.inspect_pending(Clone::clone), Some(Action::Done));
            assert!(!root.is_finished());

            let inspected_root = root.clone();
            let inspection = gate.assert_state::<Vec<Action>>(move |state| {
                assert_eq!(state, &[Action::Start(1)]);
                if cancel {
                    inspected_root.cancel();
                }
            });
            let released = gate.release_pending_with_receipt().unwrap();
            runtime.run_until_stalled().unwrap();
            futures_util::FutureExt::now_or_never(inspection.finish())
                .expect("queued state inspection must be resolved")
                .unwrap();
            let committed = futures_util::FutureExt::now_or_never(released.finish())
                .expect("released timer must receive its commit result");
            if cancel {
                committed.expect_err("cancelled timer must not commit after release");
            } else {
                let _accepted = committed.expect("live timer commits after release");
            }

            runtime.run_until_stalled().unwrap();
            futures_util::FutureExt::now_or_never(root.finish())
                .expect("all cancelled or committed work must be complete");
            assert_eq!(root.is_cancelled(), cancel);
            assert!(!gate.has_pending());
            let expected = if cancel {
                vec![Action::Start(1)]
            } else {
                vec![Action::Start(1), Action::Done]
            };
            assert_eq!(
                runtime
                    .run(store.read(Clone::clone))
                    .await
                    .unwrap()
                    .unwrap(),
                expected
            );
            let snapshot = runtime
                .run(store.runtime_snapshot())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(snapshot.live_task_ids, [] as [u64; 0]);
            assert_eq!(
                snapshot.timer_deadlines,
                [] as [rca_core::MonotonicInstant; 0]
            );
            assert_eq!(runtime.pending_sleeps(), [] as [rca_test::PendingSleep; 0]);
            runtime.run(store.shutdown()).await.unwrap().unwrap();
        });
    }
}
