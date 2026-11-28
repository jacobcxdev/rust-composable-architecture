# Compose server lifetimes

This headless library example manages the lifetimes of Minecraft servers.
It has no daemon, configuration loader, production network client or deployment command.
The host supplies clients for the backend, player queries and proxy sessions.

Start with the owner of the resource:

| If you need to… | Use | What removal means |
| --- | --- | --- |
| Drop a proxy stream that owns its local resources | `server::reducer()` and `servers::reducer()` | RCA cancels the child's local work. It does not stop a remote backend. |
| Wait for an external service to acknowledge shutdown | `managed_proxy` and `managed_servers::compose` | The host retains the external owner. The managed roster removes only a stopped server. |

## Run the example tests

Use the [workspace toolchain](../../rust-toolchain.toml).
On macOS, use the [Cargo memory guard](../../CONTRIBUTING.md#build-memory-safety).

```sh
cargo test --locked -p rca-example-mcmd-server
```

The [server/proxy tests](tests/server_proxy.rs) cover wake-up, independent cancellation, backend joins, proxy failure, end of stream and keyed replacement.
The [qualification guide](../../docs/qualification.md) distinguishes these test results from release evidence.

## Start, stop and poll a backend

`backend::Backend` records the latest requested target: running or stopped.
It starts at most one external operation per backend lifetime.
If a new request reverses the target, the backend waits for the current operation, then acts on the latest request.
A failure retains its message and stops automatic retries.

Supply an `Arc<dyn BackendClient>` through `BackendKey` on the Store or TestStore builder.
The client must report completion of the external operation. Admission to a queue is not completion.
The example has no default client. A missing client causes a panic.
`Started` and `Stopped` report transitions to the parent. Internal completion actions are trusted messages, not authenticated external input.

`polling::Polling` runs a query immediately, then on five-second ticks.
Supply an `Arc<dyn QueryClient>` through `QueryKey`.
If a query is pending, the next tick skips it.
Stop cancels the timer and its current query. Restart creates a new lifetime.
State contains no task handles.

A successful query updates the player count and clears failures.
An error preserves the previous count, records the message and increments the failure count up to its maximum.
`QueryError` preserves the `ConnectionFailed`, `Protocol` and `Other` categories and payloads.
Diagnostic text preserves quotes and escapes in those payloads. The error category does not select a retry or shutdown policy.

`server::reducer()` combines the backend, polling and proxy.
Create the Store with `server::State::new(proxy_config)`. For proxy work, supply all three client keys.
The coordinator starts polling only when the backend is running and the caller still wants it to run.
A stop request cancels polling immediately. A successful backend stop also clears the count, failures and query error.
A failed stop preserves those observations but leaves polling off.

Polling controls run during the same reduction as the backend action. They do not create a separate Store commit.
A restart follows the `Stopped` commit, not the end of unrelated effects.
The coordinator creates the child IDs together. It does not validate arbitrary restored state.

## Route actions to several servers

`servers::reducer()` uses `for_each` to route actions to existing servers.
`servers::State::default()` starts empty. An Add action takes a `proxy::Config` and inserts one stopped server.
Adding an existing ID preserves that server and its lifetime.
One reducer handles every entry.

Forget removes local state, cancels the child's RCA timers and queries, and rejects obsolete outputs.
If you reuse that ID, the new child has its own lifetime.
**Forget does not shut down the remote backend.** Stop it first or retain an independent owner that can finish cleanup.
The controlled clients test sibling isolation, duplicate adds and removal during a query.
Cancellation of a client future cannot undo a completed external operation.

## Validate configuration at the input

The `config` module keeps floor, table and server configuration in its original order.
Validation rejects duplicate table or server IDs, empty tables, zero inactivity timeouts, zero ports and repeated ports within one server.
The duplicate check covers both Java and Bedrock ports.
An empty floor, empty port lists and ports shared by different servers are valid.
Validation does not check whether the host can bind a socket.

Table and server IDs are separate strings.
The module does not flatten tables into keyed state or enforce inactivity timeouts.
Validate external input before constructing `proxy::Config` or sending `servers::Action::Add`.
Neither entry point does this for you.

## Own a proxy stream locally

`proxy::Proxy` uses one cancellable effect to acquire a session and consume its event stream.
Supply an `Arc<dyn ProxyClient>` through `ProxyKey`.
Ready commits before the first event. Stop cancels acquisition or streaming.
At end of stream, the effect drops the stream before Ended changes the phase to Stopped.
If acquisition fails, an explicit Start retries it.

The client must release partial resources on cancellation and live resources on stream drop.
The type system cannot enforce either obligation.
`StartFailed` and `Other` retain their typed payloads through acquisition. State records the diagnostic text.
`StopFailed` belongs to the managed contract below.

A join event can wake a stopped backend.
Its backend effect does not inherit the proxy's cancellation ID.
It still belongs to the root task and its structural scope.
The [controlled tests](tests/server_proxy.rs) exercise old joins after cancellation and the effect on a live sibling.
The [local socket tests](tests/proxy_socket.rs) separately check that a client releases a socket it owns.

## Wait for external proxy cleanup

If stopping a proxy needs an asynchronous reply that can fail, dropping its stream is not enough.
Create a `channel(config, client)`. Run `Owner::run` independently of RCA.
Connect the feature with `ManagedProxy::new(control)`.
Keep one owner per external server ID. Join its result and retain any `Recovery` it returns.

The owner processes one start or stop at a time through a queue with capacity one.
RCA owns the reply waits and event delivery. The owner owns the external operations.
Cancelling an accepted wait does not cancel its operation.
If the receiver abandons a successful start reply, the owner requests cleanup.
A queued stop can proceed without waiting for that reply to be consumed.
Dropping a stream after the caller receives it does not request cleanup.

Stop ends event delivery and enters Stopping. The phase becomes Stopped only after the client reports release.
End of stream also requests cleanup.
A failed start can leave partial resources. A failed stop does not establish release.
Both require an explicit Stop before another Start.
A full or closed queue reports an admission error.

When the last control closes, the owner attempts cleanup once for a live session without a retained error.
It does not retry a failure automatically.
`Recovery` keeps the client, server ID, error and unfinished cleanup.
Cancelling `retry(&mut self)` preserves the caller's ability to retry. The external stop operation must be idempotent.

`managed_servers::compose` takes a fixed roster and rejects duplicate IDs before it creates owners.
Drive each returned owner independently. If a server still has active polling or an unconfirmed stop, Remove leaves it in the roster.
Restore reuses the same external owner. The [managed roster tests](tests/managed_servers.rs) cover failed cleanup, retry, removal and restoration.

The [managed socket tests](tests/managed_proxy_socket.rs) use a local TCP adapter.
The peer must observe end of stream after release. A pending or failed stop leaves the socket usable.
The example does not add a timeout, forced abort or backoff.
If the client never completes an operation, later work for that owner also waits.
The application remains responsible for duplicate owners, discarded recovery values, host failure and process exit.
