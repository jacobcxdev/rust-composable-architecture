//! Captured graphs retain `TestStore`'s projected coverage and existing snapshot bounds.

use std::cell::Cell;
use std::convert::Infallible;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Arc, Mutex};

use futures_executor::block_on;
use rca_core::diagnostics::{CaptureLimits, CapturedGraph, NodeCapture, capture_graph};
use rca_core::{ActionPath, Effect, ReduceContext, Reducer, StatePath, scope};
use rca_test::{CaptureKey, Exhaustivity, TestStore};

/// Mixes a non-`Sync` leaf, shared mutable text and a deliberately omitted field.
struct State {
    private: Cell<u32>,
    shared: Arc<Mutex<String>>,
    ignored: u32,
}

enum Action {
    Change,
    Private,
    Ignore,
    Start,
}

/// Changes captured or omitted state, either directly or through an emitted action.
struct Feature;

impl Reducer for Feature {
    type State = State;
    type Action = Action;

    fn reduce(
        &mut self,
        state: &mut State,
        action: &Action,
        _: &mut ReduceContext<'_>,
    ) -> Effect<Action> {
        match action {
            Action::Private => state.private.set(state.private.get() + 1),
            Action::Change => {
                state.private.set(state.private.get() + 1);
                state.shared.lock().unwrap().push('!');
            }
            Action::Ignore => state.ignored += 1,
            Action::Start => return Effect::send(Action::Change),
        }
        Effect::none()
    }
}

/// Copies both captured leaves into a one-node graph with no shared mutable alias.
fn capture(state: &State) -> CapturedGraph<u8, (Cell<u32>, String)> {
    capture_graph(0, CaptureLimits::new(1, 0), |_| {
        NodeCapture::new(
            (
                Cell::new(state.private.get()),
                state.shared.lock().unwrap().to_owned(),
            ),
            vec![],
        )
    })
    .unwrap()
}

/// Creates fresh storage so separate tests cannot share the hidden text.
fn state() -> State {
    State {
        private: Cell::new(0),
        shared: Arc::new(Mutex::new("secret".into())),
        ignored: 0,
    }
}

#[test]
fn projected_graph_supports_non_sync_snapshot_and_real_receive_obligations() {
    // These bounds remain required even with a custom renderer; Sync is not required.
    fn snapshot_bounds<T: Clone + std::fmt::Debug + PartialEq + Send + 'static>() {}
    fn assert_send<T: Send>(_: T) {}
    snapshot_bounds::<CapturedGraph<u8, (Cell<u32>, String)>>();
    let mut store = TestStore::projected_builder(state(), Feature, capture)
        .with_snapshot_renderer(|_| "redacted".into())
        .build();
    assert_send(store.send(Action::Ignore));
    block_on(async {
        let _task = store.send(Action::Ignore).await.unwrap(); // Omission is projected, not full coverage.
        let task = store.send(Action::Start).await.unwrap();
        assert!(
            store
                .finish()
                .await
                .unwrap_err()
                .finish_diagnostics()
                .unwrap()
                .has_pending_action()
        );
        let _received = store
            .receive_with(
                |action| matches!(action, Action::Change),
                |expected| {
                    let leaves = expected.leaves_mut(&0).unwrap();
                    leaves.0.set(1);
                    leaves.1.push('!');
                },
            )
            .await
            .unwrap();
        task.finish().await.unwrap();
        store.finish().await.unwrap();
    });
}

#[test]
fn private_inequality_survives_identical_redaction_and_keeps_assertion_pending() {
    let mut store = TestStore::projected_builder(state(), Feature, capture)
        .with_snapshot_renderer(|_| "redacted".into())
        .build();
    let panic = catch_unwind(AssertUnwindSafe(|| {
        let _task = block_on(store.send(Action::Private)).unwrap();
    }))
    .unwrap_err();
    let message = panic.downcast_ref::<String>().unwrap();
    assert!(message.contains("Snapshots differ semantically"));
    assert!(message.contains("during send: projected coverage"));
    assert!(!message.contains("secret"));
    assert!(
        block_on(store.finish())
            .unwrap_err()
            .is_incomplete_assertion()
    );
    // Pending duties are not cleared by graph capture; teardown reports them too.
    let panic = catch_unwind(AssertUnwindSafe(|| drop(store))).unwrap_err();
    assert!(
        panic
            .downcast_ref::<String>()
            .unwrap()
            .starts_with("TestStore teardown failed: TestStore incomplete assertion for send at ")
    );
}

/// Holds a child whose changes are absent from the root's unit projection.
struct ParentState {
    child: State,
}

struct ParentAction(Action);

/// Names the child's independently checked graph capture.
struct ChildGraph;

impl CaptureKey for ChildGraph {
    type Value = CapturedGraph<u8, (Cell<u32>, String)>;
    type Error = Infallible;
}

/// Composes the real child reducer with a separate declared graph expectation.
fn composed_store(mode: Exhaustivity) -> TestStore<ParentState, ParentAction, ()> {
    let reducer = scope(
        StatePath::new(
            |state: &ParentState| &state.child,
            |state: &mut ParentState| &mut state.child,
        ),
        ActionPath::new(|action: &ParentAction| Some(&action.0), ParentAction),
        Feature,
    );
    TestStore::projected_builder(ParentState { child: state() }, reducer, |_| ())
        .with_exhaustivity(mode)
        .with_capture::<ChildGraph>(|state| Ok(capture(&state.child)))
        .unwrap()
        .build()
}

// Bypassing declared graph equality for an unchanged root would hide this change.
#[test]
fn composed_graph_omission_is_not_hidden_by_equal_root_projection() {
    for mode in [Exhaustivity::Exhaustive, Exhaustivity::NonExhaustive] {
        let mut store = composed_store(mode);
        assert!(
            block_on(store.send(ParentAction(Action::Private)))
                .unwrap_err()
                .is_capture_mismatch()
        );
        assert!(
            block_on(store.finish())
                .unwrap_err()
                .is_incomplete_assertion()
        );
        let panic = catch_unwind(AssertUnwindSafe(|| drop(store))).unwrap_err();
        assert!(
            panic.downcast_ref::<String>().unwrap().starts_with(
                "TestStore teardown failed: TestStore incomplete assertion for send at "
            )
        );
    }
}

// Dropping child routing breaks this expected private-leaf update.
#[test]
fn composed_graph_expectation_finishes_in_both_modes() {
    for mode in [Exhaustivity::Exhaustive, Exhaustivity::NonExhaustive] {
        let mut store = composed_store(mode);
        let expected = capture_graph(0, CaptureLimits::new(1, 0), |_| {
            NodeCapture::new((Cell::new(1), "secret".to_owned()), vec![])
        })
        .unwrap();
        let task = block_on(store.send_with_captures(
            ParentAction(Action::Private),
            |()| {},
            move |captures| captures.set::<ChildGraph>(expected),
        ))
        .unwrap();
        block_on(task.finish()).unwrap();
        block_on(store.finish()).unwrap();
    }
}
