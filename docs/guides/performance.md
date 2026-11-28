# Measure and improve your app's performance

Measure what the caller needs, not just how fast the Store accepts an action.
Use a release build of your app, a fixed input, and a result you can check.
Compare the same work on the same device. For a stream of requests, check how
long the slow ones take as well as the typical request.
See [how to choose a release profile](authoring.md#choose-a-release-profile).

## Choose where the measurement ends

For a file import, these are three different measurements:

- **Time to admission:** `try_send` succeeds once the Store accepts the action. The import may not have started.
- **Time to commit:** `send(action).await` succeeds once the Store processes the action and registers its follow-up work. The file may still be open.
- **Time to a usable result:** if the caller needs the imported records, stop when that caller can read the correct records. `ActionTask::finish` waits for tracked work to end, not for success or for an independent consumer to finish.

The [receipt guide](../receipt-lifetimes.md) explains these waits. The [counter](../../examples/counter/src/main.rs) shows how to read a committed result.

## Do only the work the caller needs

- **Process a group of records together** if only the finished group matters. Use ordinary Rust to prepare it and one action to publish it. If callers can read an intermediate result, cancel at a checkpoint, or need the first error before processing continues, keep that boundary.
- **Select the smallest useful value.** `observe_distinct` skips equal deliveries, but its selector still runs on each eligible commit. If a list needs only membership and order, use `store.observe_ordered_ids(|s| &s.children)`. An edit to a child value then reuses the ID list. If a row needs that child's value, observe the row instead. See [observation](../runtime.md#observation) and the [row example](../../examples/composition-search/src/main.rs).
- **Keep long work outside Store transactions.** Use a worker for expensive computation and an effect to await its result. Keep the state change short. The [uploader example](../../examples/composition-search/docs/uploader.md) shows how to report progress and reject results from a replaced child.
