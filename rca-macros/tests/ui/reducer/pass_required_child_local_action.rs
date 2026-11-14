//! Exercises required-child projection with borrowed state and move-only local actions.
//!
//! The `d54-local-runtime` binary checks synchronous mapping before actor delivery.

#[path = "../../../../rca-core/tests/support/thread_runtime.rs"]
mod thread_runtime;

use rca_core::{Effect, ReduceContext, Reducer, Store};
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};
use thread_runtime::ThreadRuntime;

struct LocalAction(Rc<usize>);

struct Child<'a> {
    calls: &'a Cell<usize>,
    addresses: &'a RefCell<Vec<usize>>,
    reductions: usize,
    mapped: Arc<AtomicUsize>,
}

impl<'a> Child<'a> {
    fn new(
        constructions: &Cell<usize>,
        calls: &'a Cell<usize>,
        addresses: &'a RefCell<Vec<usize>>,
        mapped: Arc<AtomicUsize>,
    ) -> Self {
        constructions.set(constructions.get() + 1);
        Self {
            calls,
            addresses,
            reductions: 0,
            mapped,
        }
    }
}

impl<'a> Reducer for Child<'a> {
    type State = &'a Rc<Cell<usize>>;
    type Action = LocalAction;

    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &Self::Action,
        _: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        self.calls.set(self.calls.get() + 1);
        self.addresses
            .borrow_mut()
            .push(core::ptr::from_ref(action) as usize);
        assert_eq!(Rc::strong_count(&action.0), 1);
        state.set(state.get() + *action.0);
        self.reductions += 1;
        let mapped = Arc::clone(&self.mapped);
        Effect::send(10 + self.reductions).map(move |value| {
            mapped.fetch_add(1, Ordering::SeqCst);
            LocalAction(Rc::new(value))
        })
    }
}

#[rca_macros::reducer(
    state(generated(struct State<'a> {
        #[reducer(child = child)] value: _,
        untouched: usize,
    })),
    action(generated(enum Action { Child(LocalAction) })),
    body(generated),
    child(child, reducer = Child<'a>, kind = required,
        state = StatePaths::value(),
        action = ActionPaths::child(),
        bind(fn bind_child<'a>(child: Child<'a>)),
    ),
)]
struct Feature<'a, C> {
    #[reducer(child = child)]
    child: C,
    #[reducer(data)]
    marker: core::marker::PhantomData<&'a ()>,
}

/// Runs the local child while the shared Store holds a valid reduction context.
struct Bridge;

impl Reducer for Bridge {
    type State = usize;
    type Action = ();

    fn reduce(
        &mut self,
        observed: &mut usize,
        _: &(),
        context: &mut ReduceContext<'_>,
    ) -> Effect<()> {
        assert_eq!(*observed, 0);
        let constructions = Cell::new(0);
        let calls = Cell::new(0);
        let addresses = RefCell::new(Vec::new());
        let mapped = Arc::new(AtomicUsize::new(0));
        let outputs = Arc::new(Mutex::new(Vec::new()));
        let value = Rc::new(Cell::new(2));
        let child = Child::new(&constructions, &calls, &addresses, Arc::clone(&mapped));
        let mut feature = Feature {
            child: bind_child(child),
            marker: core::marker::PhantomData,
        };
        let mut state = State {
            value: &value,
            untouched: 17,
        };
        let action = Action::Child(LocalAction(Rc::new(3)));
        // Capture the original payload directly, independently of generated paths.
        let Action::Child(original) = &action;
        let original_address = core::ptr::from_ref(original) as usize;
        assert_eq!(constructions.get(), 1);
        assert_eq!(value.get(), 2);
        assert_eq!(calls.get(), 0);
        assert_eq!(mapped.load(Ordering::SeqCst), 0);
        assert!(outputs.lock().unwrap().is_empty());
        assert!(core::ptr::eq(*StatePaths::value().get(&state), &value));

        for (expected_value, expected_calls) in [(5, 1), (8, 2)] {
            let effect = feature.reduce(&mut state, &action, context);
            assert_eq!(value.get(), expected_value);
            assert_eq!(state.untouched, 17);
            assert_eq!(calls.get(), expected_calls);
            assert_eq!(constructions.get(), 1);
            assert_eq!(mapped.load(Ordering::SeqCst), expected_calls);
            assert_eq!(outputs.lock().unwrap().len(), expected_calls - 1);
            let recorded = Arc::clone(&outputs);
            drop(effect.map(move |parent| {
                let Action::Child(output) = parent;
                assert_eq!(Rc::strong_count(&output.0), 1);
                recorded.lock().unwrap().push(*output.0);
            }));
            // This runs before returning to the actor; no output was scheduled.
            assert_eq!(outputs.lock().unwrap().len(), expected_calls);
        }

        assert_eq!(*addresses.borrow(), [original_address, original_address]);
        assert_eq!(*outputs.lock().unwrap(), [11, 12]);
        assert_eq!(Rc::strong_count(&original.0), 1);
        assert_eq!(value.get(), 8);
        assert_eq!(mapped.load(Ordering::SeqCst), 2);
        *observed = 1;
        Effect::none()
    }
}

fn main() {
    let runtime = ThreadRuntime::new();
    let store = Store::new(0, Bridge, runtime.runtime());
    futures_executor::block_on(async {
        store.send(()).await.unwrap().finish().await;
        assert_eq!(store.read(|state| *state).await.unwrap(), 1);
        store.shutdown().await.unwrap();
    });
}
