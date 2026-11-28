# Import rows and keep an independent history

Run the [headless importer](../examples/owned_importer.rs):

```sh
cargo run -p rca-example-composition-search --example owned_importer
cargo test -p rca-example-composition-search --example owned_importer
```

The `import` async function waits for the client's owned rows and moves them into
state with a short `TaskContext::update`.
It does not borrow state across the wait or send an action just to report progress.
The mounted Initial task and the Tokio executable call the same function.
The executable requests two batches. The client rejects the first and supplies the
second to a replacement session.
The executable prints the redacted history and removes the session.

The in-process client has one prepared one-shot receiver per session generation.
A reply can arrive before the first poll or after the receiver returns Pending. Both
cases use the same client code.
A new client has independent receivers. A clone shares the original receivers. Neither
path copies the imported rows.

## What the snapshot covers

`ImportSession` does not implement Clone. It owns a Tokio `OwnedSemaphorePermit` that
the snapshot must not duplicate.
`#[derive(Snapshot)]` captures progress, owned history, private bookkeeping, failures
and the credential.
The snapshot supports Clone, equality and Debug without these capabilities on the live
state.

An earlier snapshot survives later imports and removal. Edits to an expected snapshot
do not change the live session, quota or permit.
`redact` hides the credential in Debug output only. The credential still participates
in equality and remains accessible as data.
Redaction is not encryption.

The snapshot omits two fields:

- **Shared quota.** A separate `Quota` capture reads the scalar value and can return
  its actual `SharedAccessError`.
  A successful batch spends one unit. A failed batch spends none.
  An omitted or incorrect quota expectation leaves an unresolved assertion failure,
  even if every history expectation matches.
- **Session lease.** The test waits for the permit to return to the pool. The snapshot
  must not keep the permit alive.

Together, the snapshot and capture cover the selected values, not every state field.
A write through another Shared handle does not automatically become an actor
transaction.

## Three different lifetimes

The capacity-one permit belongs to the **mounted session**, not to an I/O request.
After a successful import, the session keeps the permit until replacement or removal.
The private pool stays open.
Replacement drops the old state before it acquires the next lease. Assignment alone
evaluates the new state before it drops the old lease.

A same-key replacement cancels the old Initial task. Its successor gets a new
generation and a separate prepared response.
Removal can release the permit before the runtime destroys the old future. The test
distinguishes these events:

1. Wait until the old receiver returns Pending.
2. Replace the session.
3. Join the old task.
4. Send a late reply. The sender must return the original owned payload.

The new session can complete independently. This test establishes cancellation and
receiver destruction, not recovery of external side effects.
The [uploader](uploader.md) covers a different case: a caller retains a stale context
and actually resumes after replacement.

Client rejection and a closed response endpoint produce distinct domain failures.
The task reports access errors separately. An error does not become an empty success
or start an automatic retry.

## Focused checks

The inline tests cover ready and Pending replies, client rejection, abandonment,
same-key replacement and permit release.
They change the expected history and quota independently. Missing or incorrect quota
expectations fail after a real client result.
The test runtime controls production updates. Reading state or calling `finish` does
not acknowledge them.

The core, macro and TestStore tests cover manual snapshots, derived snapshots, private
fields, redaction and custom projections that share mutable data.
A custom Snapshot implementation must produce independent history. Derivation cannot
establish that an arbitrary capture callback does not retain mutable aliases.

The example prepares at most two batches against twelve quota units. It does not
define a general admission or quota-exhaustion policy.
