# Owned importer history

Run the [headless importer](../examples/owned_importer.rs):

```sh
cargo run -p rca-example-composition-search --example owned_importer
cargo test -p rca-example-composition-search --example owned_importer
```

The recommended path is the ordinary `import` async function: await an owned
client result, then commit it with a short `TaskContext::update`. No borrowed
state survives the await, and progress needs no synthetic response action.
The mounted Initial task and the Tokio executable use the same function.
The executable makes two explicit requests: a rejected batch, then a successful
replacement. It prints redacted owned history and finally removes the session.

The concrete in-process client has one prepared oneshot receiver per generation.
It supports a response supplied before the first poll and one supplied after an
actual Pending poll. Both follow the same client code. A fresh client has its own
receivers; cloning it intentionally shares the existing endpoint. The client
returns owned rows, which move into state without cloning. This finite endpoint
is not a network service, database, request registry or new execution framework.

## What the snapshot covers

`ImportSession` is not Clone: it owns a real Tokio `OwnedSemaphorePermit`.
`#[derive(Snapshot)]` still captures nested progress, owned history, private
bookkeeping, failures and the credential value. The generated history has its
own Clone/equality/Debug capabilities; the live state needs none of them.

Earlier history survives later imports and removal. Editing expected history
does not mutate state, its shared quota or its resource. `redact` hides the
credential only from Debug formatting. It does not remove the stored value,
change equality, encrypt it or protect direct field access.

Two fields are explicitly omitted, with reasons:

- The shared quota is asserted through a declared `Quota` capture. A successful
  batch spends one unit. Correct history expectations do not acknowledge an
  omitted or incorrect quota expectation; a failed capture comparison leaves
  sticky assertion debt. Failures spend no quota.
- The session lease is asserted through physical permit return, not equality.
  Owned history must not retain it.

This is projected coverage plus a separately declared capture, not full-state
coverage. The capture reads an independent scalar and can return its actual
`SharedAccessError`. External writes through another Shared handle are not
automatically intercepted actor transactions.

## Three different lifetimes

The capacity-one permit represents a **mounted session**, not active I/O.
Successful importing retains the lease until session replacement or removal.
The private pool is never closed. Replacement explicitly drops the old state
before acquiring the successor's lease; constructing the new state on the right
side of an assignment would acquire before dropping the old lease.

The mount also has a replacement generation. Direct same-key replacement cancels
the old Initial work and gives the successor a separate prepared client response.
State removal can release the permit before the cancelled future is destroyed.
The retirement test therefore proves the receiver reached Pending, replaces the
session, joins the old task and only then requires the late send to return its
original owned payload. The fresh successor completes independently.

That demonstrates managed cancellation and receiver destruction. It does not
demonstrate an external surviving continuation resuming with stale authority;
the [uploader](uploader.md) has that distinct control. It also promises neither
remote rollback nor durable external cleanup.

Client rejection and a closed response endpoint are explicit typed domain
failures. Task-access failure is reported separately. Neither is converted into
successful empty data, and this recipe selects no automatic retry policy.

## Focused checks

The inline tests exercise ready and Pending client completion with independently
edited history/capture expectations; actual client rejection and abandonment;
same-key replacement, fresh success and resource release; and missing/wrong quota
expectations after a real client result. The test runtime gates real production
updates; inspecting state or calling `finish` does not acknowledge them.

Existing core/macro/TestStore owned-snapshot tests supply the broader manual/
derived, private-field, redaction and deliberately aliasing custom-projection
controls. This recipe does not duplicate that compiler/negative matrix. Custom
Snapshot implementations still owe semantic independence; derive is not a proof
that arbitrary user capture callbacks cannot retain mutable aliases.

The finite demonstration prepares at most two batches against twelve quota
units. It does not define a general admission or quota-exhaustion policy.
