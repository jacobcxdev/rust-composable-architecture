# MCMaitreD migration example

This unpublished crate starts an isolated migration of the preserved MCMaitreD
server domain to RCA. It is not a runnable daemon or a complete server feature.
No original application files are changed.

Proxy/server integration has five controlled-client traces covering wake-up,
independent stop domains, stop/error joins, failure/EOF and keyed replacement.
An explicit inherited-ID cancellation boundary fixes two historically reproduced
cross-feature cancellation failures. The
[migration case study](../../docs/migration/composable-consumer.md#proxyserver-composition-qualification)
describes that boundary and its retained local verification. Those historical
results are not current-release receipts; see the
[qualification summary](../../docs/qualification.md).

`backend::Backend` owns backend lifecycle policy. Inject `Arc<dyn BackendClient>`
through `BackendKey` on the Store or TestStore builder. The client must report
completed external operations, not merely queued requests. There is no default
network or process implementation; missing client wiring panics.

Start and stop requests retain the latest target. Reconciliation runs at most one
external operation at a time per feature lifetime. Opposite requests wait for the
pending operation, then reconcile again. Failures retain their message and suspend
automatic reconciliation. Parent-facing `Started` and `Stopped` actions expose the
integration boundary. `server::reducer()` composes backend, polling and proxy scopes.

The captured application allowed overlapping start/stop operations and could lose
a start request made while stopping. Serial reconciliation deliberately repairs
that policy; this is not a literal port or an automatic guarantee supplied by RCA.
The tests assert complete backend state after each action, exact client calls,
delayed responses, both reversal directions, errors and isolated dependencies.

Run from the RCA workspace with its pinned Rust 1.98.0 toolchain:

```sh
cargo test --locked -p rca-example-mcmd-server
```

## Polling

`polling::Polling` owns an immediate query followed by five-second ticks. Inject
`Arc<dyn QueryClient>` through `QueryKey`. At most one query runs at once; ticks
while it is pending are skipped. Stop cancels the timer and its descendant query.
Restart creates a new runtime lifetime. State contains no task handles.

Success updates the count and clears failures. Errors retain the previous count,
record their message and saturate the failure counter. Stop preserves observations.
The composed server clears observations only after confirmed backend stop.
`QueryError` preserves the captured client's `ConnectionFailed`, `Protocol` and
`Other` categories and payloads through the client and action boundary. Observable
error text retains the captured diagnostic spelling, including quoted payloads.
No category-dependent retry or shutdown policy is implemented or claimed.

Eight full-state tests cover cadence, repeated starts, pending ticks, error recovery,
saturation, cancellation of immediate and timer-derived queries, restart, inactive
actions and Store-local dependencies. These prove controlled future cancellation,
not real network cleanup.

## Limits

`config` preserves the captured floor/table/server configuration and read-only
validation. It rejects duplicate table/server identities, empty tables, zero
inactivity timeouts, zero ports and repeated ports within one server (including
across Java and Bedrock lists). Validation preserves traversal order and all
metadata. Empty floors and empty port lists remain valid; ports shared by
different servers remain allowed. This is not a socket-binding feasibility check.

Table and server identities use separate String fields, not the original typed
ID wrappers. This module does not flatten tables into keyed state or implement
inactivity policy. Callers must validate configuration at their input boundary;
existing direct `proxy::Config` construction and `servers::Add` do not invoke it.

- No real network client, configuration loader, daemon
  wiring or deployment is implemented here yet.
- Cancelling/dropping a client future does not undo an external operation. Store
  shutdown and keyed removal need a real client cleanup/reconciliation
  contract before daemon adoption. A client that never completes stalls this
  feature; no timeout or recovery policy is implied.
- Completion actions are internal messages. Phase checks do not authenticate
  external forged completions or identify a prior operation of the same kind.
- Restart reconciliation follows the `Stopped` action's commit, not completion of
  arbitrary parent effects. The server coordinator applies polling controls in
  the same reduction, rather than depending on asynchronous delegate cleanup.
- No performance, native-host or complete consumer-parity claim follows from these
  deterministic tests. See the [migration report](../../docs/migration/composable-consumer.md).

## Server composition

Build a Store with `server::State::new(proxy_config)` and `server::reducer()`, injecting
all three client keys when using proxy work. The coordinator retains three required scopes and synchronously
applies polling Start/Stop after backend actions. These controls are not separate
Store commits. Polling runs only for a wanted Running backend. Stop requests
cancel polling immediately; confirmed backend stop clears count, failures and
query error. A failed stop preserves observations but leaves polling inactive.

This deliberately cancels queries earlier than the captured implementation.
Six integration traces cover successful lifecycle, delayed cancellation, both
target-reversal directions, failures and duplicate delegates. Both child IDs are
constructed together and must remain stable; this example does not validate
arbitrary restored state or forged internal actions. Required scopes preserve
local cancellation identity but do not qualify external cleanup.

## Keyed server parent

`servers::reducer()` wraps the composed server with RCA `for_each`.
`servers::State::default()` starts empty. Add accepts `proxy::Config` and inserts a stopped server with matching
key/client identities; duplicate Add preserves state and lifetime. Server routes
an existing child's action. Forget removes a local lifetime, cancelling its RCA
timers/queries and rejecting obsolete outputs. Equal-key reinsertion starts fresh.

Forget does **not** stop a remote backend. Coordinate external shutdown first or
retain independent reconciliation authority before using removal in a daemon.
The controlled-client tests prove local future ownership, not remote teardown.
They cover two siblings with equal local cancellation IDs, surviving query/timer
work, duplicate preservation and pending-query removal/reinsertion. One reducer
value handles all entries; no mutable reducer instance per key is created.

## Owned proxy sessions

`proxy::Proxy` is a local-session feature; its server/keyed composition currently
uses the cancellation boundary described above. Inject `Arc<dyn ProxyClient>` through `ProxyKey`. One
cancellable effect owns acquisition and the resulting event stream. Ready commits
before events; Stop cancels acquisition or streaming; EOF drops the stream before
Ended changes the phase to Stopped. Explicit Start retries acquisition failures.

Acquisition errors retain `StartFailed` and `Other` categories and payloads through
the client and action boundary. State records debug-shaped diagnostic text,
including quoted and escaped payloads. No category-dependent policy is added.
The captured `StopFailed` category remains outside this acquisition-only API;
the separate managed proxy below covers that asynchronous contract.

Clients must release partial resources when acquisition is dropped and live
resources when the stream is dropped. Six full-state traces use non-Clone leases
to check release, retry, ordering, duplicate suppression and Store isolation.
This is a semantic client obligation, not a type-system guarantee or real-socket
proof. Cancellation is cooperative; these tests do not promise instantaneous
physical teardown on every host.

Unlike the captured separate start/stop-by-ID interface, this boundary requires
local owned resources. Fallible asynchronous remote shutdown needs an independent
cleanup/reconciliation owner; returning a stream alone is insufficient. That
deliberate narrowing is not full old-client parity. JoinObserved now requests a
stopped backend to start; its backend effect excludes the proxy's inherited
cancellation identity while retaining root task and structural ownership.
Internal phase guards do not authenticate forged messages;
the pending-acquisition restart trace does not exercise already-queued old events.

## Managed external proxy

`managed_proxy` is a separate client contract for externally owned resources.
Create `channel(config, client)`, drive `Owner::run` independently of RCA, and
construct `ManagedProxy::new(control)`. The application must create only one
owner per external server ID, join its result and retain any pending `Recovery`.
Existing local proxy and server/keyed composition are unchanged.

The owner serialises start and idempotent, fallible stop operations through a
capacity-one command queue. RCA owns reply waits and event delivery, not the
external operations. Cancelling an admitted wait cannot cancel its operation.
A bounded acknowledgement distinguishes received starts from abandoned buffered
replies. Abandoned successful starts trigger cleanup; queued Stop remains usable
without waiting for start-reply consumption. Dropping a stream already returned
to its caller does not itself request cleanup.

Stop enters Stopping, cancels event delivery and reports Stopped only after
confirmed release. EOF also requests cleanup. StartFailed, StopFailed and Other
preserve typed payloads and Debug-shaped state diagnostics. Failed start may have
partially acquired resources; failed stop never confirms release. Both require
explicit Stop before another Start. Full/closed admission errors are explicit.

After the last control closes, the owner attempts cleanup once for a live session
without a retained error. It does not automatically retry failures. Returned
must-use `Recovery` retains the client, identity, pending obligation and exact
error. `retry(&mut self)` preserves authority if its future is cancelled; the
client's idempotent stop contract remains required.

Controlled-client proofs now include a
[test-only local TCP cleanup adapter](../../docs/migration/native-managed-proxy-ownership.md).
This is not a production native proxy or complete daemon.
Host panic/abortion, process exit, duplicate owners, and discarded recovery values
remain application responsibilities. A permanently pending client operation also
blocks later operations; this example adds no timeout, forced abort or backoff.
