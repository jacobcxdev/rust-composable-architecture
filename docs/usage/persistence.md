# Save data across restarts

A document loads an application value from storage and saves later edits.
For example, a counter can reopen a file and start from the last saved number.
Add the `rca-persistence` crate alongside `rca`. The `file` feature supplies a file backend for a Tokio runtime.

The **backend** reads and writes bytes. A **codec** converts the application's Rust value to bytes and back.
The crate selects no storage, format, or runtime by default. The Store holds the value that the app can edit.
If one Store owns your value, start with `Document::new`. The [persistent counter](../../examples/persistent-counter/README.md) shows the complete file example.

## Choose how to save a value

| What does the application need? | Starting point | What the caller owns |
| --- | --- | --- |
| Share a value in memory | [Shared state](shared-state.md) | No storage is necessary. |
| Load a value from a service or receive its updates | `SharedReaderKey`, or `SharedKey` for explicit saves | The service defines how it delivers updates and saves data. Changing the value does not call `save`. |
| Use an existing byte backend through a shared cache | [`BackendKey`](../../crates/rca-persistence/src/shared_key.rs) | Save explicitly. Reload existing cached values to see the write. There is no watcher or flush receipt. |
| Save edits to a document that one Store owns | `Document::new` | Keep the returned state and reducer together. Use one writer. |
| Give cached readers or writers access to a document | `SharedDocument::new` with `DocumentKey` | Keep the document owner alive while other parts of the app use its keys. |
| Reject a write after a competing edit | [`SharedDocument::revisioned`](#reject-conflicting-writes) | Supply a backend that detects conflicting writes. Decide how the app resolves them. |

For a shared document, keep the reducer's session before you construct the Store.
The session tracks outstanding work for that particular document.
Construct a `SharedDocumentOwner` with the route to that document, a function that copies its value independently, and the matching session.
The owner supplies the keys that grant access to that document.

If the parent feature needs different rules, see the [parent-owned persistence recipe](runtime.md#parent-owned-persistence).
For example, that recipe closes an unloaded document without saving its default.

## Save and close a document

A **revision** identifies an edit. A **receipt** lets the caller wait for the outcome of a save request.
A flush asks the backend to save the current revision. A close also prevents further edits.

Load the document before you edit it. Until the first successful load, flush and close return `NotLoaded`.
In that case, neither operation writes the default value. The document still accepts later edits.

A document permits one save at a time. Edits during a save can share a later write.
After a save fails, automatic saves stop. If the document is open, send a flush or close action to retry.
If a close has already started, flush returns `ReceiptError::Closing`.
To retry a failed close, send a new close action. Wait for its new receipt.

| Operation | What success tells you |
| --- | --- |
| Flush receipt | The backend acknowledged the revision from the flush action, or a later one. The document stays open for edits. |
| Close receipt | The backend acknowledged the final revision and the Store published the closed state. Once the document accepts close, a failed save does not reopen edits. |
| `FileBackend::close()` or `SharedDocumentOwner::close()` | The backend or owner stopped accepting work and waited for its outstanding operations. Neither operation saves the document. |
| Store shutdown | The Store stopped the work it owns. Shutdown does not flush the document or undo external writes. |

For an orderly exit:

1. Submit the document's close action.
2. Wait for its receipt to report success.
3. Await `FileBackend::close()` or `SharedDocumentOwner::close()`, as appropriate for your document.
4. Shut down the Store.

A state snapshot or a finished action task does not prove that a save succeeded.
A successful receipt does not guarantee more than the backend promises. For example, the file backend cannot guarantee survival of power loss.

Dropping the receipt does not cancel a save. Explicit task cancellation can discard the acknowledgement while a write continues.
After explicit cancellation, abort the document. Before you reopen the same storage, wait for the old backend's work to end.
The [document tests](../../crates/rca-persistence/tests/document.rs) cover failed saves, retries, and close.
The [file-session test](../../crates/rca-persistence/tests/shared_quiescence.rs) shows why restart must wait for cleanup.

## Reject conflicting writes

Two writers can read the same value and then save different edits. The second save can overwrite the first.
Even with a backend that supports revision checks, `Document::new` and `SharedDocument::new` write unconditionally.
`SharedDocument::new` uses the marker `NoBackendRevision`. Its state's `backend_revision()` returns `None`: the document has no storage version to compare.

`SharedDocument::revisioned` requires a [`RevisionBackend` and `BackendSession`](../../crates/rca-persistence/src/revision.rs).
The backend must compare the expected revision and write in one operation. No other covered writer can change the data between those two steps.
Replacing a file does not provide that guarantee. The supplied file backend does not implement `RevisionBackend`.

Three versions answer different questions:

- A **document revision** counts local edits.
- A **backend revision** identifies a version of the stored data, including the absence of data. A change back to identical bytes still has a new revision.
- A **schema version** identifies the format that the codec can read or write.

If the stored revision differs from the expected revision, the backend rejects the write. The document keeps the local edit.
`SharedDocumentState::conflict` exposes the external revision, the external bytes or their absence, and any error from the codec.
The application decides whether to merge, discard, or ask the user.
The [shared-document tests](../../crates/rca-persistence/tests/shared_document.rs) cover conflicts, failed saves, superseded loads, and retained sessions.

## Choose a codec and reuse buffers

Start with `Codec::encode` and `Codec::decode`. These methods borrow their inputs: the caller keeps ownership.
If the codec can reuse owned input, implement `encode_owned`, `decode_owned`, or both.
The owned methods must return the same result or error as the borrowed methods.
A byte-vector codec can return the original vector. A shared document borrows bytes to keep the original as conflict evidence.

A save still needs a snapshot independent of later edits. `Document` clones its value before the save effect encodes it.
Sharing immutable data works. By contrast, cloning an `Arc<Mutex<Vec<u8>>>` copies the handle, not the mutable bytes.
Another handle can change the bytes without advancing the document revision.
See the [byte-buffer codec](../../crates/rca-persistence/src/lib.rs) and its [ownership tests](../../crates/rca-persistence/tests/document.rs).

The application defines its file format and which versions it accepts.
If older writers must preserve newer fields, keep the unknown data or reject the write before replacement.
To test compatibility, use an older writer to edit data from the new version. Read the result with the new reader.
Include default values, relationships between records, and the switch to the new format in this test.
The persistent counter instead [rejects unknown versions](../../examples/persistent-counter/README.md#choose-the-document-type).
