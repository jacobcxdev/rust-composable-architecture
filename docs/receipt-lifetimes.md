# Action receipt lifetimes

Admission, commit and causal completion are different boundaries.

| Handle or operation | What it establishes | What dropping it does |
| --- | --- | --- |
| `Store::try_send` returns `ActionReceipt` | The action entered the mailbox, not that it committed | Does not retract the admitted action |
| `ActionReceipt::finish` or `Store::send` returns `ActionTask` | The actor committed the action and registered its causal work | Dropping the task does not cancel that work |
| `EffectSender::send(...).await` | That feedback action committed | Does not await the whole ancestor lineage |
| `ActionTask::finish().await` | All work in that action's causal lineage finished | Dropping this ordinary wait does not cancel work |
| `ActionTask::cancel()` | Cancellation was requested for the lineage | Does not synchronously destroy futures or undo commits |
| `ActionTask::finish_cancelling_on_drop()` | Installs an owning wait that requests cancellation when dropped | Requests cancellation even if the wait was never polled |

## Keep ancestor completion waits outside their descendants

A descendant effect can receive an old `ActionTask` for its ancestor. If it
awaits that task's `finish`, the ancestor is waiting for the descendant to end,
while the descendant is waiting for the ancestor's entire lineage to end.
The existing `ActionTask` API remains passively pending in this cycle; it does
not return a self-wait error. Yielding the actor cannot resolve this dependency.

Keep the ancestor handle with an external owner. Let descendants await ordinary
`EffectSender::send` feedback when they need acknowledgement that an action
committed. Await the ancestor's completion only outside its own lineage. A
timeout or cancellation watchdog must also run outside that lineage, so it can
request cancellation without first waiting for the cycle to finish.

After requesting cancellation, continue driving the host. Cancellation must
reach the actor, the runtime must poll or drop the aborted descendant, and the
actor must retire its completion before causal completion is established.
Dropping an ordinary `finish` wait alone does not break the cycle. An owning
`finish_cancelling_on_drop` wait can request cancellation, but it still cannot
make an ancestor-completion await inside that ancestor's descendant acyclic.

This guidance concerns existing receipts. It does not promise detection of
cross-store or arbitrary foreign-task cycles. Separate named-operation wait
guards must not be taken as proof that old `ActionTask` handles gained a cycle
detector.

## Cooperative budget boundaries

The actor already limits consecutive ready command work with a private budget.
Timer-free turns retain their count across ready ingress and control-only
continuations, then yield at the limit. The count resets when the control
listener actually returns `Pending`; an empty queue alone is not a reset.
Timer-active commands and due timers have their own existing bounded batches.
These mechanisms allow scheduling opportunities, not a wall-clock latency
guarantee, and cannot pre-empt a blocking reducer or user future.

The qualification tests are `rca-test/tests/actor_cooperative_budget.rs` and
`rca-core/tests/receipt_cycles.rs`. The latter also runs without `test-util`,
so assertion-gate scheduling cannot supply the fairness being tested. Its
external watchdog polls the actual descendant twice with independent actor
read progress between polls, then cancels and verifies capture release,
causal completion and actor closure. It uses no sleeps or detached tasks.
