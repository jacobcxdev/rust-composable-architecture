# Persistent counter

This headless CLI loads a counter, writes a new value and prints the change.
It uses `Document` and the optional file backend from `rca-persistence`. The application defines the file format.

**The commands replace the target file.** Choose an existing private directory and a filename that belongs to this example.
On macOS, use the [Cargo memory guard](../../CONTRIBUTING.md#build-memory-safety) for local builds.

```sh
cargo run -p rca-example-persistent-counter -- /existing/private/directory/counter 7
cargo run -p rca-example-persistent-counter -- /existing/private/directory/counter 12
```

The second run prints `7 -> 12`. With no counter file, the first run starts at zero.
A malformed file causes an error. The CLI does not replace it with zero.

## Choose the document type

The counter uses `Document::new` because one Store owns the value. Its file backend does not detect competing writers.
See [persistence](../../docs/usage/persistence.md) for shared documents, revision checks, and save receipts.

The codec accepts exactly 16 bytes: the eight bytes `RCACTR01`, then a little-endian unsigned 64-bit value.
It rejects unknown versions, incomplete records and trailing data. It does not migrate old records.
The backend reads at most one byte past the configured limit to detect oversized input.
It rejects oversized saves before it creates a replacement file. Only the codec needs to know the counter format.

## Save and close

The CLI uses this order:

1. Load the document. If the backend or codec reports an error, do not edit the value.
2. Set the new value.
3. Send the close action and await its persistence receipt.
4. Await `FileBackend::close()` until accepted file operations end.
5. Shut down the Store. Report success only after the save and cleanup succeed.

Keep the runtime alive through steps 3 and 4. The document receipt says the
value was saved; backend close says no file operation is still running.
To save without closing, or to retry a failed save, see
[save and close a document](../../docs/usage/persistence.md#save-and-close-a-document).

## File replacement

`FileBackend` resolves the existing parent directory and creates a unique temporary file in that directory.
It writes the encoded value there, requests file sync and replaces the target.
If a step fails before replacement, the old file remains intact.
If replacement or a later step reports an error, the new file can already be in place. Do not assume rollback.
Ordinary failure cleans up the temporary file. A process crash can leave it behind.
Loading ignores these files. It does not adopt or delete them.

`SyncMode::File` requests file sync and replacement, but no directory sync.
On Unix, the CLI uses `SyncMode::FileAndDirectory` to request directory sync after replacement.
Other platforms reject this mode at construction, so the CLI uses `SyncMode::File`.
Neither mode guarantees that the hardware honors sync requests or that data survives power loss.
Neither promises macOS `F_FULLFSYNC` or network filesystem behavior.
Process-kill tests cannot establish those guarantees. Cross-compilation does not establish that the code ran on the target.
See [tempfile persist](https://docs.rs/tempfile/3.27.0/tempfile/struct.NamedTempFile.html#method.persist) and [File::sync_all](https://doc.rust-lang.org/std/fs/struct.File.html#method.sync_all).

The backend rejects target symlinks and non-regular files.
These checks do not protect against a competing writer in an untrusted directory.
Replacement does not preserve permissions, ACLs, ownership, hard-link identity or other metadata.
The example has one writer. It has no cross-process lock, merge policy, multi-file transaction or migration service.

## Cancellation and restart

The supplied Tokio runtime runs the blocking file operations.
Dropping a future does not stop a write that has already started.
On its first poll, backend close prevents all clones of that backend from accepting new work.
It waits for accepted operations without stopping the shared runtime.

The backend admits an operation on its first poll, not when you create the future.
After close starts, an operation that has never been polled fails with `FileStage::Admission` and `BrokenPipe`.
Cancelling a close waiter does not reopen the backend. Later callers can still wait for the same work.
If the runtime drops a queued job before it runs, close no longer waits for that job.
If an operation blocks indefinitely, so does close.

Backend close does not flush document state or recover an abandoned write's result.
After explicit cancellation, submit `Action::abort()`. Wait for backend close.
Open a new backend for the same path. Load a new document from it before making another edit.
An independently constructed backend can still write the same path. The caller must prevent this.
Store shutdown neither flushes the document nor waits for all external writes.

## Tests

On macOS, run these commands through the [Cargo memory guard](../../CONTRIBUTING.md#build-memory-safety).

```sh
cargo test -p rca-persistence --features file
cargo test -p rca-example-persistent-counter
```

The persistence tests cover byte limits, codecs, I/O failures, process termination and cleanup of blocking jobs.
The example tests cover the counter format, corrupt-file preservation, receipt order and restart of the CLI.
Each real-file test uses its own temporary directory.
