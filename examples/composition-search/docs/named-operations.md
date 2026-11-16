# Named search tasks and account groups

The [runnable recipe](../examples/named_operations.rs) starts two independent
search sources through `Task::run_named`. Its inline test executes the same trace:

```sh
cargo run -p rca-example-composition-search --example named_operations
cargo test -p rca-example-composition-search --example named_operations
```

Read `SearchFeature::update` for application authoring. The `controlled` module
only makes the proof deterministic: it retains **actual runtime futures**, and
its client responses remain Pending until released or destroyed. Applications
use their normal runtime and async clients; they need no retaining scheduler.

Each task captures the query when the search starts. After its client await, it
reads the current limit through `TaskContext::read` and moves the owned result
into state through `TaskContext::update`. Editing the query while suspended does
not rewrite that captured input. A new attempt clears the previous failure and
results. The example reports a failed source without discarding its successful
sibling, then retries successfully. No progress-tick actions or result-cloning
dispatch are needed: only starting a search or editing input is a domain action.

The read and update are separate actor requests. The captured limit is current
when the read runs, not necessarily when the update runs. For a commit-time
policy, read the relevant state inside the update closure instead.

Both tasks use the `"search"` identity, but each declaration is a distinct
generation. For coexistence, both use `Coexist`. For replacement, the **first**
uses `Replace` and the second uses `Coexist`, in declaration order. Replacing
twice would cancel the newly declared first sibling. This is not an atomic
two-task-group API.

| Endpoint observation | Active generations |
| --- | ---: |
| Initially | 0 |
| First two-task search | 2 |
| Second search coexists | 4 |
| Replacement requested; four old futures still retained | 6 |
| Old four futures consume cancellation and are destroyed | 2 |
| Explicit cancellation acknowledged; replacement futures retained | 2 |
| First replacement future destroyed | 1 |
| Second replacement future destroyed | 0 |

A wait captured before the replacement completes when its original four
generations finish, while the replacement pair remains Pending. It never
retargets later work. For cancel-and-await, capture the required set, request
cancellation, then await that captured wait. Acknowledging `cancel()` means the
request was processed, **not** that client futures or their captures are gone.
Capture and cancellation are separate mailbox requests, not an atomic operation.
New generations between them can be cancelled without joining the saved wait.
This trace admits no new work between those requests.
The scheduler must still poll or drop that work. This guarantees owned-future
completion, not rollback of remote side effects or durable external cleanup.

The trace checks exact per-response destruction counts and original successful
String allocations through task update and final state handoff. Its explicit
polls are a bounded semantic witness, not a scheduler fairness test or benchmark.
It makes no equal-work allocation or throughput claim.

## Group independently replaceable jobs by account

The same executable also runs `AccountJobs`, using the existing controlled host
and client. Each job has an account and a job ID. The feature declares its task
with two names:

```rust
Task::run(move |context| async move {
    // Await the client, then record its outcome through this context.
})
.named(job, OperationPolicy::Replace)
.named(job.account, OperationPolicy::Coexist)
```

The last `named` call forms the outer group. The job key includes its account,
so account A's job 1 does not replace account B's job 1. The outer account name
uses `Coexist`, so a new job does not cancel other jobs in that account.
`Task::run_named(key, policy, body)` remains the convenience for one name.
Owner-thread `local::Task` supports the same composition with non-`Send` task bodies.

All jobs in this example belong to one feature scope. An operation endpoint must
address that same scope. Equal keys in another scope or Store do not form a
global group. Nested names do not change captured child authority.
`store.operation(job).cancel()` targets that job's current generations.
`store.operation(job.account).cancel()` targets the account's current generations.
Both calls return futures that the caller must await.

The trace starts A/1, A/2 and B/1, then replaces only A/1. A/2 and B/1 remain
pending. A wait captured for account A before replacement excludes the new A/1
generation. It completes after old A/1 ends and A/2 returns its result, while
new A/1 remains pending. Cancelling account A then stops new A/1 without
cancelling B/1. The account still reports one active generation until that
cancelled body is destroyed.

`status()` returns a snapshot at an ordered actor position, not a live property.
The names supply cancellation and completion groups, not a generic error history.
Represent client failures with domain actions or owned outcomes in State.
Here, B/1's `Err("offline")` becomes a `JobResult` through a typed state update.
Its operation still completes: completion does not mean business success.
