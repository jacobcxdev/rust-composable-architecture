//! Explicit domain windows coalesce values without storing opaque effects.

use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use futures_executor::block_on;
use rca_core::{Effect, MonotonicInstant, ReduceContext, Reducer};
use rca_test::TestStore;

/// Fixed virtual window used by the literal timestamp expectations below.
const PERIOD: Duration = Duration::from_millis(10);

/// Chooses whether samples inside a live window are discarded or remembered.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Mode {
    /// Keeps only the sample that opened the window.
    Leading,
    /// Publishes the last pending sample when the window ends.
    Latest,
}

/// Keeps the domain's window and pending value visible to state assertions.
#[derive(Clone, Debug, Eq, PartialEq)]
struct State {
    /// Determines what happens to samples received inside the current window.
    mode: Mode,
    /// End of the current window, or no live window.
    deadline: Option<MonotonicInstant>,
    /// At most one sample retained by latest-value mode.
    pending: Option<u64>,
    /// Values whose publication actions actually committed.
    published: Vec<u64>,
}

impl State {
    /// Starts with no live window, pending sample or published output.
    fn new(mode: Mode) -> Self {
        Self {
            mode,
            deadline: None,
            pending: None,
            published: Vec::new(),
        }
    }
}

/// Separates external samples and reset from effect-driven window/output actions.
#[derive(Debug, Eq, PartialEq)]
enum Action {
    Sample(u64),
    Boundary,
    Published(u64),
    Reset,
}

/// Replaces or cancels one child's current window and pending publication.
#[derive(Hash, Eq, PartialEq)]
struct Window;

/// Demonstrates explicit domain throttling using typed state and timer effects.
struct Throttle;

impl Reducer for Throttle {
    type State = State;
    type Action = Action;

    fn reduce(
        &mut self,
        state: &mut State,
        action: &Action,
        context: &mut ReduceContext<'_>,
    ) -> Effect<Action> {
        match action {
            Action::Sample(value) => {
                if state
                    .deadline
                    .is_some_and(|deadline| context.now() < deadline)
                {
                    if state.mode == Mode::Latest {
                        state.pending = Some(*value);
                    }
                    return Effect::none();
                }
                state.pending = None;
                publish_and_open(state, *value, context.now())
            }
            Action::Boundary => {
                let deadline = state.deadline.expect("live window owns boundary");
                assert!(context.now() >= deadline, "window cannot close early");
                if let Some(value) = state.pending.take() {
                    publish_and_open(state, value, context.now())
                } else {
                    state.deadline = None;
                    Effect::none()
                }
            }
            Action::Published(value) => {
                state.published.push(*value);
                Effect::none()
            }
            Action::Reset => {
                state.deadline = None;
                state.pending = None;
                Effect::cancel(Window)
            }
        }
    }
}

/// Replaces the current window and publishes a value under its cancellation ID.
fn publish_and_open(state: &mut State, value: u64, now: MonotonicInstant) -> Effect<Action> {
    let deadline = now.saturating_add(PERIOD);
    state.deadline = Some(deadline);
    Effect::merge([
        Effect::send_at(deadline, Action::Boundary),
        Effect::send(Action::Published(value)),
    ])
    .cancellable_in_flight(Window)
    .without_inherited_cancellation()
}

#[test]
fn reset_cancels_publication_before_it_commits() {
    for mode in [Mode::Leading, Mode::Latest] {
        block_on(async {
            let host = rca_test::DeterministicRuntime::new();
            let store = rca_core::Store::new(State::new(mode), Throttle, host.runtime());
            let sample = store.try_send(Action::Sample(1)).unwrap();
            let reset = store.try_send(Action::Reset).unwrap();
            let root = host.run(sample.finish()).await.unwrap().unwrap();
            let reset = host.run(reset.finish()).await.unwrap().unwrap();
            host.run_until_stalled().unwrap();
            let actual = host.run(store.read(Clone::clone)).await.unwrap().unwrap();
            assert_eq!(actual, State::new(mode));
            host.run(root.finish()).await.unwrap();
            host.run(reset.finish()).await.unwrap();
            host.advance(PERIOD).unwrap();
            let actual = host
                .run(store.shutdown_into_state())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(actual, State::new(mode));
        });
    }
}

#[test]
fn repeated_inputs_keep_fixed_window_and_one_latest_value() {
    block_on(async {
        let mut store = TestStore::new(State::new(Mode::Latest), Throttle);
        let root = store
            .send_with(Action::Sample(1), |s| {
                s.deadline = Some(MonotonicInstant::from_elapsed(Duration::from_millis(10)));
            })
            .await
            .unwrap();
        let _ = store
            .receive_with(|a| *a == Action::Published(1), |s| s.published.push(1))
            .await
            .unwrap();
        store.advance(Duration::from_millis(2)).await.unwrap();
        let _ = store
            .send_with(Action::Sample(2), |s| s.pending = Some(2))
            .await
            .unwrap();
        store.advance(Duration::from_millis(2)).await.unwrap();
        let _ = store
            .send_with(Action::Sample(3), |s| s.pending = Some(3))
            .await
            .unwrap();
        store.advance(Duration::from_millis(6)).await.unwrap();
        let _ = store
            .receive_with(
                |a| *a == Action::Boundary,
                |s| {
                    s.pending = None;
                    s.deadline = Some(MonotonicInstant::from_elapsed(Duration::from_millis(20)));
                },
            )
            .await
            .unwrap();
        let _ = store
            .receive_with(|a| *a == Action::Published(3), |s| s.published.push(3))
            .await
            .unwrap();
        store.advance(PERIOD).await.unwrap();
        let _ = store
            .receive_with(|a| *a == Action::Boundary, |s| s.deadline = None)
            .await
            .unwrap();
        root.finish().await.unwrap();
        store.finish().await.unwrap();
    });
}

#[test]
fn leading_drops_samples_until_window_closes() {
    block_on(async {
        let mut store = TestStore::new(State::new(Mode::Leading), Throttle);
        let root = store
            .send_with(Action::Sample(1), |s| {
                s.deadline = Some(MonotonicInstant::from_elapsed(Duration::from_millis(10)));
            })
            .await
            .unwrap();
        let _ = store
            .receive_with(|a| *a == Action::Published(1), |s| s.published.push(1))
            .await
            .unwrap();
        store.advance(Duration::from_millis(2)).await.unwrap();
        let _ = store.send_with(Action::Sample(2), |_| {}).await.unwrap();
        store.advance(Duration::from_millis(2)).await.unwrap();
        let _ = store.send_with(Action::Sample(3), |_| {}).await.unwrap();
        store.advance(Duration::from_millis(6)).await.unwrap();
        let _ = store
            .receive_with(|a| *a == Action::Boundary, |s| s.deadline = None)
            .await
            .unwrap();
        root.finish().await.unwrap();
        store.finish().await.unwrap();
    });
}

#[test]
fn reset_cancels_pending_value_and_restart_opens_fresh_window() {
    block_on(async {
        let mut store = TestStore::new(State::new(Mode::Latest), Throttle);
        let old = store
            .send_with(Action::Sample(1), |s| {
                s.deadline = Some(MonotonicInstant::from_elapsed(Duration::from_millis(10)));
            })
            .await
            .unwrap();
        let _ = store
            .receive_with(|a| *a == Action::Published(1), |s| s.published.push(1))
            .await
            .unwrap();
        store.advance(Duration::from_millis(4)).await.unwrap();
        let _ = store
            .send_with(Action::Sample(2), |s| s.pending = Some(2))
            .await
            .unwrap();
        let _ = store
            .send_with(Action::Reset, |s| {
                s.pending = None;
                s.deadline = None;
            })
            .await
            .unwrap();
        old.finish().await.unwrap();
        let fresh = store
            .send_with(Action::Sample(3), |s| {
                s.deadline = Some(MonotonicInstant::from_elapsed(Duration::from_millis(14)));
            })
            .await
            .unwrap();
        let _ = store
            .receive_with(|a| *a == Action::Published(3), |s| s.published.push(3))
            .await
            .unwrap();
        store.advance(Duration::from_millis(6)).await.unwrap();
        store
            .assert_state(|s| {
                assert_eq!(
                    s,
                    &State {
                        mode: Mode::Latest,
                        deadline: Some(MonotonicInstant::from_elapsed(Duration::from_millis(14))),
                        pending: None,
                        published: vec![1, 3],
                    }
                );
            })
            .await
            .unwrap();
        assert!(!fresh.is_finished());
        store.advance(Duration::from_millis(4)).await.unwrap();
        let _ = store
            .receive_with(|a| *a == Action::Boundary, |s| s.deadline = None)
            .await
            .unwrap();
        fresh.finish().await.unwrap();
        store.finish().await.unwrap();
    });
}

#[test]
fn reset_after_trailing_publication_cancels_successor_window() {
    block_on(async {
        let mut store = TestStore::new(State::new(Mode::Latest), Throttle);
        let root = store
            .send_with(Action::Sample(1), |s| {
                s.deadline = Some(MonotonicInstant::from_elapsed(Duration::from_millis(10)));
            })
            .await
            .unwrap();
        let _ = store
            .receive_with(|a| *a == Action::Published(1), |s| s.published.push(1))
            .await
            .unwrap();
        let _ = store
            .send_with(Action::Sample(2), |s| s.pending = Some(2))
            .await
            .unwrap();
        store.advance(PERIOD).await.unwrap();
        let _ = store
            .receive_with(
                |a| *a == Action::Boundary,
                |s| {
                    s.pending = None;
                    s.deadline = Some(MonotonicInstant::from_elapsed(Duration::from_millis(20)));
                },
            )
            .await
            .unwrap();
        let _ = store
            .receive_with(|a| *a == Action::Published(2), |s| s.published.push(2))
            .await
            .unwrap();
        let _ = store
            .send_with(Action::Reset, |s| s.deadline = None)
            .await
            .unwrap();
        root.finish().await.unwrap();
        store.advance(PERIOD).await.unwrap();
        store.finish().await.unwrap();
    });
}

/// Pauses the single actor task while the test advances virtual time.
///
/// Tests drive and resume it on one thread; the one waker slot is not a general
/// multi-task or concurrent pause mechanism.
#[derive(Default)]
struct PollGate {
    /// Stops the wrapped actor from being polled while the host still advances.
    paused: AtomicBool,
    /// Actor waker saved by its most recent paused poll.
    waker: Mutex<Option<std::task::Waker>>,
}

impl PollGate {
    /// Allows the next actor poll and wakes it if a paused poll registered interest.
    fn resume(&self) {
        self.paused.store(false, Ordering::SeqCst);
        if let Some(waker) = self.waker.lock().unwrap().take() {
            waker.wake();
        }
    }
}

/// Uses deterministic time but gates the actor to force boundary-order scenarios.
struct PausedDriver {
    /// Supplies timer registration and an explicitly driven task queue.
    host: rca_test::DeterministicRuntime,
    /// Shared with the test that pauses and resumes the single actor.
    gate: Arc<PollGate>,
}

impl std::fmt::Debug for PausedDriver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PausedDriver").finish_non_exhaustive()
    }
}

impl rca_core::RuntimeDriver for PausedDriver {
    fn spawn(&self, mut task: rca_core::RuntimeFuture) {
        let gate = Arc::clone(&self.gate);
        self.host
            .spawn_task(Box::pin(std::future::poll_fn(move |context| {
                if gate.paused.load(Ordering::SeqCst) {
                    *gate.waker.lock().unwrap() = Some(context.waker().clone());
                    std::task::Poll::Pending
                } else {
                    task.as_mut().poll(context)
                }
            })));
    }

    fn now(&self) -> MonotonicInstant {
        self.host.now()
    }

    fn sleep_until(&self, deadline: MonotonicInstant) -> rca_core::RuntimeFuture {
        self.host.sleep_until(deadline)
    }

    fn yield_now(&self) -> rca_core::RuntimeFuture {
        let mut yielded = false;
        Box::pin(std::future::poll_fn(move |context| {
            if yielded {
                std::task::Poll::Ready(())
            } else {
                yielded = true;
                context.waker().wake_by_ref();
                std::task::Poll::Pending
            }
        }))
    }
}

#[test]
fn exact_boundary_ingress_supersedes_old_pending_value() {
    for mode in [Mode::Leading, Mode::Latest] {
        block_on(async {
            let host = rca_test::DeterministicRuntime::new();
            let gate = Arc::new(PollGate::default());
            let runtime = rca_core::Runtime::new(PausedDriver {
                host: host.clone(),
                gate: Arc::clone(&gate),
            });
            let store = rca_core::Store::new(State::new(mode), Throttle, runtime);
            let old = host
                .run(store.send(Action::Sample(1)))
                .await
                .unwrap()
                .unwrap();
            host.run_until_stalled().unwrap();
            let _ = host
                .run(store.send(Action::Sample(2)))
                .await
                .unwrap()
                .unwrap();
            host.run_until_stalled().unwrap();
            gate.paused.store(true, Ordering::SeqCst);
            host.advance(PERIOD).unwrap();
            let receipt = store.try_send(Action::Sample(3)).unwrap();
            gate.resume();
            let fresh = host.run(receipt.finish()).await.unwrap().unwrap();
            host.run_until_stalled().unwrap();
            let actual = host.run(store.read(Clone::clone)).await.unwrap().unwrap();
            assert_eq!(
                actual,
                State {
                    mode,
                    deadline: Some(MonotonicInstant::from_elapsed(Duration::from_millis(20))),
                    pending: None,
                    published: vec![1, 3],
                }
            );
            host.run(old.finish()).await.unwrap();
            assert!(!fresh.is_finished());
            host.advance(PERIOD).unwrap();
            host.run(fresh.finish()).await.unwrap();
            let actual = host
                .run(store.shutdown_into_state())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                actual,
                State {
                    mode,
                    deadline: None,
                    pending: None,
                    published: vec![1, 3]
                }
            );
        });
    }
}

#[test]
fn late_boundary_emits_once_without_catch_up_windows() {
    block_on(async {
        let host = rca_test::DeterministicRuntime::new();
        let gate = Arc::new(PollGate::default());
        let runtime = rca_core::Runtime::new(PausedDriver {
            host: host.clone(),
            gate: Arc::clone(&gate),
        });
        let store = rca_core::Store::new(State::new(Mode::Latest), Throttle, runtime);
        let root = host
            .run(store.send(Action::Sample(1)))
            .await
            .unwrap()
            .unwrap();
        host.run_until_stalled().unwrap();
        let _ = host
            .run(store.send(Action::Sample(2)))
            .await
            .unwrap()
            .unwrap();
        host.run_until_stalled().unwrap();
        gate.paused.store(true, Ordering::SeqCst);
        host.advance(Duration::from_millis(35)).unwrap();
        gate.resume();
        host.run_until_stalled().unwrap();
        let actual = host.run(store.read(Clone::clone)).await.unwrap().unwrap();
        assert_eq!(
            actual,
            State {
                mode: Mode::Latest,
                deadline: Some(MonotonicInstant::from_elapsed(Duration::from_millis(45))),
                pending: None,
                published: vec![1, 2],
            }
        );
        assert!(!root.is_finished());
        host.advance(PERIOD).unwrap();
        host.run(root.finish()).await.unwrap();
        let actual = host
            .run(store.shutdown_into_state())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            actual,
            State {
                mode: Mode::Latest,
                deadline: None,
                pending: None,
                published: vec![1, 2]
            }
        );
    });
}

/// Routes samples or changes one throttled child's lifetime.
#[derive(Debug, Eq, PartialEq)]
enum RowsAction {
    Row(rca_core::KeyedAction<u8, Action>),
    Remove(u8),
    Insert(u8),
}

/// Removes and recreates keyed children without starting parent-owned effects.
struct Rows;

impl Reducer for Rows {
    type State = rca_core::KeyedState<u8, State>;
    type Action = RowsAction;

    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &RowsAction,
        _: &mut ReduceContext<'_>,
    ) -> Effect<RowsAction> {
        match action {
            RowsAction::Remove(id) => {
                state.remove(id);
            }
            RowsAction::Insert(id) => {
                state.insert(*id, State::new(Mode::Latest));
            }
            RowsAction::Row(_) => {}
        }
        Effect::none()
    }
}

/// Addresses the throttle state of one keyed child.
fn row(id: u8, action: Action) -> RowsAction {
    RowsAction::Row(rca_core::KeyedAction::new(id, action))
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "keep literal keyed lifetime assertions in chronological order"
)]
fn keyed_reinsertion_drops_old_window_without_stopping_sibling() {
    block_on(async {
        let reducer = rca_core::for_each(
            Rows,
            rca_core::KeyedStatePath::new(|s| s, |s| s),
            rca_core::KeyedActionPath::new(
                |a| match a {
                    RowsAction::Row(a) => Some(a),
                    RowsAction::Remove(_) | RowsAction::Insert(_) => None,
                },
                RowsAction::Row,
            ),
            Throttle,
        );
        let initial = [(1, State::new(Mode::Latest)), (2, State::new(Mode::Latest))]
            .into_iter()
            .collect();
        let mut store = TestStore::new(initial, reducer);
        let old = store
            .send_with(row(1, Action::Sample(1)), |s| {
                s.get_mut(&1).unwrap().deadline =
                    Some(MonotonicInstant::from_elapsed(Duration::from_millis(10)));
            })
            .await
            .unwrap();
        let _ = store
            .receive_with(
                |a| *a == row(1, Action::Published(1)),
                |s| s.get_mut(&1).unwrap().published.push(1),
            )
            .await
            .unwrap();
        let sibling = store
            .send_with(row(2, Action::Sample(2)), |s| {
                s.get_mut(&2).unwrap().deadline =
                    Some(MonotonicInstant::from_elapsed(Duration::from_millis(10)));
            })
            .await
            .unwrap();
        let _ = store
            .receive_with(
                |a| *a == row(2, Action::Published(2)),
                |s| s.get_mut(&2).unwrap().published.push(2),
            )
            .await
            .unwrap();
        let _ = store
            .send_with(row(1, Action::Sample(11)), |s| {
                s.get_mut(&1).unwrap().pending = Some(11);
            })
            .await
            .unwrap();
        let _ = store
            .send_with(row(2, Action::Sample(22)), |s| {
                s.get_mut(&2).unwrap().pending = Some(22);
            })
            .await
            .unwrap();
        store.advance(Duration::from_millis(4)).await.unwrap();
        let _ = store
            .send_with(RowsAction::Remove(1), |s| {
                s.remove(&1);
            })
            .await
            .unwrap();
        old.finish().await.unwrap();
        let _ = store
            .send_with(RowsAction::Insert(1), |s| {
                s.insert(1, State::new(Mode::Latest));
            })
            .await
            .unwrap();
        let fresh = store
            .send_with(row(1, Action::Sample(3)), |s| {
                s.get_mut(&1).unwrap().deadline =
                    Some(MonotonicInstant::from_elapsed(Duration::from_millis(14)));
            })
            .await
            .unwrap();
        let _ = store
            .receive_with(
                |a| *a == row(1, Action::Published(3)),
                |s| s.get_mut(&1).unwrap().published.push(3),
            )
            .await
            .unwrap();
        store.advance(Duration::from_millis(6)).await.unwrap();
        let _ = store
            .receive_with(
                |a| *a == row(2, Action::Boundary),
                |s| {
                    let child = s.get_mut(&2).unwrap();
                    child.pending = None;
                    child.deadline =
                        Some(MonotonicInstant::from_elapsed(Duration::from_millis(20)));
                },
            )
            .await
            .unwrap();
        let _ = store
            .receive_with(
                |a| *a == row(2, Action::Published(22)),
                |s| s.get_mut(&2).unwrap().published.push(22),
            )
            .await
            .unwrap();
        assert!(!fresh.is_finished());
        store.advance(Duration::from_millis(4)).await.unwrap();
        let _ = store
            .receive_with(
                |a| *a == row(1, Action::Boundary),
                |s| s.get_mut(&1).unwrap().deadline = None,
            )
            .await
            .unwrap();
        fresh.finish().await.unwrap();
        store.advance(Duration::from_millis(6)).await.unwrap();
        let _ = store
            .receive_with(
                |a| *a == row(2, Action::Boundary),
                |s| s.get_mut(&2).unwrap().deadline = None,
            )
            .await
            .unwrap();
        sibling.finish().await.unwrap();
        store.finish().await.unwrap();
    });
}
