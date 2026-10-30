# Headless integration boundaries

RCA owns serial state transitions and the lifetime of admitted work.
Applications own rendering and external services. This guide defines that boundary
and the checks needed for host and provider integrations.

## Choose imports and host explicitly

Use `rca-core` directly, or import the `rust-composable-architecture` package as
crate `rca`. The facade has empty default features. Its optional `tokio`,
`test-util`, `diagnostics`, `futures-stream`, `macros` and `local` features add
their named surfaces, not a UI framework. Clocks, typed dependencies, sharing and
scoped observation remain core capabilities. See the actual
[facade features](../rca/Cargo.toml) and [exports](../rca/src/lib.rs).

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

## Native controls belong to the host

A taskbar, window or native control owns rendering, activation and platform
resources. After `Store::ready`, use actor-ordered
[`Store::acquire(&typed_mount_site)`](../rca-core/src/store/mod.rs) with the actual
[`MountSite`](../rca-core/src/store/mount.rs). Give incarnation-sensitive callbacks
that captured [`TaskContext`](../rca-core/src/store/task_context.rs) and use its
`read`/`update`/`send` operations without blocking the event loop. Handle absent
installation or rejected access explicitly. The context never retargets after
structural replacement or configuration changes. An old callback must not
reacquire by business ID or forward through the root to bypass that rejection.

Wire rendering observations separately; select only the projection needed and
expect coalescing, not a lossless event log. The
[scoped projection contract](composition.md#scoped-store-handles) is not
mounted-incarnation authority: `Store::scope` shares root lifetime, and selector
identity is not the mounted declaration's replacement/configuration fence. An
acquired context itself is neither a renderer nor a native observation adapter.

Keep thread-affine resources on their owner thread and arrange teardown there.
The optional [`rca::local` surface](../rca-core/src/local.rs) exposes an owner-local
Store/runtime; its [driver contract](../rca-core/src/runtime/local.rs) requires
owner-thread polling and destruction, including unpolled work at host teardown.
Cross-thread wakes only notify that owner. Portable Stores can instead exchange
owned `Send` commands with a host-owned resource, as in the
[thread-affine host fixture](../rca-tokio/tests/thread_affine_host.rs).
Neither route supplies a native event-loop adapter.

Keep the host driven through Store shutdown and any separate provider cleanup.
Cancellation acknowledgement is not synchronous capture destruction or external
quiescence; see [receipt lifetimes](receipt-lifetimes.md). Before claiming a
native integration, test activation, same-identity replacement, retained
callbacks and shutdown on the selected actual platform. Synthetic host and
owner-local engine tests alone do not establish that claim. Core supplies no
turnkey native renderer or Swift/Objective-C runtime wrapper.

## Relational queries use a typed provider boundary

Inject an existing engine/client through an application-owned dependency and
return owned typed rows, revisions or events. The
[external-client recipe](guides/external-clients.md) demonstrates this boundary
without selecting a network or database provider. For shared readable resources,
[`SharedReaderKey` and `SharedKey`](../rca-core/src/sharing/cache.rs) expose typed
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
[`SharedReader`](../rca-core/src/sharing.rs) restricts that handle's mutation
surface; it is not a security authority.

Keep those services in an optional provider with explicit device/account,
offline, conflict, deletion and quiescence semantics. Qualify them against the
actual devices and accounts before claiming support. Even a
[`RevisionBackend`](../rca-persistence/src/revision.rs) must provide genuine atomic
comparison against its covered writers; the file adapter does not implement
that protocol for arbitrary external writers.

## Mixed-version writers need an explicit codec policy

[`Codec`](../rca-persistence/src/document.rs) converts values and bytes; the
application/provider chooses its versioned envelope, validation, defaults,
relationships and migration/cutover policy. A backend revision identifies a
storage version, not a schema version. RCA does not promise universal unknown-field
retention. If older writers must round-trip newer fields, preserve their opaque
representation explicitly or reject unsupported writes before replacing data.

Test an old reader editing/writing newer data, then a new reader recovering it,
including defaults, relationships and safe cutover. The persistent counter's
[explicit codec](../examples/persistent-counter/README.md) instead rejects unknown
versions; it demonstrates one deliberate policy, not mixed-version compatibility.
