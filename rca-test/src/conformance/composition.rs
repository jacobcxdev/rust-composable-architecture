//! Verifies composition through runtime-neutral Store contracts.

use rca_core::{ActionPath, Effect, ReduceContext, Reducer, StatePath, Store, combine, scope};

use super::RuntimeFactory;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
/// Records committed input and effect actions independently of reducer calls.
enum ActionCommit {
    /// The caller's owned input reached the parent.
    Start,
    /// The child's owned effect output reached the parent.
    Loaded,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
/// Records the expected child-before-parent order for both actions.
enum ReducerCall {
    /// The child receives the caller's input.
    ChildStart,
    /// The parent sees the same borrowed input after the child.
    ParentStart,
    /// The child receives its effect output.
    ChildLoaded,
    /// The parent sees that output after the child updates state.
    ParentLoaded,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
/// Captures the child state visible to the parent at each commit.
struct StateCommit {
    /// Child reductions completed before this parent call.
    child_reductions: usize,
    /// Whether the owned effect output reached the child.
    loaded: bool,
}

/// Retains ordering and allocation identities without cloning action payloads.
struct ChildState {
    /// Counts both the initial input and the returned effect action.
    reductions: usize,
    /// Marks successful handling of the returned effect action.
    loaded: bool,
    /// Address of the caller's nonempty payload, compared while it remains live.
    input_pointer: Option<usize>,
    /// Address of the effect's nonempty payload before it moves into the action.
    effect_pointer: Option<usize>,
    /// Shared trace that records child and parent calls in execution order.
    reducer_calls: Vec<ReducerCall>,
}

/// Exposes child state and independent parent commit traces to the scenario.
struct ParentState {
    /// State projected into the scoped reducer.
    child: ChildState,
    /// Confirms that both reducers saw the same input allocation.
    borrowed_shared_action: bool,
    /// Actions observed by the parent after each child reduction.
    action_commits: Vec<ActionCommit>,
    /// Child state observed at those same parent calls.
    state_commits: Vec<StateCommit>,
}

/// Carries distinct owned payload types through input and effect routing.
enum ChildAction {
    /// Caller-owned bytes borrowed by both reducers.
    Start(Vec<u8>),
    /// Child-owned bytes moved through effect mapping without cloning.
    Loaded(Box<[u8]>),
}

/// Embeds the child action without copying its payload.
enum ParentAction {
    /// Action selected by the shared scope path.
    Child(ChildAction),
}

/// Checks input identity and returns one owned output action.
struct Child;

impl Reducer for Child {
    type State = ChildState;
    type Action = ChildAction;

    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &Self::Action,
        _context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        state.reductions += 1;
        match action {
            ChildAction::Start(bytes) => {
                assert_eq!(bytes, &[1, 2, 3]);
                state.input_pointer = Some(bytes.as_ptr() as usize);
                state.reducer_calls.push(ReducerCall::ChildStart);

                let loaded: Box<[u8]> = Box::from([4_u8, 5, 6]);
                state.effect_pointer = Some(loaded.as_ptr() as usize);
                Effect::send(ChildAction::Loaded(loaded))
            }
            ChildAction::Loaded(bytes) => {
                assert_eq!(bytes.as_ref(), [4, 5, 6]);
                assert_eq!(state.effect_pointer, Some(bytes.as_ptr() as usize));
                state.loaded = true;
                state.reducer_calls.push(ReducerCall::ChildLoaded);
                Effect::none()
            }
        }
    }
}

/// Checks that child state changes precede each parent reduction.
struct Parent;

impl Reducer for Parent {
    type State = ParentState;
    type Action = ParentAction;

    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &Self::Action,
        _context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        match action {
            ParentAction::Child(ChildAction::Start(bytes)) => {
                assert_eq!(state.child.reductions, 1);
                assert_eq!(state.child.input_pointer, Some(bytes.as_ptr() as usize));
                state.borrowed_shared_action = true;
                state.child.reducer_calls.push(ReducerCall::ParentStart);
                state.action_commits.push(ActionCommit::Start);
            }
            ParentAction::Child(ChildAction::Loaded(bytes)) => {
                assert_eq!(state.child.reductions, 2);
                assert!(state.child.loaded);
                assert_eq!(state.child.effect_pointer, Some(bytes.as_ptr() as usize));
                state.child.reducer_calls.push(ReducerCall::ParentLoaded);
                state.action_commits.push(ActionCommit::Loaded);
            }
        }
        state.state_commits.push(StateCommit {
            child_reductions: state.child.reductions,
            loaded: state.child.loaded,
        });
        Effect::none()
    }
}

/// Runs the shared parent-child composition scenario.
///
/// # Panics
///
/// Panics when the adapter violates a composition or clean-shutdown contract.
pub async fn run_composition(factory: impl RuntimeFactory) {
    let child_state = StatePath::new(
        |state: &ParentState| &state.child,
        |state: &mut ParentState| &mut state.child,
    );
    let child_action = ActionPath::new(
        |action: &ParentAction| match action {
            ParentAction::Child(action) => Some(action),
        },
        ParentAction::Child,
    );
    let reducer = combine((scope(child_state, child_action, Child), Parent));
    let store = Store::new(
        ParentState {
            child: ChildState {
                reductions: 0,
                loaded: false,
                input_pointer: None,
                effect_pointer: None,
                reducer_calls: Vec::new(),
            },
            borrowed_shared_action: false,
            action_commits: Vec::new(),
            state_commits: Vec::new(),
        },
        reducer,
        factory.runtime(),
    );

    store
        .send(ParentAction::Child(ChildAction::Start(vec![1, 2, 3])))
        .await
        .expect("composition start commit")
        .finish()
        .await;

    let trace = store
        .read(|state| {
            (
                state.borrowed_shared_action,
                state.action_commits.clone(),
                state.state_commits.clone(),
                state.child.reducer_calls.clone(),
            )
        })
        .await
        .expect("composition trace read");
    assert!(trace.0);
    assert_eq!(trace.1, [ActionCommit::Start, ActionCommit::Loaded]);
    assert_eq!(
        trace.2,
        [
            StateCommit {
                child_reductions: 1,
                loaded: false,
            },
            StateCommit {
                child_reductions: 2,
                loaded: true,
            },
        ]
    );
    assert_eq!(
        trace.3,
        [
            ReducerCall::ChildStart,
            ReducerCall::ParentStart,
            ReducerCall::ChildLoaded,
            ReducerCall::ParentLoaded,
        ]
    );

    store.shutdown().await.expect("clean composition shutdown");
    assert!(store.is_closed());
}
