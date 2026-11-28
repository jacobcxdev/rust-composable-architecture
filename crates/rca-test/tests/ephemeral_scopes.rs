//! Exercises real ephemeral dismissal and enclosing scope authority.

use futures_executor::block_on;
use rca_core::{
    ActionPath, CaseStatePath, Dismiss, Effect, EffectSender, KeyedAction, KeyedActionPath,
    KeyedState, KeyedStatePath, OptionalStatePath, PresentationAction, ReduceContext, Reducer,
    ScopedStore, StatePath, Store, ephemeral, for_each, if_case, if_some, presentation,
};
use rca_test::DeterministicRuntime;
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

/// Presented data with no reducer, `Clone` or `Debug` implementation.
struct Payload {
    id: u8,
    value: u8,
}

/// Keeps parent work and dismissal counts separate from the presented data.
struct Feature {
    data: Option<Payload>,
    completed: u8,
    dismissals: u8,
}
impl Feature {
    /// Reuses the same visible ID and value to challenge stale handles.
    fn initial() -> Self {
        Self {
            data: Some(Payload { id: 1, value: 7 }),
            completed: 0,
            dismissals: 0,
        }
    }
}

/// Captures dismissal handles and exercises their immediate or delayed replay.
enum DataAction {
    Capture(Arc<Mutex<Option<Dismiss>>>),
    Replay(Dismiss),
    Delayed(Dismiss),
    ReplaceAndDismiss,
    CloseAndStartParent,
    StartParent,
    DismissAndStartParent,
}

/// Separates matched data actions from actions handled only by the parent.
enum Action {
    Data(PresentationAction<DataAction>),
    Clear,
    Insert,
    Tick,
    CaptureOutput(mpsc::Sender<EffectSender<Action>>),
    ReplayUnmatched(Dismiss),
    CaptureUnmatched(Arc<Mutex<Option<Dismiss>>>),
}

/// Handles data actions without giving the data its own reducer.
struct Parent;
impl Reducer for Parent {
    type State = Feature;
    type Action = Action;
    fn reduce(
        &mut self,
        state: &mut Feature,
        action: &Action,
        context: &mut ReduceContext<'_>,
    ) -> Effect<Action> {
        match action {
            Action::Data(PresentationAction::Presented(DataAction::Capture(slot)))
            | Action::CaptureUnmatched(slot) => {
                *slot.lock().unwrap() = Some(context.dismiss());
            }
            Action::Data(PresentationAction::Presented(DataAction::Replay(dismiss)))
            | Action::ReplayUnmatched(dismiss) => return nested_dismiss(dismiss),
            Action::Data(PresentationAction::Presented(DataAction::Delayed(dismiss))) => {
                return nested_dismiss(dismiss).delay(Duration::from_secs(1));
            }
            Action::Data(PresentationAction::Presented(DataAction::ReplaceAndDismiss)) => {
                state.data = Some(Payload { id: 2, value: 42 });
                return context.dismiss().effect();
            }
            Action::Data(PresentationAction::Presented(
                DataAction::CloseAndStartParent | DataAction::StartParent,
            )) => return Effect::send_after(Duration::from_secs(1), Action::Tick),
            Action::Data(PresentationAction::Presented(DataAction::DismissAndStartParent)) => {
                return Effect::merge([
                    context.dismiss().effect(),
                    Effect::send_after(Duration::from_secs(1), Action::Tick),
                ]);
            }
            Action::Data(PresentationAction::Dismiss) => {
                assert!(
                    state.data.is_some(),
                    "parent inspects data before dismissal"
                );
                state.dismissals += 1;
            }
            Action::Clear => state.data = None,
            Action::Insert => state.data = Some(Payload { id: 1, value: 7 }),
            Action::Tick => state.completed += 1,
            Action::CaptureOutput(captured) => {
                let captured = captured.clone();
                return Effect::run(move |sender| async move {
                    captured.send(sender).unwrap();
                    std::future::pending::<()>().await;
                });
            }
        }
        Effect::none()
    }
}

/// Places dismissal inside effect containers that must preserve its original target.
fn nested_dismiss(dismiss: &Dismiss) -> Effect<Action> {
    Effect::concatenate([
        Effect::none(),
        Effect::merge([
            Effect::none(),
            dismiss
                .effect()
                .cancellable("dismiss")
                .without_inherited_cancellation(),
        ]),
    ])
}

/// Removes data only for the selected close action or an authorised dismissal.
fn reducer() -> impl Reducer<State = Feature, Action = Action> + Send {
    ephemeral(
        Parent,
        StatePath::new(
            |state: &Feature| &state.data,
            |state: &mut Feature| &mut state.data,
        ),
        ActionPath::new(
            |action: &Action| match action {
                Action::Data(action) => Some(action),
                _ => None,
            },
            Action::Data,
        ),
        |data: &Payload| &data.id,
        |_: &Payload, action: &DataAction| matches!(action, DataAction::CloseAndStartParent),
    )
}

/// Runs one caller operation without advancing virtual time.
fn drive<F: Future + Send>(runtime: &DeterministicRuntime, future: F) -> F::Output
where
    F::Output: Send,
{
    block_on(runtime.run(future)).unwrap()
}

fn data(action: DataAction) -> Action {
    Action::Data(PresentationAction::Presented(action))
}

/// Captures the target selected by an actual presented action.
fn capture(runtime: &DeterministicRuntime, store: &Store<Feature, Action>) -> Dismiss {
    let slot = Arc::new(Mutex::new(None));
    let _ = drive(
        runtime,
        store.send(data(DataAction::Capture(Arc::clone(&slot)))),
    )
    .unwrap();
    slot.lock().unwrap().take().unwrap()
}

fn value(state: &Feature) -> Option<(u8, u8)> {
    state.data.as_ref().map(|data| (data.id, data.value))
}

// Binding only top-level controls or requiring a newly matching action breaks this.
#[test]
fn retained_dismissal_binds_nested_containers_on_unmatched_parent_action() {
    let runtime = DeterministicRuntime::new();
    let store = Store::new(Feature::initial(), reducer(), runtime.runtime());
    let dismiss = capture(&runtime, &store);
    let task = drive(&runtime, store.send(Action::ReplayUnmatched(dismiss))).unwrap();
    drive(&runtime, task.finish());
    assert_eq!(
        drive(
            &runtime,
            store.read(|state| (value(state), state.dismissals))
        )
        .unwrap(),
        (None, 1)
    );
    drive(&runtime, store.shutdown()).unwrap();
}

// Discarding the captured leaf token would clear the replacement after commit.
#[test]
fn same_action_replacement_retires_its_newly_bound_dismiss_effect() {
    let runtime = DeterministicRuntime::new();
    let store = Store::new(Feature::initial(), reducer(), runtime.runtime());
    let task = drive(&runtime, store.send(data(DataAction::ReplaceAndDismiss))).unwrap();
    drive(&runtime, task.finish());
    assert_eq!(
        drive(
            &runtime,
            store.read(|state| (value(state), state.dismissals))
        )
        .unwrap(),
        (Some((2, 42)), 0)
    );
    drive(&runtime, store.shutdown()).unwrap();
}

// Visible equality must not grant either delayed or retained controls fresh authority.
#[test]
fn equal_data_reinsertion_rejects_old_delayed_and_retained_dismissals() {
    let runtime = DeterministicRuntime::new();
    let store = Store::new(Feature::initial(), reducer(), runtime.runtime());
    let old = capture(&runtime, &store);
    let delayed = drive(&runtime, store.send(data(DataAction::Delayed(old.clone())))).unwrap();
    runtime.run_until_stalled().unwrap();
    let _ = drive(&runtime, store.send(Action::Clear)).unwrap();
    let _ = drive(&runtime, store.send(Action::Insert)).unwrap();
    let replay = drive(&runtime, store.send(data(DataAction::Replay(old)))).unwrap();
    drive(&runtime, replay.finish());
    runtime.advance(Duration::from_secs(1)).unwrap();
    assert!(delayed.is_finished());
    assert_eq!(drive(&runtime, store.read(value)).unwrap(), Some((1, 7)));
    let fresh = capture(&runtime, &store);
    let close = drive(&runtime, store.send(data(DataAction::Replay(fresh)))).unwrap();
    drive(&runtime, close.finish());
    assert_eq!(drive(&runtime, store.read(value)).unwrap(), None);
    drive(&runtime, store.shutdown()).unwrap();
}

// Applying the ephemeral scope to the whole parent effect cancels these timers.
#[test]
fn plain_parent_work_survives_selected_clear_and_later_explicit_clear() {
    for selected in [false, true] {
        let runtime = DeterministicRuntime::new();
        let store = Store::new(Feature::initial(), reducer(), runtime.runtime());
        let action = if selected {
            DataAction::CloseAndStartParent
        } else {
            DataAction::StartParent
        };
        let task = drive(&runtime, store.send(data(action))).unwrap();
        if !selected {
            let _ = drive(
                &runtime,
                store.send(Action::Data(PresentationAction::Dismiss)),
            )
            .unwrap();
        }
        runtime.run_until_stalled().unwrap();
        assert!(!task.is_finished());
        runtime.advance(Duration::from_secs(1)).unwrap();
        assert!(task.is_finished());
        assert_eq!(
            drive(
                &runtime,
                store.read(|state| (value(state), state.completed))
            )
            .unwrap(),
            (None, 1)
        );
        drive(&runtime, store.shutdown()).unwrap();
    }
}

/// Shares one adapter between stores to separate adapter identity from store ownership.
struct Shared<R>(Arc<Mutex<R>>);
impl<R: Reducer> Reducer for Shared<R> {
    type State = R::State;
    type Action = R::Action;
    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &Self::Action,
        context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        self.0.lock().unwrap().reduce(state, action, context)
    }
}

// Identical numeric scope and shared adapter identity are not Store authority.
#[test]
fn stores_sharing_an_adapter_cannot_retarget_each_others_dismissal() {
    assert_store_namespace(true);
}

#[test]
fn independent_stores_cannot_retarget_each_others_dismissal() {
    assert_store_namespace(false);
}

/// Rejects a foreign dismissal while proving its original store can still use it.
fn assert_store_namespace(shared_adapter: bool) {
    let runtime = DeterministicRuntime::new();
    let shared = Arc::new(Mutex::new(reducer()));
    let source = Store::new(
        Feature::initial(),
        Shared(Arc::clone(&shared)),
        runtime.runtime(),
    );
    let issues = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&issues);
    let destination_reducer = if shared_adapter {
        shared
    } else {
        Arc::new(Mutex::new(reducer()))
    };
    let destination = Store::builder(
        Feature::initial(),
        Shared(destination_reducer),
        runtime.runtime(),
    )
    .with_issue_handler(move |issue| recorded.lock().unwrap().push(issue.code))
    .build()
    .unwrap();
    let dismiss = capture(&runtime, &source);
    let foreign = drive(
        &runtime,
        destination.send(data(DataAction::Replay(dismiss.clone()))),
    )
    .unwrap();
    drive(&runtime, foreign.finish());
    assert_eq!(
        drive(&runtime, destination.read(value)).unwrap(),
        Some((1, 7))
    );
    assert_eq!(
        issues.lock().unwrap().as_slice(),
        ["rca.dismiss_unavailable"]
    );
    let local = drive(&runtime, source.send(data(DataAction::Replay(dismiss)))).unwrap();
    drive(&runtime, local.finish());
    assert_eq!(drive(&runtime, source.read(value)).unwrap(), None);
    drive(&runtime, source.shutdown()).unwrap();
    drive(&runtime, destination.shutdown()).unwrap();
}

/// Provides an enum-case lifetime alongside optional and keyed lifetimes.
enum Choice {
    Active(Feature),
    Empty,
}

/// Gives each container the same data so visible values cannot establish ownership.
struct Containers {
    optional: Option<Feature>,
    case: Choice,
    rows: KeyedState<u8, Feature>,
}

/// Uses family 0 for optional, 1 for case and 2 for the first keyed row.
enum ContainerAction {
    Optional(Action),
    Case(Action),
    Row(KeyedAction<u8, Action>),
    Remove(u8),
    Insert(u8),
    Reorder,
}

/// Removes, reinserts or reorders one container without changing the others.
struct ContainersParent;
impl Reducer for ContainersParent {
    type State = Containers;
    type Action = ContainerAction;
    fn reduce(
        &mut self,
        state: &mut Containers,
        action: &ContainerAction,
        _: &mut ReduceContext<'_>,
    ) -> Effect<ContainerAction> {
        match action {
            ContainerAction::Remove(0) => state.optional = None,
            ContainerAction::Remove(1) => state.case = Choice::Empty,
            ContainerAction::Remove(2) => {
                state.rows.remove(&1);
            }
            ContainerAction::Insert(0) => state.optional = Some(Feature::initial()),
            ContainerAction::Insert(1) => state.case = Choice::Active(Feature::initial()),
            ContainerAction::Insert(2) => {
                state.rows.insert(1, Feature::initial());
            }
            ContainerAction::Reorder => state.rows.move_index(1, 0),
            _ => {}
        }
        Effect::none()
    }
}

/// Recreates the optional projection without naming its closure types.
macro_rules! optional_state {
    () => {
        OptionalStatePath::new(
            |state: &Containers| state.optional.as_ref(),
            |state: &mut Containers| state.optional.as_mut(),
        )
    };
}

/// Routes only optional-child actions.
macro_rules! optional_action {
    () => {
        ActionPath::new(
            |action: &ContainerAction| match action {
                ContainerAction::Optional(action) => Some(action),
                _ => None,
            },
            ContainerAction::Optional,
        )
    };
}

/// Selects the active case without constructing replacement data.
macro_rules! case_state {
    () => {
        CaseStatePath::new(
            |state: &Containers| match &state.case {
                Choice::Active(state) => Some(state),
                Choice::Empty => None,
            },
            |state: &mut Containers| match &mut state.case {
                Choice::Active(state) => Some(state),
                Choice::Empty => None,
            },
        )
    };
}

/// Routes only active-case actions.
macro_rules! case_action {
    () => {
        ActionPath::new(
            |action: &ContainerAction| match action {
                ContainerAction::Case(action) => Some(action),
                _ => None,
            },
            ContainerAction::Case,
        )
    };
}

/// Recreates the keyed projection for both composition and public scopes.
macro_rules! keyed_state {
    () => {
        KeyedStatePath::new(
            |state: &Containers| &state.rows,
            |state: &mut Containers| &mut state.rows,
        )
    };
}

/// Preserves row identity when routing keyed actions.
macro_rules! keyed_action {
    () => {
        KeyedActionPath::new(
            |action: &ContainerAction| match action {
                ContainerAction::Row(action) => Some(action),
                _ => None,
            },
            ContainerAction::Row,
        )
    };
}

/// Composes three independent lifetime-owning wrappers around the same fixture.
fn containers() -> impl Reducer<State = Containers, Action = ContainerAction> + Send {
    let keyed = for_each(ContainersParent, keyed_state!(), keyed_action!(), reducer());
    let case = if_case(keyed, case_state!(), case_action!(), reducer());
    if_some(case, optional_state!(), optional_action!(), reducer())
}

/// Adds a second keyed row as a live control for removal and reordering.
fn container_store(runtime: &DeterministicRuntime) -> Store<Containers, ContainerAction> {
    Store::new(
        Containers {
            optional: Some(Feature::initial()),
            case: Choice::Active(Feature::initial()),
            rows: [(1, Feature::initial()), (2, Feature::initial())]
                .into_iter()
                .collect(),
        },
        containers(),
        runtime.runtime(),
    )
}

/// Selects optional (0), case (1), first row (2), or the surviving second row.
fn scope(
    runtime: &DeterministicRuntime,
    store: &Store<Containers, ContainerAction>,
    family: u8,
) -> ScopedStore<Feature, Action> {
    match family {
        0 => drive(
            runtime,
            store.scope_some(optional_state!(), optional_action!()),
        )
        .unwrap(),
        1 => drive(runtime, store.scope_case(case_state!(), case_action!())).unwrap(),
        2 => drive(
            runtime,
            store.scope_keyed(keyed_state!(), keyed_action!(), 1),
        )
        .unwrap(),
        _ => drive(
            runtime,
            store.scope_keyed(keyed_state!(), keyed_action!(), 2),
        )
        .unwrap(),
    }
}

// Each branch uses a real wrapper; equal visible data cannot revive its old handle.
#[test]
fn optional_case_and_keyed_reinsertion_reject_old_outputs_callbacks_and_readers() {
    for family in 0..3 {
        let runtime = DeterministicRuntime::new();
        let store = container_store(&runtime);
        let old = scope(&runtime, &store, family);
        let mut reader = drive(&runtime, old.observe_distinct(|state| state.completed)).unwrap();
        assert_eq!(drive(&runtime, reader.next()), Some(0));
        let dismiss_slot = Arc::new(Mutex::new(None));
        let _ = drive(
            &runtime,
            old.send(data(DataAction::Capture(Arc::clone(&dismiss_slot)))),
        )
        .unwrap();
        let dismiss = dismiss_slot.lock().unwrap().take().unwrap();
        let (send, receive) = mpsc::channel();
        let work = drive(&runtime, old.send(Action::CaptureOutput(send))).unwrap();
        let timer = drive(&runtime, old.send(data(DataAction::StartParent))).unwrap();
        runtime.run_until_stalled().unwrap();
        let output = receive.try_recv().unwrap();
        let callback = {
            let old = old.clone();
            move || {
                let old = old.clone();
                async move { old.send(Action::Tick).await }
            }
        };
        let _ = drive(&runtime, store.send(ContainerAction::Remove(family))).unwrap();
        let _ = drive(&runtime, store.send(ContainerAction::Insert(family))).unwrap();
        runtime.run_until_stalled().unwrap();
        assert!(work.is_finished());
        assert!(timer.is_finished());
        assert!(
            drive(&runtime, output.send(Action::Tick))
                .unwrap_err()
                .is_cancelled()
        );
        assert!(drive(&runtime, callback()).unwrap_err().is_scope_ended());
        assert_eq!(drive(&runtime, reader.next()), None);
        let fresh = scope(&runtime, &store, family);
        let replay = drive(&runtime, fresh.send(data(DataAction::Replay(dismiss)))).unwrap();
        drive(&runtime, replay.finish());
        assert_eq!(
            drive(
                &runtime,
                fresh.read(|state| (value(state), state.completed))
            )
            .unwrap(),
            (Some((1, 7)), 0)
        );
        let _ = drive(&runtime, fresh.send(Action::Tick)).unwrap();
        assert_eq!(
            drive(&runtime, fresh.read(|state| state.completed)).unwrap(),
            1
        );
        runtime.advance(Duration::from_secs(1)).unwrap();
        assert_eq!(
            drive(&runtime, fresh.read(|state| state.completed)).unwrap(),
            1
        );
        drive(&runtime, store.shutdown()).unwrap();
    }
}

// Reconciliation must not retire untouched keyed siblings when their indices change.
#[test]
fn keyed_survivor_keeps_output_callback_reader_and_dismissal_after_reordering() {
    let runtime = DeterministicRuntime::new();
    let store = container_store(&runtime);
    let survivor = scope(&runtime, &store, 3);
    let mut reader = drive(&runtime, survivor.observe_distinct(|state| state.completed)).unwrap();
    assert_eq!(drive(&runtime, reader.next()), Some(0));
    let slot = Arc::new(Mutex::new(None));
    let _ = drive(
        &runtime,
        survivor.send(data(DataAction::Capture(Arc::clone(&slot)))),
    )
    .unwrap();
    let dismiss = slot.lock().unwrap().take().unwrap();
    let (send, receive) = mpsc::channel();
    let work = drive(&runtime, survivor.send(Action::CaptureOutput(send))).unwrap();
    runtime.run_until_stalled().unwrap();
    let output = receive.try_recv().unwrap();
    let _ = drive(&runtime, store.send(ContainerAction::Reorder)).unwrap();
    let _ = drive(&runtime, store.send(ContainerAction::Remove(2))).unwrap();
    let _ = drive(&runtime, store.send(ContainerAction::Insert(2))).unwrap();
    assert!(!work.is_finished());
    drive(&runtime, output.send(Action::Tick)).unwrap();
    assert_eq!(drive(&runtime, reader.next()), Some(1));
    let callback = || survivor.send(Action::Tick);
    let _ = drive(&runtime, callback()).unwrap();
    assert_eq!(drive(&runtime, reader.next()), Some(2));
    let close = drive(&runtime, survivor.send(data(DataAction::Replay(dismiss)))).unwrap();
    drive(&runtime, close.finish());
    assert_eq!(drive(&runtime, survivor.read(value)).unwrap(), None);
    let replacement = scope(&runtime, &store, 2);
    assert_eq!(
        drive(&runtime, replacement.read(value)).unwrap(),
        Some((1, 7))
    );
    drive(&runtime, store.shutdown()).unwrap();
}

// A live delayed request must eventually dismiss, rather than always being dropped.
#[test]
fn delayed_matching_dismissal_keeps_its_control_through_nested_containers() {
    let runtime = DeterministicRuntime::new();
    let store = Store::new(Feature::initial(), reducer(), runtime.runtime());
    let dismiss = capture(&runtime, &store);
    let task = drive(&runtime, store.send(data(DataAction::Delayed(dismiss)))).unwrap();
    runtime.run_until_stalled().unwrap();
    assert_eq!(drive(&runtime, store.read(value)).unwrap(), Some((1, 7)));
    assert!(!task.is_finished());
    runtime.advance(Duration::from_secs(1)).unwrap();
    assert!(task.is_finished());
    assert_eq!(
        drive(
            &runtime,
            store.read(|state| (value(state), state.dismissals))
        )
        .unwrap(),
        (None, 1)
    );
    drive(&runtime, store.shutdown()).unwrap();
}

// The narrower Dismiss leaf must not confer its lifetime on a merged parent timer.
#[test]
fn mixed_dismissal_and_parent_work_keep_distinct_lifetimes() {
    let runtime = DeterministicRuntime::new();
    let store = Store::new(Feature::initial(), reducer(), runtime.runtime());
    let task = drive(
        &runtime,
        store.send(data(DataAction::DismissAndStartParent)),
    )
    .unwrap();
    runtime.run_until_stalled().unwrap();
    assert_eq!(drive(&runtime, store.read(value)).unwrap(), None);
    assert!(!task.is_finished());
    runtime.advance(Duration::from_secs(1)).unwrap();
    assert!(task.is_finished());
    assert_eq!(
        drive(
            &runtime,
            store.read(|state| (state.dismissals, state.completed))
        )
        .unwrap(),
        (1, 1)
    );
    drive(&runtime, store.shutdown()).unwrap();
}

// Equal semantic IDs in independent routes must not make their controls exchangeable.
#[test]
fn independent_same_id_routes_reject_foreign_controls_but_keep_the_source_live() {
    let runtime = DeterministicRuntime::new();
    let issues = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&issues);
    let store = Store::builder(
        Containers {
            optional: Some(Feature::initial()),
            case: Choice::Active(Feature::initial()),
            rows: KeyedState::new(),
        },
        containers(),
        runtime.runtime(),
    )
    .with_issue_handler(move |issue| recorded.lock().unwrap().push(issue.code))
    .build()
    .unwrap();
    let left = scope(&runtime, &store, 0);
    let right = scope(&runtime, &store, 1);
    let slot = Arc::new(Mutex::new(None));
    let _ = drive(
        &runtime,
        left.send(data(DataAction::Capture(Arc::clone(&slot)))),
    )
    .unwrap();
    let dismiss = slot.lock().unwrap().take().unwrap();
    let foreign = drive(
        &runtime,
        right.send(data(DataAction::Replay(dismiss.clone()))),
    )
    .unwrap();
    drive(&runtime, foreign.finish());
    assert_eq!(drive(&runtime, right.read(value)).unwrap(), Some((1, 7)));
    assert_eq!(drive(&runtime, left.read(value)).unwrap(), Some((1, 7)));
    assert_eq!(
        issues.lock().unwrap().as_slice(),
        ["rca.dismiss_unavailable"]
    );
    let local = drive(&runtime, left.send(data(DataAction::Replay(dismiss)))).unwrap();
    drive(&runtime, local.finish());
    assert_eq!(drive(&runtime, left.read(value)).unwrap(), None);
    assert_eq!(drive(&runtime, right.read(value)).unwrap(), Some((1, 7)));
    drive(&runtime, store.shutdown()).unwrap();
}

/// Places a real worker beside, not inside, the ephemeral data.
struct WithSibling {
    data: Option<Payload>,
    worker: Option<u8>,
    parent_calls: u8,
}

enum WorkerAction {
    Start,
    Tick,
}

enum SiblingAction {
    Data(PresentationAction<WorkerAction>),
}

/// Counts parent reductions to detect duplicated routing through sibling wrappers.
struct SiblingParent;
impl Reducer for SiblingParent {
    type State = WithSibling;
    type Action = SiblingAction;
    fn reduce(
        &mut self,
        state: &mut WithSibling,
        _: &SiblingAction,
        _: &mut ReduceContext<'_>,
    ) -> Effect<SiblingAction> {
        state.parent_calls += 1;
        Effect::none()
    }
}
/// Starts delayed work whose lifetime belongs to the optional sibling.
struct Worker;
impl Reducer for Worker {
    type State = u8;
    type Action = WorkerAction;
    fn reduce(
        &mut self,
        state: &mut u8,
        action: &WorkerAction,
        _: &mut ReduceContext<'_>,
    ) -> Effect<WorkerAction> {
        match action {
            WorkerAction::Start => Effect::send_after(Duration::from_secs(1), WorkerAction::Tick),
            WorkerAction::Tick => {
                *state += 1;
                Effect::none()
            }
        }
    }
}

// with_scope around the parent would mount this genuine sibling below ephemeral data.
#[test]
fn sibling_mounted_during_presented_action_survives_ephemeral_clear() {
    let parent = if_some(
        SiblingParent,
        OptionalStatePath::new(
            |state: &WithSibling| state.worker.as_ref(),
            |state: &mut WithSibling| state.worker.as_mut(),
        ),
        ActionPath::new(
            |action: &SiblingAction| match action {
                SiblingAction::Data(PresentationAction::Presented(action)) => Some(action),
                SiblingAction::Data(PresentationAction::Dismiss) => None,
            },
            |action| SiblingAction::Data(PresentationAction::Presented(action)),
        ),
        Worker,
    );
    let reducer = ephemeral(
        parent,
        StatePath::new(
            |state: &WithSibling| &state.data,
            |state: &mut WithSibling| &mut state.data,
        ),
        ActionPath::new(
            |action: &SiblingAction| match action {
                SiblingAction::Data(action) => Some(action),
            },
            SiblingAction::Data,
        ),
        |data: &Payload| &data.id,
        |_: &Payload, action: &WorkerAction| matches!(action, WorkerAction::Start),
    );
    let runtime = DeterministicRuntime::new();
    let issues = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&issues);
    let store = Store::builder(
        WithSibling {
            data: Some(Payload { id: 1, value: 7 }),
            worker: Some(0),
            parent_calls: 0,
        },
        reducer,
        runtime.runtime(),
    )
    .with_issue_handler(move |issue| recorded.lock().unwrap().push(issue.code))
    .build()
    .unwrap();
    let task = drive(
        &runtime,
        store.send(SiblingAction::Data(PresentationAction::Presented(
            WorkerAction::Start,
        ))),
    )
    .unwrap();
    runtime.run_until_stalled().unwrap();
    assert!(!task.is_finished());
    assert_eq!(
        drive(
            &runtime,
            store.read(|state| (state.data.is_none(), state.worker, state.parent_calls))
        )
        .unwrap(),
        (true, Some(0), 1)
    );
    runtime.advance(Duration::from_secs(1)).unwrap();
    assert!(task.is_finished());
    assert_eq!(
        drive(
            &runtime,
            store.read(|state| (state.data.is_none(), state.worker, state.parent_calls))
        )
        .unwrap(),
        (true, Some(1), 2)
    );
    // The shared input route is absent for ephemeral data but still reaches the live sibling.
    assert_eq!(issues.lock().unwrap().as_slice(), ["rca.missing_child"]);
    drive(&runtime, store.shutdown()).unwrap();
}

/// Records what the enclosing presentation sees before removing its child.
struct OuterState {
    feature: Option<Feature>,
    dismissed_value: Option<(u8, u8)>,
}

/// Observes outer dismissal without installing an inner target.
struct OuterParent;
impl Reducer for OuterParent {
    type State = OuterState;
    type Action = PresentationAction<Action>;
    fn reduce(
        &mut self,
        state: &mut OuterState,
        action: &Self::Action,
        _: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        if matches!(action, PresentationAction::Dismiss) {
            state.dismissed_value = value(state.feature.as_ref().unwrap());
        }
        Effect::none()
    }
}

// Installing an inner target on an unmatched action would dismiss only the inner data.
#[test]
fn unmatched_action_preserves_the_enclosing_presentation_context() {
    let reducer = presentation(
        OuterParent,
        StatePath::new(
            |state: &OuterState| &state.feature,
            |state: &mut OuterState| &mut state.feature,
        ),
        ActionPath::new(
            |action: &PresentationAction<Action>| Some(action),
            |action| action,
        ),
        |_: &Feature| &(),
        reducer(),
    );
    let runtime = DeterministicRuntime::new();
    let store = Store::new(
        OuterState {
            feature: Some(Feature::initial()),
            dismissed_value: None,
        },
        reducer,
        runtime.runtime(),
    );
    let slot = Arc::new(Mutex::new(None));
    let _ = drive(
        &runtime,
        store.send(PresentationAction::Presented(Action::CaptureUnmatched(
            Arc::clone(&slot),
        ))),
    )
    .unwrap();
    let dismiss = slot.lock().unwrap().take().unwrap();
    let task = drive(
        &runtime,
        store.send(PresentationAction::Presented(Action::ReplayUnmatched(
            dismiss,
        ))),
    )
    .unwrap();
    drive(&runtime, task.finish());
    assert_eq!(
        drive(
            &runtime,
            store.read(|state| (state.feature.is_none(), state.dismissed_value))
        )
        .unwrap(),
        (true, Some((1, 7)))
    );
    drive(&runtime, store.shutdown()).unwrap();
}
