# Migrating RCA 0.7 to 0.8

This is a historical transition guide, not the current TestStore contract.
The unpublished package line is now `0.1.0`. Use the
[current composition migration guide](migration-0.9.md) for exhaustive state
assertions, automatic teardown checks, path namespaces and owned-state extraction.
Sections explicitly labelled as current describe later additions.

RCA 0.7 already processes live Store actions on a dedicated thread. RCA 0.8
moves execution behind an explicit `Runtime` adapter and shares its actor-owned
kernel with deterministic tests. This is an intentional break, not a
compatibility layer.

Here, 0.7 means the released `v0.7.0` API. A preserved counter lives in
`tests/downstream-0_7`. Its source revision
`4542ff7d2f650cf2d612e4ed001972825b311fbc` has the same `composable` package
source as that tag. The migrated executable is `examples/counter`.

## Migration map

| 0.7 | 0.8 | Why |
| --- | --- | --- |
| `Store<State>` | `Store<State, Action>` | Store handles carry state and action types explicitly. |
| state implements `Reducer` | standalone reducer value | State remains data; reducer can hold policy and configuration. |
| non-blocking `send(&self, impl Into<Action>)` | `send(&self, Action).await` | Admission is bounded. The caller awaits action commit. |
| blocking `sync(&self, action)` | async `send`, then optional `ActionTask::finish` | Action commit and causal effect completion are separate waits. |
| fire-and-forget `send` | returned `ActionTask` | Caller can await or cancel causal effect work. |
| consuming `into_inner()` returns `Reducer::Output` | ordered `read`, then explicit `shutdown` | Read a projection while the Store remains alive. |
| dedicated thread with `LocalPool` per Store | explicit `Runtime` adapter | The host selects execution. Each Store retains serial state ownership. |
| locally spawned effect streams | `Send + 'static` `Effect` futures | Effects can run on current- or multi-thread executors. |
| no built-in Store observation API | latest-value `Observation<Value>` | Slow consumers stay bounded and receive newest projection. |
| thread-local `Dependency<T>` values | Store-owned `DependencyKey` values | Typed access remains. Values now require `Send + Sync` and are captured during reduction. |
| dropping or cancelling an effect `Task` | explicit `ActionTask::cancel` or typed cancellation identities | Dropping `ActionTask` does not cancel work. Identity replacement rejects stale output. |
| `TestStore::send`/`recv` with expected-state mutation | async `send`/`receive` and separate state assertions | State cloning is no longer required. Equality assertions remain opt-in. |
| `RecursiveReducer` derive | handwritten standalone reducers | The old derive is removed. Reducer contracts remain public. |
| views send through `Effects` | caller-supplied `SendAction` closure | UI chooses immediate admission and error reporting policy. |

## Store and reducer shape

Before:

```rust,ignore
impl Reducer for State {
    type Action = Action;
    type Output = usize;

    fn reduce(&mut self, action: Action, _send: impl Effects<Action>) {
        match action { Action::Increment => self.count += 1 }
    }
}

let store = Store::with_initial(State::default());
store.send(Action::Increment);
let count = store.into_inner();
```

After:

```rust,ignore
impl Reducer for Counter {
    type State = State;
    type Action = Action;

    fn reduce(
        &mut self,
        state: &mut State,
        action: &Action,
        _context: &mut ReduceContext<'_>,
    ) -> Effect<Action> {
        match action { Action::Increment => state.count += 1 }
        Effect::none()
    }
}

let store = Store::new(State::default(), Counter, runtime);
store.send(Action::Increment).await?;
let count = store.read(|state| state.count).await?;
store.shutdown().await?;
```

`Store::send` suspends until action commit and returns its causal `ActionTask`.
Use `Store::try_send` for synchronous bounded admission. A full-mailbox error
retains the unsent action through `TrySendError::into_action`.

Unlike 0.7 `sync`, `send(...).await` waits only for that action's commit, not
for follow-up actions to drain. `ActionTask::finish` also waits for asynchronous
effect work and can remain pending for long-lived effects.

If you need causal completion, await the returned task's `finish()`. Where 0.7
relied on implicit `Into<Action>` conversion, pass `action.into()` explicitly.

## State reads and observation

The 0.7 Store exposes final output through consuming `into_inner()`, not a
borrowed `state()` method. In 0.8, read or observe an ordered projection while
the Store remains alive:

```rust,ignore
let count = store.read(|state| state.count).await?;
let mut counts = store.observe(|state| state.count).await?;
assert_eq!(counts.next().await, Some(count));
```

Selectors execute serially on the Store actor. `Observation` retains at most an
in-flight value and the newest pending value. Dropping it unregisters the
selector; Store shutdown closes it.

## Effects and cancellation

Reducers return effect programs instead of invoking an effect handle:

```rust,ignore
Effect::run(move |sender| async move {
    let response = client.fetch(query).await;
    let _ = sender.send(Action::Response(response)).await;
})
.cancellable_in_flight(SearchRequestId)
```

Clone required dependency handles and owned inputs during reduction. Every
effect future and action crossing runtime boundaries must be `Send + 'static`.
`cancellable_in_flight(id)` invalidates all older matching generations before
starting the replacement. `Effect::cancel(id)` cancels all live generations.
Dropping a send future does not withdraw an admitted action. Effect outputs
still validate their generation before reduction, so cancellation can reject
stale queued output. It cannot undo an output that already committed.

Use `Effect::merge` for concurrent source-ordered starts and
`Effect::concatenate` for sequential child completion. `send_after` and
`interval` use the Store clock, so live and deterministic runtimes share timer
semantics.

### Stream delivery in the current candidate

The historical version labels above describe the runtime transition. The
current unpublished package line is 0.1.0. For a Send stream, consume items
inside `Effect::run` and await each `sender.send(action)`. Stop the loop when
delivery returns an error; continuing to poll a source after its lifetime has
ended can consume inputs that will never reach state.

This is not the same producer-progress contract as legacy `effects.stream`.
The legacy adapter detaches its producer and synchronously enqueues ready
items. Awaited RCA sends wait for commit before polling the next item. The
[three-engine stream fixture](../tests/composable-stream)
checks this difference at first-value reduction and compares the final ordered
trace. Its earlier passing checkpoint is retained as historical local evidence,
not a current release receipt. Do not collect a stream eagerly merely to reproduce
legacy queueing: that changes memory use and when external inputs are consumed.

For a finite job, establish completion before extracting state. A terminal
action can acknowledge the intended reduced trace; causal `finish()` also
waits for the producer to finish. For a long-lived subscription, `finish()`
normally remains pending until the source ends or work is cancelled. Dropping
the returned `ActionTask` does not cancel the subscription. Choose explicit
task cancellation or a child/effect lifetime according to ownership; do not
infer cancellation from losing an observation handle.

Current RCA effects require Send work. Moving a non-Send stream into the effect
does not become valid because a current-thread Tokio runtime is selected.
Keep genuinely thread-affine resources in their owning host and communicate
through an explicit Send boundary; resource teardown needs its own completion
protocol. Stream exhaustion, stream destruction and external-resource release
are separate facts.

## Dependencies

The 0.7 `Dependency<T>` API is already typed, but resolves values from
dynamically scoped thread-local storage. Replace that lookup with explicit
Store-owned keys.

Define typed keys manually or enable `macros`:

```rust,ignore
#[derive(DependencyKey)]
#[dependency(value = ApiClient, live = ApiClient::production())]
struct ApiClientKey;
```

Read once in reduction with `context.dependency::<ApiClientKey>()`. Override
per Store with `Store::builder(...).with_dependency::<ApiClientKey>(test)`.
There is no thread-local dependency authority.

## Action tasks

`ActionTask` represents work causally launched by one committed action:

```rust,ignore
let task = store.send(Action::Start).await?;
task.cancel();
task.finish().await;
```

Dropping a task does not cancel it. Typed effect cancellation can complete a
parent action task normally; explicit `ActionTask::cancel` marks its lineage
cancelled.

## TestStore

The 0.7 TestStore owns a caller-driven `LocalPool`, separate from the live
Store's dedicated-thread runtime. The 0.8 TestStore runs the production Store
kernel on deterministic virtual time. Effect actions are gated until explicitly
received:

```rust,ignore
let mut store = TestStore::new(State::default(), Feature);
let task = store.send(Action::Start).await?;
store.advance(Duration::from_secs(1)).await?;
store.receive(|action| matches!(action, Action::Tick)).await?;
store.assert_state(|state| assert_eq!(state.count, 1)).await?;
task.cancel().await?;
store.finish().await?;
```

In 0.7, TestStore requires `Action: Debug`. Its `send` and `recv` methods also
require `State: Clone + Debug + PartialEq`. In 0.8, ordinary sends, receives,
and `assert_state` require neither state cloning nor equality. Only
`assert_state_eq` adds `State: Debug + PartialEq`. Action matching uses a
predicate without a global `Action: Debug` bound. `finish` rejects unreceived
actions, live effects, timers, observations, and unfinished actor work with
diagnostics.

## Macros and composition

0.8 foundation keeps only optional `DependencyKey` syntax. It intentionally
does not provide the 0.7 `RecursiveReducer` compatibility macro. Composition
will be built over explicit public reducer combinators rather than hidden
runtime semantics.

## View callback adapter

`composable-views` no longer depends on Store or an executor. Adapt admission
at the application boundary and handle both full and stopped cases:

```rust,ignore
let send_tap = move |action| {
    if let Err(error) = store.try_send(action) {
        report_view_admission(error);
    }
};
```

The callback is synchronous by design. Do not discard `try_send` errors or
pretend async backpressure fits a synchronous event callback.

## Shutdown

The 0.7 `into_inner()` both stops the runtime and converts state into
`Reducer::Output`, with only best-effort progress for pending async work.

In 0.8, await any required action tasks first. Then read the needed output
before shutdown.

Call `Store::shutdown` before dropping the host runtime. Shutdown is ordered
after earlier mailbox work, rejects later admission, cancels causal work,
closes observations, drops actor-owned state, and then acknowledges. Final
Store-handle loss performs abortive cleanup when graceful shutdown was omitted.
