# Measure and improve your app's performance

Measure the result your caller needs, not just how fast the Store accepts an action.
Suppose your app imports a file. Putting the request in a queue, changing the Store's state, and displaying the imported records are different milestones.

## Choose where the measurement ends

| Question | Stop the measurement here |
| --- | --- |
| How long does it take to submit the import? | `try_send` succeeds. The Store accepted the action into its queue. The import need not have started. |
| How long until the Store processes the request? | `send(action).await` succeeds. The Store processed the action and registered its follow-up work. That work can still be running. |
| How long until someone can use the records? | The intended caller can read the correct imported records. Include the work that makes them available to that caller. |

RCA calls the first milestone **admission** and the second **commit**.
`ActionTask::finish` waits for work that RCA tracks for the action. It does not prove success or wait for an independent consumer.
The [receipt guide](receipt-lifetimes.md) explains these waits. The [counter](../../examples/counter/src/main.rs) shows how to read a committed result.

## Compare the same work

Use a [release build](authoring.md#choose-a-release-profile) of the app, fixed input, and a result you can check.
Compare both versions on the same device. Keep their build settings, input, and required result the same.
Repeat each measurement. For a stream of requests, compare the slow requests as well as the typical one.

Elapsed time measures the caller's wait. CPU time measures the processor time the app uses, including work outside the calling thread.
A reduction in allocations does not by itself prove that the caller waited less.
If elapsed time varies more than the difference between versions, the run does not establish a speedup.

## Do only the work the caller needs

- **Group records.** If only the finished group matters, prepare the records in ordinary Rust. Publish the group with one action.
  If callers need intermediate results, the first error, or cancellation between records, preserve those points.
- **Observe a small value.** `observe_distinct` skips equal updates, but its selector still runs on each eligible commit.
  A selector reads the part of the state that the observer needs. For membership and order alone, use `store.observe_ordered_ids(|s| &s.children)`.
  An edit to a child value reuses the ID list. If a row needs its child's value, observe the row itself.
  See [observation](runtime.md#observation) and the [row example](../../examples/composition-search/src/main.rs).
- **Keep state changes short.** Give expensive computation to a worker. Use an effect to await its result without blocking the Store.
  The [uploader example](../../examples/composition-search/docs/uploader.md) shows how to report progress and reject results from a replaced child.
