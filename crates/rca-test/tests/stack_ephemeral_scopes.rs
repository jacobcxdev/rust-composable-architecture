//! Qualifies data-only ephemeral authority inside actual stack entries.

use futures_executor::block_on;
use rca_core::{
    ActionPath, Dismiss, Effect, EffectSender, PresentationAction, ReduceContext, Reducer,
    ScopedStore, StackAction, StackId, StackState, StatePath, Store, ephemeral, stack,
};
use rca_test::DeterministicRuntime;
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

/// Presented data with no reducer, `Clone` or `Debug` implementation.
struct Data {
    id: u8,
    value: u8,
}

/// Separates the data lifetime from work owned by its stack entry.
struct Entry {
    data: Option<Data>,
    completed: u8,
}
impl Entry {
    /// Reuses visible values so only lifetime identity can distinguish replacements.
    fn initial() -> Self {
        Self {
            data: Some(Data { id: 1, value: 7 }),
            completed: 0,
        }
    }
}

/// Exercises captured dismissal after immediate replay, delay or local removal.
enum DataAction {
    Capture(Arc<Mutex<Option<Dismiss>>>),
    Replay(Dismiss),
    Delayed(Dismiss),
    CloseAndStartParent,
}

/// Keeps parent timers and output handles outside the presented data's lifetime.
enum EntryAction {
    Data(PresentationAction<DataAction>),
    StartParent,
    Tick,
    CaptureOutput(mpsc::Sender<EffectSender<EntryAction>>),
}

/// Inspects data on dismissal and owns the longer-lived entry work.
struct EntryParent;
impl Reducer for EntryParent {
    type State = Entry;
    type Action = EntryAction;
    fn reduce(
        &mut self,
        state: &mut Entry,
        action: &EntryAction,
        context: &mut ReduceContext<'_>,
    ) -> Effect<EntryAction> {
        match action {
            EntryAction::Data(PresentationAction::Presented(DataAction::Capture(slot))) => {
                *slot.lock().unwrap() = Some(context.dismiss());
            }
            EntryAction::Data(PresentationAction::Presented(DataAction::Replay(dismiss))) => {
                return dismiss.effect();
            }
            EntryAction::Data(PresentationAction::Presented(DataAction::Delayed(dismiss))) => {
                return dismiss.effect().delay(Duration::from_secs(1));
            }
            EntryAction::Data(PresentationAction::Presented(DataAction::CloseAndStartParent))
            | EntryAction::StartParent => {
                return Effect::send_after(Duration::from_secs(1), EntryAction::Tick);
            }
            EntryAction::Data(PresentationAction::Dismiss) => {
                assert_eq!(state.data.as_ref().map(|data| data.value), Some(7));
            }
            EntryAction::Tick => state.completed += 1,
            EntryAction::CaptureOutput(captured) => {
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

/// Recreates a stack entry with the same public ID but a fresh lifetime.
enum Action {
    Path(StackAction<(), EntryAction>),
    Recreate(StackId),
}

/// Performs remove-and-reinsert in one parent reduction.
struct Parent;
impl Reducer for Parent {
    type State = StackState<Entry>;
    type Action = Action;
    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &Action,
        _: &mut ReduceContext<'_>,
    ) -> Effect<Action> {
        if let Action::Recreate(id) = action {
            state.try_pop_from(id).unwrap();
            state
                .try_push(id.clone(), Entry::initial())
                .unwrap_or_else(|_| panic!("selected ID was removed"));
        }
        Effect::none()
    }
}

/// Nests data-only dismissal inside a real stack-entry lifetime.
fn reducer() -> impl Reducer<State = StackState<Entry>, Action = Action> + Send {
    let entry = ephemeral(
        EntryParent,
        StatePath::new(
            |entry: &Entry| &entry.data,
            |entry: &mut Entry| &mut entry.data,
        ),
        ActionPath::new(
            |action: &EntryAction| match action {
                EntryAction::Data(action) => Some(action),
                _ => None,
            },
            EntryAction::Data,
        ),
        |data: &Data| &data.id,
        |_: &Data, action: &DataAction| matches!(action, DataAction::CloseAndStartParent),
    );
    stack(
        Parent,
        StatePath::new(
            |state: &StackState<Entry>| state,
            |state: &mut StackState<Entry>| state,
        ),
        ActionPath::new(
            |action: &Action| match action {
                Action::Path(action) => Some(action),
                Action::Recreate(_) => None,
            },
            Action::Path,
        ),
        |(): &(), _: &mut ReduceContext<'_>| Entry::initial(),
        entry,
    )
}

/// Runs one caller operation without advancing virtual time.
fn drive<F: Future + Send>(runtime: &DeterministicRuntime, future: F) -> F::Output
where
    F::Output: Send,
{
    block_on(runtime.run(future)).unwrap()
}

/// Acquires a generation-qualified handle through the public stack API.
fn scope(
    runtime: &DeterministicRuntime,
    store: &Store<StackState<Entry>, Action>,
    id: StackId,
) -> ScopedStore<Entry, EntryAction> {
    drive(
        runtime,
        store.scope_stack(
            StatePath::new(
                |state: &StackState<Entry>| state,
                |state: &mut StackState<Entry>| state,
            ),
            ActionPath::new(
                |action: &Action| match action {
                    Action::Path(action) => Some(action),
                    Action::Recreate(_) => None,
                },
                Action::Path,
            ),
            id,
        ),
    )
    .unwrap()
}

fn presented(action: DataAction) -> EntryAction {
    EntryAction::Data(PresentationAction::Presented(action))
}

/// Obtains dismissal from a presented action in the selected entry.
fn capture(runtime: &DeterministicRuntime, entry: &ScopedStore<Entry, EntryAction>) -> Dismiss {
    let slot = Arc::new(Mutex::new(None));
    let _ = drive(
        runtime,
        entry.send(presented(DataAction::Capture(Arc::clone(&slot)))),
    )
    .unwrap();
    slot.lock().unwrap().take().unwrap()
}

/// Reads only comparable values, without adding traits to the data fixture.
fn snapshot(entry: &Entry) -> (Option<u8>, u8) {
    (entry.data.as_ref().map(|data| data.value), entry.completed)
}

// Public-ID-only authority or globally retiring the shared ephemeral slot breaks this.
#[test]
fn stack_reinsertion_retires_old_ephemeral_authority_but_preserves_the_prefix() {
    let runtime = DeterministicRuntime::new();
    let mut state = StackState::new();
    let prefix_id = state.push(Entry::initial());
    let selected_id = state.push(Entry::initial());
    let store = Store::new(state, reducer(), runtime.runtime());
    let prefix = scope(&runtime, &store, prefix_id);
    let old = scope(&runtime, &store, selected_id.clone());
    let prefix_dismiss = capture(&runtime, &prefix);
    let old_dismiss = capture(&runtime, &old);
    let delayed = drive(
        &runtime,
        old.send(presented(DataAction::Delayed(old_dismiss.clone()))),
    )
    .unwrap();
    let old_timer = drive(&runtime, old.send(EntryAction::StartParent)).unwrap();
    let prefix_timer = drive(&runtime, prefix.send(EntryAction::StartParent)).unwrap();
    let mut old_reader = drive(&runtime, old.observe_distinct(|entry| entry.completed)).unwrap();
    let mut prefix_reader =
        drive(&runtime, prefix.observe_distinct(|entry| entry.completed)).unwrap();
    assert_eq!(drive(&runtime, old_reader.next()), Some(0));
    assert_eq!(drive(&runtime, prefix_reader.next()), Some(0));
    let old_callback = || old.send(EntryAction::Tick);
    let prefix_callback = || prefix.send(EntryAction::Tick);
    let (send, receive) = mpsc::channel();
    let work = drive(&runtime, old.send(EntryAction::CaptureOutput(send))).unwrap();
    runtime.run_until_stalled().unwrap();
    let output = receive.try_recv().unwrap();
    let _ = drive(&runtime, store.send(Action::Recreate(selected_id.clone()))).unwrap();
    runtime.run_until_stalled().unwrap();
    for task in [&delayed, &old_timer, &work] {
        assert!(task.is_finished());
    }
    assert!(!prefix_timer.is_finished());
    assert!(
        drive(&runtime, old_callback())
            .unwrap_err()
            .is_scope_ended()
    );
    assert!(
        drive(&runtime, old.read(snapshot))
            .unwrap_err()
            .is_scope_ended()
    );
    assert_eq!(drive(&runtime, old_reader.next()), None);
    assert!(
        drive(&runtime, output.send(EntryAction::Tick))
            .unwrap_err()
            .is_cancelled()
    );
    let fresh = scope(&runtime, &store, selected_id);
    let replay = drive(
        &runtime,
        fresh.send(presented(DataAction::Replay(old_dismiss))),
    )
    .unwrap();
    drive(&runtime, replay.finish());
    assert_eq!(drive(&runtime, fresh.read(snapshot)).unwrap(), (Some(7), 0));
    let _ = drive(&runtime, prefix_callback()).unwrap();
    assert_eq!(drive(&runtime, prefix_reader.next()), Some(1));
    runtime.advance(Duration::from_secs(1)).unwrap();
    assert!(prefix_timer.is_finished());
    assert_eq!(drive(&runtime, prefix_reader.next()), Some(2));
    assert_eq!(drive(&runtime, fresh.read(snapshot)).unwrap(), (Some(7), 0));
    let fresh_dismiss = capture(&runtime, &fresh);
    let close = drive(
        &runtime,
        fresh.send(presented(DataAction::Replay(fresh_dismiss))),
    )
    .unwrap();
    drive(&runtime, close.finish());
    assert_eq!(drive(&runtime, fresh.read(snapshot)).unwrap(), (None, 0));
    let close = drive(
        &runtime,
        prefix.send(presented(DataAction::Replay(prefix_dismiss))),
    )
    .unwrap();
    drive(&runtime, close.finish());
    assert_eq!(drive(&runtime, prefix.read(snapshot)).unwrap(), (None, 2));
    assert_eq!(drive(&runtime, store.read(StackState::len)).unwrap(), 2);
    drive(&runtime, store.shutdown()).unwrap();
}

// Assigning the whole parent effect to ephemeral data kills the surviving timer;
// stripping its real enclosing stack scope leaves the removed timer alive.
#[test]
fn local_clear_keeps_parent_work_until_its_enclosing_stack_entry_ends() {
    let runtime = DeterministicRuntime::new();
    let mut state = StackState::new();
    let survivor_id = state.push(Entry::initial());
    let removed_id = state.push(Entry::initial());
    let store = Store::new(state, reducer(), runtime.runtime());
    let survivor = scope(&runtime, &store, survivor_id);
    let removed = scope(&runtime, &store, removed_id.clone());
    let surviving_work = drive(
        &runtime,
        survivor.send(presented(DataAction::CloseAndStartParent)),
    )
    .unwrap();
    let removed_work = drive(
        &runtime,
        removed.send(presented(DataAction::CloseAndStartParent)),
    )
    .unwrap();
    runtime.run_until_stalled().unwrap();
    assert_eq!(drive(&runtime, survivor.read(snapshot)).unwrap(), (None, 0));
    assert_eq!(drive(&runtime, removed.read(snapshot)).unwrap(), (None, 0));
    assert!(!surviving_work.is_finished());
    assert!(!removed_work.is_finished());
    let _ = drive(
        &runtime,
        store.send(Action::Path(StackAction::PopFrom { id: removed_id })),
    )
    .unwrap();
    runtime.run_until_stalled().unwrap();
    assert!(removed_work.is_finished());
    assert!(!surviving_work.is_finished());
    assert!(
        drive(&runtime, removed.read(snapshot))
            .unwrap_err()
            .is_scope_ended()
    );
    runtime.advance(Duration::from_secs(1)).unwrap();
    assert!(surviving_work.is_finished());
    assert_eq!(drive(&runtime, survivor.read(snapshot)).unwrap(), (None, 1));
    assert_eq!(drive(&runtime, store.read(StackState::len)).unwrap(), 1);
    drive(&runtime, store.shutdown()).unwrap();
}
