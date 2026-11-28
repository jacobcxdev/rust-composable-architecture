# Owned external clients

An RCA feature can request data, listen for events or ask the host to open a URL.
The application supplies the client and decides which operations it permits.
The Store decides whether output from that client still belongs to a live feature.

The executable recipe is [`rca-test/tests/external_clients.rs`](../../rca-test/tests/external_clients.rs).
It uses the public `rca-core` and `rca-test` APIs, a deterministic runtime and
controlled endpoints. It never performs network requests or opens URLs.

## Choose a dependency or a captured value

Use an action payload for the input to one operation, such as a search query.
Use state for domain information that changes through RCA transactions.
Use a dependency for an application-owned capability, such as a network client
or file service. A dependency is not another mutable state store.

If the library cannot choose a safe provider, use an interface-only `DependencyKey`
and explicit client injection. A live default suits an intentional
application default, not an implicit provider selected by RCA. TestStore uses
test defaults for missing keys and never falls back implicitly to live defaults.
Explicit test injection can still select a live client, so tests must choose it
deliberately. See [testing](testing.md#make-dependencies-and-progress-explicit).

A task keeps the client and inputs it captured. A later change to State does not
replace them. If the task needs the current policy, read it from the Store.
If the policy determines whether a change is allowed, check it inside the update.
A separate read leaves time for the policy to change.
The [session-refresh tests](../../rca-test/tests/session_client_refresh.rs)
distinguish captured clients, current services and request identity.

## Define the boundary in the application

The recipe's `ClientsKey` is interface-only: its `DependencyKey` implementation
declares `type Value = Clients` without a live factory. Inject the owned value
with `Store::builder(...).with_dependency::<ClientsKey>(client)`; a `TestStore`
can inject it in the same way. Missing registration fails explicitly rather than
silently selecting ambient I/O. `DependencyClient` can generate endpoint
construction when useful, but the recipe does not need a macro or another crate.

The application-owned endpoint shapes are:

```rust
type Response<T> = Pin<Box<dyn Future<Output = Result<T, ControlCancelled>> + Send>>;
type Events = Pin<Box<dyn Stream<Item = Result<Reply, ControlCancelled>> + Send>>;

struct Clients {
    request: Arc<dyn Fn(u8) -> Response<Result<Reply, Denied>> + Send + Sync>,
    open_url: Arc<dyn Fn(String) -> Response<Result<(), Denied>> + Send + Sync>,
    events: Arc<dyn Fn(u8) -> Events + Send + Sync>,
}
```

These are recipe types, not RCA platform interfaces. In a live application, use
your own request, URL and cancellation types and return owned futures/streams
from your chosen provider. Keep URL validation, authorisation and real side
effects in that provider. `ControlCancelled` belongs to the deterministic
fixture, not an imposed HTTP or operating-system error vocabulary.

Only endpoint handles are shared. The recipe's `Reply` and `Denied` contain
`Cell<u64>` and are deliberately neither `Clone`, `Debug` nor `Sync`. Moving a
payload through a `Send` effect does not impose those additional domain bounds.

## Capture owned work, then forward through the effect sender

Resolve a dependency during reduction and take its owned future or stream out
before returning the effect. Do not capture `&mut ReduceContext` or read a
global/task-local registry after suspension:

```rust
let response = (context.dependency::<ClientsKey>().request)(state.id);
return Effect::run(move |sender| async move {
    // This one-shot effect ends on either admission or rejection; never retry via root input.
    let _ = sender.send(FeatureAction::Response(response.await)).await;
});
```

Creating a provider future can itself run synchronous provider code. Its actual
I/O and cancellation behaviour belong to that provider's contract. RCA does not
make an arbitrary client lazy or undo work that the client already started.

For events, poll the owned stream and send each typed item through that same
sender. Stop forwarding when output is rejected. In the fixture a cancelled
stream sends one distinct cancellation action and stops; graceful end-of-stream
sends `EventsClosed`. The reducer records each observable outcome.

## Keep the outcomes separate

| Boundary | Meaning in the fixture | Consumer-visible proof |
| --- | --- | --- |
| Domain request/URL result | `Ok(Err(Denied(...)))` | `RequestDenied` or `UrlDenied`, not cancellation |
| Controlled source cancellation | Outer `Err(ControlCancelled)` | `RequestCancelled`, `UrlCancelled` or `EventsCancelled` |
| Graceful event-source shutdown | `StreamControl::close()` drains admitted items, then EOF | Ordered `Event` actions followed by `EventsClosed` |
| Bounded input overflow | `try_send` returns `Full(value)` | Rejected value remains owned by the caller; no event is committed |
| Dropped input receiver | `ReceiverDropped(value)` | A late request completion/event is rejected and its value retained |
| Ended effect-output authority | Effect sender rejects output | Old work cannot commit to a same-identity replacement |
| Store shutdown | Pending owned work is dropped | Pending completions, event admission and retained output are rejected |

When a send succeeds, the queue has accepted the item. The reducer need not have
processed it yet. Capacity limits the items in the queue. Each waiting
`send(value)` still owns one more item outside the queue. The recipe uses
`try_send` to avoid this wait. On overflow, the application can retry, discard
the item, slow the source or report the problem. A full queue does not mean the
provider denied the request.

After Store shutdown, the effect sender cannot deliver more output. A
cancellation receipt alone does not tell you when a suspended future drops its
receivers. The test calls `DeterministicRuntime::run_until_stalled()` after
shutdown. It then checks that pending request completions, URL completions and
new events fail because their receivers are gone.

Use a controlled client to test RCA routing and ownership without real I/O.
Use a real provider test to establish sockets, permissions, provider cancellation
and background-worker cleanup. Neither test replaces the other. A client with
timeouts must use an injected RCA clock for deterministic TestStore advancement.
An unrelated timer or real service will not advance with that virtual clock.

## Respect child lifetime, without revoking fresh root input

The recipe uses real `if_some`, `if_case` and `for_each` composition and real
scoped Store handles. For each family it starts a request, URL operation and
event subscription, commits an event, removes the child, then inserts the same optional/case/keyed
identity. It checks that the historical future and stream receivers are dropped,
retained effect senders reject output, and the replacement remains unchanged.
Fresh work started through the replacement succeeds.

If a child sender rejects an action, do not retry it through the root Store.
The Store treats that retry as a new input. It can reach a replacement child
that happens to have the same ID. Child removal does not disable the root:
the test separately sends fresh `RootInput` and checks that it succeeds.
This recipe does not exercise stack entries. The [stack example](../recursive-jobs.md#lifetime-and-ownership)
shows how removing an ancestor retires a descendant's timer and rejects its later output.

## Reproduce

From the repository root:

```sh
cargo test -p rca-test --test external_clients
```

The controlled fixture records each exact request/URL/subscription argument and
returns its controller to the test. Tests choose completion, denial, cancellation,
queue saturation or closure explicitly. The real dependency lookup, reducer,
Store, scoped lifetime and effect output paths remain active throughout.
