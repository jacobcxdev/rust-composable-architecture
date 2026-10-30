# Debounce and first/latest throttle

These recipes use existing `rca-core` effects, reducer state and injected clocks.
They need no process-global scheduler, new dependency or standalone timer
combinator. The executable recipe and consumer witnesses live in
`rca-core/tests/debounce_throttle.rs`.

## Choose an input policy first

Debounce waits for a quiet period before starting the latest request. Continuous
input can postpone that start indefinitely. Use it for search text or another
input where only the latest settled value matters.

Throttle keeps a fixed window instead of restarting it for each input. Use it
when work must continue during a sustained stream. This guide retains one pending
value and emits it at the window boundary. `First` retains the first in-window
value. `Latest` replaces it with each new value.

These policies discard or replace input deliberately. Neither implements a
lossless queue or bounds concurrent client operations. Use explicit job capacity
when that bound matters. Cancelling obsolete work cannot undo an external side effect.

### Why the two throttle recipes differ

The [runtime guide's leading/latest recipe](../runtime.md#leading-and-latest-action-throttling)
defines a different policy. Its `Leading` mode drops in-window input. Its `Latest`
mode lets a new input at an elapsed boundary supersede the old pending value.
This guide instead flushes the due pending value before processing that new input.

For inputs `1@0`, `2@2` and `3@12` with a ten-second window, this guide emits
`1@0, 2@12` and retains `3` for 22. If the new input commits before the old
boundary in the other recipe, it publishes `3` instead of the older pending `2`.
Choose the rule your domain needs. The names alone do not define deadline order.

## Debounce: delay client start

Construct client work lazily and put cancellation outside the delay:

```rust
use rca_core::Effect;
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

Each input replaces the previous waiting request with the same typed identity.
The quiet interval delays **start**, not merely delivery of an already-started
client. `Effect::cancel(RequestId::Search)` retires the wait and execution.
`delay` uses the Store runtime clock; inject that clock through the runtime in a
controlled test. Keep client construction inside the lazy effect. Putting the
ID inside `delay` registers it only after the wait and does not debounce waiting
requests.

## Throttle: explicit first/latest fixed windows

Use a typed mode, `First` or `Latest`, and feature-owned state:

```rust
use rca_core::MonotonicInstant;

enum Mode { First, Latest }
struct Window {
    deadline: MonotonicInstant,
    nonce: u64,
    pending: Option<u8>,
}
```

The reducer keeps `Option<Window>` and a checked monotonically increasing nonce.
The example uses a ten-second interval and an explicitly injected `Clock`
dependency. The interval is fixed; later inputs do not move its deadline.

1. With no window, emit the input immediately and open a window at observed
   `clock.now() + interval`.
2. Inside the window, `First` retains the first pending input. `Latest` replaces
   it with the latest input.
3. Before processing any input at or after the deadline, flush the due pending
   value once and retire the old window/nonce. If a pending value was flushed,
   open the next window at **observed now**, then process the incoming input
   inside that new window. If there was no pending value, the incoming input
   emits immediately and opens a new window.
4. A timer action carries its nonce. Accept it only when that nonce is current
   and its deadline is due. Flush by the same rule; retired or early timer
   actions do nothing.
5. Explicit cancellation clears pending state and cancels the typed timer ID.
   Structural removal drops the child-owned state and cancels its effects.

The timer is ordinary lazy work: `Effect::run` awaits the injected clock's
`sleep_until(deadline)` and sends `Tick(nonce)`. Replace it using
`.without_inherited_cancellation().cancellable_in_flight(TimerId::Window)`.
The boundary transfers inherited cancellation when a timer-fired `Tick` opens
its successor. It preserves explicit local cancellation, the root task and
structural ownership. Do not use that boundary to detach work from its feature.

The recipe emits into feature state during reduction. Client-backed output can
instead return the corresponding effect, keeping the ordering and ownership
explicit. Payload ownership remains a domain choice; this recipe does not add
framework-wide `Clone`, `Debug` or `Send` bounds.

### Hand-checked boundary examples

With a ten-second interval, inputs `1@0`, `2@2`, `3@7` emit `1@0, 2@10` in
first mode and `1@0, 3@10` in latest mode. A later empty-window timer emits
nothing.

After `1@0, 2@2`, observe time 12 and admit input `3`. Both timer-before-input
and input-before-timer commit order emit `1@0, 2@12`; `3` becomes pending until
22. Replaying the retired timer nonce does not emit again. The next deadline is
22, not 20: the next interval starts at observed time 12.

After only `1@0`, input `2@12` emits immediately, whether the empty-window timer
has committed first or not. A further input becomes pending for 22.

## Two different fences

The **logical timer nonce** protects once-only processing within one live
feature. It is not child lifetime authority. A newly inserted child can begin
with nonce 1 again.

The framework's **private structural generation** protects historical effect
senders and scoped handles after optional removal, case exit or keyed removal
and same-ID reinsertion. The tests deliberately reuse logical nonce 1 and retain
an old timer sender: it cannot deliver into the new child. Root external input
remains fresh authority; do not use the logical nonce to infer historical
ownership of an externally supplied action.

Cancellation IDs are scoped to the child. An equal ID in an optional, case or
keyed sibling does not cancel the other sibling's debounce request. Use actual
Store/TestStore lifecycle transitions for this proof, not isolated calls to a
composition reducer.

## Controlled verification and limits

Run the focused executable qualification with:

```sh
rustup run 1.98.0 cargo test --locked -p rca-core --test debounce_throttle
```

The fixture supplies a deterministic runtime and clock, no wall-clock sleeps or
ambient IO. Debounce assertions separately observe client starts and emitted
results. Throttle assertions cover fixed first/latest bursts, both deadline/input
orderings, elapsed windows without pending work, cancellation, retired timer
nonces, independently injected clock origins, and actual optional/case/keyed
removal/reinsertion. Advancing the runtime clock cannot complete a throttle
mistakenly supplied with a different clock; advancing that injected clock can.

This is an explicit Rust reducer/clock ordering contract, not a promise of
native process-global scheduler parity. Root, optional, case and keyed
consumers are covered; future stack consumers remain separate work. The
standalone D23 timer is not a dependency of either recipe. These witnesses do
not certify overall release readiness or comparative performance.
