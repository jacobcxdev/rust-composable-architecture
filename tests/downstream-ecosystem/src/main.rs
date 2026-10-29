//! Checks registry-only clients, virtual clocks, shared state and scoped lifecycle APIs.

use std::num::NonZeroUsize;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use architecture::diagnostics::Change;
use architecture::{
    ActionPath, CaptureKey, DependencyClient, DependencyKey, DeterministicRuntime, Effect,
    Exhaustivity, Issue, PredicateWaitOutcome, PresentationAction, ReduceContext, Reducer,
    ReducerExt, Shared, SharedAccessError, SharedReducer, StackAction, StackState, StatePath,
    Store, TestStore, ephemeral, scope_shared, stack,
};
use futures_util::StreamExt;
use testing::{controlled_future, controlled_gate, controlled_stream};

/// Erases one owned client response while retaining its Send contract.
type Response = Pin<Box<dyn Future<Output = u8> + Send>>;

/// Combines a required asynchronous endpoint with a generated test default.
#[derive(Clone, DependencyClient)]
struct Client {
    /// Produces the caller-controlled response future.
    load: Arc<dyn Fn() -> Response + Send + Sync>,
    /// Supplies the generated scalar endpoint default.
    #[dependency(test = Arc::new(|| 5))]
    count: Arc<dyn Fn() -> u8 + Send + Sync>,
}

/// Requires explicit live configuration and permits generated test defaults.
#[derive(DependencyKey)]
#[dependency(value = Client, live = panic!("explicit client required"), test = Client::test_value())]
struct ClientKey;

/// Deliberately omits Debug to exercise typed diagnostics and custom rendering.
enum Action {
    /// Reports an issue and begins the controlled client request.
    Load,
    /// Commits the received client value.
    Loaded(u8),
}

/// Exercises issue reporting and dependency-backed asynchronous work.
struct Feature;
impl Reducer for Feature {
    type State = Vec<u8>;
    type Action = Action;
    fn reduce(
        &mut self,
        state: &mut Vec<u8>,
        action: &Action,
        context: &mut ReduceContext<'_>,
    ) -> Effect<Action> {
        match action {
            Action::Load => {
                context
                    .report_issue(Issue::new("request", "explicit client request"))
                    .unwrap();
                let client = context.dependency::<ClientKey>().clone();
                assert_eq!((client.count)(), 5);
                Effect::future(async move { Some(Action::Loaded((client.load)().await)) })
            }
            Action::Loaded(value) => {
                state.push(*value);
                Effect::none()
            }
        }
    }
}

/// Requires clock advancement and output commitment before task completion.
async fn client_clock_and_diagnostics() {
    let (completion, response) = controlled_future();
    let response = Arc::new(Mutex::new(Some(response)));
    let traces = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&traces);
    let reducer =
        Feature.trace_changes(Clone::clone, move |change: Change<'_, Action, Vec<u8>>| {
            let rendered = change.format_with(
                |_| "[action]".into(),
                |state| format!("{} values", state.len()),
            );
            sink.lock().unwrap().push((
                change.before().clone(),
                change.after().clone(),
                change.is_changed(),
                rendered,
            ));
        });
    let mut store = TestStore::builder(Vec::new(), reducer)
        .with_clock_dependency::<ClientKey>(move |clock| Client {
            load: Arc::new(move || {
                let response = response.lock().unwrap().take().unwrap();
                let wait = clock.sleep(Duration::from_millis(2));
                Box::pin(async move {
                    wait.await;
                    response.await.unwrap()
                })
            }),
            ..Client::test_value()
        })
        .build();
    let started = store.now();
    let task = store.send(Action::Load).await.unwrap();
    store
        .expect_issue(|issue| issue.code == "request")
        .await
        .unwrap();
    completion.complete(17).unwrap();
    task.finish()
        .await
        .expect_err("client clock is not advanced");
    store.advance(Duration::from_millis(2)).await.unwrap();
    task.finish()
        .await
        .expect_err("response still needs reducer commitment");
    let _ = store
        .receive_with(
            |action| matches!(action, Action::Loaded(17)),
            |state| state.push(17),
        )
        .await
        .unwrap();
    assert_eq!(
        store.now(),
        started.saturating_add(Duration::from_millis(2))
    );
    task.finish().await.unwrap();
    store.finish().await.unwrap();

    let traces = traces.lock().unwrap();
    if cfg!(debug_assertions) {
        assert_eq!(traces.len(), 2);
        assert_eq!(
            (&traces[0].0, &traces[0].1, traces[0].2),
            (&vec![], &vec![], false)
        );
        assert_eq!(
            (&traces[1].0, &traces[1].1, traces[1].2),
            (&vec![], &vec![17], true)
        );
        assert!(traces[0].3.contains("state unchanged"));
        assert!(traces[1].3.contains("1 values"));
    } else {
        assert!(traces.is_empty());
    }
}

/// Updates local state and its borrowed shared value together.
struct Child;
impl SharedReducer for Child {
    type State = u8;
    type Shared = u8;
    type Action = u8;
    fn reduce(
        &mut self,
        state: &mut u8,
        shared: &mut u8,
        action: &u8,
        _: &mut ReduceContext<'_>,
    ) -> Effect<u8> {
        *shared += action;
        *state = *shared;
        Effect::none()
    }
}

/// Checks shared reduction and externally controlled gate and stream completion.
async fn shared_and_controls() {
    let reducer = scope_shared(
        |state: &mut (u8, u8)| (&mut state.0, &mut state.1),
        ActionPath::new(|action: &u8| Some(action), |action| action),
        Child,
    );
    let mut store = TestStore::new((0, 0), reducer);
    let _ = store.send_with(3, |state| *state = (3, 3)).await.unwrap();
    let _ = store.send_with(2, |state| *state = (5, 5)).await.unwrap();
    store.finish().await.unwrap();

    let (control, gate) = controlled_gate();
    control.release().unwrap();
    gate.await.unwrap();

    let (control, mut stream) = controlled_stream(NonZeroUsize::new(1).unwrap());
    control.try_send(23).unwrap();
    assert_eq!(stream.next().await, Some(Ok(23)));
    control.close();
    assert_eq!(stream.next().await, None);
}

/// Supplies a stable identity and value for ephemeral child state.
struct Data {
    /// Identifies the currently presented child.
    id: u8,
    /// Lets the parent observe the child before automatic removal.
    value: u8,
}

/// Separates ephemeral data from the parent's completion flag.
struct Entry {
    /// Exists until the close action removes the presented child.
    data: Option<Data>,
    /// Becomes false when the parent observes the close action.
    busy: bool,
}

impl Entry {
    /// Creates the literal initial state used by old and replacement entries.
    fn initial() -> Self {
        Self {
            data: Some(Data { id: 1, value: 7 }),
            busy: true,
        }
    }
    /// Copies the relevant values without retaining a state borrow.
    fn snapshot(&self) -> (Option<u8>, bool) {
        (self.data.as_ref().map(|data| data.value), self.busy)
    }
}
/// Requests removal of the presented data.
enum DataAction {
    /// Lets the parent observe data before the child disappears.
    Close,
}

/// Wraps the child action in its presentation lifecycle.
type EntryAction = PresentationAction<DataAction>;

/// Proves parent reduction sees the child before automatic removal.
struct EntryParent;
impl Reducer for EntryParent {
    type State = Entry;
    type Action = EntryAction;
    fn reduce(
        &mut self,
        state: &mut Entry,
        action: &EntryAction,
        _: &mut ReduceContext<'_>,
    ) -> Effect<EntryAction> {
        if matches!(action, PresentationAction::Presented(DataAction::Close)) {
            assert_eq!(state.data.as_ref().map(|data| data.value), Some(7));
            state.busy = false;
        }
        Effect::none()
    }
}
/// Carries stack insertion, removal and entry actions.
type PathAction = StackAction<(), EntryAction>;

/// Leaves entry routing and lifetime changes to stack composition.
struct PathParent;
impl Reducer for PathParent {
    type State = StackState<Entry>;
    type Action = PathAction;
    fn reduce(
        &mut self,
        _: &mut Self::State,
        _: &PathAction,
        _: &mut ReduceContext<'_>,
    ) -> Effect<PathAction> {
        Effect::none()
    }
}
/// Checks old-route rejection and fresh-route access after equal-ID replacement.
fn stack_ephemeral_waits() {
    let host = DeterministicRuntime::new();
    futures_executor::block_on(host.run(async {
        let child = ephemeral(
            EntryParent,
            StatePath::new(|entry: &Entry| &entry.data, |entry| &mut entry.data),
            ActionPath::new(|action: &EntryAction| Some(action), |action| action),
            |data: &Data| &data.id,
            |_: &Data, action: &DataAction| matches!(action, DataAction::Close),
        );
        let reducer = stack(
            PathParent,
            StatePath::new(|state: &StackState<Entry>| state, |state| state),
            ActionPath::new(|action: &PathAction| Some(action), |action| action),
            |(): &(), _: &mut ReduceContext<'_>| Entry::initial(),
            child,
        );
        let mut state = StackState::new();
        let prefix_id = state.push(Entry::initial());
        let tail_id = state.push(Entry::initial());
        let store = Store::new(state, reducer, host.runtime());
        let prefix = store
            .scope_stack(
                StatePath::new(|state: &StackState<Entry>| state, |state| state),
                ActionPath::new(|action: &PathAction| Some(action), |action| action),
                prefix_id,
            )
            .await
            .unwrap();
        let old = store
            .scope_stack(
                StatePath::new(|state: &StackState<Entry>| state, |state| state),
                ActionPath::new(|action: &PathAction| Some(action), |action| action),
                tail_id.clone(),
            )
            .await
            .unwrap();
        assert_eq!(
            prefix
                .send_and_wait_while(PresentationAction::Presented(DataAction::Close), |entry| {
                    entry.busy
                })
                .await,
            PredicateWaitOutcome::Satisfied
        );
        assert_eq!(prefix.read(Entry::snapshot).await.unwrap(), (None, false));
        assert_eq!(old.read(Entry::snapshot).await.unwrap(), (Some(7), true));
        assert_eq!(store.read(StackState::len).await.unwrap(), 2);

        store
            .send(StackAction::PopFrom {
                id: tail_id.clone(),
            })
            .await
            .unwrap()
            .finish()
            .await;
        assert!(
            old.read(Entry::snapshot)
                .await
                .unwrap_err()
                .is_scope_ended()
        );
        assert!(
            old.send(PresentationAction::Presented(DataAction::Close))
                .await
                .unwrap_err()
                .is_scope_ended()
        );
        // This wait begins after removal: it is a terminal smoke check.
        assert_eq!(
            old.wait_while(|_| true).await,
            PredicateWaitOutcome::ScopeEnded
        );

        store
            .send(StackAction::Push {
                id: tail_id.clone(),
                input: (),
            })
            .await
            .unwrap()
            .finish()
            .await;
        let fresh = store
            .scope_stack(
                StatePath::new(|state: &StackState<Entry>| state, |state| state),
                ActionPath::new(|action: &PathAction| Some(action), |action| action),
                tail_id,
            )
            .await
            .unwrap();
        assert_eq!(fresh.read(Entry::snapshot).await.unwrap(), (Some(7), true));
        assert_eq!(
            fresh
                .send_and_wait_while(PresentationAction::Presented(DataAction::Close), |entry| {
                    entry.busy
                })
                .await,
            PredicateWaitOutcome::Satisfied
        );
        assert_eq!(fresh.read(Entry::snapshot).await.unwrap(), (None, false));
        assert!(
            old.read(Entry::snapshot)
                .await
                .unwrap_err()
                .is_scope_ended()
        );
        assert_eq!(prefix.read(Entry::snapshot).await.unwrap(), (None, false));
        assert_eq!(store.read(StackState::len).await.unwrap(), 2);

        store.shutdown().await.unwrap();
    }))
    .unwrap();
}

/// Captures private state independently of the main projected snapshot.
struct PrivateCapture;
impl CaptureKey for PrivateCapture {
    type Value = u8;
    type Error = std::convert::Infallible;
}
/// Captures shared state through its fallible read boundary.
struct SharedCapture;
impl CaptureKey for SharedCapture {
    type Value = u8;
    type Error = SharedAccessError;
}
/// Holds a value omitted from the main projected snapshot.
struct CapturedState {
    /// Must still be asserted through its independent capture.
    private: u8,
}

/// Changes private and shared state in one reduction.
struct CaptureFeature(Shared<u8>);
impl Reducer for CaptureFeature {
    type State = CapturedState;
    type Action = ();
    fn reduce(
        &mut self,
        state: &mut CapturedState,
        (): &(),
        _: &mut ReduceContext<'_>,
    ) -> Effect<()> {
        state.private = 11;
        self.0
            .try_update(
                |value| {
                    *value = 7;
                    Ok::<_, std::convert::Infallible>(())
                },
                || {},
            )
            .unwrap();
        Effect::none()
    }
}
/// Creates independent captures under the requested exhaustivity policy.
fn capture_fixture(mode: Exhaustivity) -> (TestStore<CapturedState, (), ()>, Shared<u8>) {
    let shared = Shared::new(5);
    let captured = shared.clone();
    let store = TestStore::projected_builder(
        CapturedState { private: 0 },
        CaptureFeature(shared.clone()),
        |_| (),
    )
    .with_exhaustivity(mode)
    .with_capture::<PrivateCapture>(|state| Ok(state.private))
    .unwrap()
    .with_capture::<SharedCapture>(move |_| captured.read(|value| *value))
    .unwrap()
    .build();
    (store, shared)
}
/// Asserts both changes while preserving a copied pre-reduction value.
async fn independent_captures() {
    let (mut store, shared) = capture_fixture(Exhaustivity::Exhaustive);
    let history = shared.read(|value| *value).unwrap();
    let _ = store
        .send_with_captures(
            (),
            |()| {},
            |expected| {
                expected.set::<PrivateCapture>(11)?;
                expected.set::<SharedCapture>(7)
            },
        )
        .await
        .unwrap();
    assert_eq!(history, 5);
    assert_eq!(shared.read(|value| *value).unwrap(), 7);
    store.finish().await.unwrap();
}

/// Runs each independent registry consumer scenario to completion.
fn main() {
    stack_ephemeral_waits();
    futures_executor::block_on(async {
        client_clock_and_diagnostics().await;
        shared_and_controls().await;
        independent_captures().await;
    });
}

#[cfg(test)]
mod tests {
    #[test]
    fn omitted_shared_capture_keeps_debt_in_non_exhaustive_mode() {
        let (mut store, _) = super::capture_fixture(super::Exhaustivity::NonExhaustive);
        let error = futures_executor::block_on(store.send_with_captures(
            (),
            |()| {},
            |expected| expected.set::<super::PrivateCapture>(11),
        ))
        .unwrap_err();
        assert!(error.is_capture_mismatch());
        assert!(
            futures_executor::block_on(store.finish())
                .unwrap_err()
                .is_incomplete_assertion()
        );
        assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(store))).is_err());
    }

    #[test]
    fn registry_client_clock_diagnostics_and_shared_controls() {
        super::main();
    }
}
