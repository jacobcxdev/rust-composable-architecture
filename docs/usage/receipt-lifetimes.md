# Know when an action is done

An action can enter a Store, change its state, and start work at different times.
For example, a save button sends an action. The Store can set its state to “Saving” before the file exists.
Which result does the caller need: space in the queue, the new state, or a successful save?
If Stores and actions are new to you, start with the [basic model](authoring.md#the-basic-model).

## From admission to completion

1. **Admission:** the Store accepts the action into its input queue. The queue has limited space.
2. **Commit:** the Store processes the action, registers its follow-up work, and publishes the selected state values.
3. **Tracked completion:** the work RCA tracks for the action ends. This includes its effects and tasks, and the work they start through RCA. Together these form the action's **lineage**.

RCA also calls the third step **causal completion**. Failure and cancellation both end work.
Completion alone does not say that the work succeeded. To know whether a write succeeded, use the [document's save receipt](persistence.md#save-and-close-a-document).

| If you need to know… | Use | On success |
| --- | --- | --- |
| Can the Store accept this action now? | `Store::try_send` | You get an `ActionReceipt`. Dropping it does not remove the action from the queue. |
| Did the Store commit this action? | `receipt.finish().await`, or `store.send(action).await` to send and wait together | You get an `ActionTask`. The work it tracks can still be running. |
| Did an effect's feedback action commit? | `EffectSender::send(...).await` | You get `Ok(())`. Other work from the original action can still be running. After admission, abandoning this wait does not retract the feedback. |
| Did all work tracked for this action end? | `task.finish().await` | You get `()`. Failed and cancelled work also ends. This wait excludes unrelated actions. |

If the queue is full or the Store is closed, `try_send` returns an error that retains the original action.
The two `finish` methods answer different questions. In the [receipt example](../../crates/rca-core/src/store/receipt.rs), the first wait returns a task. The second waits for its work.
Cloning a task does not duplicate that work. Each clone tracks the same lineage.

Successful admission does not guarantee a commit. For example, the Store can stop before it commits the action.
With `ScopedStore::try_send`, the selected child can also disappear while its action waits in the queue.
In both cases the receipt returns `StoreStopped`. The root Store can still be running.
Do not retry the child's action through the root. The old action can then reach a replacement child.

## Choose the wait that answers your question

Some waits concern the Store, not one action. Others concern a single observer, a consumer that receives selected state values.

| Question | What to wait for | What it does not prove |
| --- | --- | --- |
| Can I use the configured feature tree? | `Store::ready` succeeds after the Store installs the features. | Initial asynchronous tasks and application work can still be running. |
| Is a condition false now? | `Store::wait_while` returns `PredicateWaitOutcome::Satisfied` after it sees a false value. | A short-lived false value can disappear before the observer sees it. Cancellation and closure have other outcomes. |
| Did this consumer receive a selected value? | `Observation::next` returns `Some(value)`. | Receipt does not mean that the consumer processed it or that another consumer received it. |
| Did this one-shot callback return? | `Observation::on_change(...).await` returns `true`. | Work the callback started can still be running. The result says nothing about other callbacks. |
| Did the Store close? | `Store::closed` returns. | The Store can close without an orderly shutdown. |
| Did the Store process a shutdown request? | `Store::shutdown` succeeds. | The Store cancels pending effects. It does not wait for them to succeed or for external services to finish cleanup. |
| Do I own the final state? | `Store::shutdown_into_state` succeeds. | Extraction does not wait for every effect. Wait for the required results before requesting extraction. |

The Store publishes selected state before it replies to the sender. It does not wait for independent consumers.
An observer can receive only the newest of several values. If a caller needs proof that another consumer finished, that consumer must acknowledge its own result.
Unlike `next`, `on_change` skips the initial value. It waits for one publication after a commit.
Await the returned future so the runtime can poll it (ask it to make progress).
If the observation closes before RCA admits the callback, the future returns `false`.
See [observation](runtime.md#observation) and [notification delivery](integration-boundaries.md#define-external-notification-delivery).

## Cancel work or stop waiting

Dropping an ordinary `ActionTask` or its `finish` wait does not cancel the work.
For a finite job, keep the runtime active until the task finishes. A subscription can run indefinitely.
To end a subscription, call `task.cancel()` and then await `task.finish()`.
Cancellation through any clone requests cancellation for the same work.
It does not immediately destroy pending futures, undo a commit, or reverse a change on a remote service.

If abandoning the wait must also cancel unfinished work, call `task.finish_cancelling_on_drop()`.
Dropping the returned wait requests cancellation. The runtime does not have to poll it first.
Calling the method on a task installs this rule. Dropping an earlier send or receipt does not install it.

## Keep ancestor completion waits outside their descendants

Suppose an action starts an effect. The effect receives the original action's task and waits for that task to finish.
Now the effect is waiting for itself. Neither can finish. `ActionTask` reports no self-wait error.
Letting the Store run other work does not break the cycle.

Keep the original task with a caller outside the lineage. An effect can still await `sender.send(...)` to learn whether its feedback action committed.
Put a timeout outside the lineage too. It must be able to request cancellation without waiting for the stuck work.

After a cancellation request, keep the host running. The Store must process the request.
The runtime must poll or drop the cancelled effect. The Store must then record its completion.
Dropping an ordinary wait does not break the cycle. Even a wait that cancels on drop cannot make it safe to wait for itself.
Named operations have separate wait guards. They do not detect arbitrary cycles between tasks or Stores.

## Cooperative budget boundaries

The Store periodically yields so the runtime has an opportunity to run another task.
This is not a deadline. The Store cannot interrupt a reducer or a future that blocks the runtime.

Without active timers, the Store counts consecutive commands that are ready to run.
An empty queue or a turn spent on control requests does not by itself reset the count.
Only a wait for control work that returns `Pending` resets this count.
Commands with active timers and timers that are due use separate bounded batches.

The [scheduling tests](../../crates/rca-test/tests/actor_cooperative_budget.rs) show that another ready task can run before the Store exhausts its backlog.
The [receipt-cycle tests](../../crates/rca-core/tests/receipt_cycles.rs) exercise the same behaviour without the test assertion gate.
One test lets the cycle persist through two polls, with an independent Store read between them.
It then requests cancellation from outside the cycle. It checks resource release, tracked completion, and Store closure without sleeps or detached tasks.
