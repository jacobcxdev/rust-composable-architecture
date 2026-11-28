# Connect a feature to an external service

A client is the part of an application that talks to an external service.
For example, a feature can use a client to request data, listen for events, or ask the operating system to open a URL.
The feature names the operation. The application supplies the client and decides what it permits.
The Store decides whether the result still belongs to that feature.

The [complete example](../../crates/rca-test/tests/external_clients.rs) uses `rca-core` and `rca-test` directly.
It uses no macros, network connection, or operating-system URL handler.
A controlled client lets the test decide exactly when each request finishes or each event arrives.
The test drives the runtime, which runs the pending work.

## Choose a dependency or a captured value

Keep the input for one request, such as its URL, in its action.
Supply the client that makes the request as a [dependency](dependencies.md).
For other choices, see
[where a value belongs](authoring.md#where-does-this-value-belong).

A task keeps its original client and inputs. A later change to state does not replace them.
If the task needs the current policy, read it from the Store.
If the policy determines whether to accept a change, use the policy inside that same update.
A separate read leaves time for the policy to change.
The [session-refresh tests](../../crates/rca-test/tests/session_client_refresh.rs)
distinguish captured clients, current services and request identity.

## Define the boundary in the application

The example's `ClientsKey` names the client without supplying a default implementation.
It declares `type Value = Clients` on its `DependencyKey` implementation.
Supply the client with `Store::builder(...).with_dependency::<ClientsKey>(client)`.
A `TestStore` accepts it the same way. If nobody supplies the client, lookup panics rather than performing real I/O.
For clients with generated test defaults, see [the dependency guide](dependencies.md#define-a-key-and-supply-a-client).

The client exposes three operations. A request or URL operation returns a future: one result that can arrive later.
An event subscription returns a stream: a sequence of items.
This excerpt uses Rust's `Future`, `Pin`, and `Arc`, plus `Stream` from the `futures-core` crate.
The complete example defines the reply and error types.

```rust
type Response<T> = Pin<Box<dyn Future<Output = Result<T, ControlCancelled>> + Send>>;
type Events = Pin<Box<dyn Stream<Item = Result<Reply, ControlCancelled>> + Send>>;

struct Clients {
    request: Arc<dyn Fn(u8) -> Response<Result<Reply, Denied>> + Send + Sync>,
    open_url: Arc<dyn Fn(String) -> Response<Result<(), Denied>> + Send + Sync>,
    events: Arc<dyn Fn(u8) -> Events + Send + Sync>,
}
```

The test defines these types. RCA does not require their specific inputs or errors.
In a live application, let your provider choose its request, URL, and cancellation types.
Return a future or stream that owns what it needs after the call returns.
Keep URL validation, authorisation, and external operations in the provider.
`ControlCancelled` means the test cancelled the source, not that an HTTP request failed.

The `Arc` handles let callers share the client's operations. The test moves each result to its receiver instead of sharing it.
Its `Reply` and `Denied` types contain `Cell<u64>` and do not implement `Clone`, `Debug`, or `Sync`.
Sending a result to another thread does not require those three traits.

## Capture owned work, then forward through the effect sender

A reducer handles an action and returns an effect for work that can finish later.
Read the dependency while the reducer handles the action. Give the effect the future or stream, not the borrowed `&mut ReduceContext`.
The effect awaits the result and uses its own sender to report it:

```rust
let response = (context.dependency::<ClientsKey>().request)(state.id);
return Effect::run(move |sender| async move {
    // Success means the Store committed the response. Do not retry a rejection through the root.
    let _ = sender.send(FeatureAction::Response(response.await)).await;
});
```

Creating the future can itself run provider code before the first wait.
The provider decides when I/O starts and what cancellation stops. RCA cannot undo work that the provider started.

For events, read each item from the stream. Send it with the effect's sender.
If the sender rejects an action, stop. In this example, cancellation produces one cancellation action.
A normal end of the stream produces `EventsClosed`. The reducer records each outcome.

## Keep the outcomes separate

| Boundary | Meaning in the fixture | Consumer-visible proof |
| --- | --- | --- |
| Service denies a request | `Ok(Err(Denied(...)))` | The call finished, but its result was `RequestDenied` or `UrlDenied`. |
| Test cancels the source | Outer `Err(ControlCancelled)` | The action records `RequestCancelled`, `UrlCancelled`, or `EventsCancelled`. |
| Event source closes normally | `StreamControl::close()` lets queued items drain before the stream ends | The Store receives `Event` actions in order, then `EventsClosed`. |
| Input queue fills | `try_send` returns `Full(value)` | The caller keeps the rejected value. The Store receives no event for it. |
| Input receiver no longer exists | `ReceiverDropped(value)` | The caller gets the late reply or event back. |
| Feature no longer accepts output | The effect sender rejects the action. | Old work cannot change a replacement feature with the same ID. |
| Store shuts down | The runtime drops the pending work. | Neither a pending reply nor a new event can change the Store. |

When `StreamControl::send(value).await` succeeds, the stream's input queue accepted the item.
That result does not tell you whether the effect read the item or sent an action to the Store.
When `EffectSender::send(action).await` succeeds, the Store committed that action.
Neither result means an independent observer processed the change.

Capacity limits only the number of items in the stream's input queue.
Each waiting `StreamControl::send` still owns one more item outside the queue.
The example uses `StreamControl::try_send` to avoid the wait.
If the queue is full, the application can retry, discard the item, slow the source, or report the problem.

After Store shutdown, the effect sender cannot deliver more output.
A [cancellation receipt](receipt-lifetimes.md) does not tell the caller exactly when a suspended future drops its receivers.
After shutdown, the test calls `DeterministicRuntime::run_until_stalled()` to run work that is ready.
Pending replies and new events then fail because their receivers no longer exist.

Use a controlled client to exercise the feature without real I/O.
Use a real provider to test sockets, permissions, cancellation, and cleanup of background work.
If a test must advance a timeout without a real delay, supply the client with the test Store's RCA clock.
Advancing that clock does not advance an unrelated timer or a real service.

## What happens to a request after child removal?

The [composition guide](composition.md) explains how a parent feature owns a child.
The example covers an optional child (`if_some`), an enum case (`if_case`), and a keyed collection (`for_each`).
For each kind of child, the test starts a request, a URL operation, and an event subscription.
It commits an event, removes the child, and inserts a replacement with the same identity.

The old receivers no longer exist. The old effect senders cannot change the replacement.
Work that starts through the replacement still succeeds.

If a child sender rejects an action, do not retry it through the root Store.
The Store treats that retry as a new input. It can reach a replacement child
that happens to have the same ID. Child removal does not disable the root:
the test separately sends fresh `RootInput`, which succeeds.
This recipe does not exercise stack entries. The [stack example](recursive-jobs.md#lifetime-and-ownership)
shows how removing an ancestor retires a descendant's timer and rejects its later output.

## Reproduce

Run this test from the repository root.
On macOS, pass the Cargo arguments to the [build memory guard](../../CONTRIBUTING.md#build-memory-safety).

```sh
cargo test -p rca-test --test external_clients
```

The test records each argument and controls each reply, event, and cancellation.
Only the external operations are substitutes. RCA still looks up the client, runs the reducer, and decides whether to accept each action.
