# Restore a document in stages

The [restoration tests](../../../crates/rca-persistence/tests/staged_restoration.rs) load a
checkpoint, fetch the document it names and publish the document with its selected
page.
The example uses `Backend`, `Codec` and `Feature` with test controls that own their
responses.

Run the test from the repository root. On macOS, use the [Cargo memory guard](../../../CONTRIBUTING.md#build-memory-safety).
If the locked dependencies are not cached, leave out `--offline` on the first run.

```console
cargo test --locked --offline -p rca-persistence --test staged_restoration
```

## Keep staged data separate from visible data

A checkpoint records a format version, document ID and selected page.
The codec accepts exactly four bytes. It requires version one, a non-zero document ID
and a non-zero page number.
Absent storage produces `Missing`. The example does not save or display a default
document.
A load or decode failure leaves the visible document and selection unchanged.

The task uses a valid checkpoint to request a document fetch.
The returned ID must match the saved ID, and the document must contain the selected page.
Only then does one `TaskContext` update replace both visible fields.
No state borrow crosses either wait. The tests use the same code for immediate and
pending responses, without sending substitute completion actions.

## Check the request after each wait

The root tracks the current `(restoration_epoch, request_id)`.
After the checkpoint arrives, the actor checks that pair. If it no longer matches, the task skips the fetch.
If it matches, the actor authorises the fetch. A newer request can arrive before the task starts or finishes that external call.
Before it publishes the document and selected page, the actor checks the pair again in the same update.
A stale reply cannot change either field, clear a pending stage, or erase a newer failure.
Even a stale update that changes nothing counts as an actor commit. The tests expect
that update before calling `finish`.

`ReplaceDocument` changes the epoch. A late reply cannot change the replacement, even
if it reuses the business ID and request ID.
This check belongs to the application. The root still exists throughout the test.

The [authorisation example](authorisation.md) tests a different case: RCA rejects work
for a child after its replacement.
The [client refresh tests](session-refresh.md) cover captured clients, fresh reads and
optimistic rollback.

## Limit work through completion

The application accepts at most two restoration requests at once. This lets the test
deliver their replies in either order.
A third request increments the busy counter without changing the current request,
stage or failure.
Each helper holds one permit through both waits and its final result callback.
Replacing the current request does not release that permit.

The test provides up to two responses per boundary and drains an inbox with room for
two reports.
Each fixture creates a new backend. Clones inside that fixture share it.

These limits do not bound all memory use. `Backend::load` returns a `Vec`. The backend
can allocate an oversized response before the caller can reject it.
Transport buffers, external callbacks and work outside the helper need their own
limits.
The example has no deadline or automatic retry. It cannot forcibly stop work.

## Account for a reply exactly once

The fetched document is not Clone. It contains a boxed token that is not Sync.
`Completion::complete(self, value)` consumes its sender, so the caller can reply only
once.
Dropping the controller without a reply reports abandonment, not a document fetch
failure.
The tests check the final outcome and count drops of the original allocation.

If the root rejects a stale result inside the update, the callback returns the
original document as its result.
An invalid fetched document also returns to the caller. Neither case means that
`TaskAccessError` can recover values already moved into an update callback.

The [public control tests](../../../crates/rca-test/tests/controls.rs) cover replies sent
after the receiver drops.
The [authorisation tests](../examples/authorisation.rs) cover a repeatable callback
that must return duplicate or late payloads.
A one-shot sender does not stop a third-party callback from running more than once.

## Wait for external cleanup separately

Dropping the document token proves that its in-memory owner released it.
`TestStore::finish` checks for remaining work and shuts down the Store. It does not
wait for an external storage operation.
Completion of a superseded helper does not prove that the backend stopped, rolled back
or deleted its data.

To save a plain `Document` before shutdown, submit the action from `Action::close`.
Wait for its persistence receipt to succeed. The receipt does not mean that all backend work has stopped.
Plain `Document` does not require a `BackendSession`.
If the backend supplies this capability, await `BackendSession::close` separately.
Its first poll stops new operations. Success means that all operations the session accepted have ended.
On error, only the backend's own error contract can establish that work stopped.
The [session replacement tests](../../../crates/rca-persistence/tests/shared_document_session_replacement.rs)
distinguish an obsolete actor update from waiting for the old backend session.
The [SharedDocument tests](../../../crates/rca-persistence/tests/shared_document.rs) cover
dropped close waiters and errors that remain available to later waiters.
This restoration example acquires no durable external resource.
