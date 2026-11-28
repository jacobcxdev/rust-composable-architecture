# Restore a document in stages

The [restoration tests](../../../rca-persistence/tests/staged_restoration.rs) load a
checkpoint, fetch the document it names and publish the document with its selected
page.
The example uses `Backend`, `Codec` and `Feature` with test controls that own their
responses.

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

A valid checkpoint starts the document fetch. The returned ID must match the saved ID,
and the document must contain the selected page.
Only then does one `TaskContext` update replace both visible fields.
No state borrow crosses either wait. The tests use the same code for immediate and
pending responses, without sending substitute completion actions.

## Check the request after each wait

The root tracks `(restoration_epoch, request_id)`. Accepting a new request makes it
current.
An old checkpoint reply cannot start a document fetch. An old document reply cannot
publish, clear a pending stage or erase a newer failure.
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

The [public control tests](../../../rca-test/tests/controls.rs) cover replies sent
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

For editing, saving and observing persistent documents, use the document coordinators.
Their `BackendSession::close` rejects new work and waits for physical operations to
finish.
The
[session replacement tests](../../../rca-persistence/tests/shared_document_session_replacement.rs)
distinguish an obsolete actor update from waiting for the old backend session.
The [SharedDocument tests](../../../rca-persistence/tests/shared_document.rs) cover
dropped close waiters and errors that remain available to later waiters.
The [managed-server tests](../../mcmd-server/tests/managed_servers.rs) preserve
external cleanup errors for explicit retry.
This restoration example acquires no durable external resource.
