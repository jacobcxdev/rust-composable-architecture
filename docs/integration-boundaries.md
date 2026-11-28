# Headless integration boundaries

RCA owns serial state transitions and the lifetime of admitted work.
Applications own rendering and external services. This guide defines that boundary
and the checks needed for host and provider integrations.

## Choose imports and host explicitly

Use `rca-core` directly or use the `rca` facade. The facade has no default features. Its optional `tokio`,
`test-util`, `diagnostics`, `futures-stream`, `macros` and `local` features add
their named surfaces, not a UI framework. Clocks, typed dependencies, sharing and
scoped observation remain core capabilities. See the actual
[facade features](../crates/rca/Cargo.toml) and [exports](../crates/rca/src/lib.rs).

At the same version, `rca` and `rust-composable-architecture` expose the same types.
Import macros from the package you use for those types.

If your crate directly depends on two versions of `rca` under different names,
a derive can implement the other version's trait. Cargo aliases alone do not prevent this.
Use one direct version per crate, or implement the trait explicitly through the intended alias.

Select `rca-tokio` for the Tokio adapter, `rca-test` for deterministic hosts and
controlled clients, and `rca-persistence` separately for document coordination.
Use its explicit `Document::new`/`SharedDocument::new` constructors and supplied
codec/backend rather than expecting a facade alias to select storage. These
choices do not install a runtime or choose a serialisation format implicitly.

Start with the [synchronous host contract](runtime.md#synchronous-hosts), the
[persistent counter](../examples/persistent-counter/README.md) for load/edit/close,
the [bounded-job recipe](runtime.md#finite-batches-and-bounded-retry), and the
[two-task named search](../examples/composition-search/docs/named-operations.md)
for captured inputs and live policy reads. Loading saved domain state does not
restart its former tasks: decide explicitly which jobs to launch after loading.
Inject clients per Store or TestStore as shown in the
[dependency guide](runtime.md#dependencies); no global test mode is needed.

## Choose shared state or persistence

Shared state and saved state solve different problems. A shared in-memory value
lets several consumers access one resource. It does not survive process exit
unless an explicit provider saves it.

| Requirement | Starting point | Boundary to retain |
| --- | --- | --- |
| Share one in-memory value | `Shared` or read-only `SharedReader` | Shared access does not imply persistence |
| Load or subscribe to a provider resource through a cache | `SharedReaderKey` or `SharedKey` | The provider defines loading, delivery and save guarantees |
| Connect raw storage and a codec to that cache | `BackendKey` | Saves are explicit, with no document flush barrier or watcher |
| Coordinate one actor-owned document with edits and acknowledgements | `Document` | Its state and reducer form one lifetime |
| Connect document coordination to cached shared values | `SharedDocument` with `DocumentKey` | Retain the owned session through physical cleanup |
| Reject writes that conflict with another writer | `SharedDocument::revisioned` with `RevisionBackend` | The backend must provide genuine atomic compare-and-save |

For a single-writer saved document without shared-cache integration, start with
`Document`. `BackendKey` is a smaller adapter, not a document
coordinator: a save does not refresh existing cached values. Reload explicitly.
Both `Document::new` and `SharedDocument::new` write unconditionally, even if the backend also supports revision checks.
To reject a conflicting write, choose `SharedDocument::revisioned` and supply a backend that can compare and save atomically.

Document revision, backend revision and schema version are different identities.
A document revision counts local edits. A backend revision identifies a storage
version for atomic comparison. The codec defines the schema version.
File replacement alone does not supply conflict detection. The supplied file
backend does not implement `RevisionBackend` for arbitrary external writers.

Before adding revision-aware storage, choose an explicit conflict policy.
The revisioned shared document retains local and external evidence on a conflict
instead of silently overwriting newer data. RCA does not choose a merge policy.
Application code owns recovery and any caller decision. The
[shared-document tests](../crates/rca-persistence/tests/shared_document.rs) cover conflict,
failed saves, superseded loads and retained backend sessions.

The [persistent counter](../examples/persistent-counter/README.md) separates a
successful save acknowledgement from backend cleanup and Store shutdown.
Neither a state snapshot nor a finished action task proves durable storage.

## Native controls belong to the host

A native host owns its controls, draws them and handles platform events.
For example, a taskbar item can send a command to a particular worker.
After `Store::ready`, use [`Store::acquire(&typed_mount_site)`](../crates/rca-core/src/store/mod.rs)
with that worker's [`MountSite`](../crates/rca-core/src/store/mount.rs). The actor handles the request in order.
If the worker is present, give its [`TaskContext`](../crates/rca-core/src/store/task_context.rs) to the callback.
Use its `read`/`update`/`send` operations without blocking the event loop.

Handle a missing worker or a rejected command explicitly. A replacement or
configuration change makes the old context invalid. Do not look up the current
worker by business ID to retry an old callback. Do not forward it through the root.
Either route can send the old command to the wrong worker.

Observe only the values the control displays. A slow observer can miss
intermediate values. For a list of workers, [ordered ID observation](runtime.md#observation)
reports changes in membership or order, not child progress or replacement under the same ID.
Use a separate projection for progress.

A [scoped projection](composition.md#scoped-store-handles) and an acquired
child context serve different purposes. `Store::scope` shares the root's
lifetime. It does not bind a callback to one installation and configuration.
The host still owns the connection between state observation and its controls.

Keep thread-affine resources on their owner thread and arrange teardown there.
The optional [`rca::local` surface](../crates/rca-core/src/local.rs) exposes an owner-local
Store/runtime; its [driver contract](../crates/rca-core/src/runtime/local.rs) requires
owner-thread polling and destruction, including unpolled work at host teardown.
Cross-thread wakes only notify that owner. Portable Stores can instead exchange
owned `Send` commands with a host-owned resource, as in the
[thread-affine host fixture](../crates/rca-tokio/tests/thread_affine_host.rs).
Neither route supplies a native event-loop adapter.

Keep the host driven through Store shutdown and any separate provider cleanup.
Cancellation acknowledgement is not synchronous capture destruction or external
quiescence; see [receipt lifetimes](receipt-lifetimes.md). Before claiming a
native integration, test activation, same-identity replacement, retained
callbacks and shutdown on the selected actual platform. Synthetic host and
owner-local engine tests alone do not establish that claim. Core supplies no
turnkey native renderer or Swift/Objective-C runtime wrapper.

## Define external notification delivery

[`ExternalObservation`](../crates/rca-core/src/observation/adapters.rs) owns one lazy
subscription with initial and latest values. It is not an ordered broadcast or
an all-listener completion barrier. Store observations also allow independent
consumers to progress at different rates.

Before shipping a notification adapter, define its delivery contract:

- Specify which listeners receive each post and when that selection becomes fixed.
- Specify delivery order, nested posts, and listener additions or removals during delivery.
- Define cancellation before selection, after selection, and after callback admission.
- State whether acknowledgement means callback return, consumer processing, or completion of explicitly owned asynchronous work.
- Bound pending values and define overflow, slow-consumer, failure, and shutdown behavior.
- Define source retention, owner-thread cleanup, and context transfer across asynchronous boundaries.

If records require one total order, give that order to a domain-owned sink.
Independent observation registration does not establish it. An admitted one-shot
Store callback cannot be retracted by later authority closure.

A producer's weak endpoint does not prevent user captures from retaining an
external service. The adapter owns that retention policy. Its synchronous
shutdown callback does not prove completion of asynchronous cleanup.
Exercise the actual provider's delivery, cancellation, and cleanup boundaries.
See [receipt lifetimes](receipt-lifetimes.md) for the distinct completion stages.

## Relational queries use a typed provider boundary

Inject an existing engine/client through an application-owned dependency and
return owned typed rows, revisions or events. The
[external-client recipe](guides/external-clients.md) demonstrates this boundary
without selecting a network or database provider. For shared readable resources,
[`SharedReaderKey` and `SharedKey`](../crates/rca-core/src/sharing/cache.rs) expose typed
loading, subscription and optional saving. Their contracts do not supply SQL,
an ORM, migrations, pagination or automatic query-delivery laws.

Specify registration/replay and missed-update rules. Keep database commit,
observation delivery and physical producer completion distinct; subscription
setup alone is not delivery acknowledgement. Before shipping a relational
adapter, exercise its actual engine's queries, rollback, migrations, writer
reservation and callbacks independent of reducer/cache locks. Document storage
and controlled client tests do not qualify a relational service.

## Recursive jobs need finite workload bounds

Use the [finite tree and flat-stack recipe](recursive-jobs.md) as a starting
point. Its three-level, seven-node workload bounds input, decode, route and drop
depth, and exercises delayed child routing and ancestor retirement. Those are
finite witnesses, not arbitrary recursive safety, a recursive reducer framework
or a large-workload performance claim. Bound depth, width and memory for your
input; qualify routing and cleanup at those bounds. Add pagination only when
measured workload volume justifies it, not for API parity.

## Lifetime fencing is not replication or permission enforcement

Scoped generations reject stale work; document sessions can fence admission and
join physical operations. Neither establishes multi-device replication,
cross-account access control, conflict resolution or remote rollback. A local
[`SharedReader`](../crates/rca-core/src/sharing.rs) restricts that handle's mutation
surface; it is not a security authority.

Keep those services in an optional provider with explicit device/account,
offline, conflict, deletion and quiescence semantics. Qualify them against the
actual devices and accounts before claiming support. Even a
[`RevisionBackend`](../crates/rca-persistence/src/revision.rs) must provide genuine atomic
comparison against its covered writers; the file adapter does not implement
that protocol for arbitrary external writers.

## Mixed-version writers need an explicit codec policy

[`Codec`](../crates/rca-persistence/src/document.rs) converts values and bytes; the
application/provider chooses its versioned envelope, validation, defaults,
relationships and migration/cutover policy. A backend revision identifies a
storage version, not a schema version. RCA does not promise universal unknown-field
retention. If older writers must round-trip newer fields, preserve their opaque
representation explicitly or reject unsupported writes before replacing data.

Test an old reader editing/writing newer data, then a new reader recovering it,
including defaults, relationships and safe cutover. The persistent counter's
[explicit codec](../examples/persistent-counter/README.md) instead rejects unknown
versions; it demonstrates one deliberate policy, not mixed-version compatibility.
