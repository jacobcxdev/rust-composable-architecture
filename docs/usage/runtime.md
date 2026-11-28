# RCA runtime contracts

A Store processes one command at a time. Your application supplies the runtime that runs it.

For example, the [counter](../../examples/counter/src/main.rs) sends an increment, reads the new value, and shuts down.
Tokio runs that example. RCA controls the order of state changes, not which thread runs the next task.
If this is your first feature, start with [writing a feature](authoring.md).
For a running Store, see [when a send returns](#admission-and-ordering), [which state a reader sees](#observation), or [how to stop the Store](#shutdown-and-host-lifetime).

## Choose a runtime and ownership model

The **host** is the application that keeps the runtime running. A **driver** is the adapter between RCA and that runtime.
A Store's **actor** processes its commands in order. An actor does not need its own thread.
Cloning a `Store` gives you another handle to the same actor and state.

| Where will the Store run? | Use | What the host supplies |
| --- | --- | --- |
| An existing Tokio application | `Store` with `TokioRuntime::current()` | A running Tokio runtime with timers enabled. Enable RCA's `tokio` feature for this adapter. |
| Another asynchronous host | `Store` with a shared `Runtime` | A [driver](#runtime-driver) that runs accepted tasks. State and work must meet Rust's `Send` bounds so they can move between threads. |
| One owner thread, with values such as `Rc` that cannot move between threads | `local::Store` with `local::Runtime` | An owner-thread driver and RCA's `local` feature. A single-thread Tokio runtime alone does not remove `Store`'s `Send` bounds. |
| An application test | `TestStore` | The test runs ready work and advances the clock. See [testing](testing.md). |
| A synchronous caller | An application-owned runtime | The calling thread or a background thread must run it. See [synchronous hosts](#synchronous-hosts). |

A `Runtime` handle gives the Store access to its driver. It does not keep the host running.
A `Clock` handle can read time and wait for a deadline. It cannot start tasks.
For deadlines that the Store and a client share, give the client that runtime's clock.
Two clocks on the same host can measure time from different starting points.

`rca-core` provides no executor to run futures, reactor to process I/O, wall clock, or native event loop.
There is no global Store or implicit permission to use a Store from a thread.
The [Local composition guide](local-composition.md) distinguishes ordinary synchronous calls from work that needs an owner-thread runtime.

## Feature execution

A feature describes what an action does to the state. The actor owns both the feature and the state.
The feature borrows the action and state for the duration of its callback. That callback cannot suspend.
The feature can declare later work without starting it inside the callback.

Every `Reducer` implements `Feature` automatically. A reducer returns effects that can send actions back to the Store.
A direct `Feature::update` implementation can also declare typed tasks that read or update state through the actor.
Both forms use the same Store constructors. See [structural features](structural-features.md) for child installation, typed tasks, and their lifetimes.

## Reducer transaction

A **transaction** is one unit of work that the actor processes without another command changing state in the middle.
At **commit**, the Store publishes the selected state and registers follow-up work. That work need not have finished.
The actor owns the reducer, state, dependency environment, timers, and records of effects and observations. For each action:

1. The actor records the action's source and cancellation generation. A generation distinguishes old work from its replacement.
2. It calls the reducer with `&mut State`, `&Action`, and `&mut ReduceContext`. It invalidates departed child scopes and refreshes their handles.
3. It increases the commit version and prepares the returned work. It registers that work in source order without running it.
4. It publishes the values selected by observers. It replies to the sender with an `ActionTask` for this action and its tracked work.
5. It hands the prepared tasks to the runtime. The driver cannot poll a task inside `spawn` itself.

The reply goes out before the handoff. The caller need not resume before a task starts.
Nor does the reply prove that an independent observer processed the new value.
If that result matters, wait for the consumer's own acknowledgement.

The reducer cannot await. The actor owns state until it drops the state or returns it through [final state extraction](#terminal-owned-state-extraction).
RCA does not require all state or action types to implement `Clone`.

## Admission and ordering

The Store's **mailbox** is its bounded input queue. **Admission** means the Store accepted a command into the queue.
The default capacity is `DEFAULT_MAILBOX_CAPACITY`. A Store builder accepts a different non-zero capacity.

| What does the caller need? | Operation | Result |
| --- | --- | --- |
| Wait for capacity and commit | `store.send(action).await` | On success, the action committed. Its follow-up work can still be running. |
| Return without waiting for capacity | `store.try_send(action)` | On success, an `ActionReceipt` tracks the commit. A `TrySendError` gives the caller back the original action. |

If `is_full()` is true, choose a policy: retry, combine equivalent inputs, or discard the input.
If `is_store_stopped()` is true, retrying on the same Store cannot succeed. Both errors return the action through `into_action()`.
If the caller needs to know that the action committed, await the receipt. To inspect state, use an [ordered read](#observation).
See [receipt lifetimes](receipt-lifetimes.md) for the difference between commit, completion, and shutdown.

The actor orders external actions, effect actions, reads, observation commands, cancellation, and shutdown.
An effect's `sender.send` succeeds only after its action commits.
After admission, dropping the send future does not remove the action from the queue.
Cancellation can still invalidate an effect's queued action before the reducer runs.
A busy input queue does not indefinitely postpone cancellation requests, due timers, or observation cleanup.

## Runtime driver

Most applications use the supplied Tokio or test runtime. An application with its own executor can implement [`RuntimeDriver`](../../crates/rca-core/src/runtime.rs).
The driver starts tasks (`spawn`), reads monotonic time (`now`), waits (`sleep_until`), and gives other tasks a chance to run (`yield_now`).
Monotonic time measures elapsed time from one origin. It does not move backwards. Clones of a runtime share the same origin.

A shared driver's futures must be `Send + 'static` so its executor can move work between threads.
The [local driver](../../crates/rca-core/src/runtime/local.rs) keeps futures on their owner thread. Its futures must be `'static` but need not be `Send`.
For a shared driver:

- Accept a task promptly without running it inside `spawn`.
- While the host runs, poll accepted tasks according to Rust's `Future` contract. A poll asks whether a future can make progress.
- Accept wake-ups from any thread without an RCA actor lock.
- If a deadline has passed, complete the wait on its first poll.
- For a future deadline, complete the wait no earlier than that deadline. A late wake-up is possible.
- When a caller drops a wait, cancel that wait and release its captured resources.

During teardown, the host can drop pending tasks. Dropping a runtime future ends it and releases its captures.
The affected Store can close without orderly shutdown. RCA does not move work to a different runtime.

[`TokioRuntime::current`](../../crates/rca-tokio/src/lib.rs) works on current-thread and multithread Tokio runtimes.
Without an active runtime, it returns an error. Without a timer driver, Tokio calls the panic hook.
On builds that unwind, the adapter returns a missing-timer error. With `panic = "abort"`, the process aborts instead.
`DeterministicRuntime` runs ready tasks under a poll limit. For waits with equal deadlines, it preserves their order.
Time advances only at the test's request.

### Synchronous hosts

Several Stores can share one Tokio runtime. The [synchronous host examples](../../crates/rca-tokio/tests/synchronous_host.rs) show two choices:

- **A finite command:** Enter a current-thread runtime to construct the Stores. Use Tokio's `Runtime::block_on` to run their work, wait for completion, and take the final state.
- **A callback while the app stays open:** Keep the runtime running on a background thread. The caller uses `try_send`. Outside executor threads, it can use Tokio's `Handle::block_on` to await a receipt or task.

Entering a runtime makes it available for construction. Entering alone does not run it.
Do not block the only thread that can run the Store. Inside an async task, await the result instead of nesting `block_on`.

If a callback can return before commit, it can use `try_send` and collect the result elsewhere.
If the callback must return a committed reply, the driver must make progress before it returns.
`try_send` allocates memory. It offers neither a hard real-time deadline nor a lock-free audio callback.

If a caller owns its data and needs only a local answer, a synchronous Rust function is sufficient.
Its return does not publish a Store change or notify Store observers.
If the application needs tracked work to end, await the receipt, then await the task's `finish` before extraction.
Keep the host running until shutdown completes. Native event loops, thread-bound resources, and operating-system services have [additional requirements](integration-boundaries.md).

## Effects

An `Effect` describes work for the Store to start after commit.
The runtime decides when a ready task runs.

| Operation | What it does |
| --- | --- |
| `none` | Schedules no work. |
| `send` | Requests an immediate follow-up action. The actor still processes it in order. |
| `run` | Starts a future with an `EffectSender` for feedback. |
| `merge` | Registers child effects in source order without waiting for each child to finish. |
| `concatenate` | Starts each child after the previous child finishes. |
| `send_after`, `send_at`, `delay`, `interval` | Schedule work using the Store clock. See [time](#time). |
| `cancel` | Cancels all live generations for one typed identity. |
| `cancellable` | Associates an effect with a cancellation identity. |
| `cancellable_in_flight` | Cancels old generations, then starts the replacement. |

The shared Store requires `Send + 'static` captures. A [Local Store](local-composition.md) can run an effect with non-`Send` captures on its owner thread.
Both runtimes use cooperative scheduling: a future that blocks or never yields can prevent other work from running.

### Stream-driven effects

A stream supplies values over time. Inside `Effect::run`, read one input and await `sender.send(action)`.
If the send fails, stop the loop. Otherwise, read the next input after the actor commits the previous action.
Collecting the whole stream first changes memory use and how soon the client reads external input.

A long-lived subscription can keep the original action's `ActionTask::finish()` pending indefinitely.
Dropping that ordinary task handle does not cancel the subscription. If it needs a separate stop command, give it a cancellation identity.
The stream running out of values does not prove that the provider released its resources. Test cleanup with the actual client.

### Delayed work and debounce

For search text, the app can wait for typing to stop before starting a request.
`Effect::run(operation).delay(duration).cancellable_in_flight(id)` cancels the old wait without calling the obsolete operation.
Constructing or mapping the effect can still do immediate work. Put the client call inside `run`.
`send_after` delays an action that already exists.

A relative delay starts when its element in the sequence starts, not when the application builds it.
If the original deadline must survive surrounding delays or sequences, use `Effect::send_at(deadline, action)`.
Derive the deadline from that runtime's `now()`. RCA neither compares nor converts clock origins.
If the deadline has passed before the effect starts, the action becomes eligible through the normal actor queue.
It cannot arrive early, but can arrive late. `MonotonicInstant::MAX` stays pending until cancellation or shutdown.

A cancellation ID **outside** the delay owns the wait and the later work. An ID **inside** registers only after the wait.
For example, `effect.cancellable_in_flight(id).delay(duration)` does not cancel an old pending delay.
The original action's task tracks both phases. Removal of the owning child or shutdown cancels both.

The same distinction applies to `without_inherited_cancellation`. Outside the delay, it separates both phases from inherited cancellation.
Inside, it cannot restore a wait that the inherited ID already cancelled. Explicit cancellation IDs, the original action, and the child scope retain their ownership.
Zero duration leaves the effect unchanged. An empty effect stays empty. A deadline at the maximum remains cancellable.
Mapping a timed action does not change its owners.

If a child leaves while its composed effect waits, the Store discards that child's deferred work without starting it.
Other children keep waiting. If nothing remains, the Store cancels the wait.
Even within one batch of due timers, an earlier action can remove a child before the Store runs its timer.
The [delayed-child tests](../../crates/rca-core/tests/delayed_scope_retirement.rs) cover these cases.

## Structured cancellation

A cancellation ID lets an application stop one operation without stopping unrelated work.
A child scope identifies the lifetime of one installed child, not just its application ID.

### Domain-owned request deadlines

For a visible timeout, represent the request and deadline as two effects with different cancellation IDs.
The [request example](../../crates/rca-test/tests/request_deadlines.rs) implements this policy:

| Action | New state | What happens to the work |
| --- | --- | --- |
| Start | Running | Merge the request with `send_after(duration, Deadline)`. Each effect replaces earlier work under its own ID. |
| Response | Success or failure | Cancel the deadline. |
| Deadline | Timed out | Cancel the request. |
| Cancel | Cancelled | Cancel both. |

The first terminal action the actor commits wins. Distinct IDs let it cancel the other effect without cancelling itself.
Restart replaces both generations. Each keyed child has its own scope, so one child's deadline cannot cancel a sibling's request.
The actor rejects queued output from the generation that lost. The original action's task finishes after both branches release their work.

This is an application policy, not a real-time promise or a timeout for every kind of effect.
Cancelling the original action or shutting down does not fabricate a timeout action.
The shutdown reply means the Store issued aborts. Runtime tasks can release captured resources later.
The example polls the client send manually to exercise cancellation after admission. Application effects normally await `sender.send` directly.

### Finite batches and bounded retry

For a fixed set of jobs, the parent can keep a queue and reserve a child slot before it sends each Begin action.
Each child owns its attempt count, client, and retry timer. At a terminal result, the parent records the outcome, removes the child, and admits another job.

The [bounded job example](../../crates/rca-test/tests/bounded_jobs.rs) runs at most two jobs, with at most three attempts each.
It waits 10 ms before the second attempt and 20 ms before the third, using the Store clock.
Permanent errors stop immediately. A retrying child retains its slot, so fewer new jobs can start during that wait.

A **slot** bounds logical jobs. A **permit** bounds simultaneous use of the underlying resource.
Removing a child requests cancellation but cannot make the runtime drop its resource immediately.
The example's client has two permits. Each operation holds one until its resource drops, then releases it before awaiting the result send.
Even if the runtime delays cleanup, a replacement must wait for a permit.

A queued Begin action still belongs to the parent until the child starts. Give each pending launch a cancellation ID.
On removal, cancel the launch as well as the running job. Child removal alone cannot stop an unscoped parent action.
After launch, the child owns its work and retry timer.

An explicit batch-cancel action records outcomes and stops further admission.
Cancelling the original action or shutting down stops tracked work without rewriting domain state.
One action's receipt does not track independent commands sent later.
This recipe bounds a finite input, not an unbounded stream. The application chooses input limits, capacity, and retry policy.

### Parent-owned persistence

Most applications can start with the [document coordinator](persistence.md). A parent that needs a different close policy can own it explicitly.
The [single-writer example](../../crates/rca-test/tests/persistence_coordination.rs) keeps one save in progress.
Success acknowledges only the revision that the save captured. After later edits, the parent writes the newest revision next.
Even if the backend wrote the value, a failed acknowledgement leaves the state dirty until a retry succeeds.

Close stops edits and waits for the final save. After a failure, retry flush or close.
If no load has succeeded, this recipe cancels the load without saving the default. Later commands cannot reopen the value.
The document coordinator has a different rule for that case.

Before shutdown, observe the closed state. Finishing a single action does not prove that the save succeeded.
If root cancellation interrupts a save, the parent can lose its acknowledgement. Shut down and reconstruct the Store instead of retrying that save on it.
The example checks coordination, not protection against a crash or power loss.

### Leading and latest action throttling

For periodic work with an immediate first tick, merge an immediate action and `Effect::interval` under the same cancellation ID.
For a first tick after one interval, use `Effect::interval` alone. The [timer example](../../examples/timer/src/main.rs) shows cancellation and completion.

To limit how often the app publishes inputs, keep a deadline and at most one pending value.
In the [fixed-window example](../../crates/rca-test/tests/throttled_actions.rs), Leading mode drops inputs during an open window.
Latest mode replaces the pending value without moving its deadline. At the boundary, publish the pending value and open another window.
If no value remains, close the window.

An input at `now >= deadline` is eligible immediately. If it commits before the old timer action, it replaces any older pending value.
A late boundary publishes once. Its next window starts from the observed time without a burst of catch-up publications.
Another [throttle recipe](debounce-throttle.md) makes a different choice at that boundary.

Reset clears the pending value and cancels both the timer and any publication that has not committed.
It cannot undo committed publication. Both effects share the window's cancellation generation.
A boundary replaces its own window. It uses `without_inherited_cancellation` around the new timer and publication, then registers the new window ID.
Otherwise, cancellation of the previous generation can cancel its replacement. The root action and child scope still own the new work.

If the feature must continue, send Reset. Cancelling the root action stops work without resetting state.
This policy combines scalar inputs. It does not throttle arbitrary effects or limit concurrent client requests.

### Causal tasks and identities

After commit, `Store::send` returns an `ActionTask` for the original action and the work it caused.
For an action with no tracked work, the task is already complete. Its `cancel` does nothing and `is_cancelled` stays false.
For other actions, `finish` waits until registration closes and all tracked descendants finish. `cancel` requests their cancellation.

An ID's Rust type and value both matter. Two different values remain distinct even if their hashes match.
Replacement advances the generation. An effect checks its generation before joining the input queue.
The actor checks it again immediately before running the reducer.
Even after admission, cancellation or replacement can reject a queued action without changing state. Neither undoes an earlier commit.

Cancelling a child by its typed ID counts as ordinary completion for the parent task.
Requesting cancellation of unfinished work through the parent task sets its `is_cancelled` flag. See [receipt lifetimes](receipt-lifetimes.md).

## Time

Store deadlines use `MonotonicInstant`, not calendar time. One timer queue and one active driver wait cover the Store's core timers.
For equal deadlines, timers wake in registration order. Overdue fixed-rate intervals catch up to their original schedule.
Between batches the actor gives other work a chance to run. Constructing an `Effect::interval` with a zero period panics.
`ClockTimer` rejects a zero period with `InvalidTimerInterval` instead.

The actor removes a finite batch of due timers before running their actions.
After a successful batch, the Store can reuse space for up to 64 entries without retaining actions or callbacks.
The size depends on the action type. If the actor stops early or panics, it drops unprocessed entries.

| What do you need? | Use | Timing rule |
| --- | --- | --- |
| An existing action after a duration | `Effect::send_after` | The wait starts when the effect starts. |
| An action at a known deadline | `Effect::send_at` | A delay or sequence does not move the deadline. |
| A client call that starts later | `Effect::delay` | Put the call inside `Effect::run`. |
| A fixed schedule that catches up | `Effect::interval` | Overdue ticks keep their original cadence. |
| To ask for a tick and skip missed ticks | `ClockTimer` | Each tick gives the observed time. The timer schedules the next tick from that time. |

`ClockTimer` owns its wait and starts no task. Using the Store clock does not give the Store ownership of the timer.
If it must stop with a feature, move it into that feature's effect or task.
Dropping a pending `next` future keeps the timer's wait. Cancelling or dropping the timer itself ends the wait.
None of these choices promises punctual delivery or limits concurrent external operations.

## Dependencies

Each Store has its own set of dependencies: services such as clocks or network clients.
See [supply a dependency](dependencies.md) for keys, overrides, and the client an effect retains after the reducer returns.
The [external client example](external-clients.md) runs a Store with clients that the test controls.

### Optional capabilities

For a key with value type `Option<Client>`, an explicit `None` disables the service.
It does not request fallback to the default. See [dependency lookup](dependencies.md#which-dependency-value-does-the-feature-receive).

## Observation

A **selector** is a function that reads the part of state a caller needs. It runs on the actor and must not block.

| What does the caller need? | Use |
| --- | --- |
| One value at this request's position in the queue | `read` |
| The initial value and then the latest selection | `observe` |
| The initial value and then changes to the selection | `observe_distinct` |
| Only the membership and order of a keyed collection | `observe_ordered_ids` |

The actor selects the initial value and installs the observation before it replies. The first `next` receives that initial selection.
`Store::observe` requires `Send + 'static` on the selected value, not `Clone`.
`observe_distinct` also requires `Clone + PartialEq` on that value, not the whole state.
It runs the selector on each eligible commit. It compares with the last value it published, not the last value the caller received.

For a collection named `children`, use `store.observe_ordered_ids(|s| &s.children)`. Changing only a child's value reuses the list for that collection.
An insertion, removal, reorder, or switch to another collection can require a new list.
Only a different sequence produces a new observation value. Removing and reinserting the same ID at the same position in one transaction produces no change.
If you need the child's value, select it separately.

An observation retains at most the value in delivery and the newest pending value.
A slow reader can miss intervening values, even with `observe_distinct`. Cancelling one `next` call does not lose the newest value.
Dropping the observation schedules its removal. Store shutdown closes all observations.

If every transition matters, keep the history in state or in an application-owned event log.
Choose where to store it and what to do if a writer outpaces its reader. An observation alone cannot record that history.

`wait_while` returns `PredicateWaitOutcome::Satisfied` if its initial or a later selection is false.
Other [outcomes](../../crates/rca-core/src/observation/wait.rs) distinguish cancellation, Store shutdown, and the end of a child scope.
`send_and_wait_while` commits the action before registering the condition. Both can miss a brief false value between reads.
Neither operation waits for every task to end. For that, use the action's task.

## TestStore

`TestStore` runs the production Store with a deterministic runtime. It pauses selected changes so the test can examine them.
In exhaustive mode, each change needs its matching receive operation. `receive` takes actions from effects.
Typed updates, outputs, and mounted inputs have their own receive methods. Advancing virtual time makes due actions available without releasing them.

Plain `send` and `receive` expect unchanged state. Use `send_with` or `receive_with` for a change.
Full-state tests need `Clone + Debug + PartialEq` and an independent state history. The action does not need comparison traits.
For move-only state, `projected` or `projected_builder` uses an independent comparable snapshot.
A test only covers the values it compares. A clone pointing at the same mutable value cannot supply an independent history.
The [testing guide](testing.md) shows how to choose a snapshot or track a shared value.

Once the Store admits an action, interrupting its assertion leaves an obligation that later steps cannot clear.
`finish` runs ready work up to a fixed poll limit. It does not advance time or wait for external I/O.
It requires no pending state or terminal assertion, task, cancellation entry, timer, observation, or actor work.
Diagnostics include the call site, ready task IDs, virtual time, pending sleeps, and a runtime snapshot.

Successful `finish` shuts down the Store and avoids another assertion on drop.
Ordinary drop checks outstanding obligations, cleans up, and panics on failure.
During panic unwinding, drop only aborts work so the original panic survives.

Cancellation cannot acknowledge a pending assertion.
`receive_terminal` matches the production error. `receive_output_terminal` also matches the exact output declaration, even if that output can no longer run.
Neither clears a previous mismatch in a matcher, snapshot, or capture. See the [terminal assertion tests](../../crates/rca-test/tests/terminal_frontier.rs).

In `NonExhaustive` mode, `skip_received` can skip a pending mutation before user code runs. It then checks that state stayed unchanged.
It cannot skip child installation, satisfy an earlier failed expectation, turn off state or teardown checks, or run the skipped work.

## Panic and failure

Production RCA does not catch a panic from a reducer, selector, or effect. The executor's panic policy applies.
On unwind, RCA closes the affected Store, drops its state and effect captures, and rejects later work.
The [Tokio panic tests](../../crates/rca-tokio/tests/panic_subprocess.rs) exercise all three cases.
Operational failures are different: Store errors provide classification methods and captured backtraces.

## Shutdown and host lifetime

For an orderly stop, keep the host running and await `shutdown`.
The actor processes commands admitted before shutdown. It then closes admission, invalidates effects, cancels timers, closes observations, drops state, and replies.
Repeating a successful graceful shutdown is safe. If the Store already stopped without an orderly shutdown, `shutdown` returns a classified error.

Dropping the final Store owner requests an immediate stop without an orderly drain.
If the host shuts down, it can drop pending tasks. RCA invalidates their wrappers and breaks the links that hold the actor and scheduler together.
Retaining a runtime handle is not a substitute for keeping the host running through shutdown.

Dropping the last `DeterministicRuntime` clone closes admission to its scheduler before it discards pending tasks.
A retained `Runtime` handle cannot revive it. The closed scheduler drops new tasks without polling them, so a new Store on that handle starts closed.
The scheduler drops tasks outside its locks, even if cleanup submits more work. TestStore cleanup alone does not close a still-live test host.

### Terminal owned-state extraction

If the caller needs the final state itself, use `Store::shutdown_into_state(self)` instead of ordinary shutdown.
It consumes one full Store handle, even if other handles remain. Scoped handles and observations cannot extract state.
It returns a `Send + 'static` future with `Result<State, StoreStopped>`. State and action need only `Send + 'static`.

The first terminal command the actor processes wins. Earlier commands keep their normal commit rules. The actor discards later commands, even after admission.
If extraction wins, the actor completes its cleanup before handing over the original state without cloning it.
Other extraction callers receive `StoreStopped`. Shutdown, loss of the last owner, or host loss can also prevent extraction.
Unlike orderly shutdown, extraction cannot succeed twice.

Extraction closes admission and observations. It invalidates scoped and tracked work and requests cancellation of tasks and timers.
Old effect senders and scoped senders cannot change the returned state. Extraction does not drain effects or finish external cleanup.
A panic in a reducer or destructor is a programming error. It produces neither a successful value nor an ordinary domain error.

For a finite job caused by one action, first await `store.send(action).await?.finish().await`.
For independent work, wait for each result separately. The [Store example](../../crates/rca-core/src/store/mod.rs) extracts a boxed value without cloning it.
The [owned resource guide](owned-resources.md) explains which external cleanup remains with the caller.

Constructing the extraction future sends no command. The future holds its Store handle until it completes or the caller drops it.
Before admission, dropping the future releases only that handle. If it was the final owner, RCA requests an immediate stop.
After admission, abandoning the reply does not retract the command.
With another owner alive, the actor still closes the Store and drops unclaimed state exactly once.
Loss of the host or final owner can instead discard the command. No caller can recover the state by retrying.
