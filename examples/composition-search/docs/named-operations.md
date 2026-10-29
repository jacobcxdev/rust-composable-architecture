# Two-task named search

The [runnable recipe](../examples/named_operations.rs) starts two independent
search sources through `Task::run_named`. Its inline test executes the same trace:

```sh
cargo run -p rca-example-composition-search --example named_operations
cargo test -p rca-example-composition-search --example named_operations
```

Read `SearchFeature::reduce` for application authoring. The `controlled` module
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
