//! Separates missing-child reports from stale effect authority after child removal.

use futures_executor::block_on;
use rca_core::{
    ActionPath, CaseStatePath, Effect, KeyedAction, KeyedActionPath, KeyedState, KeyedStatePath,
    OptionalStatePath, ReduceContext, Reducer, ReducerExt, SharedReducer, for_each,
    for_each_shared, if_case, if_case_identified, if_some_identified,
};
use rca_core::{EffectSender, Store};
use rca_test::DeterministicRuntime;
use rca_test::TestStore;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, mpsc};
use std::task::{Context, Waker};

/// Tracks optional-child state and exactly how often the parent reducer runs.
#[derive(Clone, Debug, PartialEq)]
struct State {
    child: Option<u8>,
    base_calls: u8,
}

#[derive(Debug, PartialEq)]
enum Action {
    Child,
    Other,
    Create,
}

/// Creates a child only after the composition wrapper checks the routed action.
struct Base;
impl Reducer for Base {
    type State = State;
    type Action = Action;
    fn reduce(
        &mut self,
        state: &mut State,
        action: &Action,
        _: &mut ReduceContext<'_>,
    ) -> Effect<Action> {
        state.base_calls += 1;
        if *action == Action::Create {
            state.child = Some(0);
        }
        Effect::none()
    }
}

/// Makes successful child delivery visible as one increment.
struct Child;
impl Reducer for Child {
    type State = u8;
    type Action = ();
    fn reduce(&mut self, state: &mut u8, (): &(), _: &mut ReduceContext<'_>) -> Effect<()> {
        *state += 1;
        Effect::none()
    }
}

/// Routes creation through the child path to detect accidental retroactive delivery.
fn reducer() -> impl Reducer<State = State, Action = Action> {
    Base.if_some(
        OptionalStatePath::new(
            |s: &State| s.child.as_ref(),
            |s: &mut State| s.child.as_mut(),
        ),
        ActionPath::new(
            |a: &Action| match a {
                Action::Child | Action::Create => Some(&()),
                Action::Other => None,
            },
            |()| Action::Child,
        ),
        Child,
    )
}

// Catch the old fatal branch and accidental omission of required evidence.
#[test]
fn missing_optional_child_reports_once_before_base_and_is_required_evidence() {
    let mut store = TestStore::new(
        State {
            child: None,
            base_calls: 0,
        },
        reducer(),
    );
    block_on(async {
        let _ = store
            .send_with(Action::Child, |s| s.base_calls = 1)
            .await
            .unwrap();
        let error = store.finish().await.unwrap_err();
        assert_eq!(error.unconsumed_issues().len(), 1);
        let issue = store
            .expect_issue(|i| i.code == "rca.missing_child")
            .await
            .unwrap();
        assert!(issue.message.contains("IfSome"));
        assert!(
            store
                .expect_issue(|_| true)
                .await
                .unwrap_err()
                .is_no_issue()
        );
        store.finish().await.unwrap();
    });
}

#[test]
fn unmatched_is_silent_and_base_created_child_is_not_delivered_retroactively() {
    let mut store = TestStore::new(
        State {
            child: None,
            base_calls: 0,
        },
        reducer(),
    );
    block_on(async {
        let _ = store
            .send_with(Action::Other, |s| s.base_calls = 1)
            .await
            .unwrap();
        assert!(
            store
                .expect_issue(|_| true)
                .await
                .unwrap_err()
                .is_no_issue()
        );
        let _ = store
            .send_with(Action::Create, |s| {
                s.base_calls = 2;
                s.child = Some(0);
            })
            .await
            .unwrap();
        store
            .expect_issue(|i| i.code == "rca.missing_child")
            .await
            .unwrap();
        let _ = store
            .send_with(Action::Child, |s| {
                s.base_calls = 3;
                s.child = Some(1);
            })
            .await
            .unwrap();
        assert!(
            store
                .expect_issue(|_| true)
                .await
                .unwrap_err()
                .is_no_issue()
        );
        store.finish().await.unwrap();
    });
}

/// Requires one named missing-child issue and one parent call for a wrapper.
fn assert_missing<R: Reducer<State = State, Action = Action> + Send + 'static>(
    reducer: R,
    name: &str,
) {
    let mut store = TestStore::new(
        State {
            child: None,
            base_calls: 0,
        },
        reducer,
    );
    block_on(async {
        let _ = store
            .send_with(Action::Child, |s| s.base_calls = 1)
            .await
            .unwrap();
        let issue = store
            .expect_issue(|i| i.code == "rca.missing_child")
            .await
            .unwrap();
        assert!(issue.message.starts_with(name));
        assert!(
            store
                .expect_issue(|_| true)
                .await
                .unwrap_err()
                .is_no_issue()
        );
        store.finish().await.unwrap();
    });
}

#[test]
fn case_and_identified_wrappers_share_required_default() {
    assert_missing(
        if_some_identified(
            Base,
            OptionalStatePath::new(
                |s: &State| s.child.as_ref(),
                |s: &mut State| s.child.as_mut(),
            ),
            ActionPath::new(|_: &Action| Some(&()), |()| Action::Child),
            |s: &u8| s,
            Child,
        ),
        "IfSomeIdentified",
    );
    assert_missing(
        if_case(
            Base,
            CaseStatePath::new(
                |s: &State| s.child.as_ref(),
                |s: &mut State| s.child.as_mut(),
            ),
            ActionPath::new(|_: &Action| Some(&()), |()| Action::Child),
            Child,
        ),
        "IfCase",
    );
    assert_missing(
        if_case_identified(
            Base,
            CaseStatePath::new(
                |s: &State| s.child.as_ref(),
                |s: &mut State| s.child.as_mut(),
            ),
            ActionPath::new(|_: &Action| Some(&()), |()| Action::Child),
            |s: &u8| s,
            Child,
        ),
        "IfCaseIdentified",
    );
}

/// Records ordered keyed values separately from child/parent execution order.
struct Rows {
    rows: KeyedState<u8, u8>,
    trace: Vec<&'static str>,
}

/// Creates a missing row after child lookup and records its parent-side visit.
struct RowsBase;
impl Reducer for RowsBase {
    type State = Rows;
    type Action = KeyedAction<u8, ()>;
    fn reduce(
        &mut self,
        state: &mut Rows,
        action: &Self::Action,
        _: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        state.trace.push("base");
        if !state.rows.contains_id(action.id()) {
            state.rows.insert(*action.id(), 0);
        }
        Effect::none()
    }
}

/// Records child delivery in both row state and explicitly shared state.
struct SharedChild;
impl SharedReducer for SharedChild {
    type State = u8;
    type Shared = Vec<&'static str>;
    type Action = ();
    fn reduce(
        &mut self,
        state: &mut u8,
        shared: &mut Self::Shared,
        (): &(),
        _: &mut ReduceContext<'_>,
    ) -> Effect<()> {
        *state += 1;
        shared.push("child");
        Effect::none()
    }
}

/// Copies keyed values and the trace without retaining mutable state aliases.
fn rows_snapshot(s: &Rows) -> (Vec<(u8, u8)>, Vec<&'static str>) {
    (
        s.rows.iter().map(|(id, value)| (*id, *value)).collect(),
        s.trace.clone(),
    )
}

/// Checks missing-row creation, later delivery, ordering and untouched siblings.
fn check_keyed<R: Reducer<State = Rows, Action = KeyedAction<u8, ()>> + Send + 'static>(
    reducer: R,
    name: &str,
    shared: bool,
) {
    let mut store = TestStore::projected(
        Rows {
            rows: [(1, 10), (2, 20)].into_iter().collect(),
            trace: vec![],
        },
        reducer,
        rows_snapshot,
    );
    block_on(async {
        let _ = store
            .send_with(KeyedAction::new(3, ()), |s| {
                s.0.push((3, 0));
                s.1.push("base");
            })
            .await
            .unwrap();
        let issue = store
            .expect_issue(|i| i.code == "rca.missing_child")
            .await
            .unwrap();
        assert!(issue.message.starts_with(name));
        let _ = store
            .send_with(KeyedAction::new(3, ()), move |s| {
                s.0[2].1 = 1;
                if shared {
                    s.1.push("child");
                }
                s.1.push("base");
            })
            .await
            .unwrap();
        assert!(
            store
                .expect_issue(|_| true)
                .await
                .unwrap_err()
                .is_no_issue()
        );
        store.finish().await.unwrap();
    });
}

#[test]
fn keyed_missing_skips_child_and_mounts_base_created_entry_without_touching_siblings() {
    check_keyed(
        for_each(
            RowsBase,
            KeyedStatePath::new(|s: &Rows| &s.rows, |s: &mut Rows| &mut s.rows),
            KeyedActionPath::new(|a: &KeyedAction<u8, ()>| Some(a), |a| a),
            Child,
        ),
        "ForEach",
        false,
    );
}

#[test]
fn shared_keyed_missing_skips_split_then_valid_child_precedes_base() {
    let splits = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&splits);
    check_keyed(
        for_each_shared(
            RowsBase,
            |s: &Rows| &s.rows,
            move |s: &mut Rows| {
                observed.fetch_add(1, Ordering::SeqCst);
                (&mut s.rows, &mut s.trace)
            },
            KeyedActionPath::new(|a: &KeyedAction<u8, ()>| Some(a), |a| a),
            SharedChild,
        ),
        "ForEachShared",
        true,
    );
    assert_eq!(splits.load(Ordering::SeqCst), 1);
}

/// Adds a second optional level to distinguish reached and skipped parent reducers.
#[derive(Clone, Debug, PartialEq)]
struct Root {
    middle: Option<State>,
    calls: u8,
}

/// Counts outer parent calls without changing either child lifetime.
struct RootBase;
impl Reducer for RootBase {
    type State = Root;
    type Action = Action;
    fn reduce(
        &mut self,
        state: &mut Root,
        _: &Action,
        _: &mut ReduceContext<'_>,
    ) -> Effect<Action> {
        state.calls += 1;
        Effect::none()
    }
}

#[test]
fn nested_missing_is_reported_at_the_detecting_layer_with_each_reached_base_once() {
    for middle in [
        None,
        Some(State {
            child: None,
            base_calls: 0,
        }),
    ] {
        let reducer = RootBase.if_some(
            OptionalStatePath::new(
                |s: &Root| s.middle.as_ref(),
                |s: &mut Root| s.middle.as_mut(),
            ),
            ActionPath::new(|a: &Action| Some(a), |a| a),
            reducer(),
        );
        let mut store = TestStore::new(Root { middle, calls: 0 }, reducer);
        block_on(async {
            let _ = store
                .send_with(Action::Child, |s| {
                    s.calls = 1;
                    if let Some(middle) = &mut s.middle {
                        middle.base_calls = 1;
                    }
                })
                .await
                .unwrap();
            store
                .expect_issue(|i| i.code == "rca.missing_child")
                .await
                .unwrap();
            assert!(
                store
                    .expect_issue(|_| true)
                    .await
                    .unwrap_err()
                    .is_no_issue()
            );
            store.finish().await.unwrap();
        });
    }
}

/// Selects the actual optional, case or keyed composition under test.
#[derive(Clone, Copy)]
enum Family {
    Optional,
    Case,
    Keyed,
}

enum Screen {
    Off,
    On(u8),
}

/// Keeps the three child forms and a sibling alongside a parent-commit counter.
struct LifetimeState {
    optional: Option<u8>,
    screen: Screen,
    rows: KeyedState<u8, u8>,
    base: usize,
}

enum ChildAction {
    Capture,
    Increment,
}

enum LifetimeAction {
    Optional(ChildAction),
    Case(ChildAction),
    Row(KeyedAction<u8, ChildAction>),
    Remove(Family),
    Reinsert,
}

/// Removes or replaces a child while recording every action that reaches the parent.
struct LifetimeBase;
impl Reducer for LifetimeBase {
    type State = LifetimeState;
    type Action = LifetimeAction;
    fn reduce(
        &mut self,
        state: &mut LifetimeState,
        action: &LifetimeAction,
        _: &mut ReduceContext<'_>,
    ) -> Effect<LifetimeAction> {
        state.base += 1;
        match action {
            LifetimeAction::Remove(Family::Optional) => state.optional = None,
            LifetimeAction::Remove(Family::Case) => state.screen = Screen::Off,
            LifetimeAction::Remove(Family::Keyed) => {
                state.rows.remove(&1);
            }
            LifetimeAction::Reinsert => {
                state.rows.remove(&1);
                state.rows.insert(1, 0);
            }
            _ => {}
        }
        Effect::none()
    }
}

/// Exposes the real effect sender while leaving its owning effect alive.
struct CapturingChild(mpsc::Sender<EffectSender<ChildAction>>);
impl Reducer for CapturingChild {
    type State = u8;
    type Action = ChildAction;
    fn reduce(
        &mut self,
        state: &mut u8,
        action: &ChildAction,
        _: &mut ReduceContext<'_>,
    ) -> Effect<ChildAction> {
        match action {
            ChildAction::Increment => {
                *state += 1;
                Effect::none()
            }
            ChildAction::Capture => {
                let captured = self.0.clone();
                Effect::run(move |sender| async move {
                    captured.send(sender).unwrap();
                    std::future::pending::<()>().await;
                })
            }
        }
    }
}

/// Wraps fresh root input for the selected child, distinct from captured output.
fn lifetime_action(family: Family, a: ChildAction) -> LifetimeAction {
    match family {
        Family::Optional => LifetimeAction::Optional(a),
        Family::Case => LifetimeAction::Case(a),
        Family::Keyed => LifetimeAction::Row(KeyedAction::new(1, a)),
    }
}

/// Composes all three child forms with the same sender-capture behaviour.
fn lifetime_reducer(
    capture: mpsc::Sender<EffectSender<ChildAction>>,
) -> impl Reducer<State = LifetimeState, Action = LifetimeAction> {
    LifetimeBase
        .if_some(
            OptionalStatePath::new(
                |s: &LifetimeState| s.optional.as_ref(),
                |s: &mut LifetimeState| s.optional.as_mut(),
            ),
            ActionPath::new(
                |a: &LifetimeAction| match a {
                    LifetimeAction::Optional(a) => Some(a),
                    _ => None,
                },
                LifetimeAction::Optional,
            ),
            CapturingChild(capture.clone()),
        )
        .if_case(
            CaseStatePath::new(
                |s: &LifetimeState| match &s.screen {
                    Screen::On(s) => Some(s),
                    Screen::Off => None,
                },
                |s: &mut LifetimeState| match &mut s.screen {
                    Screen::On(s) => Some(s),
                    Screen::Off => None,
                },
            ),
            ActionPath::new(
                |a: &LifetimeAction| match a {
                    LifetimeAction::Case(a) => Some(a),
                    _ => None,
                },
                LifetimeAction::Case,
            ),
            CapturingChild(capture.clone()),
        )
        .for_each(
            KeyedStatePath::new(
                |s: &LifetimeState| &s.rows,
                |s: &mut LifetimeState| &mut s.rows,
            ),
            KeyedActionPath::new(
                |a: &LifetimeAction| match a {
                    LifetimeAction::Row(a) => Some(a),
                    _ => None,
                },
                LifetimeAction::Row,
            ),
            CapturingChild(capture),
        )
}

/// Queues removal before old output, then checks cancellation and surviving authority.
///
/// The sibling sender and fresh root input are independent positive controls.
/// Shutdown must also finish the sibling's intentionally pending effect.
fn check_stale(family: Family, reinsert: bool) {
    let runtime = DeterministicRuntime::new();
    let (capture, receiver) = mpsc::channel();
    let reducer = lifetime_reducer(capture);
    let reports = Arc::new(AtomicUsize::new(0));
    let reported = Arc::clone(&reports);
    let store = Store::builder(
        LifetimeState {
            optional: Some(0),
            screen: Screen::On(0),
            rows: [(1, 0), (2, 0)].into_iter().collect(),
            base: 0,
        },
        reducer,
        runtime.runtime(),
    )
    .with_issue_handler(move |i| {
        assert_eq!(i.code, "rca.missing_child");
        reported.fetch_add(1, Ordering::SeqCst);
    })
    .build()
    .unwrap();
    let driver = runtime.clone();
    block_on(driver.run(async move {
        let old_task = store
            .send(lifetime_action(family, ChildAction::Capture))
            .await
            .unwrap();
        runtime.run_until_stalled().unwrap();
        let old = receiver.try_recv().unwrap();
        let sibling_task = store
            .send(LifetimeAction::Row(KeyedAction::new(
                2,
                ChildAction::Capture,
            )))
            .await
            .unwrap();
        runtime.run_until_stalled().unwrap();
        let sibling = receiver.try_recv().unwrap();
        let removal = store
            .try_send(if reinsert {
                LifetimeAction::Reinsert
            } else {
                LifetimeAction::Remove(family)
            })
            .unwrap();
        let mut stale = Box::pin(old.send(ChildAction::Increment));
        assert!(
            stale
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending()
        );
        removal.finish().await.unwrap().finish().await;
        assert!(stale.await.unwrap_err().is_cancelled());
        old_task.finish().await;
        assert_eq!(reports.load(Ordering::SeqCst), 0);
        assert_eq!(store.read(|s| s.base).await.unwrap(), 3);
        assert_eq!(store.runtime_snapshot().await.unwrap().commit_version, 3);
        // Sibling authority survives; fresh root ingress is separately authorised.
        sibling.send(ChildAction::Increment).await.unwrap();
        assert_eq!(store.read(|s| s.rows[&2]).await.unwrap(), 1);
        store
            .send(lifetime_action(family, ChildAction::Increment))
            .await
            .unwrap()
            .finish()
            .await;
        assert_eq!(store.read(|s| s.base).await.unwrap(), 5);
        assert_eq!(reports.load(Ordering::SeqCst), usize::from(!reinsert));
        if reinsert {
            assert_eq!(store.read(|s| s.rows[&1]).await.unwrap(), 1);
        }
        store.shutdown().await.unwrap();
        sibling_task.finish().await;
    }))
    .unwrap();
}

#[test]
fn stale_optional_output_is_not_a_recoverable_missing_action() {
    check_stale(Family::Optional, false);
}

#[test]
fn stale_case_output_is_not_a_recoverable_missing_action() {
    check_stale(Family::Case, false);
}

#[test]
fn stale_keyed_output_is_not_a_recoverable_missing_action() {
    check_stale(Family::Keyed, false);
}

#[test]
fn same_id_reinsertion_rejects_old_output_but_accepts_fresh_root_input() {
    check_stale(Family::Keyed, true);
}
