# Owned resources in borrowed actions

RCA owns each admitted action, but lends it to reducers through `&Action`.
An action need not implement `Clone` or `Sync`. This permits move-only messages.
It does not permit moving an ordinary field out of the shared borrow.

Borrowed composition lets multiple reducers inspect ordinary actions without
routing clones. It cannot give each recipient ownership of the same unique resource.

## Choose where the resource belongs

Start with the resource's owner and lifetime, not a requirement to make everything
`Clone`. Different ownership needs have different routes:

| Need | Route | Obligation |
| --- | --- | --- |
| One asynchronous operation owns the resource | Keep it inside an effect or task | Join the provider's cleanup when dropping the future is insufficient. |
| A task returns a resource that must become state | Move it through `TaskContext::update` | Keep the update closure short and handle ended authority. |
| A borrowed action must transfer the resource to one reducer | Use an explicit one-shot slot | Designate one consumer and preserve immutable metadata for other readers. |
| A native resource must stay on its owner thread | Use Local state/work or a host-owned adapter | Keep polling, destruction and external cleanup on the required thread. |

The [owned importer](../../examples/composition-search/docs/owned-importer.md)
awaits an owned result and moves it into state with `TaskContext::update`.
This avoids a one-shot action slot when no domain event needs that payload.
The [owned action tests](../../rca-test/tests/owned_actions.rs) demonstrate the
separate reducer route, including a resource that implements neither `Clone` nor `Sync`.

When an effect owns an ongoing operation, keep its resource in that effect. Return
domain events rather than the resource itself. Cancellation and resource Drop
must still match the external system's cleanup requirements. Dropping a future
does not prove that a process stopped or an asynchronous flush completed.

When the domain needs exclusive ownership across actions, keep the resource in
state. An action can carry `Cell<Option<Resource>>`. One designated reducer
calls `take()` and moves the returned resource into an ordinary state field.
This pattern needs no shared owner, mutex or resource clone.

Resources in a shared Store must satisfy its `Send + 'static` boundary.
A current-thread executor does not remove that requirement.
The owner-local APIs provide separate non-`Send` contracts.

If a host or thread-affinity requirement needs a separate host actor, use one.
A command handle can cross RCA's Send boundary, but the host then
owns admission, reply cancellation, affinity and teardown. This is not an
automatic replacement for arbitrary local state.

## One-shot consumption is an application contract

`Cell<Option<Resource>>` is an explicit ownership slot, not an immutable event
payload. The first `take()` returns the resource. Subsequent calls return
`None`. A reducer must decide how to handle an empty slot. Composition order
therefore matters, and the compiler does not enforce a single consumer.

Put stable metadata beside the slot for other reducers and tests.
Examples include request identity, operation kind or independently meaningful result metadata.
Later reducers can inspect that information after the designated child consumes
the resource. They cannot also take ownership of the original resource.

Do not take a resource merely to inspect or format an action. In particular,
TestStore's receive matcher runs before reduction. A consuming matcher can
steal the payload from the reducer. Match immutable metadata instead, then
assert the resulting state's observable behaviour with an independent snapshot.
Projected assertions cover the chosen projection, not arbitrary omitted fields
or hidden resource state.

## Preserve independent test history

If a clone preserves independent state history, use `Clone` for test snapshots.
Cloning an `Rc<RefCell<_>>` or `Arc<Mutex<_>>` creates another owner of the same
mutable value. It does not preserve its earlier contents.

For move-only state, use a snapshot or comparable projection of observable values.
The [snapshot guide](../snapshot-derive.md) explains manual and derived history.
An omitted resource needs separate assertions for its identity, lifetime or cleanup.
Snapshot redaction changes diagnostic formatting, not equality or access to stored values.
The importer example combines state history, a declared shared-value capture and
a physical resource-release assertion. No single projection establishes all three.

## Reject stale delivery without retargeting

Child-scoped effect delivery and an externally sent action addressed to a key
are different authorities. Scoped output belongs to a particular child lifetime.
An external key alone does not identify that lifetime. Use the lifecycle-scoped
path for acquired resources that must not reach a replacement child with the
same key. If rejected delivery drops the resource, its destructor must provide
the required synchronous cleanup. External asynchronous cleanup remains a
separate obligation.

## Returning an owned result

To return a completed product, first stop input producers and finish required work
and provider cleanup. `Store::shutdown_into_state` transfers the final state to
the winning terminal caller. The caller can then
move its resource or product out normally. Extraction does not drain unfinished
effects, provide a durable result ticket or perform asynchronous destruction.
See the [terminal extraction contract](../runtime.md#terminal-owned-state-extraction).

## Test the resource boundary

A resource test must check allocation or handle identity, rejected delivery,
competing consumers and cleanup completion. A state assertion alone cannot prove
that an external resource was released. Each client must provide evidence for
its own synchronous or asynchronous cleanup contract.

RCA provides no consuming-reducer phase or generic one-shot payload wrapper.
A wrapper alone cannot enforce exclusive use across reducers.
See the [qualification summary](../qualification.md) for the scope of recorded evidence.
