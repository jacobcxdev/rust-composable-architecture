# Name search tasks and group jobs by account

The [runnable example](../examples/named_operations.rs) starts two independent
searches with `Task::run_named`.
The test runs the same searches.

Run these commands from the repository root. On macOS, use the [Cargo memory guard](../../../CONTRIBUTING.md#build-memory-safety).

```sh
cargo run -p rca-example-composition-search --example named_operations
cargo test -p rca-example-composition-search --example named_operations
```

`SearchFeature::update` contains the application logic. The `controlled` module lets
the test choose the order of events.
It keeps the actual runtime futures pending until the test releases a response or the
runtime destroys its future.
Applications use their usual runtime and clients.

Each task captures the query at the start. Edits to the query do not change a running
search.
After the client responds, the task reads the current limit through
`TaskContext::read`.
It moves the result into state through `TaskContext::update`. The read and update are
separate actor requests.
If the limit must apply at commit time, read it inside the update closure.

A new search clears the old results and failure. A failure from one source does not
discard a result from the other.
The example retries the search successfully. Only starting a search or editing input
needs a domain action.
The tasks report results without cloning them or sending progress actions.

## Count active tasks

Both tasks use the name `"search"`. Each declaration creates a separate generation.
To keep previous searches running, both tasks use `Coexist`.
To replace previous searches, only the first task uses `Replace`. The second uses
`Coexist`.
A second `Replace` cancels the new first task. RCA processes the declarations in
order, not as one group.

| Event | Active generations |
| --- | ---: |
| Before the first search | 0 |
| First two-task search starts | 2 |
| Second two-task search starts with `Coexist` | 4 |
| Replacement starts while the runtime still holds the four old futures | 6 |
| The runtime polls cancellation and destroys the four old futures | 2 |
| The Store acknowledges explicit cancellation but the runtime still holds the two replacement futures | 2 |
| The runtime destroys the first replacement future | 1 |
| The runtime destroys the second replacement future | 0 |

A wait captured before replacement ends after its original four generations finish. It
does not include the pending replacement pair.
To cancel and wait for specific generations, capture the wait first. Then request
cancellation. Finally, await that wait.
Capture and cancellation are separate mailbox requests. Cancellation can include newer
generations that the wait excludes.
The example starts no new work between those requests.

An acknowledgement from `cancel()` means the Store processed the request. The
scheduler still has to poll or drop the work.
Neither acknowledgement nor completion rolls back a remote side effect or guarantees
durable cleanup.
The test counts how often each response future drops. It also checks that state
receives the original String allocations.
Its explicit polls do not measure scheduler fairness, allocation costs or throughput.

## Group independently replaceable jobs by account

The same executable runs `AccountJobs` with the same controlled host and client.
Each job has an account and a job ID. The task has two names:

```rust
Task::run(move |context| async move {
    // Await the client, then record its outcome through this context.
})
.named(job, OperationPolicy::Replace)
.named(job.account, OperationPolicy::Coexist)
```

The last `named` call forms the outer group. The inner job key includes the account.
Account A's job 1 cannot replace account B's job 1. The account uses `Coexist` so
starting a job keeps its siblings running.
For one name, use `Task::run_named(key, policy, body)`. Owner-thread `local::Task`
supports the same groups for non-`Send` task bodies.

An operation endpoint targets one feature scope. Equal names in another scope or Store
do not join these groups.
Adding an outer name does not give the task authority over a different child.
`store.operation(job).cancel()` targets the current generations of one job.
`store.operation(job.account).cancel()` targets the current generations of the
account. Both return futures that the caller must await.

The example starts A/1, A/2 and B/1, then replaces A/1. A/2 and B/1 keep running.
An earlier wait for account A ends after the old A/1 stops and A/2 returns its result.
The new A/1 remains pending.
Cancelling account A then stops only the new A/1. The account still reports one active
generation until the runtime destroys that task.

`status()` reports a snapshot at the point where the actor handles the request. It
does not track future changes or keep an error history.
An operation can complete with a business failure. For example, B/1 records
`Err("offline")` in a `JobResult` through a typed state update.
