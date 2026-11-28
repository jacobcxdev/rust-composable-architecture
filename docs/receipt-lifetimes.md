# Action receipt lifetimes

An action can enter the queue, change state and start a task at different
times. Choose the wait that matches the result you need.

- **Admission** means that an action entered the bounded input queue.
- **Commit** means that the actor processed the action and registered its follow-up work.
- **Causal completion** means that this action's tracked work and descendants ended, including work that ended through cancellation.

Even completed work can have failed or ended through cancellation. If success
means a saved document or a closed connection, check the result from that service.

| Handle or operation | What it establishes | What dropping it does |
| --- | --- | --- |
| `Store::try_send` returns `ActionReceipt` | The action entered the mailbox, not that it committed | Does not retract the admitted action |
| `ActionReceipt::finish` or `Store::send` returns `ActionTask` | The actor committed the action and registered its causal work | Dropping the task does not cancel that work |
| `EffectSender::send(...).await` | That feedback action committed | Does not await the whole ancestor lineage |
| `ActionTask::finish().await` | All work in that action's causal lineage finished | Dropping this ordinary wait does not cancel work |
| `ActionTask::cancel()` | Cancellation was requested for the lineage | Does not synchronously destroy futures or undo commits |
| `ActionTask::finish_cancelling_on_drop()` | Installs an owning wait that requests cancellation when dropped | Requests cancellation even if the wait was never polled |

## Choose the wait that answers your question

| Question | Operation | What it does not establish |
| --- | --- | --- |
| Did this admitted action commit? | Await its `ActionReceipt`, or call `ActionReceipt::finish` | Follow-up work can still be pending. |
| Did all work caused by this action end? | `ActionTask::finish` | Cancelled or failed domain work also ends. Unrelated actions are not included. |
| Is the feature structurally installed? | `Store::ready` | Business work and initial asynchronous tasks need not be complete. |
| Has a selected state condition become false? | `Store::wait_while` | Transient states can coalesce. This is not task completion. |
| Did this consumer receive a selected value? | `Observation::next` returns `Some(value)` | Other consumers can remain pending. Delivery does not imply processing. |
| Did this one-shot callback return? | `Observation::on_change(...).await` returns `true` | Asynchronous work started by the callback can remain pending. This does not acknowledge other consumers. |
| Did the Store close? | `Store::closed` | Closure can be abortive. This is not successful graceful shutdown. |
| Did the actor process orderly shutdown? | `Store::shutdown` | Pending effects are cancelled, not drained to success. External cleanup is separate. |
| Can I take the final owned state? | `Store::shutdown_into_state` | Extraction does not first finish every effect. Establish required completion before extraction. |

`ActionReceipt::finish` and `ActionTask::finish` share a name but wait for
different boundaries. The receipt yields a task after commit. The task's wait
returns `()` when tracked work ends. Cloned tasks track the same lineage, not
independent copies. Cancellation through any clone affects that shared lineage.

Commit publishes selected state before its reply, but it does not wait for
independent observation consumers. Their processing needs an explicit application
acknowledgement when completion matters. Observations can coalesce intermediate
values. See the [observation contract](runtime.md#observation) and
[notification adapter rules](integration-boundaries.md#define-external-notification-delivery).

The receipt remains fallible after successful admission. Store shutdown can win
before commit. For `ScopedStore::try_send`, the child can also end before its
queued action runs. That receipt returns `StoreStopped` for either case, even
when the root Store remains open. The error does not justify retrying through
the root to bypass child-lifetime checks.

For finite work, await the action's task before shutdown. For a subscription,
request cancellation, then await its task while the host remains active. A
long-lived subscription can otherwise keep `finish` pending indefinitely.
Graceful shutdown refers to ordered actor closure, not successful completion of
every effect. A cancelled network request can still have changed the remote service.

Use `finish_cancelling_on_drop` when abandoning the wait must also request
cancellation. The policy starts only after a task is acquired. Dropping an
earlier send or receipt future does not install that guard.
The [receipt API example](../rca-core/src/store/receipt.rs) shows both waits.

## Keep ancestor completion waits outside their descendants

An action starts an effect. Suppose that effect receives the original action's
`ActionTask` and awaits its `finish` method. The effect now waits for all the
action's work, including itself. Neither can finish. `ActionTask` does not
report a self-wait error, and yielding the actor does not break the cycle.

Keep the ancestor handle with an external owner. Let descendants await ordinary
`EffectSender::send` feedback when they need acknowledgement that an action
committed. Await the ancestor's completion only outside its own lineage. A
timeout or cancellation watchdog must also run outside that lineage, so it can
request cancellation without first waiting for the cycle to finish.

After requesting cancellation, keep the host running. The actor must receive
the request. The runtime must poll or drop the aborted descendant, and the
actor must record its completion. Dropping an ordinary `finish` wait does not
break the cycle. Dropping a `finish_cancelling_on_drop` wait requests
cancellation. It does not make it safe for a task to wait for itself.

Named-operation waits have their own guards. They do not detect cycles between
arbitrary tasks or Stores, or change how `ActionTask` works.

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
