//! Checks supplied and generated reducer bodies through real actor execution.

#[path = "support/thread_runtime.rs"]
mod thread_runtime;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use rca_core::{Effect, ReduceContext, Reducer, Store};
use thread_runtime::ThreadRuntime;

/// Exercises a user-supplied body without generated child routing.
mod supplied {
    use super::{Arc, AtomicUsize, Effect, Ordering, ReduceContext, Reducer};

    /// Stores the accumulated value without generated construction.
    pub struct State(pub usize);

    /// Carries a borrowed increment without requiring `Clone` or `Debug`.
    pub struct Action(pub usize);

    /// Counts actual invocations separately from the accumulated value.
    pub struct Body(pub Arc<AtomicUsize>);

    impl Reducer for Body {
        type State = State;
        type Action = Action;

        fn reduce(
            &mut self,
            state: &mut Self::State,
            action: &Self::Action,
            _: &mut ReduceContext<'_>,
        ) -> Effect<Self::Action> {
            self.0.fetch_add(1, Ordering::SeqCst);
            state.0 += action.0;
            Effect::none()
        }
    }

    /// Routes each reduction through the original owned body.
    #[rca_macros::reducer(state(supplied(State)), action(supplied(Action)), body(supplied(body)))]
    pub struct Feature<R> {
        pub body: R,
    }
}

#[test]
fn supplied_body_reduces_each_action_exactly_once() {
    let runtime = ThreadRuntime::new();
    let calls = Arc::new(AtomicUsize::new(0));
    let store = Store::new(
        supplied::State(3),
        supplied::Feature {
            body: supplied::Body(Arc::clone(&calls)),
        },
        runtime.runtime(),
    );
    futures_executor::block_on(async {
        store
            .send(supplied::Action(4))
            .await
            .unwrap()
            .finish()
            .await;
        store
            .send(supplied::Action(5))
            .await
            .unwrap()
            .finish()
            .await;
        assert_eq!(store.read(|state| state.0).await.unwrap(), 12);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        store.shutdown().await.unwrap();
    });
}

/// Exercises child binding, generated paths and embedding child effects.
mod generated {
    use super::{Arc, AtomicUsize, Effect, Ordering, ReduceContext, Reducer};

    /// Separates the accumulated input count from emitted reduction numbers.
    pub struct ChildState {
        pub count: usize,
        pub outputs: Vec<usize>,
    }

    /// Moves an input or records its sequence without extra domain trait bounds.
    pub enum ChildAction {
        Add(Vec<u8>),
        Recorded(usize),
    }

    /// Retains reducer-local state across incoming actions and emitted effects.
    pub struct Counter {
        calls: Arc<AtomicUsize>,
        reductions: usize,
    }

    impl Counter {
        /// Counts construction independently from later reduction calls.
        pub fn new(constructions: &AtomicUsize, calls: Arc<AtomicUsize>) -> Self {
            constructions.fetch_add(1, Ordering::SeqCst);
            Self {
                calls,
                reductions: 0,
            }
        }
    }

    impl Reducer for Counter {
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
                ChildAction::Add(bytes) => {
                    self.reductions += 1;
                    state.count += bytes.len();
                    Effect::send(ChildAction::Recorded(self.reductions))
                }
                ChildAction::Recorded(sequence) => {
                    state.outputs.push(*sequence);
                    Effect::none()
                }
            }
        }
    }

    /// Owns its bound child rather than constructing a child on each reduction.
    #[rca_macros::reducer(
        state(generated(pub struct State {
            #[reducer(child = counter)]
            pub counter: _,
        })),
        action(generated(pub enum Action {
            #[reducer(child = counter)]
            Counter(_),
        })),
        body(generated),
        child(
            counter,
            reducer = Counter,
            kind = required,
            state = StatePaths::counter(),
            action = ActionPaths::counter(),
            bind(pub fn bind_counter(child: Counter)),
        ),
    )]
    pub struct Feature<C> {
        #[reducer(child = counter)]
        pub counter: C,
    }
}

#[test]
fn generated_binding_retains_the_child_and_embeds_its_effects() {
    let runtime = ThreadRuntime::new();
    let constructions = AtomicUsize::new(0);
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = generated::Counter::new(&constructions, Arc::clone(&calls));
    let feature = generated::Feature {
        counter: generated::bind_counter(counter),
    };
    let store = Store::new(
        generated::State {
            counter: generated::ChildState {
                count: 7,
                outputs: Vec::new(),
            },
        },
        feature,
        runtime.runtime(),
    );

    futures_executor::block_on(async {
        for bytes in [vec![2, 3], vec![4, 5, 6]] {
            store
                .send(generated::ActionPaths::counter().embed(generated::ChildAction::Add(bytes)))
                .await
                .unwrap()
                .finish()
                .await;
        }
        assert_eq!(
            store
                .read(|state| (state.counter.count, state.counter.outputs.clone()))
                .await
                .unwrap(),
            (12, vec![1, 2]),
        );
        assert_eq!(constructions.load(Ordering::SeqCst), 1);
        assert_eq!(calls.load(Ordering::SeqCst), 4);
        store.shutdown().await.unwrap();
    });
}
