A state container for application testing.

`TestStore` is a deterministic, single-threaded harness for testing [`Reducer`](crate::Reducer)
logic and the actions emitted by [`Effects`](crate::Effects).

## What `TestStore` does (and does not) do

- `send(action, assert)` runs the reducer with `action` and asserts the expected state mutation.
- Effects emitted during `send` are **queued**, not automatically executed. You must explicitly
  handle them with `recv`.
- `recv(action, assert)` asserts that the next queued effect action equals `action`, then runs the
  reducer with it and asserts the resulting state mutation.

This mirrors the core philosophy of effect testing: you don't want "something happened eventually";
you want "this exact action is emitted next".

### Strictness: unhandled actions fail the test

`TestStore` is intentionally strict:

- Calling `send` while there is a queued action will fail the test.
- Dropping a `TestStore` while actions remain queued will fail the test.

This prevents tests from silently ignoring effects.

## Time and scheduling

If your reducer uses scheduling APIs (e.g. [`Scheduler::after`](crate::effects::Scheduler::after),
[`Scheduler::debounce`](crate::effects::Scheduler::debounce), etc.), use
[`TestClock::advance`](crate::TestClock::advance) to deterministically move time forward and drive
scheduled work without sleeping.

If your reducer spawns tasks that complete without time (e.g. immediate futures), you can use
[`TestStore::wait`](crate::TestStore::wait) to run the local executor until it is idle. Be careful:
if you spawn an infinite stream, `wait` will never return — use a timeout.

## Example

Here is the second [`Reducer`] example being tested with a [`TestStore`].

```rust
# use composable::*;
#
#[derive(Clone, Debug, Default, PartialEq)]
struct State {
    n: usize,
}

#[derive(Debug, PartialEq)]
enum Action {
    Increment,
    Decrement,
}

use Action::*;
impl Reducer for State {
    type Action = Action;
    type Output = Self;

    // This reducer ensures the value is always an even number.
    fn reduce(&mut self, action: Action, send: impl Effects<Action>) {
        match action {
            Increment => {
                self.n += 1;
                if self.n % 2 == 1 {
                    send.action(Increment);
                }
            }
            Decrement => {
                self.n -= 1;
                if self.n % 2 == 1 {
                    send.action(Decrement);
                }
            }
        }
    }
}

let mut store = TestStore::<State>::default();

store.send(Increment, |state| state.n = 1);
// The follow-up effect action is queued, and must be asserted explicitly:
store.recv(Increment, |state| state.n = 2);

store.send(Increment, |state| state.n = 3);
store.recv(Increment, |state| state.n = 4);

store.send(Decrement, |state| state.n = 3);
store.recv(Decrement, |state| state.n = 2);

let n = store.into_inner().n;
assert_eq!(n, 2);
```
