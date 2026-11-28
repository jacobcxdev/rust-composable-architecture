# Persistence

If one Store owns a value that must survive a restart, start with `Document::new` from `rca-persistence`.
Supply a backend to read and write bytes and a codec to convert between bytes and values.
The crate selects no storage, format, or runtime by default.
The [persistent counter](../../examples/persistent-counter/README.md) shows how to load, edit, save, and reopen a real file.

## Choose how to save a value

| What does the application need? | Starting point | What the caller owns |
| --- | --- | --- |
| Share a value in memory | [Shared state](shared-state.md) | No storage is necessary. |
| Load or observe a provider's typed resource | `SharedReaderKey`, or `SharedKey` for explicit saves | The provider defines delivery and save guarantees. A mutation does not call `save`. |
| Use an existing byte backend through a shared cache | [`BackendKey`](../../crates/rca-persistence/src/shared_key.rs) | Save explicitly. Reload existing cached values to see the write. There is no watcher or flush receipt. |
| Save edits to one actor-owned document | `Document::new` | Keep the state and reducer together. Use one writer. |
| Give cached readers or writers access to a document | `SharedDocument::new` with `DocumentKey` | Retain the reducer's session before constructing the Store. Pair it with the captured route in a `SharedDocumentOwner`. |
| Reject a write after a competing edit | [`SharedDocument::revisioned`](#reject-conflicting-writes) | Supply a backend with atomic revision checks and decide how to resolve a conflict. |

If the parent feature needs different rules, see the [parent-owned persistence recipe](../runtime.md#parent-owned-persistence).
For example, that recipe closes an unloaded document without saving its default.

## Save and close a document

Load the document before you edit it. Until the first successful load, flush and close return `NotLoaded` without writing the default or preventing later edits.
A document permits one save at a time. Edits during a save can share a later write.
After a save fails, automatic saves stop. An explicit flush or close can retry.

| Operation | What success tells you |
| --- | --- |
| Flush receipt | The backend acknowledged the revision at the time of the flush action, or a later revision. Flush leaves the document open for edits. |
| Close receipt | The backend acknowledged the final revision and the Store published the closed state. Once the document accepts close, even a failed save does not reopen edits. |
| `FileBackend::close()` or `SharedDocumentOwner::close()` | The backend or owner stopped accepting work and waited for its outstanding operations. Neither operation saves the document. |
| Store shutdown | The Store stopped the work it owns. Shutdown does not flush the document or undo external writes. |

For an orderly exit, submit the document's close action and await its persistence receipt.
Then await backend or owner cleanup before Store shutdown.
A state snapshot or a finished action task does not prove that a save succeeded.
A successful receipt does not guarantee more than the backend promises. For example, the file backend cannot guarantee survival of power loss.

Dropping the receipt does not cancel a save. Explicit task cancellation can discard the acknowledgement while a write continues.
After explicit cancellation, abort the document. Before you reopen the same storage, wait for the old backend's work to end.
The [document tests](../../crates/rca-persistence/tests/document.rs) cover failed saves, retries, and close.
The [file-session test](../../crates/rca-persistence/tests/shared_quiescence.rs) shows why restart must wait for cleanup.

## Reject conflicting writes

Even with a backend that supports revision checks, `Document::new` and `SharedDocument::new` write unconditionally.
`SharedDocument::revisioned` requires a [`RevisionBackend` and `BackendSession`](../../crates/rca-persistence/src/revision.rs).
The backend must compare the expected revision and write in one atomic operation across all covered writers.
Replacing a file does not provide that guarantee. The supplied file backend does not implement `RevisionBackend`.

Three versions answer different questions:

- A **document revision** counts local edits.
- A **backend revision** identifies a version of storage, including absence. A change back to identical bytes still has a new revision.
- A **schema version** identifies the format that the codec can read or write.

On a comparison conflict, the backend rejects the write. The document keeps the local edit.
`SharedDocumentState::conflict` exposes the external revision, the original bytes or their absence, and any decode error.
The application decides whether to merge, discard, or ask the user.
The [shared-document tests](../../crates/rca-persistence/tests/shared_document.rs) cover conflicts, failed saves, superseded loads, and retained sessions.

## Choose a codec and reuse buffers

Start with `Codec::encode` and `Codec::decode`, which borrow their inputs.
If the codec can reuse owned input, implement `encode_owned`, `decode_owned`, or both.
The owned methods must return the same result or error as the borrowed methods.
A byte-vector codec can return the original vector. A shared document borrows bytes to keep the original as conflict evidence.

A save still needs a snapshot independent of later edits. `Document` clones its value before the save effect encodes it.
Sharing immutable data works. A clone of an `Arc<Mutex<Vec<u8>>>` still shares mutable bytes.
Another handle can change the bytes without advancing the document revision.
See the [byte-buffer codec](../../crates/rca-persistence/src/lib.rs) and its [ownership tests](../../crates/rca-persistence/tests/document.rs).

The application defines the format, validation, defaults, relationships, and migration policy.
If older writers must preserve newer fields, retain their opaque representation or reject the write before replacing data.
Use an older writer to edit data from the new version. Read the result with the new reader.
Include defaults, relationships, and the switch to the new format in this test.
The persistent counter instead [rejects unknown versions](../../examples/persistent-counter/README.md#choose-the-document-type).
