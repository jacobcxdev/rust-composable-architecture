# Authoring headless RCA applications

Use this guide when writing an RCA feature, including work delegated to an agent.
It is a route through the existing APIs and examples, not another application
framework. RCA owns serial domain transitions and admitted work. Your application
owns its executor, external services and any user interface.

## The basic model

- **State** holds the facts that describe the application, such as queued jobs or a selected document.
- An **action** is an input that asks the application to do something or records an external result.
- A **feature** defines how inputs change State and which follow-up work they start.
- A **Store** owns State and runs changes one at a time. Its actor is the task that processes those requests.
- A **transaction** is one synchronous change, including child reconciliation and publication of the resulting state.
- An **effect** declares work that can return actions. A typed **task** can also ask the actor to read or update State.
- A **context** grants specific access. A context is not the State itself, and it does not keep a departed child alive.
- A **runtime** polls asynchronous work and supplies the clock. The host application supplies the runtime.

One Store can contain many features. A child does not need a separate Store or
executor. Separate Stores have separate transaction order, so they do not provide
one atomic update across both states.

## Find a decision

| Question | Start here |
| --- | --- |
| Which crate, Cargo features and runtime do I need? | [Host and ownership](#1-choose-the-host-and-ownership-model), then the [support policy](../support.md#features) |
| Should I implement `Reducer` or `Feature`? | [Choose an authoring form](#choose-an-authoring-form) |
| Do I need macros or can I write ordinary Rust? | [Macros are optional](#macros-are-optional) |
| Should children use reducer composition, mounts or scoped handles? | [Composition](../composition.md) and [installed children](../structural-features.md) |
| Which ID survives a reorder, replacement or configuration change? | [Child identity](../structural-features.md) |
| Should data live in State, a dependency or `Shared`? | [Dependencies and shared state](dependencies-and-shared-state.md) |
| Should work return an action or update State through a task? | [Choose an authoring form](#choose-an-authoring-form) and [work lifetimes](#4-declare-work-and-handle-ended-authority) |
| Which send, wait, cancellation or shutdown operation do I need? | [Runtime](../runtime.md) and [receipt lifetimes](../receipt-lifetimes.md) |
| Does observation record every event? When does a reaction run? | [Runtime observation](../runtime.md#observation) and [structural Features](../structural-features.md) |
| How do I test tasks, real clients, clocks and move-only State? | [Testing](testing.md), [external clients](external-clients.md) and [owned resources](owned-resources.md) |
| Which diagnostics run in production? | [Diagnostics](diagnostics.md) |
| Does a save receipt prove durability or safe restart? | [Persistent counter](../../examples/persistent-counter/README.md) |
| What remains the host's responsibility? | [Integration boundaries](../integration-boundaries.md) |

## 1. Choose the host and ownership model

Start with the [facade features](../../rca/Cargo.toml) and
[support policy](../support.md). Default features are empty. Select `tokio` for
the Tokio adapter, `test-util` for deterministic tests, and other features only
when needed. Persistence is a separate `rca-persistence` dependency.

Use shared APIs from `rca` for portable `Send` work. Use `rca::local` when State,
actions or futures must retain non-`Send` values such as `Rc`. The Local runtime
must poll and destroy that work on its owner thread. A cross-thread wake does
not transfer ownership. Neither choice supplies a native event loop or renderer.

Local execution does not remove every thread-safety bound. Dependency keys and
values still require `Send + Sync`. Local tests use `rca_test::local::TestStore`
from `rca-test` with `local`, not `rca::local::TestStore`.

The [counter example](../../examples/counter/src/main.rs) uses an existing Tokio
host. This extract is inside its async `main`, with its `Counter` and `Action`:

```rust
let store = Store::new(0, Counter, TokioRuntime::current()?);
let task = store.send(Action::Increment).await?;
task.finish().await;
assert_eq!(store.read(|state| *state).await?, 1);
store.shutdown().await?;
```

`send` waits for commit. `finish` waits for the action's causal work, meaning work
started by that action and its descendants. Neither proves that an external
service has stopped its background work. See [receipt lifetimes](../receipt-lifetimes.md)
before choosing a wait.

## 2. Model domain changes and child identity

### Choose an authoring form

Start with `Reducer` when actions describe the meaningful changes in your domain.
Use `Reduce::new` for a small closure, or implement `Reducer` on a named type.
Both return `Effect`, including asynchronous effects. A network request does not
by itself require a direct `Feature` implementation.

Use `Feature::update` when asynchronous work needs typed access to its current
State through the actor. For example, a worker can read current retry policy
after an await, then update its progress without an action for each internal step.
An update that depends on current policy must inspect and change State in the
same update closure. An earlier read can become stale before the update.

| Need | Use | Reason |
| --- | --- | --- |
| A short state transition or effect-returning callback | `Reduce::new` | It implements `Reducer` without a named implementation. |
| Named inputs and results, such as `Search` and `SearchFinished` | `Reducer` with `Effect` | Follow-up results pass through the same action handler. |
| Current-state reads or owned updates between asynchronous steps | `Feature` with `Task` | The task submits typed requests to the actor. |
| `combine`, `scope`, `if_some`, `if_case` or `for_each` | `Reducer` | These combinators route borrowed state and actions and map returned effects. |
| Independently installed children, commands, outputs or reactions | Mounts with either form | Installation defines lifetime and routing, not the choice of callback trait. |

Every `Reducer` is a `Feature` automatically, without a wrapper. Both use
`Store::new`, `Store::builder` and the corresponding `TestStore` constructors.
Implementing `Feature` directly is not an upgrade that every component needs.
It adds a way to declare work, not another executor or permission to mutate
State concurrently.

Both callbacks run synchronously. Neither can hold its borrowed State across
an await. Both forms can start asynchronous work after commit. A direct Feature
can still append an `Effect` with `context.effect(...)`.

The split preserves a useful restriction: reducer combinators only need borrowed
state paths and action mapping. A typed task also needs authority to reach the
correct installed state after the borrow ends. A direct Feature therefore cannot
replace a Reducer argument to those combinators. Use installed children to
compose direct Features, including a mixture of reducers and direct Features.

Implement either `Reducer` or `Feature` for a type, not both. The automatic
implementation conflicts with a second explicit implementation.
Use separate wrapper types when the same data needs different behaviours.
The same rule applies to `local::Reducer` and `local::Feature`.

The [search example](../../examples/search/src/main.rs) demonstrates action-based
effects. The [two-task search](../../examples/composition-search/docs/named-operations.md)
demonstrates typed tasks. The [paired tests](../../rca-test/tests/legacy_feature_compatibility.rs)
exercise both forms through the same constructors, including cancellation and
late-result rejection. Typed task updates need their own TestStore assertions,
not fabricated response actions.

### Macros are optional

Start with ordinary structs, enums, closures and explicit paths. The `macros`
Cargo feature reduces repeated syntax for dependency clients, composition,
paths and snapshots. It does not change ownership, runtime or testing contracts.
The `Feature` trait is unrelated to a Cargo feature flag.
See [manual composition](../composition.md) and
[snapshot derives](../snapshot-derive.md) before choosing generated code.

### Choose child identity and routing

For reducer composition, use `combine`, `scope`, optional/case composition and
`for_each` as described in [manual composition](../composition.md). Keyed State
needs stable business IDs. Reordering and replacing a child are different events.
Do not use a row position as identity when rows can move.

Installed children accept either form. Use the [structural Feature guide](../structural-features.md).
`MountIdentity` separates business, replacement and configuration identity.
After `Store::ready`, acquire the installed context through its typed mount site.
Handle absence or rejected acquisition explicitly. Retain that context for its
work; do not reacquire by business ID to let stale work reach a replacement.
`Store::scope` is a root-lifetime projection, not an installed-child authority.

Keep parent-owned State separate from child captures. Choose commands, outputs
and folds for their documented roles, rather than routing every operation through
the root action. The structural guide links executable examples for each route.

## 3. Inject clients and use the correct clock

Define application-owned dependencies for network, storage and platform services.
Inject them per Store or TestStore. Test defaults do not implicitly fall back to
live factories, but an explicitly injected live client can still perform real I/O.
See [dependencies](../runtime.md#dependencies) and the
[controlled external-client recipe](external-clients.md).

Resolve an owned client or future before leaving synchronous reduction. Do not
capture a borrowed reducer context across an await. Captured clients stay captured;
they do not refresh automatically. Use an actor-ordered read when current State
is required. If policy must hold at commit, inspect it inside the update closure,
not in an earlier separate read. The [session refresh tests](../../rca-test/tests/session_client_refresh.rs)
distinguish captured clients, current services and request identity.

Use the Store runtime's clock for clients whose deadlines must follow that Store.
TestStore creates its deterministic runtime during `build`. Its existing
[clock-injection doctest](../../rca-test/src/store.rs) uses this builder extract:

```rust
let store = TestStore::builder((), Feature)
    .with_clock_dependency::<ClientKey>(|clock| Client { clock })
    .build();
```

The linked doctest defines `Client`, `ClientKey` and `Feature`. No clock dependency
is installed implicitly. Do not mix wall time or an unrelated Tokio timer with
virtual deadlines and expect TestStore advancement to drive it.

## 4. Declare work and handle ended authority

Use owned `Effect` work for reducer feedback, or typed `Task` work with the captured
Feature context. The [two-task search](../../examples/composition-search/docs/named-operations.md)
shows captured request inputs, current policy reads and owned result updates.
Its manual retaining host is test machinery, not a required application layer.

Choose cancellation and replacement deliberately. A named operation wait captures
existing task instances; it does not retarget later work. Cancellation requests
termination, not rollback of remote side effects or synchronous capture disposal.
Keep the runtime running until the required work and provider cleanup finish.

Handle typed rejection from effect senders and task contexts. Do not retry stale
feedback through the root Store to bypass its ended authority. Keep domain denial,
source cancellation, queue overflow and Store closure separate. The external-client
recipe checks these distinct outcomes with real controlled endpoints.

Do not await an ancestor's `ActionTask::finish` from that ancestor's descendant.
That creates a completion cycle. Keep the wait with an external owner. Removing
inherited cancellation keys does not remove root, installation or Store ownership.

## 5. Assert transactions and independent history

The [testing guide](testing.md) explains which assertion method matches each
transaction and when a projected snapshot is necessary.

Start with exhaustive `TestStore`. Use `send_with` or `receive_with` for expected
State changes; plain `send` and `receive` assert unchanged State. Receive actual
effect actions and use the matching methods for task updates, outputs and mounted
inputs. Do not manually send a fabricated response in place of real client work.

For focused scenarios, [`receive_until`](../../rca-test/src/store/focused.rs)
executes earlier transactions, then runs the matched action and checks its expected
changes. `receive_until_with_captures` adds capture expectations, while
`receive_omitting` runs one transaction with no change assertions. These calls adopt
omitted transactions' snapshots and declared captures instead of cancelling work.
`OmissionReporting::Warn` retains store-local warnings for `take_omissions` to drain,
while `Silent` skips warning creation.
Prior failed assertions, production errors and `finish` requirements still apply.
Each receive uses one poll budget and does not advance virtual time or wait for
external progress.

Use controlled clients and explicit virtual-time advancement instead of sleeps.
Assert pending work before releasing a response, then assert its committed outcome.
Finish the Store explicitly. The [basic TestStore cases](../../rca-test/tests/test_store.rs)
show pending receives, mismatches, bounded driving and cleanup.

For move-only State, use an independent comparable projection. An `Rc<RefCell<_>>`
alias is not historical State: later mutation changes what the old alias observes.
Direct mutation through application aliases also bypasses RCA transactions and
reactions. Redacted rendering must not weaken equality or omit relevant assertions.
See the [owned importer](../../examples/composition-search/docs/owned-importer.md).

Add cases for the boundaries your feature uses: cancellation, rejection, same-ID
replacement, retained callbacks, failed clients and final owner release. Keep
expected outcomes independent of the implementation. A final value alone does not
prove that an old request, callback or resource was disposed correctly.

## 6. Finish ownership and verify the actual consumer

Distinguish admission, commit, causal completion and physical cleanup. Dropping an
ordinary action task does not cancel it. Closing an input gate does not remove
the child or cancel its tasks. Observations can coalesce values and cannot replace
an exhaustive transaction log.

For persistence, await the required document close/flush acknowledgement before
Store shutdown. Before restart, use the backend's checks to establish that old
callbacks and background work have stopped. The
[persistent counter](../../examples/persistent-counter/README.md) shows one explicit
codec/backend policy, not a universal durability guarantee.

Follow [contribution checks](../../CONTRIBUTING.md#toolchain-and-formatting) for
repository changes. Use the owning example or test manifest and its real feature
selection. Test the minimal required features as well as enabled optional paths.
An all-features build alone does not prove either boundary. Keep standalone
consumers and packaged dependencies in the final release checks.

Before handing off a feature, state which commands ran, their results and any
untested provider or platform boundary. Keep runtime errors explicit; do not copy
fixture `unwrap` calls into fallible application paths without deciding a policy.
Measure proposed optimisations against a representative workload. Extra cloning,
boxing or shared locks can alter ownership and cost even when the final value agrees.

For native interfaces, databases and external services, use the
[integration boundary guide](../integration-boundaries.md). A controlled client
or Local engine test does not qualify the actual external system.
