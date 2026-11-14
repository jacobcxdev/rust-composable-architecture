# Captured clients and current services

Keep an owned client when an operation belongs to the session that admitted it.
Read the current State service explicitly when the operation should use the new
session instead. A fresh state read does not change an earlier dependency capture.

The executable [session-client trace](../../../rca-test/tests/session_client_refresh.rs)
uses ordinary root Feature inputs and task updates. It is an integration recipe,
not another standalone example binary or a dependency-refresh API.

## One explicit handover

State and Feature are constructed with A. The Store's dependency values are then
prepared with B, and State's current service is explicitly changed to B before
admission. A task captures B synchronously:

```rust,ignore
let client = context.reducer_context().dependency::<ClientKey>().clone();
```

The owned client then enters the linked `refresh` async function, whose body
returns `Result<(), TaskAccessError>`:

```rust,ignore
let first = captured.call(Request::Fetch).await;
let (current, policy) = context
    .read(|state| (state.current.clone(), state.policy))
    .await?;
// `captured` is still B. `current` is the State service this read observed.
```

The source contains the complete body and commit. C is an ordered **State service
lookup**, not a retroactive override of the dependency container. A is construction
provenance, not an invented network request or an override applied to eager State
arguments after they were constructed.

In the Pending schedule, B's first request really suspends. An ordinary root input then
selects C and its policy. After resuming, the task calls captured B again and calls
the explicitly read C. The endpoint log is B/fetch, B/captured, C/fresh. In the
Ready control, responses are already supplied and C is selected before admission;
Ready is not described as a suspension. Both schedules use the same async body.

## Request identity is separate from lifetime

A live owner can have two valid requests whose responses arrive out of order.
Cancellation and incarnation checks do not decide which response the domain wants.
For this recipe, the newest intent wins. Each edit records a request ID, applies
its optimistic value and clears the previous error. Reply validation and mutation
belong in the same serial update:

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

Do not read/check the request ID and later perform an unchecked update: a newer
input could arrive between them. An older failure cannot roll back a newer success;
an older success cannot replace it either. Failure of the current request restores
the last accepted confirmed value. Invalid domain input starts no client call.

This is a local latest-intent policy, not remote write ordering, distributed
serializability or durable rollback. Applications whose external writes must be
ordered need a backend protocol or explicit serialization. The existing
[persistence coordination trace](../../../rca-test/tests/persistence_coordination.rs)
demonstrates a different policy: one in-flight save with coalesced later edits.

## Assert every committed response

`send_with` asserts each root input. `receive_update` asserts each actual client
completion, including a stale-ID update that changes no selected value. An omitted
update prevents `finish`; receiving it as an input cannot acknowledge it. Ordered
reads do not create mutation obligations. Snapshot projections contain independent
domain values and service identity, not mutable client handles. Endpoint calls and
client-call completion counters are asserted separately.

Constructing fresh clients creates independent backends and response controls.
Cloning a client intentionally shares its existing backend. The test completes one
backend while another remains pending, then checks their distinct domain outcomes.

The [mounted-provider trace](../../../rca-core/tests/feature_provider_handover.rs)
separately proves A/B/C ownership, subscription release and retired authority.
The [uploader](uploader.md) proves post-await policy, progress, output ownership and
same-ID replacement. Neither request IDs nor acquiring a fresh service renews a
retired TaskContext. These recipes do not stand in for private-child input testing,
local-profile parity, document-provider reacquisition or release-wide coverage.
