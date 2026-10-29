//! Missing children and contradictory projections must fail at the right boundary.

use std::any::Any;
use std::sync::{Arc, Mutex};

use futures_executor::block_on;
use rca_core::{
    ActionPath, CaseStatePath, Effect, Issue, IssueReportError, IssueSeverity, KeyedAction,
    KeyedActionPath, KeyedState, KeyedStatePath, MissingChildPolicy, OptionalStatePath, Reduce,
    ReduceContext, Reducer, ReducerExt, Store, for_each,
};

mod support;
use support::ManualRuntime;

/// Ordered observations shared by reducers and the issue reporter.
type Events = Arc<Mutex<Vec<&'static str>>>;

/// Records parent entry without changing optional child state.
struct Base(Events);
impl Reducer for Base {
    type State = Option<u8>;
    type Action = bool;
    fn reduce(&mut self, _: &mut Self::State, _: &bool, _: &mut ReduceContext<'_>) -> Effect<bool> {
        self.0.lock().unwrap().push("base");
        Effect::none()
    }
}
/// Records child entry before incrementing its state.
struct Child(Events);
impl Reducer for Child {
    type State = u8;
    type Action = ();
    fn reduce(&mut self, state: &mut u8, (): &(), _: &mut ReduceContext<'_>) -> Effect<()> {
        self.0.lock().unwrap().push("child");
        *state += 1;
        Effect::none()
    }
}
/// Builds the same optional adapter under each missing-child policy.
fn optional(
    events: &Events,
    policy: MissingChildPolicy,
) -> impl Reducer<State = Option<u8>, Action = bool> + use<> {
    Base(Arc::clone(events))
        .if_some(
            OptionalStatePath::new(|s: &Option<u8>| s.as_ref(), |s: &mut Option<u8>| s.as_mut()),
            ActionPath::new(|a: &bool| a.then_some(&()), |()| true),
            Child(Arc::clone(events)),
        )
        .missing_child_policy(policy)
}
/// Returns the actor's original panic after proving that the store closed.
fn failure<R>(
    state: R::State,
    reducer: R,
    action: R::Action,
    handler: Option<fn(&Issue)>,
) -> Box<dyn Any + Send>
where
    R: Reducer + Send + 'static,
    R::State: Send + 'static,
    R::Action: Send + 'static,
{
    let runtime = ManualRuntime::new();
    let builder = Store::builder(state, reducer, runtime.runtime());
    let builder = if let Some(handler) = handler {
        builder.with_issue_handler(handler)
    } else {
        builder
    };
    let store = builder.build().unwrap();
    runtime.start_all();
    block_on(store.send(action)).unwrap_err();
    block_on(store.closed());
    assert!(store.is_closed());
    runtime
        .join_all()
        .into_iter()
        .find_map(Result::err)
        .expect("actor panic retained by test host")
}

#[test]
fn disabled_required_report_fails_before_base_with_original_typed_evidence() {
    let events = Events::default();
    let panic = failure(
        None,
        optional(&events, MissingChildPolicy::ReportAndBase),
        true,
        None,
    );
    assert!(events.lock().unwrap().is_empty());
    let error = panic
        .downcast::<IssueReportError>()
        .expect("diagnostic failure is not stringified");
    assert!(error.is_disabled());
    let issue = error.into_issue();
    assert_eq!(issue.code, "rca.missing_child");
    assert_eq!(issue.severity, IssueSeverity::Error);
    assert!(issue.message.contains("IfSome"));
    assert!(issue.location.file().ends_with("lifecycle.rs"));
}

#[test]
fn explicit_fail_fast_never_calls_child_or_base() {
    let events = Events::default();
    let panic = failure(
        None,
        optional(&events, MissingChildPolicy::FailFast),
        true,
        None,
    );
    assert!(events.lock().unwrap().is_empty());
    assert!(
        panic
            .downcast_ref::<String>()
            .unwrap()
            .contains("IfSome received child action")
    );
}

#[test]
fn reporter_panic_propagates_before_base() {
    fn panicking_reporter(_: &Issue) {
        panic!("reporter panic");
    }
    let events = Events::default();
    let panic = failure(
        None,
        optional(&events, MissingChildPolicy::ReportAndBase),
        true,
        Some(panicking_reporter),
    );
    assert!(events.lock().unwrap().is_empty());
    assert_eq!(panic.downcast_ref::<&str>(), Some(&"reporter panic"));
}

#[test]
fn accepted_missing_report_precedes_base_and_valid_child_still_precedes_base() {
    for (initial, expected) in [
        (None, vec!["report", "base"]),
        (Some(0), vec!["child", "base"]),
    ] {
        let events = Events::default();
        let reports = Arc::clone(&events);
        let runtime = ManualRuntime::new();
        let store = Store::builder(
            initial,
            optional(&events, MissingChildPolicy::ReportAndBase),
            runtime.runtime(),
        )
        .with_issue_handler(move |i| {
            assert_eq!(i.code, "rca.missing_child");
            reports.lock().unwrap().push("report");
        })
        .build()
        .unwrap();
        runtime.start_all();
        block_on(async {
            store.send(true).await.unwrap().finish().await;
            assert_eq!(*events.lock().unwrap(), expected);
            assert_eq!(store.read(|s| *s).await.unwrap(), initial.map(|v| v + 1));
            store.shutdown().await.unwrap();
        });
        assert!(runtime.join_all().into_iter().all(|r| r.is_ok()));
    }
}

#[test]
fn contradictory_optional_projection_is_fatal_not_a_missing_report() {
    let events = Events::default();
    let reducer = Base(Arc::clone(&events)).if_some(
        OptionalStatePath::new(|s: &Option<u8>| s.as_ref(), |_: &mut Option<u8>| None),
        ActionPath::new(|_: &bool| Some(&()), |()| true),
        Child(Arc::clone(&events)),
    );
    let panic = failure(Some(0), reducer, true, None);
    assert!(events.lock().unwrap().is_empty());
    assert!(
        panic
            .downcast_ref::<String>()
            .unwrap()
            .contains("present lifecycle child must remain borrowable")
    );
}

/// Separates immutable and mutable projections to test semantic identity checks.
struct SplitIdentity {
    observed: u8,
    projected: u8,
}

/// Checks mismatched identities, equal identities at different addresses and parent-only actions.
fn assert_identified_projection_boundary<R>(make_reducer: impl Fn(&Events) -> R)
where
    R: Reducer<State = SplitIdentity, Action = bool> + Send + 'static,
{
    let events = Events::default();
    let panic = failure(
        SplitIdentity {
            observed: 1,
            projected: 2,
        },
        make_reducer(&events),
        true,
        None,
    );
    assert!(events.lock().unwrap().is_empty());
    assert_eq!(
        panic.downcast_ref::<&str>(),
        Some(&"identified lifecycle child must retain its identity through the mutable projection")
    );

    // Equal identities may be stored at different addresses. Parent-only
    // actions do not enter the mutable child projection at all.
    for (projected, action, expected) in
        [(1, true, vec!["child", "base"]), (2, false, vec!["base"])]
    {
        let events = Events::default();
        let runtime = ManualRuntime::new();
        let store = Store::new(
            SplitIdentity {
                observed: 1,
                projected,
            },
            make_reducer(&events),
            runtime.runtime(),
        );
        runtime.start_all();
        block_on(async {
            store.send(action).await.unwrap().finish().await;
            assert_eq!(*events.lock().unwrap(), expected);
            store.shutdown().await.unwrap();
        });
        assert!(runtime.join_all().into_iter().all(|result| result.is_ok()));
    }
}

/// Records base reduction without repairing the deliberately split projections.
fn split_identity_parent(
    events: &Events,
) -> impl Reducer<State = SplitIdentity, Action = bool> + use<> {
    let events = Arc::clone(events);
    Reduce::new(move |_: &mut SplitIdentity, _: &bool, _| {
        events.lock().unwrap().push("base");
        Effect::none()
    })
}

/// Records child entry and changes only the value selected for mutation.
fn split_identity_child(events: &Events) -> impl Reducer<State = u8, Action = ()> + use<> {
    let events = Arc::clone(events);
    Reduce::new(move |_: &mut u8, (): &(), _| {
        events.lock().unwrap().push("child");
        Effect::none()
    })
}

#[test]
fn identified_optional_projection_checks_semantic_identity_before_child_entry() {
    assert_identified_projection_boundary(|events| {
        split_identity_parent(events).if_some_identified(
            OptionalStatePath::new(
                |state: &SplitIdentity| Some(&state.observed),
                |state: &mut SplitIdentity| Some(&mut state.projected),
            ),
            ActionPath::new(|action: &bool| action.then_some(&()), |()| true),
            |identity: &u8| identity,
            split_identity_child(events),
        )
    });
}

#[test]
fn identified_case_projection_checks_semantic_identity_before_child_entry() {
    assert_identified_projection_boundary(|events| {
        split_identity_parent(events).if_case_identified(
            CaseStatePath::new(
                |state: &SplitIdentity| Some(&state.observed),
                |state: &mut SplitIdentity| Some(&mut state.projected),
            ),
            ActionPath::new(|action: &bool| action.then_some(&()), |()| true),
            |identity: &u8| identity,
            split_identity_child(events),
        )
    });
}

/// Reports an active key while exposing a separate mutable collection.
struct Rows {
    active: KeyedState<u8, u8>,
    projected: KeyedState<u8, u8>,
}
/// Detects forbidden parent entry after contradictory keyed projections.
struct RowsBase(Events);
impl Reducer for RowsBase {
    type State = Rows;
    type Action = KeyedAction<u8, ()>;
    fn reduce(
        &mut self,
        _: &mut Rows,
        _: &Self::Action,
        _: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        self.0.lock().unwrap().push("base");
        Effect::none()
    }
}
#[test]
fn active_key_with_missing_mutable_projection_is_fatal_not_a_missing_report() {
    let events = Events::default();
    let reducer = for_each(
        RowsBase(Arc::clone(&events)),
        KeyedStatePath::new(|s: &Rows| &s.active, |s: &mut Rows| &mut s.projected),
        KeyedActionPath::new(|a: &KeyedAction<u8, ()>| Some(a), |a| a),
        Child(Arc::clone(&events)),
    );
    let panic = failure(
        Rows {
            active: [(1, 0)].into_iter().collect(),
            projected: KeyedState::new(),
        },
        reducer,
        KeyedAction::new(1, ()),
        None,
    );
    assert!(events.lock().unwrap().is_empty());
    assert!(
        panic
            .downcast_ref::<String>()
            .unwrap()
            .contains("active keyed child must remain borrowable")
    );
}
