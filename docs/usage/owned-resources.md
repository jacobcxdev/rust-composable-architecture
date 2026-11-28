# Give a feature ownership of a resource

If your feature opens a file, who closes it?
Rust lets you transfer ownership without copying the file. In RCA, the Store owns each action that it accepts.
A reducer borrows the action as `&Action`. Other reducers can still inspect it.
An action does not need to implement `Clone` or `Sync`.

If you send an open file in an action, a reducer cannot take that field out of the borrowed action.
Choose the owner before deciding how to transfer the resource.

## Choose where the resource belongs

| Who needs the resource? | Where does it belong? |
| --- | --- |
| One asynchronous operation | Keep the resource inside that effect or task. Report the outcome as an action, such as “import complete”. |
| A task acquires something that the feature must keep | Move the result into state with a short `TaskContext::update` call. If the feature no longer exists, handle the rejected update. |
| One of several reducers needs the resource in an action | Give the action a slot that one designated reducer can empty. Leave identifying information outside the slot. |
| One host thread must use and destroy it | Use RCA's owner-thread APIs or keep the resource in the host. Send commands to the host instead of sending the resource. |

The [owned importer](../../examples/composition-search/docs/owned-importer.md) waits for a client to return rows, then moves the rows into state.
It needs no separate action to carry the rows.
If only an external operation uses the resource, keep it there until cleanup finishes.
Dropping a Rust future does not prove that an external process stopped or that a file finished flushing.

RCA's ordinary Store requires owned state and actions to satisfy `Send + 'static`.
This means Rust permits their transfer between threads and they do not borrow temporary data.
It does not mean the resource must live forever. Running the Store on one thread does not relax these requirements.
For a resource that cannot move between threads, use the [Local APIs](runtime.md) with the `local` Cargo feature.
Alternatively, let the owning thread accept commands, handle cancelled replies, and release the resource itself.

## Transfer a resource through a borrowed action

For a resource that one reducer must take, use a field of type `Cell<Option<Resource>>`.
Think of it as a slot. At first it contains `Some(resource)`. The first `take()` returns the resource and leaves `None`.
Later calls return `None`. No mutex, shared ownership, or copy is necessary.

Designate one reducer to call `take()`. Decide how it handles an empty slot.
If another reducer takes the resource first, the compiler does not reject the program.
Keep immutable information, such as the request ID, beside the slot.
Other reducers can inspect that information even after the designated reducer takes the resource.

Do not take a resource to format or inspect an action. In particular, TestStore runs its receive matcher before the reducer processes the action.
A matcher decides whether an action meets the test's expectation. If it calls `take()`, the reducer receives an empty slot.
Match on the request ID instead. Then assert what the reducer did with the resource.
The [owned-action tests](../../crates/rca-test/tests/owned_actions.rs) exercise both correct transfer and accidental consumption.

## Preserve independent test history

A snapshot records earlier state so that a test can compare it with later state.
If cloning makes an independent copy, that copy can serve as the snapshot.
Cloning an `Rc<RefCell<_>>` or `Arc<Mutex<_>>` gives you another handle to the same mutable value.

If the state cannot be copied, [take a snapshot](snapshot-derive.md) of the values your test needs.
Redaction hides fields in diagnostic output. It does not hide the stored data from code or change equality.
To test an omitted resource, assert its identity, lifetime, or cleanup directly.

For example, the importer checks its state history, reads a separately shared quota, and waits for a permit to return to its pool.
A history assertion alone cannot prove that the permit returned.

## Reject late results from a removed child

Suppose a child starts an import, leaves the Store, and another child uses its key.
Sending the old reply through the root Store with that key can address the **new** child.
Use the old child's scoped effect sender for results that belong to the old child alone.
Removal ends its permission. Reusing the key does not restore that permission.

If rejected delivery drops the resource, its destructor must perform any required synchronous cleanup.
For cleanup that needs time, wait for a separate signal from the external service.
The [child-lifetime guide](structural-features.md) explains why two children with the same key can have different lifetimes.

## Return the final resource to the caller

Before taking the final state, stop new input. Wait for required work and any provider cleanup.
Then call `Store::shutdown_into_state`. If this call wins the race to shut down the Store, it returns the final state.
The caller can move the resource out normally.

This operation does not drain unfinished effects or run asynchronous destructors. No other caller receives a reusable result token.
See [extract the final state](runtime.md#terminal-owned-state-extraction) for shutdown behaviour.

A state assertion can show that the Store no longer holds a resource.
Only the part of the system that releases the resource can signal external cleanup.
RCA supplies neither a separate reducer phase for taking resources nor a wrapper that enforces one consumer across reducers.
