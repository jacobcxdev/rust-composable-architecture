# Owned resources in borrowed actions

RCA owns each admitted action, but lends it to reducers through `&Action`.
An action need not implement `Clone` or `Sync`. This permits move-only messages.
It does not permit moving an ordinary field out of the shared borrow.

Upstream Composable instead passes an action by value to its reducer. Moving
that action's resource into state requires less caller machinery. RCA's borrowed
composition lets multiple reducers inspect ordinary actions without routing
clones. Neither contract gives every recipient ownership of the same unique
resource.

## Choose where the resource belongs

When an effect owns an ongoing operation, keep its resource in that effect. Return
domain events rather than the resource itself. Cancellation and resource Drop
must still match the external system's cleanup requirements: dropping a future
does not prove that a process stopped or an asynchronous flush completed.

When the domain needs exclusive ownership across actions, keep the resource in
state. An action can carry `Cell<Option<Resource>>`. One designated reducer
calls `take()` and moves the returned resource into an ordinary state field.
This pattern needs no shared owner, mutex or resource clone. The resource must
still satisfy RCA's `Send + 'static` boundary. A current-thread executor does
not remove that requirement.

If a host or thread-affinity requirement needs a separate host actor, use one.
A command handle can cross RCA's Send boundary, but the host then
owns admission, reply cancellation, affinity and teardown. This is not an
automatic replacement for arbitrary local state.

## One-shot consumption is an application contract

`Cell<Option<Resource>>` is an explicit ownership slot, not an immutable event
payload. The first `take()` returns the resource. Subsequent calls return
`None`. A reducer must decide how to handle an empty slot. Composition order
therefore matters, and the compiler does not enforce a single consumer.

Put stable information needed by other reducers or tests beside the slot:
request identity, operation kind or independently meaningful result metadata.
Later reducers can inspect that information after the designated child consumes
the resource. They cannot also take ownership of the original resource.

Do not take a resource merely to inspect or format an action. In particular,
TestStore's receive matcher runs before reduction. A consuming matcher can
steal the payload from the reducer. Match immutable metadata instead, then
assert the resulting state's observable behaviour with an independent snapshot.
Projected assertions cover the chosen projection, not arbitrary omitted fields
or hidden resource state.

Child-scoped effect delivery and an externally sent action addressed to a key
are different authorities. Scoped output belongs to a particular child lifetime.
An external key alone does not identify that lifetime. Use the lifecycle-scoped
path for acquired resources that must not reach a replacement child with the
same key. If rejected delivery drops the resource, its destructor must provide
the required synchronous cleanup. External asynchronous cleanup remains a
separate obligation.

## Returning an owned result

Once the application establishes quiescence, `Store::shutdown_into_state`
transfers the final state to the winning terminal caller. The caller can then
move its resource or product out normally. Extraction does not drain unfinished
effects, provide a durable result ticket or perform asynchronous destruction.
See the [terminal extraction contract](../runtime.md#terminal-owned-state-extraction).

## Qualification boundary

Historical local ownership checks exercised keyed receiver and finite buffer
transfers. They covered stale-delivery cleanup, allocation identity and negative
controls for competing consumers. Their retained records qualify those patterns,
not arbitrary resource lifecycle equivalence. They are not public reproduction
inputs or current release receipts. See the
[qualification summary](../qualification.md) for the current candidate boundary.

This work introduces no consuming-reducer phase or generic payload wrapper. A
consuming phase needs owned routing through every composition form, ordering rules
and TestStore integration. A wrapper alone does not enforce exclusive use.
If concrete callers show unacceptable costs, those alternatives remain design
options. Any such change needs explicit contracts and new verification; the
historical comparison alone does not justify it.
