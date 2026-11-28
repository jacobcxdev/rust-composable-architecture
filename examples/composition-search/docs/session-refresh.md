# Captured clients and current services

If an operation belongs to the session that admitted it, keep its owned client.
If the operation needs the current service, read it from state. This read does not
change a client that the task already captured.

The [session-client test](../../../crates/rca-test/tests/session_client_refresh.rs) uses root
Feature inputs and task updates.
It is an integration test, not a standalone binary or an API that replaces
dependencies.

Run the test from the repository root. On macOS, use the [Cargo memory guard](../../../CONTRIBUTING.md#build-memory-safety).

```sh
cargo test --locked -p rca-test --test session_client_refresh
```

## One explicit handover

The example constructs State and Feature with client A.
It then configures the Store's dependency values with B and sets B as the current
service in state.
When the Store admits the task, the task captures B synchronously:

```rust,ignore
let client = context.reducer_context().dependency::<ClientKey>().clone();
```

The `refresh` async function takes that owned client. Its result type is
`Result<(), TaskAccessError>`:

```rust,ignore
let first = captured.call(Request::Fetch).await;
let (current, policy) = context
    .read(|state| (state.current.clone(), state.policy))
    .await?;
// `captured` is still B. `current` is the State service this read observed.
```

In the Pending case, B's first request waits for a reply. A root input selects C and
its policy before the reply arrives.
After the wait, the task reads C from state. It calls B again and calls the newly
selected C.
The endpoint log records B/fetch, B/captured, C/fresh.

In the Ready case, the replies are already available and the root selects C before
admission. The same function runs without a forced suspension.
A identifies the construction client. The test does not call A or change the arguments
used to construct the original state.
C comes from a state read, not a retroactive dependency override.

## Request identity is separate from lifetime

Two valid requests can belong to the same live owner and return in the opposite order.
Cancellation and child identity do not decide which response the application wants.
Here, the most recent edit wins.

Each valid edit records a request ID, applies its optimistic value and clears the
previous error.
Compare the ID and update state in the same serial transaction:

```rust,ignore
let response = client.call(Request::Set(request, value)).await;
context.update(move |state| {
    if state.request != request {
        return; // A stale reply is an explicit equal-state commit.
    }
    state.pending = false;
    match response {
        Ok(value) => {
            state.visible = value;
            state.confirmed = value;
        }
        Err(failure) => {
            state.visible = state.confirmed;
            state.failure = Some(failure);
        }
    }
}).await?;
```

A separate ID read is insufficient: a newer edit can arrive before the update.
Neither an older failure nor an older success changes the new value.
If the current request fails, the application restores the last confirmed value.
Invalid input starts no client call.

This rule controls local state. It does not order remote writes or guarantee durable
rollback.
If external writes must arrive in order, the application needs a backend protocol or
explicit serial execution.
The
[persistence coordination test](../../../crates/rca-test/tests/persistence_coordination.rs)
illustrates another policy: one save at a time, with later edits combined.

## Assert every committed response

`send_with` asserts each root input. `receive_update` asserts the state update for
each client completion.
A stale-ID update still counts, even if it changes no selected value.
An omitted update prevents `finish` from succeeding. An input assertion cannot
acknowledge it.
An ordered read creates no mutation to assert.

Snapshots keep independent domain values and service identities, not mutable client
handles.
The test checks endpoint calls and completion counts separately. Fresh clients get
independent backends and response controls.
Clones share the same backend. The test completes one backend while the other still
awaits a reply.

The [mounted-provider test](../../../crates/rca-core/tests/feature_provider_handover.rs)
covers client ownership, subscription release and stale access after retirement.
The [uploader](uploader.md) covers policy reads after a wait, progress, owned output
and same-ID replacement.
Neither a request ID nor a fresh service gives a retired TaskContext access to new
state.
These examples do not cover private-child inputs, the Local profile, document-provider
reacquisition or release-wide coverage.
