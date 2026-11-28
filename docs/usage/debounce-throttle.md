# Wait for a pause or limit how often a feature publishes

A search box and a progress display need different responses to frequent input.
A search can wait until the user stops typing. A progress display needs updates even while work continues.
The recipes below use `Effect` and clock types from `rca`. They need no extra Cargo feature or global timer service.

## Choose an input policy first

- **Debounce** waits for a quiet period. Each new keystroke restarts the wait. Continuous typing can postpone the search indefinitely.
- **Throttle** limits how often a feature publishes a value. The first input opens a time interval, or *window*. Further inputs do not move its deadline.

Both policies deliberately discard some inputs. Neither limits how many external requests can run at once.
If every input must run, use a queue. If only a fixed number of requests can run together, limit that number separately.
Cancelling a request cannot undo an external change that already occurred.

## Debounce: delay client start

In a reducer, return an effect that starts the search after 300 milliseconds of silence.
An effect describes work for the Store to start after the reducer returns.
Here `42` stands in for the search result. The sender passes that result back as an action.

```rust
use rca::Effect;
use std::time::Duration;

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum RequestId { Search }

let effect = Effect::run(move |sender| async move {
    // Start the client here, not while constructing the effect.
    let result = 42;
    let _ = sender.send(result).await;
})
.delay(Duration::from_millis(300))
.cancellable_in_flight(RequestId::Search);
```

Start the request inside the `Effect::run` callback. Otherwise the delay postpones the result, not the request.
Put `.cancellable_in_flight(RequestId::Search)` **after** `.delay(...)`. The new effect can then replace the old one during its wait.
In the reverse order, the Store registers the ID only after the wait ends.
To cancel without starting another search, return `Effect::cancel(RequestId::Search)`.
The delay uses the Store runtime's clock.

## Throttle: explicit first/latest fixed windows

For a progress display, publish the first value immediately. Keep at most one further value for the end of the window.
Choose `First` to keep the first value that arrives inside the window. Choose `Latest` to replace it with each new input.
These are application choices, not built-in RCA modes.

```rust
use rca::MonotonicInstant;

enum Mode { First, Latest }
struct Window {
    deadline: MonotonicInstant,
    nonce: u64,
    pending: Option<u8>,
}
```

The reducer keeps an `Option<Window>`: either there is an open interval or there is none.
The deadline uses a clock that cannot move backwards.
Each new window gets a number (`nonce`). The example checks for overflow so that it never reuses a number within one feature.
Its timer includes that number in a `Tick(nonce)` action.

| What happens? | What does the reducer do? |
| --- | --- |
| An input arrives with no window. | Publish it now. Open a window with its deadline one interval from now. |
| An input arrives before the deadline. | Keep the first or latest pending value, according to the chosen mode. |
| An input arrives at or after the deadline. | Close the old window first. If a value was pending, publish it and open a new window from now. Then process the incoming value. |
| A timer action arrives. | If its number matches the open window and its deadline has arrived, close that window. Ignore any other tick. |
| The user cancels. | Clear the pending value and cancel the timer. Removing the feature also removes its child-owned state and cancels its effects. |

Closing an empty window publishes nothing and does not open another window.
The implementation writes each published value into feature state. A feature can instead return an effect to deliver it to a client.
The application chooses who owns the value. The recipe needs no extra `Clone`, `Debug` or `Send` requirements for its payload.

The timer effect waits on `clock.sleep_until(deadline)`, then sends `Tick(nonce)`.
A tick can start the next timer while the Store is still processing the previous timer's work.
Use `.without_inherited_cancellation().cancellable_in_flight(TimerId::Window)` for the new timer effect.
This lets the new timer replace the old one without inheriting the old timer's cancellation.
Its own cancellation ID, root task cancellation, and child removal still apply.

### What happens if a timer runs late?

Suppose the interval lasts ten seconds. Input values 1, 2, and 3 arrive at 0, 2, and 7 seconds.
Both modes publish 1 immediately. At ten seconds, `First` publishes 2 and `Latest` publishes 3.

Now suppose 1 and 2 arrive as before, but neither the timer nor input 3 runs until twelve seconds.
Both modes publish the pending 2 at twelve seconds and keep 3 for twenty-two seconds.
The result is the same regardless of whether the Store processes the tick or input 3 first.
Without a pending 2, the reducer publishes 3 immediately. An old tick cannot publish it again.

The [leading/latest recipe](runtime.md#leading-and-latest-action-throttling) makes a different choice.
Its leading mode drops inputs within the window. In its latest mode, an input at the deadline can replace an older pending value.
If input 3 runs before the late tick, that recipe publishes 3 rather than 2.
Choose the behaviour your application needs.

## Why a timer and a child need different identities

A window number tells one live feature which tick to accept. It cannot identify the feature itself.
For example, removing a search row and inserting another with the same key can produce two timers numbered 1.
The Store tracks each installation of the row separately. A sender or scoped handle from the old installation cannot send a tick to the new one.
An action sent through the root Store is a new request, not output from that old sender.

A cancellation ID belongs to its child. Two sibling features can both use `RequestId::Search` without cancelling each other's searches.
To test this, remove the child through a Store or TestStore. Calling its reducer in isolation does not exercise the child's lifetime.

## Test the timing without waiting in real time

The [complete recipes and tests](../../crates/rca-core/tests/debounce_throttle.rs) use a clock that the test advances.
The debounce test observes the start of the request separately from its result.
For this throttle recipe, supply the same clock for the deadline and the timer. Advancing an unrelated clock cannot wake that timer.
The tests exercise late timers, cancellations, and replacements of optional, enum-case, and keyed children.
They do not establish dispatch order on an external scheduler or cover stack entries.
