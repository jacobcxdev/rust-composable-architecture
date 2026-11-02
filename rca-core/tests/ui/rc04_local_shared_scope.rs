use std::{cell::Cell, marker::PhantomData, rc::Rc, sync::atomic::AtomicUsize};

use rca_core::{
    ActionPath, Effect, ReduceContext, Reducer, SharedReducer, SharedScope, scope_shared,
};

struct Child<S, A>(PhantomData<fn(S, A)>);

impl<S, A> SharedReducer for Child<S, A> {
    type State = S;
    type Shared = S;
    type Action = A;

    fn reduce(&mut self, _: &mut S, _: &mut S, _: &A, _: &mut ReduceContext<'_>) -> Effect<A> {
        Effect::none()
    }
}

// Neither a Send nor a lifetime bound is available for state; actions only have
// the retained 'static bound. Reintroducing either payload bound breaks this.
fn domains<S, A: 'static>(state: &mut (S, S), action: &A, context: &mut ReduceContext<'_>) {
    let mut constructor = SharedScope::new(
        |state: &mut (S, S)| (&mut state.0, &mut state.1),
        ActionPath::new(|action: &A| Some(action), |action| action),
        Child::<S, A>(PhantomData),
    );
    let _ = Reducer::reduce(&mut constructor, state, action, context);
    let mut free = scope_shared(
        |state: &mut (S, S)| (&mut state.0, &mut state.1),
        ActionPath::new(|action: &A| Some(action), |action| action),
        Child::<S, A>(PhantomData),
    );
    let _ = Reducer::reduce(&mut free, state, action, context);
}

// A borrowed AtomicUsize is Send: this independently isolates callback 'static.
fn borrowed_callbacks(capture: &AtomicUsize, context: &mut ReduceContext<'_>) {
    let mut reducer = scope_shared(
        |state: &mut ((), ())| {
            let _ = capture.load(std::sync::atomic::Ordering::Relaxed);
            (&mut state.0, &mut state.1)
        },
        ActionPath::new(
            |action: &()| {
                let _ = capture.load(std::sync::atomic::Ordering::Relaxed);
                Some(action)
            },
            |action| action,
        ),
        Child::<(), ()>(PhantomData),
    );
    let _ = Reducer::reduce(&mut reducer, &mut ((), ()), &(), context);
}

// Owned Rc captures are 'static: this independently isolates callback Send.
fn owned_callbacks(context: &mut ReduceContext<'_>) {
    let split_capture = Rc::new(Cell::new(0));
    let extract_capture = Rc::clone(&split_capture);
    let mut reducer = SharedScope::new(
        move |state: &mut ((), ())| {
            split_capture.set(split_capture.get() + 1);
            (&mut state.0, &mut state.1)
        },
        ActionPath::new(
            move |action: &()| {
                extract_capture.set(extract_capture.get() + 1);
                Some(action)
            },
            |action| action,
        ),
        Child::<(), ()>(PhantomData),
    );
    let _ = Reducer::reduce(&mut reducer, &mut ((), ()), &(), context);
}

fn concrete_local(context: &mut ReduceContext<'_>) {
    let value = 0;
    domains(&mut (&value, &value), &(), context);
    let cell = Cell::new(0);
    let local = Rc::new(());
    domains(&mut (&cell, &cell), &Rc::clone(&local), context);
    domains(&mut (Rc::clone(&local), local), &(), context);
}

fn main() {
    // Domain markers alone must not suppress the adapter's structural auto traits.
    fn send_sync<T: Send + Sync>(_: &T) {}
    let reducer = scope_shared(
        |state: &mut (Rc<()>, Rc<()>)| (&mut state.0, &mut state.1),
        ActionPath::new(|action: &Rc<()>| Some(action), |action| action),
        Child::<Rc<()>, Rc<()>>(PhantomData),
    );
    send_sync(&reducer);
    let _ = (borrowed_callbacks, owned_callbacks, concrete_local);
}
