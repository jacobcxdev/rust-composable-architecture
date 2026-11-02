//! Checks borrowed local domains and separates reduction order from effect completion order.
//!
//! The `d54-local-runtime` consumer uses channels, not delays, to control completion order.

#![cfg(test)]

#[path = "../../../../rca-core/tests/support/thread_runtime.rs"]
mod thread_runtime;

use rca_core::{Effect, ReduceContext, Reducer, Store};
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};
use thread_runtime::ThreadRuntime;

struct LocalState<'a> {
    value: &'a Cell<usize>,
    trace: &'a RefCell<Vec<(&'static str, usize, usize)>>,
}
struct LocalAction<'a> {
    amount: &'a usize,
    token: Rc<()>,
}
struct First<'a>(&'a Cell<usize>);
struct Second<'a>(&'a Cell<usize>);

impl<'a> Reducer for First<'a> {
    type State = LocalState<'a>;
    type Action = LocalAction<'a>;
    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &Self::Action,
        _: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        self.0.set(self.0.get() + 1);
        state.trace.borrow_mut().push((
            "first",
            state.value.get(),
            core::ptr::from_ref(action) as usize,
        ));
        state.value.set(state.value.get() + *action.amount);
        assert_eq!(Rc::strong_count(&action.token), 1);
        Effect::none()
    }
}

impl<'a> Reducer for Second<'a> {
    type State = LocalState<'a>;
    type Action = LocalAction<'a>;
    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &Self::Action,
        _: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        self.0.set(self.0.get() + 1);
        state.trace.borrow_mut().push((
            "second",
            state.value.get(),
            core::ptr::from_ref(action) as usize,
        ));
        assert_eq!(Rc::strong_count(&action.token), 1);
        Effect::none()
    }
}

#[rca_macros::reducer(
    state(supplied(LocalState<'a>)), action(supplied(LocalAction<'a>)), body(generated),
)]
struct LocalFeature<'a, A, B> {
    #[reducer(reduce)]
    first: A,
    #[reducer(data)]
    constructions: &'a Cell<usize>,
    #[reducer(reduce)]
    second: B,
}

#[rca_macros::reducer(
    state(supplied(LocalState<'a>)), action(supplied(LocalAction<'a>)), body(generated),
)]
struct LocalNoop<'a> {
    #[reducer(data)]
    marker: core::marker::PhantomData<&'a ()>,
}

/// Runs borrowed local reducers inside one shared Store reduction.
struct Bridge;
impl Reducer for Bridge {
    type State = Vec<(&'static str, usize)>;
    type Action = usize;
    fn reduce(
        &mut self,
        observed: &mut Self::State,
        amount: &usize,
        context: &mut ReduceContext<'_>,
    ) -> Effect<usize> {
        let constructions = Cell::new(0);
        let value = Cell::new(2);
        let trace = RefCell::new(Vec::new());
        let first_calls = Cell::new(0);
        let second_calls = Cell::new(0);
        constructions.set(constructions.get() + 1);
        let mut feature = LocalFeature {
            first: First(&first_calls),
            constructions: &constructions,
            second: Second(&second_calls),
        };
        let mut state = LocalState {
            value: &value,
            trace: &trace,
        };
        let action = LocalAction {
            amount,
            token: Rc::new(()),
        };
        let address = core::ptr::from_ref(&action) as usize;
        let mut noop = LocalNoop {
            marker: core::marker::PhantomData,
        };
        drop(noop.reduce(&mut state, &action, context));
        assert_eq!(value.get(), 2);
        assert!(trace.borrow().is_empty());
        for _ in 0..2 {
            drop(feature.reduce(&mut state, &action, context));
        }
        assert_eq!(feature.constructions.get(), 1);
        assert_eq!((first_calls.get(), second_calls.get()), (2, 2));
        assert_eq!(value.get(), 8);
        assert!(trace.borrow().iter().all(|entry| entry.2 == address));
        observed.extend(
            trace
                .borrow()
                .iter()
                .map(|(name, value, _)| (*name, *value)),
        );
        Effect::none()
    }
}

#[test]
fn generated_composition_borrows_local_non_send_domains_in_declaration_order() {
    let runtime = ThreadRuntime::new();
    let store = Store::new(Vec::new(), Bridge, runtime.runtime());
    futures_executor::block_on(async {
        store.send(3).await.unwrap().finish().await;
        assert_eq!(
            store.read(Clone::clone).await.unwrap(),
            [("first", 2), ("second", 5), ("first", 5), ("second", 8)]
        );
        store.shutdown().await.unwrap();
    });
}

enum Action {
    Start,
    Output(u8),
}
struct Emit {
    value: u8,
    constructed: std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
}
impl Reducer for Emit {
    type State = Vec<u8>;
    type Action = Action;
    fn reduce(
        &mut self,
        state: &mut Vec<u8>,
        action: &Action,
        _: &mut ReduceContext<'_>,
    ) -> Effect<Action> {
        match action {
            Action::Start => {
                self.constructed.lock().unwrap().push(self.value);
                Effect::send(Action::Output(self.value))
            }
            Action::Output(value) if self.value == 1 => {
                state.push(*value);
                Effect::none()
            }
            Action::Output(_) => Effect::none(),
        }
    }
}

#[rca_macros::reducer(
    state(supplied(Vec<u8>)), action(supplied(Action)), body(generated),
)]
struct Ordered {
    #[reducer(reduce)]
    first: Emit,
    #[reducer(reduce)]
    second: Emit,
}

#[test]
fn generated_effects_preserve_construction_order() {
    let runtime = ThreadRuntime::new();
    let constructed = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let store = Store::new(
        Vec::new(),
        Ordered {
            first: Emit {
                value: 1,
                constructed: std::sync::Arc::clone(&constructed),
            },
            second: Emit {
                value: 2,
                constructed: std::sync::Arc::clone(&constructed),
            },
        },
        runtime.runtime(),
    );
    futures_executor::block_on(async {
        store.send(Action::Start).await.unwrap().finish().await;
        assert_eq!(*constructed.lock().unwrap(), [1, 2]);
        let mut outputs = store.read(Clone::clone).await.unwrap();
        outputs.sort_unstable();
        assert_eq!(outputs, [1, 2]);
        store.shutdown().await.unwrap();
    });
}

/// Holds each effect until the test releases its individual channel.
struct Gated {
    value: u8,
    gate: Option<futures_channel::oneshot::Receiver<()>>,
    started: std::sync::mpsc::Sender<u8>,
}

impl Reducer for Gated {
    type State = Vec<u8>;
    type Action = Action;
    fn reduce(
        &mut self,
        state: &mut Vec<u8>,
        action: &Action,
        _: &mut ReduceContext<'_>,
    ) -> Effect<Action> {
        match action {
            Action::Start => {
                let gate = self.gate.take().expect("starts once");
                let started = self.started.clone();
                let value = self.value;
                Effect::run(move |sender| async move {
                    started.send(value).unwrap();
                    gate.await.unwrap();
                    sender.send(Action::Output(value)).await.unwrap();
                })
            }
            Action::Output(value) if self.value == 1 => {
                state.push(*value);
                Effect::none()
            }
            Action::Output(_) => Effect::none(),
        }
    }
}

#[rca_macros::reducer(
    state(supplied(Vec<u8>)), action(supplied(Action)), body(generated),
)]
struct Concurrent {
    #[reducer(reduce)]
    first: Gated,
    #[reducer(reduce)]
    second: Gated,
}

#[test]
fn generated_construction_order_does_not_claim_async_completion_order() {
    let runtime = ThreadRuntime::new();
    let (first_send, first_receive) = futures_channel::oneshot::channel();
    let (second_send, second_receive) = futures_channel::oneshot::channel();
    let (started_send, started_receive) = std::sync::mpsc::channel();
    let store = Store::new(
        Vec::new(),
        Concurrent {
            first: Gated {
                value: 1,
                gate: Some(first_receive),
                started: started_send.clone(),
            },
            second: Gated {
                value: 2,
                gate: Some(second_receive),
                started: started_send,
            },
        },
        runtime.runtime(),
    );
    futures_executor::block_on(async {
        let mut values = store.observe_distinct(Clone::clone).await.unwrap();
        assert_eq!(values.next().await, Some(Vec::new()));
        let work = store.send(Action::Start).await.unwrap();
        let mut started = [
            started_receive
                .recv_timeout(std::time::Duration::from_secs(5))
                .unwrap(),
            started_receive
                .recv_timeout(std::time::Duration::from_secs(5))
                .unwrap(),
        ];
        started.sort();
        assert_eq!(started, [1, 2]);
        second_send.send(()).unwrap();
        assert_eq!(values.next().await, Some(vec![2]));
        first_send.send(()).unwrap();
        work.finish().await;
        assert_eq!(store.read(Clone::clone).await.unwrap(), [2, 1]);
        store.shutdown().await.unwrap();
    });
}
