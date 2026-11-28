# Refresh a captured recipient group

The test in [`crates/rca-test/tests/refresh_all.rs`](../../../crates/rca-test/tests/refresh_all.rs)
sends one document version to a fixed list of recipients.
Each recipient exposes a `CommandPort<Refresh>`. One full mailbox must not prevent
delivery to another recipient.

Run the test from the repository root. On macOS, use the [Cargo memory guard](../../../CONTRIBUTING.md#build-memory-safety).
If the locked dependencies are not cached, leave out `--offline` on the first run.

```sh
cargo test --locked --offline -p rca-test --test refresh_all
```

## Capture once, then send without waiting

The parent Feature owns the current document. It accepts notifications with a positive
revision and 1–64 bytes of UTF-8 text.
A valid notification replaces the document in one actor transaction. The document
shares immutable text through `Arc<str>`.
The tests copy that text into history. Changes to a separate mutable container do not
become actor transactions.

When the parent admits a refresh, it captures the current document and the ordered
ports.
A later notification changes the parent document, not this batch.
The list keeps its original order, including duplicates. This example starts one
refresh.

1. After the initial wait, call `try_send` for every recipient.
2. Keep one result per list entry.
3. Await the admitted receipts in list order.
4. Commit one ordered report in the parent.

The list bounds the number of attempts. The operation creates no task per recipient,
retry queue or all-or-nothing broadcast.

## Read a partial result

A full mailbox returns the original command. Rejection before handler entry also
returns the original command.
After entry, a failure need not return the command. The report keeps each failure's
reason and progress.
One rejection does not undo successful deliveries.

If a child changes installation during the wait, its captured port rejects the command
before it maps or updates the replacement.
Only a new operation with a fresh port can address the replacement.
The tests use non-Clone, non-Sync command tickets. They check the original allocation
on both full and stale rejection.
The shared document is separate from each owned ticket.

Completion means that each attempt has a final admission or commit result and the
parent has committed the report.
It does not mean that tasks started by recipients have finished or that external
cleanup has succeeded.
Cancellation before any admission sends nothing. After admission, cancellation stops
the caller's wait.
Dropping a receipt cannot retract admitted work. The destination owns that work.

## What the tests cover

One test sends to three recipients: one full, one stale and one live. Other tests
cover an empty group, cancellation and commit-only completion.
Cloning a port shares its mailbox. A new recipient has independent state, declaration
and scheduler.
The parent uses a public `TestStore`. The child Stores use the production
implementation on deterministic test hosts.

The Shared test covers completion at commit. The
[command-frontier tests](../../../crates/rca-test/tests/command_frontier.rs) cover child
commands on Shared and Local TestStores.
They exercise `mount_with_children`, `command_port` and `receive_command`.
Shared TestStore also supports `mount_with_command` for a root-declared command.

The example in [`session-refresh.md`](session-refresh.md) distinguishes a captured
client from a later read of the current service.
The tests in [`command_port.rs`](../../../crates/rca-core/tests/command_port.rs) cover
command ownership and capacity.
Their `finite_fanout` module joins a captured list of accepted `ActionTask`s,
including an empty list.
Later tasks, replacements and unrelated work stay outside that wait. Acknowledgement
of cancellation does not mean the runtime has destroyed the owned work.
That separate test does not change this recipe's completion rule or establish Local
fan-out coverage.
