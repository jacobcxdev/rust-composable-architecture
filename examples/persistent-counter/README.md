# Persistent counter

A runnable headless example using the optional `rca-persistence` document and
file backend, with an explicit application codec.

```sh
cargo run -p rca-example-persistent-counter -- /existing/private/directory/counter 7
cargo run -p rca-example-persistent-counter -- /existing/private/directory/counter 12
```

The second run prints `7 -> 12`. The parent directory must already exist. Only
the supplied target is replaced; use an application-owned directory and a
dedicated filename. This command is intentionally a writer, not a viewer.

The generic `Document` owns value/revision coordination and coalesces edits into
the latest snapshot, with at most one save in flight. Failures suspend automatic
saving; flush or close explicitly retries. Close fences edits permanently and
succeeds only after the final revision is acknowledged. Before successful
loading, close returns `NotLoaded` without fencing or writing defaults.

The CLI uses this explicit order:

1. Load and reject any backend/codec failure before editing.
2. Edit, then submit close and await its persistence receipt.
3. Await `FileBackend::close()` for admitted blocking-job quiescence.
4. Shut down the Store and report success.

A causal action task finishing alone is not a successful flush. Dropping its
handle does not cancel persistence. Corrupt input fails without being rewritten.
The codec is deliberately small: exactly 16 bytes, `RCACTR01` followed by a
little-endian u64. Unknown versions, truncation and trailing data are rejected;
no legacy migration is silently attempted. The backend's explicit 16-byte limit
rejects oversized reads with a one-byte-over-limit probe and oversized saves
before creating a replacement. The adapter itself knows nothing about counters
or their format.

## File and cancellation boundaries

`FileBackend` resolves an existing parent and creates a unique same-directory
temporary file. It writes the complete encoded value, requests file sync,
replaces the target, and optionally requests directory sync. Before replacement,
failures leave the old snapshot intact. Replacement or later failure must be
treated as potentially committed, never as rollback. Normal failure drops the
owned temporary file; process death may leave an orphan. Loading ignores orphans
and never adopts or deletes them.

`SyncMode::File` requests file sync and replacement but not directory sync.
`SyncMode::FileAndDirectory` additionally requests directory sync on Unix;
construction rejects it elsewhere before writing. The CLI selects the latter
on Unix and the former elsewhere. Local cross-target compilation is not fresh
Windows or Linux execution evidence. Neither mode claims power-loss durability,
macOS `F_FULLFSYNC`, hardware-cache honesty or network-filesystem guarantees.
Process-kill tests do not prove those guarantees. The underlying contracts are
documented by [tempfile persist](https://docs.rs/tempfile/3.27.0/tempfile/struct.NamedTempFile.html#method.persist)
and [File::sync_all](https://doc.rust-lang.org/std/fs/struct.File.html#method.sync_all).

Blocking jobs use the explicitly supplied Tokio host. Dropping an operation
future does **not** stop a started write. Backend close permanently fences every
clone, then waits for admitted operations without stopping the shared host.
The fence starts on its first poll. Operations are admitted only when polled:
an earlier-created operation first polled after the fence fails with
`FileStage::Admission` and `BrokenPipe`. Cancelling a close waiter does not reopen
admission; independent and later waiters observe the same quiescence. Admission
ownership is released even when the host drops a queued job before it runs.

Backend close is quiescence, not successful persistence: it does not flush
domain state or recover an abandoned write's result. After explicit cancellation
submit `Action::abort()`, await backend quiescence, then construct a **new**
document and load afresh before editing. An indefinitely blocked filesystem
operation also blocks backend close. Independently constructed backends are
not fenced; callers still own the single-writer rule. Store shutdown is abortive,
not a flush, and is not a backend quiescence guarantee.

There is no cross-process locking or merge policy. Target symlinks and
non-regular files are rejected, but checks do not protect against concurrent
writers in an untrusted directory. Replacement does not preserve permissions,
ACLs, ownership, hard-link identity or other metadata. No multi-file transaction,
shared-alias state, migration framework or retry service is included.

## Verification

```sh
cargo test -p rca-persistence --features file
cargo test -p rca-example-persistent-counter
```

The persistence crate owns the extracted fault, cleanup, process-kill and
blocking-job lifetime tests, plus generic byte limits and distinct codecs. The
example tests its explicit codec, corrupt-input preservation, document receipt
ordering, and executable restart. Each real-file test uses a separate temporary
directory.
