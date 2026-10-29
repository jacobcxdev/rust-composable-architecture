# Resumable headless uploads

Run the [compiled uploader example](../examples/uploader.rs):

```sh
cargo run -p rca-example-composition-search --example uploader
cargo test -p rca-example-composition-search --example uploader
```

The recommended path is the small `upload` async function and its `automatic`
mount factory. Fetch owned bytes, commit fetched progress, read current policy,
process the bytes, await upload, commit uploaded progress, then emit one meaningful
receipt. Progress needs no synthetic response actions. Each `update` is a short
serial transaction; no borrowed state survives an await.

The Initial factory captures its injected client once. A fresh `read` after await
sees a changed byte limit, but does not silently replace that client. The example
uses a two-step controlled endpoint instead of a network service, so immediate
and Pending responses exercise exactly the same application code. Separate
`Client::new` calls create independent backends; cloning a client intentionally
shares its existing endpoint. Replace this concrete client with the application's
I/O implementation, not the runtime or reducer.

The limit is current at the read's serial position. Reading it and starting the
upload are not one atomic operation: policy or ownership can change between them.
The controlled endpoint supports one waiter and one prepared response per gate;
cloning it does not make it a multiplexed client for concurrent uploads.

This trace changes policy, not the dependency provider. It proves that one
captured client serves the operation; provider replacement and an explicit
fresh-provider read are separate scenarios, not claims of this recipe.

## Ownership and completion

An optional upload is installed under an explicit business key and incarnation.
An acquired `TaskContext` names that incarnation, not whatever later occupies the
same key. Removing and restoring key 7 makes old reads, updates and sends fail
before their user callback runs. Reacquire deliberately for new work. The example
also tests a caller-owned continuation that survives removal: cancellation of
managed work alone cannot prove stale external capabilities are fenced.

The move-only receipt flows through a typed output to its root handler. Its String
allocation remains the original client's result. The declaration slot only binds
the output before structural installation; it is not a response queue, request
registry or snapshot. An unhandled or rejected result is reported explicitly;
this small example then drops it. A production application can instead persist or
retry the recovered value under its own policy. It must not promote stale child
authority to the parent. This recovery applies to the later output boundary. If
the uploaded-progress update fails first, `upload` returns the access error and
discards its receipt. That is this example's policy, not automatic recovery or
rollback of the already-recorded external request.

The worker's separate typed command is used by the ownership/admission controls.
It is not automatically wrapped into a root action. The exhaustive completion
trace uses `receive_output`, while progress uses `receive_update`. The separate
[command-frontier tests](../../../rca-test/tests/command_frontier.rs) exercise
`mount_with_children`, `command_port` and `receive_command` on Shared and Local
TestStores; this uploader trace does not establish that separate coverage.

## Deterministic checks

The tests assert every progress commit and completion output. Merely inspecting
state or calling `finish` cannot acknowledge missing work. A failed exact-output
matcher leaves the same receipt available for cleanup but retains sticky assertion
debt, even after a later match. A wrong committed progress expectation also leaves
sticky debt. The historical snapshot includes every root/child field
and every receipt field, but contains no live capability or client alias.

Client gates record actual Pending polls and future destruction. Retirement's
causal completion waits for managed work to release its client future. A separate
caller-owned trace replaces the incarnation while the upload response is Pending:
the external request remains recorded, but its late progress cannot change the
replacement's state or deliver a receipt. The earlier fetch-wait trace verifies
rejection before any upload request starts. A separate
capacity-one trace fills ingress, observes an owned send Pending, closes the Store
and recovers the original unadmitted action. No sleeps choose the schedule.

These are in-process ownership guarantees, not proof of remote upload rollback,
durable deletion or server-side resource cleanup. Real services need their own
idempotency and acknowledgement protocol. The recipe adds no renderer, operation
registry, client framework, executor or package dependency.
