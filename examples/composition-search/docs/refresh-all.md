# Refresh a captured recipient group

The executable recipe is [`rca-test/tests/refresh_all.rs`](../../../rca-test/tests/refresh_all.rs).
It uses one parent Feature, a finite list of public `CommandPort<Refresh>` values,
and the existing bounded actor mailbox. There is no refresh registry or new
runtime abstraction.

The parent owns the canonical document. Notifications enter as owned inputs
after validating a positive revision and 1–64 UTF-8 bytes. Applying a valid
notification replaces that document in one synchronous actor transaction.
The immutable `Arc<str>` is safe to share; historical assertions copy its text.
Observing a mutable external container would not turn its writes into actor
transactions, and is not an alternative notification path here.

This finite recipe starts one Refresh. At admission, capture the ordered ports
and the current document once.
This recipe deliberately keeps that version across suspension. A notification
received while suspended changes the parent's current document, not the captured
batch. Recipient order is the caller's finite list order, including duplicates.

After suspension, call `try_send` for **every** recipient before awaiting any
receipt. Retain at most one attempt per listed recipient. A full first mailbox
must not block or suppress a later destination. Then join admitted receipts in
the same list order and publish one ordered report. This bounds memory by the
explicit group; it does not create a task per destination, retry backlog, or
all-or-nothing broadcast.

Outcomes are independent: Full returns the original command, pre-entry rejection
returns the original command, and entered failure does not promise payload
recovery. The report preserves the rejection/failure reason and progress rather
than calling every error stale. Partial success is retained without rollback. A replacement during
suspension invalidates the captured port: it rejects without mapping or updating
the replacement. An explicit new operation must reacquire a fresh port; this
operation never silently retargets.

The test deliberately uses non-Clone, non-Sync command tickets and verifies the
original allocation identities on Full and stale rejection. The shared document
is not the command ticket and does not disguise reconstructed payloads.

Completion means all attempted admissions have terminal commit results and the
parent report has committed. It does not await each recipient's resulting task
or durable external cleanup. Cancelling before admission sends nothing.
Cancelling after admission drops the caller's wait; dropping receipts does not
retract admitted work. Destinations own their subsequent work independently.

The schedules cover three recipients with Full/stale/success, an empty group,
cloned handles sharing one mailbox versus independently constructed backends,
and cancellation/commit-only completion. The parent is a public `TestStore`;
recipients are real production mounted Stores on explicit deterministic hosts.
The hosts are test controls, not another application runtime. Each constructor
creates fresh State, declaration and scheduler; cloning a port explicitly shares
its owner. This Shared, commit-only recipe does not establish commanded-child
TestStore or Local-profile parity. The separate
[command-frontier tests](../../../rca-test/tests/command_frontier.rs) exercise
`mount_with_children`, `command_port` and `receive_command` on both profiles;
Shared TestStore also provides the root-declared `mount_with_command` convenience.

For captured-client versus explicit-current-service reads, use the separate
[`session-refresh.md`](session-refresh.md) recipe. A fresh State service read is
not an automatic refresh of a previously captured dependency. For core command
ownership and capacity contracts, see
[`command_port.rs`](../../../rca-core/tests/command_port.rs).

Its `finite_fanout` module separately demonstrates completion of an
application-owned captured list of accepted `ActionTask`s, including an empty
list. Later, unrelated and replacement work is excluded. Cancellation
acknowledgement is not destruction; the captured wait still joins owned work.
That witness does not change this recipe's commit-only completion policy or
establish Local fan-out coverage.

Run the focused witness with `cargo test --locked --offline -p rca-test --test refresh_all`.
