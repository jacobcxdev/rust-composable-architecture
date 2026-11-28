# Build an application with RCA

Rust Composable Architecture (RCA) helps you organise a program around its state and the events that change it.
It does not draw a user interface or start an event loop. Your application supplies both if it needs them.
To try the code before choosing an architecture, start with the [runnable counter](../../README.md#quick-start).

## The basic model

Consider a counter. Its **state** is a number. An **action** such as `Increment` tells the program what happened.
A **reducer** handles an action and can change state. It returns an **effect** that describes follow-up work.
The counter increments the number and returns no work.

A **feature** is a unit of application behaviour, such as a counter or a search. The **Store** owns its state.
The Store's **actor** handles one request at a time. One **transaction** can change state, update which children exist, and publish the result to observers.
An observer is a caller that asks the Store for updates.

A **runtime** drives asynchronous work and supplies a clock. Your application, also called the **host**, supplies the runtime.
Several features can share one Store. They do not each need a runtime.
Two separate Stores process their own requests. A change across both is not one transaction.

## Find a decision

| What do you need to do? | Start here |
| --- | --- |
| Add RCA to a program and choose a runtime | [Choose the host](#1-choose-the-host-and-ownership-model) |
| Choose `Reducer` or `Feature`, with or without macros | [Choose an authoring form](#choose-an-authoring-form) and [macros](#macros-are-optional) |
| Decide who owns a value or a service | [Where does this value belong?](#where-does-this-value-belong) |
| Give a parent several children or route a result after a reorder | [Child identity and routing](#choose-child-identity-and-routing) |
| Read a current value, react to a change, or maintain a total | [Observation](runtime.md#observation) and [child queries and totals](structural-features.md#choose-a-query-or-a-maintained-aggregate) |
| Replace a client or control time in a test | [Inject clients](#3-inject-clients-and-use-the-correct-clock) and [test an application](testing.md) |
| Cancel work, wait for a result, or close a Store | [Receipt lifetimes](receipt-lifetimes.md) |
| Find out why state changed or why a request is slow | [Diagnostics](diagnostics.md) and [app performance](performance.md) |
| Choose speed or executable size for a release | [Choose a release profile](#choose-a-release-profile) |

## 1. Choose the host and ownership model

The main `rca` crate has no default Cargo features. A Cargo feature is a switch that enables optional code.
Enable `tokio` to use an existing Tokio runtime. Enable `test-util` for deterministic application tests.
For storage, add the separate `rca-persistence` crate. See the [support policy](../support.md#features) for the full list.

The [counter example](../../examples/counter/src/main.rs) defines `Counter` and `Action`. This code runs inside its asynchronous `main` function:

```rust
let store = Store::new(0, Counter, TokioRuntime::current()?);
let task = store.send(Action::Increment).await?;
task.finish().await;
assert_eq!(store.read(|state| *state).await?, 1);
store.shutdown().await?;
```

The Store starts at zero. A successful `send` means that the Store **committed** the action: it processed it and registered its follow-up work.
`finish` waits for tracked work from the action and its descendants to end. It does not prove that an external service succeeded.
The read returns one. Shutdown ends the Store.

Use `rca::Store` for work that Rust can move between threads (`Send`).
If state, actions, or futures must stay on one thread, enable `local` and use `rca::local`.
For example, `Rc` shares ownership on a single thread. The owner thread must drive and destroy local work.
A wake from another thread does not transfer ownership. Local dependency keys and values still require `Send + Sync`.
See [runtime choices](runtime.md) and [local testing](testing.md) before choosing this mode.

## 2. Model domain changes and child identity

### Where does this value belong?

Start with ordinary feature state. For a search, the current query is state.
The service that sends the query is a dependency. A reply from the service can become an action.

| What do you need? | Use | Example |
| --- | --- | --- |
| Input for one operation | An action payload | The text of a new search |
| Facts for the next decision | Feature state | A request ID or retry count |
| A service that a test can replace | A [dependency](dependencies.md) | A clock or network client |
| A child that changes its own state and a parent value together | `SharedReducer` with `scope_shared` or `for_each_shared` | Several workers spend one quota |
| Independent owners that share a value outside Store transactions | [`Shared<T>` or `SharedReader<T>`](shared-state.md) | A host writes a value that a feature reads |
| Several handles to one provider's value in a session | [`SharedKeyCache`](shared-state.md#when-is-a-shared-key-cache-appropriate) | Two readers open the same document |
| A document that must load or save | [`rca-persistence`](persistence.md) | A counter must save its last edit before exit |

`SharedReducer` is not `Shared<T>`. The parent lends its value and the child state to the reducer for one synchronous call.
Neither borrow can escape into a task. See the [shared reducer examples](../../crates/rca-test/tests/shared_composition.rs).

### Choose an authoring form

Start with `Reducer`. It works for a counter and for a search that makes network requests.
In the [search example](../../examples/search/src/main.rs), an action starts a request. An effect sends the result back as another action.

| What does the feature need? | Choose |
| --- | --- |
| A short action handler | `Reduce::new` wraps a closure as a reducer. |
| Named inputs and results, or reusable behaviour | Implement `Reducer` on a named type. Return an `Effect` for follow-up work. |
| Asynchronous work that reads or changes current state between steps | Implement `Feature::update`. A `Task` can send typed requests to the actor. |
| Child reducers that borrow part of the parent's state | Compose reducers with `combine`, `scope`, `if_some`, `if_case`, or `for_each`. |
| Independent child lifetimes, commands, outputs, or reactions | Install children with either authoring form. See [structural features](structural-features.md). |

Both callbacks run synchronously. Neither can keep a borrow of state across `.await`. Both can start asynchronous work after commit.
A **context** gives a callback or task permission to perform specific operations. It does not own the state or keep a removed child alive.
A direct `Feature` can also use `context.effect(...)` to start an effect.

For example, a worker can ask the actor for its current retry policy after a request fails.
If a change depends on that policy, read the policy inside the same update that changes state.
Between a separate read and update, another request can change the policy. See the [two-task search](../../examples/composition-search/docs/named-operations.md).

Every `Reducer` automatically implements `Feature`. Implement **one**, not both, for the same Rust type.
If you need two behaviours for the same data, use separate wrapper types. The same rule applies to `local::Reducer` and `local::Feature`.
Both authoring forms work with `Store::new`, `Store::builder`, and the corresponding `TestStore` constructors.

Reducer combinators borrow child state only during the parent's call. They cannot give a task access to that child after the call ends.
To compose direct `Feature` implementations, install the children. One installation can contain both forms.
The [paired examples](../../crates/rca-test/tests/legacy_feature_compatibility.rs) exercise cancellation and late replies with both forms.

### Macros are optional

A derive macro generates a trait implementation from the fields or variants of your type.
Enable the `macros` Cargo feature to use these derives. They do not change who owns state or when effects run.

| Task | Usual choice | Alternative |
| --- | --- | --- |
| Name an injected service with a live default | Derive `DependencyKey` on a unit struct. | Implement `DependencyKey` directly. |
| Replace named client operations in tests | Derive `DependencyClient` on a client with closure fields. | Supply a concrete client or implement a client trait. |
| Route child actions and state | Derive `StatePaths` and `ActionPaths`. | Write paths for custom routes. |
| Record selected fields for tests | Derive `Snapshot`. | Supply a function that captures the values. |

Import the macros from the [same crate](integration-boundaries.md#choose-imports-and-host-explicitly) as the rest of the API.
If Cargo renames the crate, use that alias. See [dependency keys](dependencies.md), [child paths](composition.md#derived-paths-and-tuple-composition), and [snapshots](snapshot-derive.md).
If a parent generates both its state and action types from child reducers, use [`#[reducer]`](../../crates/rca-macros/src/lib.rs).

### Choose child identity and routing

A stable ID identifies a row even after the rows change order. A position does not.
Start with [manual composition](composition.md). It already tracks child lifetimes for routed work.
If each child needs a separate feature instance or a private input, use [installed children](structural-features.md).
For an optional installation, `MountIdentity` distinguishes the business ID, a replacement, and a configuration change.
For a keyed installation, the collection tracks replacement itself.

After `Store::ready`, request the installed child's context through its declared mount site.
If the child no longer exists, handle the missing child or rejected request.
For later replies, keep the context for that specific instance. Do not look up the ID again.
The ID can now name a replacement. `Store::scope` gives a view for the root Store's lifetime, not permission to target an installed child.

## 3. Inject clients and use the correct clock

Supply external services as [dependencies](dependencies.md). An [external client](external-clients.md) can send requests or receive a stream.
In a test, replace it with a client whose responses you control.

If a client needs the shared Store's deadlines, give it the shared clock.
This [shared clock example](../../crates/rca-test/src/store.rs) defines `Client`, `ClientKey`, and `Feature`:

```rust
let store = TestStore::builder((), Feature)
    .with_clock_dependency::<ClientKey>(|clock| Client { clock })
    .build();
```

`TestStore` creates its clock during `build` and passes it to the client constructor.
Advancing the test clock does not advance an unrelated Tokio timer or the system clock.
The local clock stays on its owner thread. It cannot be a dependency value, because those values must be safe to share between threads.
The [local clock example](../../crates/rca-test/src/store/local.rs) keeps it with the owner instead.

## 4. Start and cancel work

For an action-based reply, return an `Effect`. For a typed `Task`, keep its context.
If the Store rejects feedback from a removed child, do not send it through the root Store instead.
A business decision to deny a request differs from cancellation, a full queue, or Store shutdown.
The [external-client example](external-clients.md) shows how to tell these outcomes apart.

A wait on a named operation covers the tasks with that name at the time of the request.
It does not switch to later tasks with the same name. Cancellation requests an end to work.
It does not undo a remote write or promise to release every resource before it returns.

Do not wait for a parent action's `ActionTask::finish` from work started by that action.
The parent waits for its descendants, so neither can finish. Wait from outside that group.
Removing an inherited cancellation key does not remove ownership by the root action, child installation, or Store.

## 5. Assert transactions and independent history

Use [`TestStore`](testing.md) to send external inputs and receive results that the application produced.
For an action that changes state, specify the expected change with `send_with` or `receive_with`.
For a task that changes state directly, receive the update. Do not invent a response action to make the test pass.

If state cannot be cloned independently, compare an [owned snapshot](snapshot-derive.md).
A cloned mutable handle can show a later value instead of the original value.
For example, the [owned importer](../../examples/composition-search/docs/owned-importer.md) keeps its resource outside the comparison and checks its lifetime separately.

## 6. Wait for the result the caller needs

Choose a wait that matches the result your caller needs. A [receipt](receipt-lifetimes.md) records a particular stage of work.
Dropping an ordinary action task does not cancel it. Closing an input gate does not remove the child or cancel its tasks.
An observer can receive the latest state without receiving every intermediate state.

If the application must preserve a document, wait for its save or close acknowledgement before Store shutdown.
Before reopening the same resource, wait for the previous backend's work to stop.
The [persistent counter](../../examples/persistent-counter/README.md) shows the guarantees of one file backend.

For a real storage service or native interface, also try the operation on the actual host.
A controlled client can establish the feature's response, not the provider's behaviour. See [integration boundaries](integration-boundaries.md).
For speed claims, [measure the result at its caller](performance.md).

### Choose a release profile

A Cargo profile selects build settings. Your [workspace profile](https://doc.rust-lang.org/cargo/reference/profiles.html) applies to your code and RCA.
Cargo ignores profiles in dependencies, including local dependencies.
Start with `cargo build --release`. By default, this profile uses `opt-level = 3` for speed.

If executable size matters more, add this profile to the `Cargo.toml` at your workspace root:

```toml
[profile.release-small]
inherits = "release"
opt-level = "s"
```

Run `cargo build --profile release-small`. Compare the sizes of both executables.
Time the same workload with each build on your target device. The size setting does not guarantee a smaller executable.
