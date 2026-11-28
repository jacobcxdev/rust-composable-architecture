# Report upload progress and reject stale results

Run the [uploader example and its tests](../examples/uploader.rs) from the repository root.
On macOS, use the [Cargo memory guard](../../../CONTRIBUTING.md#build-memory-safety).

```sh
cargo run -p rca-example-composition-search --example uploader
cargo test -p rca-example-composition-search --example uploader
```

The `automatic` mount factory starts the `upload` async function.
The function fetches the bytes, records fetch progress and reads the current byte
limit. It processes the bytes before sending them to the client.
After the upload, it records upload progress and returns the receipt. The factory
emits the receipt as an output.
Each `update` changes state in a short transaction. The function never holds a state
borrow across a wait or sends an action for progress.

The Initial factory captures its injected client once. A `read` after the fetch sees
the current limit, but the function keeps the original client.
The read and the upload are separate operations. Policy or ownership can change
between them.
This example changes the limit, not the dependency provider.

The example client has two gates in place of a network service. Each gate supports one
waiter and one prepared response.
The same application code handles immediate and pending responses.
Each `Client::new` call creates a separate backend. A clone shares its backend and
cannot support extra waiters.
An application can replace the client with its own I/O code without changing the
runtime or reducer.

## Send results to the original child

Each installation of the optional child has a business key and its own identity.
A `TaskContext` belongs to one installation. Removing and restoring key 7 does not
make an old context valid again.
The actor rejects its reads, updates and sends before the user callback runs.
For new work, acquire a new context. One test keeps a caller-owned task alive across
removal to check this rule independently of managed cancellation.

The root receives the move-only receipt through a typed output. Its String keeps the
client's original allocation.
The declaration slot binds that output before installation. It is not a reply queue, a
request registry or a state snapshot.

If output delivery fails, the example reports the result and drops the recovered
receipt.
An application can instead save the receipt or retry under its own policy. It must not
use the parent to bypass a stale child context.
If the upload-progress update fails first, `upload` returns the access error and drops
the receipt.
Neither path undoes the upload request.

The worker also has a typed command. It does not wrap that command in a root action.
The upload test expects progress with `receive_update` and the final receipt with
`receive_output`.
The separate [command tests](../../../crates/rca-test/tests/command_frontier.rs) cover
`mount_with_children`, `command_port` and `receive_command` on Shared and Local
TestStores.

## What the tests check

The tests expect every progress update and the final output. Reading state does not
acknowledge an update or an output.
Neither does calling `finish`. If an output expectation fails, the same receipt
remains available for cleanup.
A later match cannot erase the failure. The same rule applies to an incorrect progress
expectation.
The saved snapshot includes every field of the root, child and receipt. It keeps no
live client or task handle.

The client gates record pending polls and future destruction. Waiting for retirement
to finish includes waiting for the managed task to release its client future.
Two tests hold caller-owned work across child replacement:

- During the fetch wait, the stale task cannot start an upload.
- During the upload wait, the external request stays on record. A late response cannot
  change the replacement or deliver a receipt.

Another test fills a Store's single input slot. A second send waits for room.
After the Store closes, the sender recovers the original action that the Store never
accepted. No test uses sleeps to choose event order.

These guarantees apply within the process. They do not guarantee remote rollback,
deletion or server-side cleanup.
A real service needs its own protocol to acknowledge requests and prevent duplicate
effects.
