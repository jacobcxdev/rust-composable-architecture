//! Demand-driven timers release waits and cannot outlive their child scope.

use std::pin::pin;
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use rca_core::{Clock, ClockTimer, InvalidTimerInterval, IssueReporter, MonotonicInstant};
use rca_test::DeterministicRuntime;

/// A short virtual interval keeps expected timestamps readable.
const INTERVAL: Duration = Duration::from_millis(10);

/// Expresses literal expected timestamps in milliseconds from the origin.
fn instant(millis: u64) -> MonotonicInstant {
    MonotonicInstant::from_elapsed(Duration::from_millis(millis))
}

/// Polls once, then drops the borrowed `next` future without dropping the timer.
fn poll_next(timer: &mut ClockTimer) -> Poll<Option<MonotonicInstant>> {
    pin!(timer.next()).poll(&mut Context::from_waker(Waker::noop()))
}

// Catches anchoring at construction or at an unpolled next invocation.
#[test]
fn first_poll_anchors_first_deadline() {
    let host = DeterministicRuntime::new();
    let mut timer = ClockTimer::new(host.runtime().clock(), INTERVAL).unwrap();
    let next = timer.next();
    host.advance(Duration::from_millis(30)).unwrap();
    assert!(host.pending_sleeps().is_empty());
    drop(next);
    assert_eq!(poll_next(&mut timer), Poll::Pending);
    assert_eq!(host.pending_sleeps()[0].deadline(), instant(40));
    host.advance(Duration::from_millis(9)).unwrap();
    assert_eq!(poll_next(&mut timer), Poll::Pending);
    host.advance(Duration::from_millis(1)).unwrap();
    assert_eq!(poll_next(&mut timer), Poll::Ready(Some(instant(40))));
    assert!(host.pending_sleeps().is_empty());
}

// Catches scheduled-deadline output and immediate catch-up storms after lag.
#[test]
fn lagged_poll_coalesces_to_observed_now_then_waits_a_full_interval() {
    let host = DeterministicRuntime::new();
    let mut timer = ClockTimer::new(host.runtime().clock(), INTERVAL).unwrap();
    assert_eq!(poll_next(&mut timer), Poll::Pending);
    host.advance(Duration::from_millis(105)).unwrap();
    assert_eq!(poll_next(&mut timer), Poll::Ready(Some(instant(105))));
    assert_eq!(poll_next(&mut timer), Poll::Pending);
    assert_eq!(host.pending_sleeps()[0].deadline(), instant(115));
    assert_eq!(host.pending_sleeps().len(), 1);
    host.advance(INTERVAL).unwrap();
    assert_eq!(poll_next(&mut timer), Poll::Ready(Some(instant(115))));
}

// Catches a producer queue/spawn while no next call is being polled.
#[test]
fn zero_demand_keeps_no_tick_queue_or_prearmed_sleep() {
    let host = DeterministicRuntime::new();
    let mut timer = ClockTimer::new(host.runtime().clock(), INTERVAL).unwrap();
    assert_eq!(poll_next(&mut timer), Poll::Pending);
    host.advance(INTERVAL).unwrap();
    assert_eq!(poll_next(&mut timer), Poll::Ready(Some(instant(10))));
    assert!(host.pending_sleeps().is_empty());
    let report = host.advance(Duration::from_secs(1)).unwrap();
    assert_eq!(report.polls(), 0);
    assert!(host.pending_sleeps().is_empty());
    assert_eq!(poll_next(&mut timer), Poll::Ready(Some(instant(1010))));
    assert_eq!(poll_next(&mut timer), Poll::Pending);
    assert_eq!(host.pending_sleeps()[0].deadline(), instant(1020));
}

// Catches timer state shared across consumers using the same clock.
#[test]
fn independent_consumers_anchor_and_cancel_independently() {
    let host = DeterministicRuntime::new();
    let clock = host.runtime().clock();
    let mut first = ClockTimer::new(clock.clone(), INTERVAL).unwrap();
    let mut second = ClockTimer::new(clock, INTERVAL).unwrap();
    assert_eq!(poll_next(&mut first), Poll::Pending);
    host.advance(Duration::from_millis(3)).unwrap();
    assert_eq!(poll_next(&mut second), Poll::Pending);
    assert_eq!(host.pending_sleeps().len(), 2);
    host.advance(Duration::from_millis(7)).unwrap();
    assert_eq!(poll_next(&mut first), Poll::Ready(Some(instant(10))));
    assert_eq!(poll_next(&mut second), Poll::Pending);
    first.cancel();
    host.advance(Duration::from_millis(3)).unwrap();
    assert_eq!(poll_next(&mut second), Poll::Ready(Some(instant(13))));
    assert_eq!(poll_next(&mut first), Poll::Ready(None));
}

// Catches zero periods before any clock access (including forbidden fixtures).
#[test]
fn zero_interval_rejects_without_using_the_clock() {
    let fixture = rca_core::ForbiddenClock::new(
        "zero interval",
        MonotonicInstant::ZERO,
        IssueReporter::default(),
    );
    let clock = Clock::new(fixture.clone());
    assert_eq!(
        ClockTimer::new(clock, Duration::ZERO).unwrap_err(),
        InvalidTimerInterval
    );
    fixture.assert_unused().unwrap();
}

// Catches cancellation of the stream when merely abandoning one next future.
#[test]
fn dropped_next_retains_registered_wait_and_original_deadline() {
    let host = DeterministicRuntime::new();
    let mut timer = ClockTimer::new(host.runtime().clock(), INTERVAL).unwrap();
    assert_eq!(poll_next(&mut timer), Poll::Pending);
    let original = host.pending_sleeps();
    host.advance(Duration::from_millis(5)).unwrap();
    assert_eq!(poll_next(&mut timer), Poll::Pending);
    assert_eq!(host.pending_sleeps(), original);
    host.advance(Duration::from_millis(5)).unwrap();
    assert_eq!(poll_next(&mut timer), Poll::Ready(Some(instant(10))));
}

// Catches retained sleep resources and restart after explicit cancellation.
#[test]
fn explicit_cancel_retires_sleep_and_stays_terminal() {
    let host = DeterministicRuntime::new();
    let mut timer = ClockTimer::new(host.runtime().clock(), INTERVAL).unwrap();
    assert_eq!(poll_next(&mut timer), Poll::Pending);
    assert_eq!(host.pending_sleeps().len(), 1);
    timer.cancel();
    timer.cancel();
    assert!(host.pending_sleeps().is_empty());
    host.advance(Duration::from_secs(1)).unwrap();
    assert_eq!(poll_next(&mut timer), Poll::Ready(None));
    assert_eq!(poll_next(&mut timer), Poll::Ready(None));
    assert!(host.pending_sleeps().is_empty());
}

#[test]
fn dropping_timer_retires_registered_sleep() {
    let host = DeterministicRuntime::new();
    let mut timer = ClockTimer::new(host.runtime().clock(), INTERVAL).unwrap();
    assert_eq!(poll_next(&mut timer), Poll::Pending);
    assert_eq!(host.pending_sleeps().len(), 1);
    drop(timer);
    assert!(host.pending_sleeps().is_empty());
}

// Catches overflow or a permanently-ready loop at the representable time limit.
#[test]
fn saturation_delivers_one_final_tick_then_terminates() {
    let host = DeterministicRuntime::with_now(instant(1));
    let mut timer = ClockTimer::new(host.runtime().clock(), Duration::MAX).unwrap();
    assert_eq!(poll_next(&mut timer), Poll::Pending);
    assert_eq!(host.pending_sleeps()[0].deadline(), MonotonicInstant::MAX);
    host.advance(Duration::MAX).unwrap();
    assert_eq!(
        poll_next(&mut timer),
        Poll::Ready(Some(MonotonicInstant::MAX))
    );
    assert_eq!(poll_next(&mut timer), Poll::Ready(None));
    assert!(host.pending_sleeps().is_empty());
}

#[test]
fn first_poll_at_maximum_terminates_without_a_wait() {
    let host = DeterministicRuntime::with_now(MonotonicInstant::MAX);
    let mut timer = ClockTimer::new(host.runtime().clock(), INTERVAL).unwrap();
    assert_eq!(poll_next(&mut timer), Poll::Ready(None));
    assert_eq!(poll_next(&mut timer), Poll::Ready(None));
    assert!(host.pending_sleeps().is_empty());
}

#[test]
fn cancelling_before_first_poll_never_uses_clock() {
    let fixture = rca_core::ForbiddenClock::new(
        "cancelled timer",
        MonotonicInstant::ZERO,
        IssueReporter::default(),
    );
    let clock = Clock::new(fixture.clone());
    let mut timer = ClockTimer::new(clock, INTERVAL).unwrap();
    timer.cancel();
    assert_eq!(poll_next(&mut timer), Poll::Ready(None));
    fixture.assert_unused().unwrap();
}

mod forwarding {
    //! Retained output senders must lose access when a child lifetime ends.

    use super::{ClockTimer, DeterministicRuntime, INTERVAL, MonotonicInstant, instant};
    use futures_executor::block_on;
    use rca_core::{
        ActionPath, CaseStatePath, Clock, Effect, EffectSender, KeyedAction, KeyedActionPath,
        KeyedState, KeyedStatePath, OptionalStatePath, ReduceContext, Reducer, ReducerExt,
        ScopedStore, Store,
    };
    use std::sync::mpsc;

    /// Starts a timer consumer or records one observed tick.
    enum ChildAction {
        Start,
        Tick(MonotonicInstant),
    }

    /// Forwards timer ticks while exposing a sender for stale-access checks.
    struct Client {
        /// The deterministic host clock shared with the actor.
        clock: Clock,
        /// Unbounded test output; sends never block while the actor is driven.
        outputs: mpsc::Sender<(EffectSender<ChildAction>, MonotonicInstant)>,
    }

    impl Reducer for Client {
        type State = Vec<MonotonicInstant>;
        type Action = ChildAction;

        fn reduce(
            &mut self,
            state: &mut Self::State,
            action: &ChildAction,
            _: &mut ReduceContext<'_>,
        ) -> Effect<ChildAction> {
            match action {
                ChildAction::Start => {
                    let mut timer = ClockTimer::new(self.clock.clone(), INTERVAL).unwrap();
                    let outputs = self.outputs.clone();
                    Effect::run(move |sender| async move {
                        while let Some(now) = timer.next().await {
                            outputs.send((sender.clone(), now)).unwrap();
                            if sender.send(ChildAction::Tick(now)).await.is_err() {
                                break;
                            }
                        }
                    })
                }
                ChildAction::Tick(now) => {
                    state.push(*now);
                    Effect::none()
                }
            }
        }
    }

    /// Selects one child-lifetime mechanism for the same regression scenario.
    #[derive(Clone, Copy)]
    enum Family {
        Optional,
        Case,
        Keyed,
    }

    /// The case-scoped child is present only in `On`.
    enum Screen {
        Off,
        On(Vec<MonotonicInstant>),
    }

    /// Holds one instance of each child-lifetime mechanism.
    struct State {
        /// Optional child's received timestamps.
        optional: Option<Vec<MonotonicInstant>>,
        /// Case-scoped child's received timestamps.
        screen: Screen,
        /// Keyed child's timestamps, always addressed by key `1` in this fixture.
        rows: KeyedState<u8, Vec<MonotonicInstant>>,
    }

    /// Routes child work or removes and recreates the selected child.
    enum Action {
        Optional(ChildAction),
        Case(ChildAction),
        Row(KeyedAction<u8, ChildAction>),
        Remove(Family),
        Insert(Family),
    }

    /// Changes child lifetimes without starting any work of its own.
    struct Parent;

    impl Reducer for Parent {
        type State = State;
        type Action = Action;

        fn reduce(
            &mut self,
            state: &mut State,
            action: &Action,
            _: &mut ReduceContext<'_>,
        ) -> Effect<Action> {
            match action {
                Action::Remove(Family::Optional) => state.optional = None,
                Action::Remove(Family::Case) => state.screen = Screen::Off,
                Action::Remove(Family::Keyed) => {
                    state.rows.remove(&1);
                }
                Action::Insert(Family::Optional) => state.optional = Some(Vec::new()),
                Action::Insert(Family::Case) => state.screen = Screen::On(Vec::new()),
                Action::Insert(Family::Keyed) => state.rows.insert(1, Vec::new()),
                _ => {}
            }
            Effect::none()
        }
    }

    /// Reuses the optional projection in both composition and store scoping.
    macro_rules! optional_state {
        () => {
            OptionalStatePath::new(
                |s: &State| s.optional.as_ref(),
                |s: &mut State| s.optional.as_mut(),
            )
        };
    }

    /// Reuses the optional action projection without naming closure types.
    macro_rules! optional_action {
        () => {
            ActionPath::new(
                |a: &Action| {
                    if let Action::Optional(a) = a {
                        Some(a)
                    } else {
                        None
                    }
                },
                Action::Optional,
            )
        };
    }

    /// Reuses the active-case projection in composition and store scoping.
    macro_rules! case_state {
        () => {
            CaseStatePath::new(
                |s: &State| {
                    if let Screen::On(s) = &s.screen {
                        Some(s)
                    } else {
                        None
                    }
                },
                |s: &mut State| {
                    if let Screen::On(s) = &mut s.screen {
                        Some(s)
                    } else {
                        None
                    }
                },
            )
        };
    }

    /// Reuses the case action projection without naming closure types.
    macro_rules! case_action {
        () => {
            ActionPath::new(
                |a: &Action| {
                    if let Action::Case(a) = a {
                        Some(a)
                    } else {
                        None
                    }
                },
                Action::Case,
            )
        };
    }

    /// Reuses the keyed collection projection in composition and store scoping.
    macro_rules! keyed_state {
        () => {
            KeyedStatePath::new(|s: &State| &s.rows, |s: &mut State| &mut s.rows)
        };
    }

    /// Reuses the keyed action projection without naming closure types.
    macro_rules! keyed_action {
        () => {
            KeyedActionPath::new(
                |a: &Action| {
                    if let Action::Row(a) = a {
                        Some(a)
                    } else {
                        None
                    }
                },
                Action::Row,
            )
        };
    }

    /// Resolves the selected child, which the scenario has already inserted.
    async fn scope(
        store: &Store<State, Action>,
        family: Family,
    ) -> ScopedStore<Vec<MonotonicInstant>, ChildAction> {
        match family {
            Family::Optional => store
                .scope_some(optional_state!(), optional_action!())
                .await
                .unwrap(),
            Family::Case => store
                .scope_case(case_state!(), case_action!())
                .await
                .unwrap(),
            Family::Keyed => store
                .scope_keyed(keyed_state!(), keyed_action!(), 1)
                .await
                .unwrap(),
        }
    }

    /// Checks timer teardown, stale access and a fresh child's independent ticks.
    fn check_child(family: Family) {
        let host = DeterministicRuntime::new();
        let (outputs, received) = mpsc::channel();
        let client = || Client {
            clock: host.runtime().clock(),
            outputs: outputs.clone(),
        };
        let reducer = Parent
            .if_some(optional_state!(), optional_action!(), client())
            .if_case(case_state!(), case_action!(), client())
            .for_each(keyed_state!(), keyed_action!(), client());
        let store = Store::new(
            State {
                optional: Some(Vec::new()),
                screen: Screen::On(Vec::new()),
                rows: [(1, Vec::new())].into_iter().collect(),
            },
            reducer,
            host.runtime(),
        );
        let driver = host.clone();
        block_on(driver.run(async move {
            let old = scope(&store, family).await;
            let task = old.send(ChildAction::Start).await.unwrap();
            host.run_until_stalled().unwrap();
            assert_eq!(host.pending_sleeps().len(), 1);
            host.advance(INTERVAL).unwrap();
            let (stale, timestamp) = received.try_recv().unwrap();
            assert_eq!(timestamp, instant(10));
            assert_eq!(old.read(Clone::clone).await.unwrap(), vec![instant(10)]);

            store
                .send(Action::Remove(family))
                .await
                .unwrap()
                .finish()
                .await;
            host.run_until_stalled().unwrap();
            task.finish().await;
            assert!(host.pending_sleeps().is_empty());
            store
                .send(Action::Insert(family))
                .await
                .unwrap()
                .finish()
                .await;
            let fresh = scope(&store, family).await;
            // Forward an actual old timer result with retained old output authority.
            assert!(
                stale
                    .send(ChildAction::Tick(timestamp))
                    .await
                    .unwrap_err()
                    .is_cancelled()
            );
            assert!(
                old.send(ChildAction::Tick(timestamp))
                    .await
                    .unwrap_err()
                    .is_scope_ended()
            );
            assert!(fresh.read(Clone::clone).await.unwrap().is_empty());

            let fresh_task = fresh.send(ChildAction::Start).await.unwrap();
            host.run_until_stalled().unwrap();
            host.advance(INTERVAL).unwrap();
            assert_eq!(fresh.read(Clone::clone).await.unwrap(), vec![instant(20)]);
            store.shutdown().await.unwrap();
            // Shutdown issues aborts; the host must reap cooperative task wrappers.
            host.run_until_stalled().unwrap();
            fresh_task.finish().await;
            assert!(host.pending_sleeps().is_empty());
        }))
        .unwrap();
    }

    // Each catches stale timer output reaching the replacement incarnation.
    #[test]
    fn optional_timer_output_cannot_reach_reinserted_child() {
        check_child(Family::Optional);
    }

    #[test]
    fn case_timer_output_cannot_reach_reinserted_case() {
        check_child(Family::Case);
    }

    #[test]
    fn keyed_timer_output_cannot_reach_reinserted_id() {
        check_child(Family::Keyed);
    }

    #[test]
    fn root_consumer_delivers_ticks_and_cancellation_retires_sleep() {
        let host = DeterministicRuntime::new();
        let (outputs, received) = mpsc::channel();
        let store = Store::new(
            Vec::new(),
            Client {
                clock: host.runtime().clock(),
                outputs,
            },
            host.runtime(),
        );
        let driver = host.clone();
        block_on(driver.run(async move {
            let task = store.send(ChildAction::Start).await.unwrap();
            host.run_until_stalled().unwrap();
            host.advance(INTERVAL).unwrap();
            let (stale, timestamp) = received.try_recv().unwrap();
            assert_eq!(store.read(Clone::clone).await.unwrap(), vec![instant(10)]);
            task.cancel();
            task.finish().await;
            assert!(host.pending_sleeps().is_empty());
            assert!(
                stale
                    .send(ChildAction::Tick(timestamp))
                    .await
                    .unwrap_err()
                    .is_cancelled()
            );
            host.advance(INTERVAL).unwrap();
            assert_eq!(store.read(Clone::clone).await.unwrap(), vec![instant(10)]);
            store.shutdown().await.unwrap();
        }))
        .unwrap();
    }
}
