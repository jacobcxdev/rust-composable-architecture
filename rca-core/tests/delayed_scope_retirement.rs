//! Cancellation of delayed continuations with a clock held at zero.
//!
//! Future deadlines never elapse here. Completion must come from retiring the
//! correct owner, while unrelated sibling work remains usable.

use std::{
    fmt,
    sync::{Arc, Mutex},
    task::{Context, Poll, Waker},
    time::Duration,
};

use rca_core::{
    Effect, KeyedAction, KeyedActionPath, KeyedState, KeyedStatePath, MonotonicInstant,
    ReduceContext, Reducer, ReducerExt, Runtime, RuntimeDriver, RuntimeFuture, Store, for_each,
};

#[derive(Clone, Default)]
/// Retains tasks for bounded explicit polling and counts constructed timer waits.
struct Host {
    /// Is drained before polling so callbacks never run under the queue lock.
    tasks: Arc<Mutex<Vec<RuntimeFuture>>>,
    /// Counts construction, not polling, including already elapsed deadlines.
    sleeps: Arc<Mutex<usize>>,
}

impl fmt::Debug for Host {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Host").finish_non_exhaustive()
    }
}

impl RuntimeDriver for Host {
    fn spawn(&self, task: RuntimeFuture) {
        self.tasks.lock().unwrap().push(task);
    }
    fn now(&self) -> MonotonicInstant {
        MonotonicInstant::ZERO
    }
    fn sleep_until(&self, deadline: MonotonicInstant) -> RuntimeFuture {
        *self.sleeps.lock().unwrap() += 1;
        if deadline <= self.now() {
            Box::pin(std::future::ready(()))
        } else {
            Box::pin(std::future::pending())
        }
    }
    fn yield_now(&self) -> RuntimeFuture {
        let mut yielded = false;
        Box::pin(std::future::poll_fn(move |cx| {
            if yielded {
                Poll::Ready(())
            } else {
                yielded = true;
                cx.waker().wake_by_ref();
                Poll::Pending
            }
        }))
    }
}

impl Host {
    /// Gives retained work eight scheduling turns without advancing the clock.
    fn pump(&self) {
        // Fixed scheduling opportunities, never wall-clock time or thread sleeps.
        for _ in 0..8 {
            let tasks = std::mem::take(&mut *self.tasks.lock().unwrap());
            for mut task in tasks {
                if task
                    .as_mut()
                    .poll(&mut Context::from_waker(Waker::noop()))
                    .is_pending()
                {
                    self.tasks.lock().unwrap().push(task);
                }
            }
        }
    }

    /// Completes a finite request and drains the work it released.
    fn complete<F: Future>(&self, future: F) -> F::Output {
        let mut future = std::pin::pin!(future);
        for _ in 0..32 {
            if let Poll::Ready(output) = future
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
            {
                self.pump();
                return output;
            }
            self.pump();
        }
        panic!("causal operation did not complete within the bounded schedule");
    }
}

/// Records which child's captured owner was destroyed, exactly once.
struct Capture {
    /// Selects the child or independent sibling counter.
    slot: usize,
    drops: Arc<Mutex<[u8; 2]>>,
}

#[test]
fn host_clock_counts_sleep_construction_and_completes_elapsed_deadlines() {
    let host = Host::default();
    let mut context = Context::from_waker(Waker::noop());
    let mut elapsed = host.sleep_until(MonotonicInstant::ZERO);
    assert_eq!(*host.sleeps.lock().unwrap(), 1);
    assert!(
        elapsed.as_mut().poll(&mut context).is_ready(),
        "elapsed deadline must be ready on its first poll"
    );
    assert_eq!(*host.sleeps.lock().unwrap(), 1);

    for (index, deadline) in [
        MonotonicInstant::from_elapsed(Duration::from_nanos(1)),
        MonotonicInstant::MAX,
    ]
    .into_iter()
    .enumerate()
    {
        let mut future = host.sleep_until(deadline);
        assert_eq!(*host.sleeps.lock().unwrap(), index + 2);
        assert!(future.as_mut().poll(&mut context).is_pending());
        assert!(future.as_mut().poll(&mut context).is_pending());
        assert_eq!(*host.sleeps.lock().unwrap(), index + 2);
        assert_eq!(host.now(), MonotonicInstant::ZERO);
    }
}

#[test]
fn zero_delay_commits_in_sequence_without_admitting_a_timer() {
    let host = Host::default();
    let reducer = rca_core::Reduce::new(|state: &mut Vec<u8>, action: &Option<u8>, _| {
        if let Some(value) = action {
            state.push(*value);
            Effect::none()
        } else {
            Effect::concatenate([
                Effect::send(Some(1)).delay(Duration::ZERO),
                Effect::none().delay(Duration::from_secs(60)),
                Effect::send(Some(2)).delay(Duration::ZERO),
            ])
        }
    });
    let store = Store::new(Vec::new(), reducer, Runtime::new(host.clone()));

    let task = host.complete(store.send(None)).unwrap();
    host.complete(task.finish());
    assert!(!task.is_cancelled());
    assert_eq!(host.complete(store.read(Clone::clone)).unwrap(), [1, 2]);
    assert_eq!(*host.sleeps.lock().unwrap(), 0);
    host.complete(store.shutdown()).unwrap();
    assert!(host.tasks.lock().unwrap().is_empty());
}

#[test]
fn retiring_parent_finishes_a_live_root_wait_without_cancelling_sibling() {
    assert_retired_wait_finishes(false);
}

#[test]
fn retiring_parent_finishes_an_already_cancelled_parent_wait_once() {
    assert_retired_wait_finishes(true);
}

#[expect(
    clippy::too_many_lines,
    reason = "one causal schedule keeps wait ownership, disposal and sibling isolation together"
)]
/// Contrasts a root-owned wait with one already cancelled by its parent scope.
fn assert_retired_wait_finishes(delay_inside_parent: bool) {
    struct State {
        parent: Option<KeyedState<u8, ChildState>>,
        sibling_pings: usize,
    }
    enum Action {
        Child(KeyedAction<u8, ChildAction>),
        RemoveParent,
        StartSibling,
        SiblingPing,
    }

    let host = Host::default();
    let starts = Arc::new(Mutex::new([0; 2]));
    let drops = Arc::new(Mutex::new([0; 2]));
    let mut children = for_each(
        rca_core::Reduce::new(
            |_: &mut KeyedState<u8, ChildState>, _: &KeyedAction<u8, ChildAction>, _| {
                Effect::none()
            },
        ),
        KeyedStatePath::new(|state| state, |state| state),
        KeyedActionPath::new(|action| Some(action), std::convert::identity),
        Child {
            starts: Arc::clone(&starts),
            drops: Arc::clone(&drops),
        },
    );
    let parent = rca_core::Reduce::new(
        move |state: &mut KeyedState<u8, ChildState>,
              action: &KeyedAction<u8, ChildAction>,
              context| {
            let effect = children.reduce(state, action, context);
            if delay_inside_parent && matches!(action.action(), ChildAction::Start) {
                // The optional composition adds the parent scope to this wait;
                // the remaining keyed effect retains its descendant scope.
                effect.delay(Duration::from_secs(60))
            } else {
                effect
            }
        },
    );
    let (release, released) = futures_channel::oneshot::channel::<()>();
    let mut released = Some(released);
    let sibling_starts = Arc::clone(&starts);
    let sibling_drops = Arc::clone(&drops);
    let root = rca_core::Reduce::new(move |state: &mut State, action: &Action, _| match action {
        Action::RemoveParent => {
            state.parent = None;
            Effect::none()
        }
        Action::StartSibling => {
            let released = released.take().expect("one independent sibling starts");
            let capture = Capture {
                slot: 1,
                drops: Arc::clone(&sibling_drops),
            };
            let starts = Arc::clone(&sibling_starts);
            Effect::run(move |sender| async move {
                starts.lock().unwrap()[capture.slot] += 1;
                std::hint::black_box(&capture);
                released.await.expect("test releases the live sibling");
                sender.send(Action::SiblingPing).await.unwrap();
                std::future::pending::<()>().await;
            })
        }
        Action::SiblingPing => {
            state.sibling_pings += 1;
            Effect::none()
        }
        Action::Child(_) => Effect::none(),
    });
    let mut family = root.if_some(
        rca_core::OptionalStatePath::new(
            |state: &State| state.parent.as_ref(),
            |state: &mut State| state.parent.as_mut(),
        ),
        rca_core::ActionPath::new(
            |action: &Action| match action {
                Action::Child(child) => Some(child),
                _ => None,
            },
            Action::Child,
        ),
        parent,
    );
    let reducer = rca_core::Reduce::new(move |state: &mut State, action: &Action, context| {
        let effect = family.reduce(state, action, context);
        if !delay_inside_parent
            && matches!(action, Action::Child(child) if matches!(child.action(), ChildAction::Start))
        {
            // The outer composed effect has no scope: its wait stays at
            // root while its remaining optional/keyed effect is scoped.
            effect.delay(Duration::from_secs(60))
        } else {
            effect
        }
    });
    let store = Store::new(
        State {
            parent: Some(
                [(1, ChildState { slot: 0, pings: 0 })]
                    .into_iter()
                    .collect(),
            ),
            sibling_pings: 0,
        },
        reducer,
        Runtime::new(host.clone()),
    );

    // This separately committed sibling is not part of the delayed queue.
    let sibling = host.complete(store.send(Action::StartSibling)).unwrap();
    assert_eq!(*starts.lock().unwrap(), [0, 1]);
    assert!(!sibling.is_finished());
    let delayed = host
        .complete(store.send(Action::Child(KeyedAction::new(1, ChildAction::Start))))
        .unwrap();
    assert!(!delayed.is_finished());
    assert_eq!(*host.sleeps.lock().unwrap(), 1);
    assert_eq!(*starts.lock().unwrap(), [0, 1]);
    assert_eq!(*drops.lock().unwrap(), [0, 0]);

    let removal = host.complete(store.send(Action::RemoveParent)).unwrap();
    host.complete(removal.finish());
    // Host time is always zero and this future deadline stays Pending.
    host.complete(delayed.finish());
    assert!(delayed.is_finished());
    assert_eq!(*host.sleeps.lock().unwrap(), 1);
    assert_eq!(*starts.lock().unwrap(), [0, 1]);
    assert_eq!(*drops.lock().unwrap(), [1, 0]);
    assert!(!sibling.is_finished());
    assert!(!sibling.is_cancelled());
    assert_eq!(
        host.complete(store.read(|state| (state.parent.is_none(), state.sibling_pings)))
            .unwrap(),
        (true, 0)
    );

    // The surviving effect can still send and commit an ordinary output.
    release.send(()).unwrap();
    host.pump();
    assert_eq!(
        host.complete(store.read(|state| (state.parent.is_none(), state.sibling_pings)))
            .unwrap(),
        (true, 1)
    );
    assert!(!sibling.is_finished());
    assert!(!sibling.is_cancelled());
    assert_eq!(*drops.lock().unwrap(), [1, 0]);

    host.complete(store.shutdown()).unwrap();
    host.complete(sibling.finish());
    assert!(sibling.is_cancelled());
    assert_eq!(*starts.lock().unwrap(), [0, 1]);
    assert_eq!(*drops.lock().unwrap(), [1, 1]);
    assert!(host.tasks.lock().unwrap().is_empty());
}

impl Drop for Capture {
    fn drop(&mut self) {
        self.drops.lock().unwrap()[self.slot] += 1;
    }
}

/// Separates lifetime counters from outputs committed by a surviving child.
struct ChildState {
    /// Chooses the independent start/drop counters for this child.
    slot: usize,
    pings: usize,
}
/// Starts owned pending work or proves that a surviving route still commits.
enum ChildAction {
    Start,
    Ping,
}
/// Counts entry and destruction independently for each pending child operation.
struct Child {
    /// Distinguishes constructed effects from operations that actually started.
    starts: Arc<Mutex<[u8; 2]>>,
    /// Records capture destruction even when a retired operation never starts.
    drops: Arc<Mutex<[u8; 2]>>,
}
impl Reducer for Child {
    type State = ChildState;
    type Action = ChildAction;
    fn reduce(
        &mut self,
        state: &mut ChildState,
        action: &ChildAction,
        _: &mut ReduceContext<'_>,
    ) -> Effect<ChildAction> {
        match action {
            ChildAction::Ping => {
                state.pings += 1;
                Effect::none()
            }
            ChildAction::Start => {
                let capture = Capture {
                    slot: state.slot,
                    drops: Arc::clone(&self.drops),
                };
                let starts = Arc::clone(&self.starts);
                Effect::run(move |_| async move {
                    starts.lock().unwrap()[capture.slot] += 1;
                    std::hint::black_box(&capture);
                    std::future::pending::<()>().await;
                })
            }
        }
    }
}

/// Removes child one during the same transaction that creates its pending work.
struct Parent;
impl Reducer for Parent {
    type State = KeyedState<u8, ChildState>;
    type Action = KeyedAction<u8, ChildAction>;
    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &Self::Action,
        _: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        if *action.id() == 1 && matches!(action.action(), ChildAction::Start) {
            state.remove(&1);
        }
        Effect::none()
    }
}

/// Delays only the retiring child's continuation, leaving its sibling immediate.
struct DelayRetired<R>(R);
impl<R: Reducer<State = KeyedState<u8, ChildState>, Action = KeyedAction<u8, ChildAction>>> Reducer
    for DelayRetired<R>
{
    type State = R::State;
    type Action = R::Action;
    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &Self::Action,
        context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        let effect = self.0.reduce(state, action, context);
        if *action.id() == 1 {
            effect.delay(Duration::from_secs(60))
        } else {
            effect
        }
    }
}

#[test]
fn retiring_the_only_delayed_continuation_finishes_without_waiting_or_cancelling_sibling() {
    // Retaining the private wait after its only scoped continuation is pruned
    // leaves the retired task unfinished and admits an unnecessary timer.
    let host = Host::default();
    let starts = Arc::new(Mutex::new([0; 2]));
    let drops = Arc::new(Mutex::new([0; 2]));
    let reducer = DelayRetired(for_each(
        Parent,
        KeyedStatePath::new(|state| state, |state| state),
        KeyedActionPath::new(|action| Some(action), std::convert::identity),
        Child {
            starts: Arc::clone(&starts),
            drops: Arc::clone(&drops),
        },
    ));
    let store = Store::new(
        [
            (1, ChildState { slot: 0, pings: 0 }),
            (2, ChildState { slot: 1, pings: 0 }),
        ]
        .into_iter()
        .collect(),
        reducer,
        Runtime::new(host.clone()),
    );
    let sibling = host
        .complete(store.send(KeyedAction::new(2, ChildAction::Start)))
        .unwrap();
    assert_eq!(*starts.lock().unwrap(), [0, 1]);
    assert!(!sibling.is_finished());
    let retired = host
        .complete(store.send(KeyedAction::new(1, ChildAction::Start)))
        .unwrap();
    host.complete(retired.finish());
    assert!(retired.is_finished());
    assert_eq!(*host.sleeps.lock().unwrap(), 0);
    assert_eq!(*starts.lock().unwrap(), [0, 1]);
    assert_eq!(*drops.lock().unwrap(), [1, 0]);
    assert!(!sibling.is_finished());
    assert!(!sibling.is_cancelled());
    let ping = host
        .complete(store.send(KeyedAction::new(2, ChildAction::Ping)))
        .unwrap();
    host.complete(ping.finish());
    assert_eq!(
        host.complete(store.read(|state| (state.contains_id(&1), state.get(&2).unwrap().pings)))
            .unwrap(),
        (false, 1)
    );
    host.complete(store.shutdown()).unwrap();
    host.complete(sibling.finish());
    assert!(sibling.is_cancelled());
    assert_eq!(*drops.lock().unwrap(), [1, 1]);
    assert!(host.tasks.lock().unwrap().is_empty());
}

#[test]
fn pruning_retired_child_preserves_live_singleton_continuation() {
    // Removing a retired prefix must not discard its surviving non-Wait suffix.
    let host = Host::default();
    let starts = Arc::new(Mutex::new([0; 2]));
    let drops = Arc::new(Mutex::new([0; 2]));
    let mut children = for_each(
        Parent,
        KeyedStatePath::new(|state| state, |state| state),
        KeyedActionPath::new(|action| Some(action), std::convert::identity),
        Child {
            starts: Arc::clone(&starts),
            drops: Arc::clone(&drops),
        },
    );
    let reducer = rca_core::Reduce::new(
        move |state: &mut KeyedState<u8, ChildState>,
              action: &KeyedAction<u8, ChildAction>,
              context| {
            let effect = children.reduce(state, action, context);
            if *action.id() == 1 && matches!(action.action(), ChildAction::Start) {
                Effect::concatenate([effect, Effect::send(KeyedAction::new(2, ChildAction::Ping))])
            } else {
                effect
            }
        },
    );
    let store = Store::new(
        [
            (1, ChildState { slot: 0, pings: 0 }),
            (2, ChildState { slot: 1, pings: 0 }),
        ]
        .into_iter()
        .collect(),
        reducer,
        Runtime::new(host.clone()),
    );
    let sibling = host
        .complete(store.send(KeyedAction::new(2, ChildAction::Start)))
        .unwrap();
    assert_eq!(*starts.lock().unwrap(), [0, 1]);
    assert!(!sibling.is_finished());

    let retired = host
        .complete(store.send(KeyedAction::new(1, ChildAction::Start)))
        .unwrap();
    host.complete(retired.finish());
    assert!(retired.is_finished());
    assert!(!retired.is_cancelled());
    assert_eq!(*host.sleeps.lock().unwrap(), 0);
    assert_eq!(*starts.lock().unwrap(), [0, 1]);
    assert_eq!(*drops.lock().unwrap(), [1, 0]);
    assert_eq!(
        host.complete(store.read(|state| (state.contains_id(&1), state.get(&2).unwrap().pings)))
            .unwrap(),
        (false, 1)
    );
    assert!(!sibling.is_finished());
    assert!(!sibling.is_cancelled());

    host.complete(store.shutdown()).unwrap();
    host.complete(sibling.finish());
    assert!(sibling.is_cancelled());
    assert_eq!(*starts.lock().unwrap(), [0, 1]);
    assert_eq!(*drops.lock().unwrap(), [1, 1]);
    assert!(host.tasks.lock().unwrap().is_empty());
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one causal schedule keeps retirement and resumed-continuation assertions together"
)]
fn retiring_a_child_prunes_an_already_deferred_effect_and_keeps_the_live_suffix() {
    // Dropping every deferred effect would lose the sibling send; retaining the
    // retired child capture would keep it alive until the sequence resumes.
    enum Action {
        Start,
        Remove,
        Ping,
    }
    let host = Host::default();
    let starts = Arc::new(Mutex::new([0; 2]));
    let drops = Arc::new(Mutex::new([0; 2]));
    let child_starts = Arc::clone(&starts);
    let child_drops = Arc::clone(&drops);
    let child = rca_core::Reduce::new(
        move |state: &mut ChildState, action: &Action, _| match action {
            Action::Start => {
                let capture = Capture {
                    slot: state.slot,
                    drops: Arc::clone(&child_drops),
                };
                let starts = Arc::clone(&child_starts);
                Effect::run(move |_| async move {
                    starts.lock().unwrap()[capture.slot] += 1;
                    std::hint::black_box(&capture);
                })
            }
            Action::Remove => Effect::none(),
            Action::Ping => {
                state.pings += 1;
                Effect::none()
            }
        },
    );
    let parent = rca_core::Reduce::new(
        |state: &mut KeyedState<u8, ChildState>, action: &KeyedAction<u8, Action>, _| {
            if matches!(action.action(), Action::Remove) {
                state.remove(action.id());
            }
            Effect::none()
        },
    );
    let mut children = for_each(
        parent,
        KeyedStatePath::new(|state| state, |state| state),
        KeyedActionPath::new(|action| Some(action), std::convert::identity),
        child,
    );
    let (release, released) = futures_channel::oneshot::channel::<()>();
    let mut released = Some(released);
    let reducer = rca_core::Reduce::new(
        move |state: &mut KeyedState<u8, ChildState>, action: &KeyedAction<u8, Action>, context| {
            let child = children.reduce(state, action, context);
            if matches!(action.action(), Action::Start) {
                let released = released.take().expect("one sequence starts");
                Effect::concatenate([
                    Effect::run(move |_| async move {
                        released.await.expect("test releases the running prefix");
                    }),
                    child,
                    Effect::send(KeyedAction::new(2, Action::Ping)),
                ])
            } else {
                child
            }
        },
    );
    let store = Store::new(
        [
            (1, ChildState { slot: 0, pings: 0 }),
            (2, ChildState { slot: 1, pings: 0 }),
        ]
        .into_iter()
        .collect(),
        reducer,
        Runtime::new(host.clone()),
    );

    let sequence = host
        .complete(store.send(KeyedAction::new(1, Action::Start)))
        .unwrap();
    assert!(!sequence.is_finished());
    assert_eq!(*starts.lock().unwrap(), [0, 0]);
    assert_eq!(*drops.lock().unwrap(), [0, 0]);

    let removal = host
        .complete(store.send(KeyedAction::new(1, Action::Remove)))
        .unwrap();
    host.complete(removal.finish());
    assert!(!sequence.is_finished());
    assert_eq!(*starts.lock().unwrap(), [0, 0]);
    assert_eq!(*drops.lock().unwrap(), [1, 0]);
    assert_eq!(
        host.complete(store.read(|state| (state.contains_id(&1), state.get(&2).unwrap().pings)))
            .unwrap(),
        (false, 0)
    );

    release.send(()).unwrap();
    host.complete(sequence.finish());
    assert!(!sequence.is_cancelled());
    assert_eq!(*starts.lock().unwrap(), [0, 0]);
    assert_eq!(*drops.lock().unwrap(), [1, 0]);
    assert_eq!(
        host.complete(store.read(|state| state.get(&2).unwrap().pings))
            .unwrap(),
        1
    );
    host.complete(store.shutdown()).unwrap();
    assert!(host.tasks.lock().unwrap().is_empty());
}
