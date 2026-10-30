# RCA runtime contracts

This document defines observable core runtime behaviour. The
[qualification summary](qualification.md) records the current evidence boundary.
`rca-core` owns no executor, reactor, wall clock, UI loop, global Store, or
thread-local authority.

## Feature execution

The Store actor owns state and its `Feature`. The feature callback runs
synchronously with borrowed state and action values. It cannot suspend.
Its declared effects and typed tasks start after the transaction commits.

Every `Reducer` implements `Feature` automatically. Reducers return action-based
effects. Direct `Feature::update` implementations can also declare typed tasks
that access state through the actor. Both use the same Store constructors and
runtime contracts. See [structural features](structural-features.md) for typed
tasks, mounted children and installation lifetimes.

## Reducer transaction

For action-based reducers, the actor exclusively owns state, reducer, dependency
values, effect registry, timers and observation registry. It processes one command
at a time. For each action it:

1. records action provenance and cancellation generation;
2. invokes reducer synchronously with `&mut State`, `&Action`, and
   `&mut ReduceContext`, then invalidates departed scopes and refreshes handles;
3. increments the commit version and publishes observation projections;
4. compiles the returned effect program in source order into pending launches;
5. launches compiled work and acknowledges commit with one causal
   `ActionTask`.

Reducers cannot await. State stays actor-owned until destruction or successful
terminal extraction. No global `Clone` bound exists on state or actions.

## Admission and ordering

`Store::send` uses bounded asynchronous admission and returns after its action
commits. `Store::try_send` returns immediately with an `ActionReceipt` or a
`TrySendError` retaining the action. The default mailbox capacity is
`DEFAULT_MAILBOX_CAPACITY`; builders can select another non-zero bound.

External actions, effect actions, reads, observation operations, cancellation,
and shutdown linearise through Store authority. An effect's `sender.send`
returns only after its output commits. Dropping a send future after admission
does not withdraw its mailbox position. Effect cancellation can still invalidate
queued output before reduction, as described under structured cancellation.

Control-plane cancellation, due timers, and observation cleanup are serviced
without allowing a sustained mailbox flood to starve them.

## Runtime driver

`RuntimeDriver` provides four capabilities: no-inline `spawn`, monotonic `now`,
`sleep_until`, and cooperative `yield_now`. Its futures are `Send + 'static`.
Dropping any runtime future is terminal and must release captured resources.
Cloned runtimes share one monotonic origin.

The public `RuntimeDriver` rustdoc defines third-party adapter obligations:
prompt ownership transfer without inline polling, task progress while the host
is live, cross-thread wake-ups without an actor lock, immediately ready elapsed
sleeps, and cancellation-safe waits. Host teardown is the exception to task
progress: it may drop pending tasks, causing abortive Store closure. Retaining
a `Runtime` handle does not by itself retain host authority.

`TokioRuntime::current` validates an active Tokio runtime and timer driver. It
works with current-thread and multi-thread runtimes. Construction failure
distinguishes missing runtime from missing timer authority.

`DeterministicRuntime` implements the same contract with explicit ready queues,
virtual time, stable equal-deadline order, and a bounded poll budget. It never
advances time implicitly.

### Synchronous hosts

A synchronous headless application can own one Tokio runtime shared by several
Stores. It does not need an executor or thread per Store. Two supported usage
patterns are exercised in `rca-tokio/tests/synchronous_host.rs`:

- A finite caller constructs Stores while the current-thread runtime is entered,
  then uses `Runtime::block_on` to drive admitted actions, await causal completion
  and extract terminal state. Merely entering the runtime does not drive it.
- A background thread continuously drives a current-thread runtime while the
  synchronous caller uses `try_send`. The caller can use Tokio's `Handle::block_on`
  outside executor threads to await receipts and causal tasks. The background
  runtime must remain driven until those operations complete.

Do not block the executor thread waiting for work that requires that same
executor. In an async task, await instead of nesting `block_on`. A latency-sensitive
host callback should submit with `try_send`, handle `Full`/stopped errors and
return; collect completion elsewhere. `try_send` allocates and is not a hard
real-time or lock-free audio callback API. Decide explicitly whether rejected
input is retried, coalesced or discarded; the error returns ownership of it.

Admission is not commit, and commit is not causal completion. Await the receipt,
then its `ActionTask::finish` when completion matters, before terminal extraction.
Keep the host alive through shutdown acknowledgement. These tests qualify
ordinary synchronous Rust callers, not native event-loop, thread-affine resource
or operating-system service integration.

## Effects

`Effect` is an opaque post-commit work program:

- `none`: no work;
- `send`: immediate follow-up action;
- `run`: one runtime future with an `EffectSender`;
- `merge`: children start concurrently in source order;
- `concatenate`: each child starts after predecessor completion;
- `send_after`: one monotonic delayed action;
- `send_at`: one action at an existing absolute monotonic deadline;
- `delay`: wait before starting an owned effect program;
- `interval`: fixed-rate action generation without drift;
- `cancel`: all live generations for one typed identity;
- `cancellable`: associate identity;
- `cancellable_in_flight`: cancel old generations, then start replacement.

Effects and their captured values must be `Send + 'static`. A spinning future
that never yields can starve any cooperative current-thread executor; this is a
host-program bug, not a reason for hidden threads in core.

### Stream-driven effects

Consume an input stream inside `Effect::run`. Await each `sender.send(action)`.
If the send returns an error, stop the loop. The loop polls its next input only
after the previous action commits. Collecting the whole stream first changes memory
use and the timing of external input.

A long-lived subscription keeps its causal `ActionTask::finish()` pending until
the subscription ends or is cancelled. Dropping the ordinary `ActionTask` does
not cancel it. If the domain needs independent cancellation, give the subscription
an explicit cancellation identity.

Stream exhaustion, stream destruction and external-resource cleanup are separate
facts. Test the cleanup contract of the client that owns the resource.
These shared Store effects require `Send + 'static`. Owner-local APIs have
separate contracts for non-`Send` work.

### Delayed work and debounce

Use `Effect::run(operation).delay(duration).cancellable_in_flight(id)` to
debounce execution: obsolete waiting work is dropped without calling the
operation. Put client calls inside `run`; effect construction and mapping still
happen eagerly. `send_after` instead delays an already constructed action.

Use `Effect::send_at(deadline, action)` when the deadline must survive deferred
execution. It uses the Store runtime's monotonic origin (for example, a deadline
derived from that runtime's `now()`); origins are not checked or translated.
Unlike `send_after`, a surrounding delay or sequence does not rebase it. An
already elapsed deadline is eligible when the program starts, through the normal
actor queue. This guarantees no early delivery, not punctual delivery or a
wall-clock deadline. `MonotonicInstant::MAX` remains pending until cancellation
or shutdown. Mapping and cancellation retain the same ownership as other timers.

Delay begins when its sequence element starts, not when the effect is built.
Root task completion waits for both the delay and child work. Cancellation IDs
outside delay own both phases; IDs inside register only after waiting. Thus
`effect.cancellable_in_flight(id).delay(duration)` does not debounce pending
delays. Structural removal and shutdown also cancel both phases.

An inherited-cancellation boundary outside delay transfers both phases; one
inside cannot rescue a wait already cancelled by an inherited ID. Explicit IDs,
root lineage and scope ownership remain intact. Zero duration returns the
original effect; a statically empty effect stays empty. Saturated deadlines
retain the existing timer contract and remain cancellable.

This also applies when delay wraps a composed reducer's result. Ended child
scopes are pruned from deferred programs immediately, without starting them.
Surviving siblings keep waiting; when no continuation survives, the private
delay wait is cancelled. An earlier timer action may remove a child whose wait
was already collected in the same due batch; that terminal wait is skipped.

## Structured cancellation

### Domain-owned request deadlines

For requests with visible timeout state, current effects can express the policy:

| Action | State transition | Returned effect |
| --- | --- | --- |
| Start | Running | Merge operation under cancel-in-flight Operation ID with `send_after(duration, Deadline)` under cancel-in-flight Deadline ID |
| Response | Success or failure | Cancel Deadline ID |
| Deadline | TimedOut | Cancel Operation ID |
| Cancel | Cancelled | Cancel both IDs |

Distinct IDs let either terminal branch cancel its loser without cancelling its
own commit. Restart replaces both generations. Inside keyed composition those
IDs are scope-qualified, so siblings remain independent. Existing provenance
checks reject losing output queued when cancellation commits. The request's root
task finishes only after both branches release their work.

This policy ends a domain request at the first terminal action committed by the
actor; it is not a realtime bound or a generic opaque-program timeout. Root
cancellation and shutdown cancel work without inventing a timeout action.
Shutdown acknowledges issued aborts; runtime-owned captures drop when the host
cooperatively polls the aborted task wrappers, not necessarily before that reply.
The executable fixture is `rca-test/tests/request_deadlines.rs`. Its controlled
client send is deliberately polled for the queued-output test; application code
normally awaits `sender.send` directly. Generic timeout operators remain a
separate design decision, especially for multi-action subtrees and explicit
cross-feature cancellation handoff.

### Finite batches and bounded retry

For domain-visible jobs, a parent can own a finite input queue and reserve keyed
child slots before emitting their Begin actions. Each child owns its attempt
count, injected client and retry timer. The parent sees terminal child state,
records the outcome, removes the child and admits the next queued item.

`rca-test/tests/bounded_jobs.rs` qualifies two slots and at most three attempts,
with 10 ms then 20 ms Store-clock backoffs. Backoff keeps its slot: predictable
bounds at the cost of throughput. Permanent errors terminate immediately.
The injected client also owns two permits. Each actual operation retains one
until its resource is destroyed, then releases it before awaiting its result
send. This matters during cancellation: removing a keyed child only requests
abort, so a replacement must wait for the old resource to drop rather than
assuming the freed state slot means physical cleanup has completed. An
adversarial scheduling trace withholds aborted wrappers and proves this bound.
Finite input and attempts bound result production; this is not a streaming pool.

Parent-emitted Begin actions need ownership before the child starts. The fixture
wraps them in typed launch cancellation IDs and cancels those IDs when removing
unfinished jobs. Child scope removal alone cannot invalidate an unscoped launch
already queued by the parent. Once running, child scope owns operation and retry.

Explicit batch cancellation records domain outcomes and stops admission. Root
cancellation or shutdown only invalidates causal work; it does not rewrite state.
A root receipt is not a persistent batch handle across separately sent commands.
This fixture proves a composition recipe, not production adoption or a generic
retry/worker-pool API. Input validation, capacity and retry policy remain domain
decisions; native-host and comparative performance qualification remain separate.

### Parent-owned persistence

`rca-test/tests/persistence_coordination.rs` qualifies an injected storage recipe:
the parent owns a value, current revision, acknowledged revision and one in-flight
snapshot. Edits during a save coalesce into the latest value. An older successful
save acknowledges only its captured revision, then starts the latest snapshot.
Load status is explicit; missing storage remains unmaterialised until a save.

A failed save leaves state dirty, including failure after backend commit. Flush
retries the latest revision. Close fences edits and becomes closed only after the
final revision succeeds; failure needs explicit Flush or Close retry. Observe
closed state before shutdown when persistence matters. Finishing a causal action
receipt alone does not prove a successful flush or close.
Close before successful loading cancels any pending load and closes without
saving the default value. Later Load and Set commands cannot reopen it.

The controlled storage model survives Store reconstruction and distinguishes
failure before and after commit. Abortive shutdown before commit leaves the old
value; root cancellation after commit but before reducer acknowledgement does
not roll the backend back. These are coordination proofs, not filesystem,
process-crash or power-loss durability guarantees. Concrete backends, native-host
interruption tests, codecs and migration remain separate qualification gates.
No generic persistence coordinator or shared-state API is introduced.
The recipe assumes one writer per storage value. Cancelling a save root is
terminal usage: shut down and reconstruct rather than retrying that same Store,
whose in-flight state may no longer receive its acknowledgement.

### Leading and latest action throttling

Leading periodic work needs no separate timer primitive: merge an immediate
action with `Effect::interval` under one cancellation ID. The MCMaitreD polling
example exercises this composition, including Stop/restart and descendant query
cancellation.

For value coalescing, start with explicit domain state: a window deadline and
at most one pending latest value. In Leading mode, suppress in-window inputs;
in Latest mode, replace that pending value without moving the deadline. A
boundary publishes the pending value and opens another window, or closes when
empty. `rca-test/tests/throttled_actions.rs` demonstrates this using
`ReduceContext::now`, `send_at` and a scope-local typed cancellation ID.

Its policy intentionally makes input at `now >= deadline` eligible. When such an
input arrives before the old boundary commits, it supersedes the older pending
value. A late boundary publishes once and starts the next window from observed
time, avoiding catch-up bursts. These are domain decisions, not universal
throttle semantics or upstream Composable equivalence.

Reset cancels the timer and uncommitted publication, and clears pending state;
it cannot undo publications already committed. Both effects share the Window
cancellation generation. Replacing a window from its
own boundary uses `without_inherited_cancellation` outside fresh timer and
publication effects, then explicitly registers the new Window ID; otherwise
old-generation cancellation could kill successor work. Root and structural
ownership remain. Root cancellation is abortive, not a domain reset; use Reset
when continuing the feature. This recipe coalesces scalar values, not arbitrary
opaque programs, and does not limit concurrent external operations. Costs are
explicit state/actions and policy code; no generic throttle registry is added.

### Causal tasks and identities

Every committed action has a lineage. Child effects register before parents can
finish. `ActionTask::finish` waits until registration closes and every child
completes. `ActionTask::cancel` invalidates its lineage and requests cancellation
of every descendant.

Typed cancellation identities include concrete Rust type and value. Hash
collisions do not alias distinct values. Replacement advances a generation;
outputs validate generation before attempting admission and again on the actor
immediately before reduction. Admission may wait for mailbox capacity; output
already queued when cancellation or replacement wins is rejected without
reducing state. Cancellation does not undo an output that already committed.

Typed cancellation of a child is normal completion for its parent action task.
Explicit lineage cancellation reports `ActionTask::is_cancelled`.

## Time

All deadlines use `MonotonicInstant`, never wall time. One Store timer queue and
one active driver sleep cover all core timers. Equal deadlines wake in
registration order. Overdue fixed-rate intervals catch up at their original
cadence with cooperative fairness between batches. Zero-period intervals panic
at construction.

## Dependencies

`DependencyValues` belongs to one Store. Keys and values are `Send + Sync +
'static`. Production missing values initialise once from `DependencyKey::live_value`.
Builder overrides are isolated per Store. Reducers clone handles or owned data
needed by effects before returning; no reducer borrow crosses an await point.

TestStore uses `DependencyKey::test_value` instead, including projected and
non-exhaustive constructors. Its default implementation panics naming the key
before invoking any live factory. Supply a reusable deterministic test default or
inject a value with `TestStore::builder(...).with_dependency::<Key>(value)`.
With the derive, write `#[dependency(value = Client, live = Client::production(),
test = Client::deterministic())]`; `test` is optional and never aliases `live`
implicitly.
The test expression must compile in normal builds, like a manual `test_value`
implementation. Inject helpers available only through dev-dependencies from the
test instead.
Each Store caches its own selected default. No global test mode exists, and
enabling `test-util` does not change ordinary production Store defaults.

Precedence remains explicit test override > action override > feature overlay >
explicit Store value > selected default. Tests intentionally exercising real
clients must inject those values explicitly; custom defaults and explicit values
remain the author's responsibility. This prevents implicit fallback, not direct
I/O or live clients deliberately placed in an override.

### Optional capabilities

Use a key whose `Value` is `Option<Client>` or `Option<Arc<Client>>` when a
capability may be unavailable. Return `None` from the live/test factories when
absence is the intended default. Access the borrowed option with ordinary Rust
`Option` methods; clone only the owned handle needed by an escaping effect.

An explicitly supplied `None` masks lower-priority values. It does not trigger
fallback to another layer. Action overrides end after their synchronous scope;
an effect retains the handle selected and captured during that scope. The
`optional_dependencies` integration tests cover these rules, disabled work and
restoration of the surrounding configuration.

RCA does not expose whether a resolved dependency came from an explicit binding
or a cached default, or provide a lookup that skips the default factory. Do not
make domain behaviour depend on lookup/cache history. If unconfigured, disabled
and enabled must differ, represent them in the key's value, for example an enum
with `Unconfigured`, `Disabled` and `Enabled(Arc<Client>)` variants. Resolve any
fallback explicitly at the domain boundary. This costs a domain type but keeps
the distinction stable across reads, scopes and test substitution.

## Observation

`Store::observe` registers a projection at an ordered mailbox position and
returns its initial value first. Selectors execute synchronously on actor-owned
state and must not block. Values need only `Send + 'static`, not `Clone`.

Each observation retains at most one value being delivered plus the newest
pending projection. Intermediate values may coalesce. A cancelled `next` call
does not lose the newest value. Dropping observation schedules unregister;
shutdown closes all observations.

## TestStore

`TestStore` is a harness around production Store plus deterministic runtime and
an assertion gate. It does not implement a second reducer kernel. In exhaustive
mode, each selected mutation needs its corresponding receive operation before
the next mutation is released. Effect actions use `receive`; typed updates,
outputs and mounted inputs use their matching receive methods. Virtual `advance`
exposes due actions but never auto-releases them.

`finish` requires no pending mutation or terminal assertion, task, cancellation
entry, timer, observation, or actor work. Diagnostics include call site, ready task IDs,
virtual time, pending sleeps, and runtime snapshot.

Every `send`/`receive` also compares state and means unchanged state unless an
expected update is supplied through `send_with`/`receive_with`. Full-state
constructors require independent `Clone + Debug + PartialEq` state; move-only
domains use `projected`/`projected_builder` with an independent comparable
snapshot. Coverage is only as complete as the chosen equality and projection;
mutable shared aliases are not tracked automatically. Action comparison traits
are not required. See the `rca-test` crate documentation for both examples.

Once an action is admitted, an interrupted assertion remains a store-owned
obligation with no recovery API. Later steps and `finish` reject it. Normal Drop
checks unfinished obligations, cleans up, then panics on failure. Unwinding Drop
only aborts work and preserves the original panic. Completion settles bounded
ready work without advancing time or waiting for external I/O; successful
`finish` shuts down and prevents a duplicate teardown assertion.

Cancellation does not acknowledge a selected boundary. `receive_terminal` matches
its actual production error; `receive_output_terminal` identifies the exact output
declaration even after its live authority ends. Terminal acknowledgement is
separate from successful state comparison and cannot clear earlier failed matcher,
snapshot or capture expectations. See the [method contracts](../rca-test/src/store/api.rs)
and the Shared/Local [terminal-frontier tests](../rca-test/tests/terminal_frontier.rs).

In `NonExhaustive` mode, explicit `skip_received` can skip a pending mutation
before user code runs, then verifies unchanged state. Structural installation
cannot be skipped. Skipping does not clear failed matcher or expectation debt,
disable state or teardown checks, or implicitly execute skipped work.

## Panic and failure

RCA does not catch reducer, selector, or effect panics in production. Executor
panic policy remains visible. Unwinding drops actor state and effect captures,
closes Store authority, rejects future work, and cannot permit later reduction.
Operational errors carry captured backtraces and classification methods.

## Shutdown and host lifetime

Graceful `shutdown` is ordered after previously admitted commands. It closes
admission, invalidates effects, cancels timers, closes observations, drops actor
state, and acknowledges. Repeated graceful shutdown is idempotent. Shutdown
after abortive authority loss returns a classified error.

Dropping the final Store owner requests abortive cleanup. Dropping runtime host
authority invalidates pending runtime wrappers and breaks scheduler/actor
retention cycles. Host runtimes must outlive Stores when graceful completion is
required.

Dropping the final `DeterministicRuntime` clone closes scheduler admission
before discarding pending tasks. Retained `Runtime` handles cannot revive that
host: newly submitted tasks are dropped without polling, so a Store constructed
with such a handle is already closed. Task destruction occurs outside scheduler
locks, including when cleanup submits more work. Discarding tasks for TestStore
cleanup alone does not close a still-live host.

### Terminal owned-state extraction

`Store::shutdown_into_state(self)` returns a `Send + 'static` future with
`Result<State, StoreStopped>`. State and Action need only `Send + 'static`.
The operation consumes one full owner, not exclusive ownership of all handles.
Scoped handles and observations do not gain extraction authority.

The first terminal command processed by the actor wins. Earlier commands obey
existing commit rules. Later commands are discarded, even after admission.
If extraction wins, actor cleanup finishes before the exact state value moves
to the receiver. Other extraction callers receive `StoreStopped`.
Ordinary shutdown, abortive closure or host loss can also prevent extraction.
Extraction is not idempotent. Ordinary shutdown keeps its existing contract.

Extraction closes ingress and observations, invalidates scoped and causal work,
and issues task/timer aborts. It does not drain pending effects or promise
completion of external asynchronous cleanup. Old effect and scoped senders
cannot mutate the returned state. Reducer/destructor panics remain programming
failures, not successful output or ordinary domain errors.

If a finite job needs completed effects, establish completion before extraction.
For one root causal tree, await `store.send(action).await?.finish().await` first.
Other independent work needs its own completion condition. The runnable
`Store::shutdown_into_state` rustdoc example returns a non-Clone boxed product.
The [owned-resource guide](guides/owned-resources.md) explains one-shot action
payloads and the separate cleanup obligations of external resources.

The future retains its consumed owner until completion or cancellation.
Construction submits no command. Before admission, dropping the future drops
only that owner. Final-owner loss requests ordinary abortive cleanup.
After admission, receiver abandonment does not retract the terminal command.
With another owner alive, the command still closes the Store and drops
unclaimed state exactly once. Final-owner or host loss can instead discard the
command during abortive cleanup. There is no recovery ticket or retry promise.
