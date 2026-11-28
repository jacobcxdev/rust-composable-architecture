# Child work and completion

## Route work to the original child

For a reducer with one required child, use `scope`. For optional state or an enum case, use `if_some` or `if_case`.
For one worker per document, keep each worker's state in `KeyedState<DocumentId, WorkerState>`.
Route `KeyedAction<DocumentId, WorkerAction>` through `for_each(parent, state_path, action_path, child)`.
One child reducer serves all entries. Put per-document progress in child **state**, not in fields of that reducer.
Use a stable domain ID, not a list position. The repository README includes a runnable keyed test with identity paths.

Reordering an entry preserves its lifetime. Removing it and reinserting the same ID starts a new lifetime.
Return the effect from the child reducer. Deliver its result through that effect, not by looking up the ID in the root Store.
RCA rejects an old child's queued output before reduction. This does not require an application generation counter.
If a late result belongs to an external protocol, keep that protocol's own request identity or acknowledgement.
RCA cannot undo a request that already reached the server.

For replacement **within** one live child, give competing requests a cancellation identity.
For example, `.cancellable_in_flight(SearchRequestId)` retires the previous search at the same scope.
If each child needs its own feature instance or typed context, use a mount. See `docs/usage/structural-features.md` in the matching revision.
After `Store::ready`, acquire the context from its typed mount site. A retained context belongs to its original installation.
`Store::scope` projects from the root. It does not track the child lifetime. Do not retry through the root after a context expires.

## Wait for the right thing

| What the caller needs | What establishes it |
| --- | --- |
| An action entered the mailbox | `Store::try_send` returns an `ActionReceipt`. It can still fail before commit. |
| The action committed | `Store::send(action).await` or `receipt.finish().await` succeeds and returns an `ActionTask`. |
| Work tracked for that action and its descendants ended | Await `task.finish()` outside that work. Completion does not mean domain success. |
| An independent consumer processed a notification, or a database saved a change | Wait for the consumer or database to acknowledge it. Neither Store wait proves this. |

Observations can coalesce state changes. They are not an event log.
For membership and order alone, `observe_ordered_ids` avoids projecting every child's value.
Store shutdown cancels pending effects. Finish the required finite work before shutdown.
For subscriptions, cancellation, and graceful closure, see `docs/usage/receipt-lifetimes.md` in the matching revision.
