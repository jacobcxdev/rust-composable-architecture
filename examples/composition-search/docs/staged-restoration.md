# Staged document restoration

The [executable recipe](../../../rca-persistence/tests/staged_restoration.rs)
loads and validates a checkpoint, hydrates its referenced document, then publishes
the document and selection together through an actor update. It uses existing
`Backend`, `Codec`, `Feature` and owned asynchronous test controls.

```console
cargo test --locked --offline -p rca-persistence --test staged_restoration
```

## Keep staged data separate from visible data

A checkpoint contains a format version, document ID and selected page. The codec
accepts exactly four bytes, version one, and non-zero ID/page. Missing storage is
`Missing`, not a request to save or publish a default. Load and decode failures
retain the previous visible document and selection.

Valid bytes only start hydration. Its returned document ID must match, and its
page count must contain the saved selection. Only then does one `TaskContext`
update replace both visible fields. No state borrow survives either wait. Both
immediate-ready and observed-Pending endpoint responses run through this path;
the tests do not supply stand-in RCA completion actions.

## Revalidate application identity after each wait

The stable root owns `(restoration_epoch, request_id)`. A new admitted restore
becomes current; old load replies cannot start hydration. An already-hydrating
old request cannot publish, clear the current pending stage or replace a newer
failure. A stale update is still an actor commit, so the tests explicitly consume
its no-op update rather than hiding it in `finish`.

`ReplaceDocument` changes the application epoch, even when the business ID and
next request ID are reused. An old helper may really resume and still cannot
update the replacement. This is **domain identity in a live root**, not structural
incarnation replacement. For captured RCA authority rejecting a true replacement,
reuse the [authorisation recipe](authorisation.md) and its externally retained
request test. The [client-refresh recipe](session-refresh.md) separately covers
captured clients, fresh reads and optimistic rollback; those suites are not
duplicated here.

## Bound work without treating cancellation as destruction

Two immediately acquired application permits allow a finite out-of-order race.
A third request records Busy without changing the current identity, stage or
failure. Each permit lives through both waits and the final result callback;
replacement does not release it. The fixture plans at most two owned responses
per boundary and drains a capacity-two report inbox. It creates a fresh backend
for every fixture; cloned handles inside that fixture deliberately share it.

These are recipe bounds, not a global memory bound. `Backend::load` returns a
`Vec`; rejecting an oversized response cannot prevent allocation inside the
backend before return. External retention of a received callback, arbitrary
transport buffers and work outside the helper need their own bounds. There is no
deadline, implicit retry or hard preemption in this recipe.

## Complete once; account for original ownership

Hydration returns a non-Clone document containing a non-Sync boxed token. The
owned `Completion::complete(self, value)` endpoint can be consumed only once.
Dropping an uncompleted controller produces explicit abandonment, not a fabricated
hydration failure. The tests observe one final disposition and exactly account
for the original allocation's lifetime.

When the live root's final identity guard rejects prepared data, the update
returns that original data as its committed owned result. Invalid hydration also
returns its prepared value. Neither is a promise that `TaskAccessError` recovers
captures already handed to an update callback. Receiver-drop recovery and a
repeatable callback adapter's duplicate/late payload recovery are already covered
by the [public controls tests](../../../rca-test/tests/controls.rs) and
[authorisation tests](../examples/authorisation.rs). A consumed one-shot does not
make an arbitrary third-party callback automatically once-only.

## Keep physical release a separate contract

The token's Drop proves only in-memory destruction. `TestStore::finish` checks
remaining work and shuts down the Store; it is not merely an assertion or an
external-storage join. A superseded helper completing does not prove physical
backend quiescence, rollback or durable cleanup.

For persistent editing/flush/observation, use the existing document coordinators.
Their public `BackendSession::close` capability fences admission and joins actual
physical operations. The [session replacement tests](../../../rca-persistence/tests/shared_document_session_replacement.rs)
retain the distinction between stale actor delivery and the exact old session's
join. The [SharedDocument tests](../../../rca-persistence/tests/shared_document.rs)
cover dropped close waiters and retained join failures; the
[managed-server tests](../../mcmd-server/tests/managed_servers.rs) retain external
cleanup failures for explicit retry. This read-and-resolve recipe acquires no
durable external resource and does not reproduce cancellation-on-drop or a
recorder/server cleanup framework.
