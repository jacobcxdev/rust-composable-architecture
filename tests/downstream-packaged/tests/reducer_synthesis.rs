//! Checks packaged reducer synthesis with persistent children and move-only payloads.

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use rca::{Effect, ReduceContext, Reducer, Store, TokioRuntime};

/// Provides an owned payload whose heap address is observable.
struct Amount(usize);

/// Separates the owned input from each child-generated output.
enum ChildAction {
    /// Adds a move-only payload to child state.
    Add(Box<Amount>),
    /// Records the persistent reducer's addition count.
    Output(usize),
}

/// Records independent value, ordering and ownership witnesses.
struct ChildState {
    /// Accumulates input amounts.
    value: usize,
    /// Preserves the order of child-generated outputs.
    outputs: Vec<usize>,
    /// Records addresses without dereferencing them after input release.
    input_addresses: Vec<usize>,
}

/// Counts construction externally and retains its own addition count.
struct Child {
    /// Counts both input and output reductions.
    calls: Arc<AtomicUsize>,
    /// Proves the reducer instance survives between sends.
    additions: usize,
}

impl Child {
    /// Records construction before returning a fresh child reducer.
    fn new(constructions: &AtomicUsize, calls: Arc<AtomicUsize>) -> Self {
        constructions.fetch_add(1, Ordering::SeqCst);
        Self {
            calls,
            additions: 0,
        }
    }
}

impl Reducer for Child {
    type State = ChildState;
    type Action = ChildAction;

    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &Self::Action,
        _: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        match action {
            ChildAction::Add(amount) => {
                state
                    .input_addresses
                    .push(core::ptr::from_ref(amount.as_ref()) as usize);
                state.value += amount.0;
                self.additions += 1;
                Effect::send(ChildAction::Output(self.additions))
            }
            ChildAction::Output(value) => {
                state.outputs.push(*value);
                Effect::none()
            }
        }
    }
}

/// Delegates to one persistent child through generated state and action paths.
#[rca::reducer(
    state(generated(struct State {
        #[reducer(child = child)] child: _,
        untouched: usize,
    })),
    action(generated(enum Action {
        #[reducer(child = child)] Child(_),
    })),
    body(generated),
    child(child, reducer = Child, kind = required,
        state = StatePaths::child(),
        action = ActionPaths::child(),
        bind(fn bind_child(child: Child)),
    ),
)]
struct Feature<C> {
    /// Holds the generated binding without reconstructing the child per action.
    #[reducer(child = child)]
    child: C,
}

#[tokio::test(flavor = "current_thread")]
async fn packaged_reducer_synthesis_preserves_child_and_owned_payloads() {
    let constructions = AtomicUsize::new(0);
    let calls = Arc::new(AtomicUsize::new(0));
    let feature = Feature {
        child: bind_child(Child::new(&constructions, Arc::clone(&calls))),
    };
    let state = State {
        child: ChildState {
            value: 3,
            outputs: Vec::new(),
            input_addresses: Vec::new(),
        },
        untouched: 17,
    };
    assert!(core::ptr::eq(StatePaths::child().get(&state), &state.child));
    assert_eq!(StatePaths::child().get(&state).value, 3);
    assert_eq!(constructions.load(Ordering::SeqCst), 1);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    let store = Store::new(state, feature, TokioRuntime::current().unwrap());
    let mut original_addresses = Vec::new();

    for (amount, expected_value, expected_calls) in [(4, 7, 2), (5, 12, 4)] {
        let payload = Box::new(Amount(amount));
        // Heap identity survives the owned actor send; capture it before embedding.
        original_addresses.push(core::ptr::from_ref(payload.as_ref()) as usize);
        let action = ActionPaths::child().embed(ChildAction::Add(payload));
        let Action::Child(original_child_action) = &action;
        let projected = ActionPaths::child().extract(&action).unwrap();
        assert!(core::ptr::eq(projected, original_child_action));
        store.send(action).await.unwrap().finish().await;
        assert_eq!(
            store.read(|state| state.child.value).await.unwrap(),
            expected_value
        );
        assert_eq!(calls.load(Ordering::SeqCst), expected_calls);
        assert_eq!(constructions.load(Ordering::SeqCst), 1);
    }

    let state = store.shutdown_into_state().await.unwrap();
    assert_eq!(state.child.value, 12);
    assert_eq!(state.untouched, 17);
    assert_eq!(state.child.outputs, [1, 2]);
    assert_eq!(state.child.input_addresses, original_addresses);
    assert_eq!(state.child.input_addresses.len(), 2);
    assert_eq!(constructions.load(Ordering::SeqCst), 1);
    assert_eq!(calls.load(Ordering::SeqCst), 4);
}
