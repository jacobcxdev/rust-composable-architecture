Effects: follow-up work triggered by reducers.

In this architecture, a `Reducer` performs two jobs:

1. Mutate state in response to an `Action`.
2. Optionally trigger follow-up `Action`s as *effects*.

Those effects are expressed through the [`Effects`](crate::effects::Effects) trait.

## Synchronous effects

The simplest effect is to immediately enqueue another action:

```rust
# use composable::*;
# #[derive(Default)]
# struct State { n: usize }
# #[derive(Clone, Debug, PartialEq)]
# enum Action { Increment, IncrementAgain }
# impl Reducer for State {
#   type Action = Action;
#   type Output = Self;
#   fn reduce(&mut self, action: Action, send: impl Effects<Action>) {
#     match action {
#       Action::Increment => { self.n += 1; send.action(Action::IncrementAgain); }
#       Action::IncrementAgain => { self.n += 1; }
#     }
#   }
# }
```

### Ordering guarantee (`Store`)

In a live [`Store`](crate::Store), internal effect actions enqueued during the handling of an
external action are drained *before* the next external action is processed. This makes "chains" of
internal effects behave like an atomic continuation.

## Asynchronous effects

Asynchronous work can be performed via:

- [`Effects::future`](crate::effects::Effects::future) — run a `Future` that returns an optional
  action.
- [`Effects::stream`](crate::effects::Effects::stream) — run a `Stream` and send each emitted
  action.
- [`Effects::task`](crate::effects::Effects::task) — like `stream`, but returns a
  [`Task`](crate::Task) handle you can cancel.

These are powered by a small local executor inside the `Store` runtime.

## Scheduling

`Effects` also implement [`Scheduler`](crate::effects::Scheduler), enabling time-based sends:

- Send after a delay (`after`).
- Send at an instant (`at`).
- Send at an interval (`every`).
- Debounce and throttle helpers.

The time primitives are implemented using a minimal "reactor" (see `scheduler.rs` and `delay.rs`).

## Testing

[`TestStore`](crate::TestStore) records effects instead of automatically draining them. This makes
tests explicit: you `send(...)` one action, then `recv(...)` the effect actions you expect.

For time-based behaviour (delays, intervals, debounce/throttle), `TestStore` implements
[`TestClock`](crate::store::testing::TestClock) and can deterministically advance simulated time.
