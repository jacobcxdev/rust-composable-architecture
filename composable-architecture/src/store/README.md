The state container for the application.

`Store` owns a reducer and provides a thread-safe entry point for sending actions into it.

At a high level:

- You create a `Store` with some initial reducer state.
- You send actions into it (`send` or `sync`).
- The store runs the reducer and any effects emitted by the reducer.
- Eventually you can shut it down and extract the final output (`into_inner`).

## Basic usage

```rust,no_run
# use composable::*;
#[derive(Default)]
struct State(usize);

#[derive(Clone, Debug)]
enum Action { Inc }

impl Reducer for State {
    type Action = Action;
    type Output = usize;

    fn reduce(&mut self, action: Action, _send: impl Effects<Action>) {
        match action {
            Action::Inc => self.0 += 1
        }
    }
}

impl From<State> for usize {
    fn from(value: State) -> Self { value.0 }
}

let store = Store::with_initial(State::default());
store.send(Action::Inc);
let out = store.into_inner();
assert_eq!(out, 1);
```

## Threading model

`Store` runs the reducer on a dedicated thread. This has a few consequences:

- `send` is non-blocking (it enqueues work).
- `sync` blocks until the *specific* action has been processed (see below).
- Reducer state must be `Send` to use `Store::with_initial`. If your reducer state is not `Send`,
  construct it inside the runtime thread using
  [`Store::with_dependencies`](crate::Store::with_dependencies).

## Effects and ordering guarantees

Reducers can emit follow-up actions via [`Effects`](crate::Effects).

In a live `Store`, internal effect actions queued while handling an external action are drained
*before* the next external action is processed. This makes internal chains of effects behave like an
atomic continuation.

## `send` vs `sync`

- [`Store::send`](crate::Store::send) — enqueues an action and returns immediately.
- [`Store::sync`](crate::Store::sync) — enqueues an action and blocks until the store has processed
  that action.

`sync` does **not** wait for asynchronous tasks to complete (futures/streams spawned via effects),
but it does wait for any *synchronous* follow-up actions emitted during that action's handling to be
drained (because those are processed before the store returns to awaiting the next external action).

## Shutting down: `into_inner`

[`Store::into_inner`](crate::Store::into_inner) stops the runtime thread and returns the reducer's
final output value.

This is a best-effort shutdown when asynchronous tasks exist: the store attempts to give pending
tasks an opportunity to run before it exits, but it does not guarantee that all background work has
completed.
