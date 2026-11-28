# Connect RCA to an application

RCA manages state. Your application supplies the runtime, the user interface, and any external services.
The **host** is the application that runs the Store. A **provider** is an implementation of a service, such as a database client.
For example, the host draws a Save button. The Store decides what to save. The provider writes the bytes.
For an introduction to Stores, see [the basic model](authoring.md#the-basic-model).

## Choose imports and host explicitly

Start with `rca`, the package that brings the public APIs together. It has no default features.
Clocks, typed dependencies, shared state, and observation are available without extra features.
The optional features (`tokio`, `test-util`, `diagnostics`, `futures-stream`, `macros`, and `local`) add individual capabilities.
None supplies a user interface. See the [feature list](../../crates/rca/Cargo.toml) and the [public exports](../../crates/rca/src/lib.rs).

At the same version, `rca` and `rust-composable-architecture` expose the same types.
Import a derive macro from the same package as the types it works on.
If a crate depends directly on two versions of `rca`, a derive can implement the other version's trait.
Different Cargo dependency names do not prevent this. Use one direct version per crate, or implement the intended trait explicitly.

| What does your host need? | Where to start |
| --- | --- |
| State and actions on a custom runtime | Use the core APIs from `rca` or depend directly on `rca-core`. See [runtime choices](runtime.md#choose-a-runtime-and-ownership-model). |
| An existing Tokio runtime | Enable the `tokio` feature or use `rca-tokio` directly. A [synchronous host](runtime.md#synchronous-hosts) can also drive a Store. |
| Tests without real time or network requests | Enable `test-util` or use `rca-test` directly. Supply [dependencies](dependencies.md) per Store or TestStore, without a global test mode. |
| A value shared in memory | Use [shared state](shared-state.md). Reading the same value does not require a database. |
| A document that survives restarts | Use [`rca-persistence`](persistence.md). The [persistent counter](../../examples/persistent-counter/README.md) loads a file, edits it, and closes it. |

Loading saved state does not restart its former tasks. The feature decides which jobs to start again.
The [bounded-job recipe](runtime.md#finite-batches-and-bounded-retry) limits work in progress.
The [search example](../../examples/composition-search/docs/named-operations.md) shows which inputs a task keeps and which settings it reads later.

## Native controls belong to the host

The host draws controls and handles platform events. Suppose a taskbar button stops one worker.
The button keeps a callback: a function it calls on a click.
The user can replace the worker with another worker that has the same identifier (ID).
The old callback must not stop the replacement.

1. Await `store.ready()` to let the Store install the feature tree.
2. Await [`store.acquire(&site)`](../../crates/rca-core/src/store/mod.rs). The [`MountSite`](../../crates/rca-core/src/store/mount.rs) identifies where the worker belongs in the tree.
3. If acquisition returns `Ok(Some(context))`, retain that [`TaskContext`](../../crates/rca-core/src/store/task_context.rs) for this callback.
4. Use the context to read state, update it, or send an action. Keep the platform event loop free to run.

The Store processes acquisition in command order. A missing child can give `Ok(None)`. A closed Store or an invalid site can give an error.
A replacement or a change of configuration invalidates the old context.
If the callback's command fails, handle that result. Do not retry through the root or look up a new child by business ID.
The [authorisation example](../../examples/composition-search/examples/authorisation.rs) keeps a site from feature setup and uses it to acquire a child.
The [acquisition tests](../../crates/rca-core/tests/mount_acquisition.rs) cover missing, reconfigured, and replaced children.

A [scoped Store](composition.md#scoped-store-handles) gives a caller access to part of the root state.
Ordinary `Store::scope` shares the root's lifetime. It does not bind a callback to one installed worker and its configuration.
The host connects observations to its controls. A slow observer can miss intermediate values.
For a list, [observe ordered IDs](runtime.md#observation) to detect changes to membership or order.
To display progress or detect a replacement under the same ID, observe values that distinguish those changes.

### Work that must stay on one thread

Some platforms require resource access and destruction on the thread that created the resource.
A [Local Store](../../crates/rca-core/src/local.rs) and its [driver](../../crates/rca-core/src/runtime/local.rs) keep tasks there.
The owner thread must poll the tasks and destroy them, including tasks that never ran.
A notification from another thread wakes the owner. It does not give that thread permission to run the task.

Alternatively, a cross-thread Store can send owned `Send` commands to a resource on its own thread.
See the [thread-affine host example](../../crates/rca-tokio/tests/thread_affine_host.rs).
Neither option supplies a native event loop, renderer, or Swift/Objective-C runtime wrapper.

Keep the runtime active through Store shutdown and any separate service cleanup. A [cancellation request](receipt-lifetimes.md#cancel-work-or-stop-waiting) does not destroy resources immediately.
Exercise activation, same-ID replacement, old callbacks, and shutdown on the target platform. An isolated Store test cannot establish native behaviour.

## Define external notification delivery

[`ExternalObservation`](../../crates/rca-core/src/observation/adapters.rs) starts one subscription on demand. Its producer and consumer stay on one thread.
It supplies an initial value, then the latest value. It does not broadcast every change or wait for every listener.
Different Store observers can also receive values at different times.
For example, a progress indicator can skip an intermediate percentage. An audit log cannot skip an entry.

Before connecting a notification source, define these rules for the adapter:

- Which listeners receive a notification? At what point does that set become final?
- In what order do listeners run? If a listener posts again or changes the set during delivery, what happens?
- What does cancellation do before selection, after selection, or once a callback has permission to run?
- Does a reply mean that the callback returned, the listener processed the value, or its asynchronous work ended?
- How many notifications can wait? What happens on overflow, a slow listener, a failure, or shutdown?
- What keeps the source alive? Which thread cleans it up? What can cross a thread or task boundary?

If every record needs the same order, give one application-owned writer responsibility for that order.
Separate observers do not establish it. Once RCA admits a one-shot Store callback, later closure cannot retract it.
A producer's weak handle does not keep its observation alive. Captures in the adapter can still keep an external service alive.
Returning from a synchronous shutdown callback does not prove that asynchronous cleanup finished.
Exercise delivery, cancellation, and cleanup with the real provider. See [receipt lifetimes](receipt-lifetimes.md) for what each RCA wait proves.

## Connect a database

Supply an application-owned client as a [dependency](external-clients.md). Return typed Rust rows, revisions, or events that the caller owns.
For shared values, [`SharedReaderKey` and `SharedKey`](../../crates/rca-core/src/sharing/cache.rs) support loading, subscriptions, and explicit saves where the provider supports them.
They do not supply a query language such as SQL, an object-relational mapper, schema migrations, or pagination.
They do not define how query results reach consumers.

Define what a new subscription sees and how it recovers missed changes.
Starting a subscription does not establish that the database committed, that an observer received the result, or that the producer finished.
With the actual database, exercise queries, rollback, schema changes, and competing writers. Exercise callbacks independently of reducer or cache locks.
A document test or a mock client cannot establish those database guarantees.

## Set limits for recursive jobs

The [tree and flat-stack recipe](recursive-jobs.md) uses three levels and seven nodes.
It bounds input, decoding, action routing, and destruction. It also shows how a delayed action behaves after a parent leaves.
The example does not establish safety at arbitrary depths, a framework for recursive reducers, or performance for larger jobs.
For your input, choose limits on depth, width, and memory. Exercise routing and cleanup at those limits.
If measured input size exceeds the limit, consider pagination.

## Separate resource lifetime from access permission

A scoped generation identifies one child lifetime. It prevents stale work from updating a replacement.
A document session can stop accepting operations and wait for its physical operations to end.
Neither provides multi-device replication, account permissions, conflict resolution, or rollback on a remote service.
A [`SharedReader`](../../crates/rca-core/src/sharing.rs) restricts mutation through that handle. It does not authenticate its holder.

If an app needs these services, define them in a provider.
State what happens across devices and accounts, during offline edits, on conflict or deletion, and during shutdown.
Exercise the provider on the devices and accounts it supports.
For competing local writers, see [conflict detection](persistence.md#reject-conflicting-writes).

The application also owns compatibility between the storage formats it reads and writes.
For example, an older writer can reject an unfamiliar format instead of silently removing fields.
See [codec policy and buffer reuse](persistence.md#choose-a-codec-and-reuse-buffers).
