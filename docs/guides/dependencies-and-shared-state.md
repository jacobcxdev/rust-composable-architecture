# Dependencies and shared state

Several RCA types use the word "shared", but they solve different problems.
Start with ordinary feature State. If another owner needs access to the same
value, consider shared ownership.

## Where does this value belong?

| Need | Choose | Example |
| --- | --- | --- |
| Data that determines the feature's next decision | Feature State | Current request ID, progress or retry count |
| A replaceable service or source of external values | `DependencyKey` | HTTP client, random bytes or wall time |
| Child-private state plus a parent-owned value during reduction | `SharedReducer` with `scope_shared` or `for_each_shared` | Several workers consume one parent-owned quota |
| Several independent owners of one mutable value | `Shared<T>` | A value retained by a host and several features |
| Access to that value without its mutation or rebinding methods | `SharedReader<T>` | A client that only needs to read the value |
| Reuse of typed provider-backed values within a chosen namespace | `SharedKeyCache` | Several consumers of the same stored document |

`SharedReducer` does not contain a `Shared<T>`. It receives two separate mutable
borrows during one synchronous reduction. The parent still owns both values.
Neither borrow can escape into asynchronous work. See
[shared reducer composition](../composition.md) and its
[executable examples](../../rca-test/tests/shared_composition.rs).

`Shared<T>` instead gives several owners access to one value outside Store
transactions. It does not automatically register observation, persist mutations,
or send actions. Putting a `Shared<T>` handle in State does not make mutations
through other aliases into Store transactions.

For decisions that must agree with other feature state, use an actor-ordered
update and keep the relevant values in that State. A mutex around a separate
value does not make its mutation atomic with a Store transaction.

## Which dependency value does the feature receive?

Use an application-owned `DependencyKey` to name a service independently of its
implementation. Its `Value` can be a concrete client, an owned function or an
`Arc` handle. Both the key and value require `Send + Sync + 'static`, including
in a Local Store.

Use `Store::builder(...).with_dependency::<Key>(client)` to supply a production
instance. Use the corresponding `TestStore` builder method to supply a controlled
test instance. A test override takes precedence over action and feature overrides.
This lets a test replace a service even when a child normally selects its own
implementation.

Lookup uses this precedence, from highest to lowest:

1. Explicit TestStore override.
2. Action-scoped override through `ReduceContext::with_dependency_override`.
3. Feature-scoped reducer wrapper through `with_dependency`.
4. Explicit value in the Store's dependency environment.
5. The selected cached default or default constructor.

Nested overrides in the same tier select the innermost matching key. An override
for another key leaves this key unchanged. The
[dependency-scope tests](../../rca-test/tests/dependency_scope.rs) demonstrate
the full precedence and an asynchronous client.

Ordinary stores use live defaults. TestStore uses test defaults. A missing test
default panics with the key name instead of calling the live constructor.
Explicit preview policy uses preview defaults, with a test fallback, never a
live fallback. Enabling `test-util` does not change an ordinary Store's policy.

These defaults prevent implicit live fallback, not all real I/O. A live client
explicitly injected into a test can still contact its service. Prefer
[controlled clients](external-clients.md) for deterministic tests.

For an optional service, use `Option<Client>` as the dependency value. An explicit
`None` disables it and masks lower-priority values. It does not request fallback.
If unconfigured and disabled must differ, use an enum that represents both states.
The [optional dependency tests](../../rca-test/tests/optional_dependencies.rs)
show absence, nested overrides and captured effects.

## When are clients created or replaced?

The environment creates a default lazily and caches a successful result. It has
no global or thread-local dependency lookup. Independent environments have
independent override and cache maps. They can still share an explicitly supplied
client, such as a cloned `Arc`.

If an interface crate must not depend on its provider, use `DependencyDefaults`
and `register_defaults`. Registration supplies lazy constructors, not a running
service. Register before the first default attempt for that key. A failed attempt
also closes registration for that key. The
[modular dependency fixture](../../rca-core/tests/fixtures/modular-dependencies/consumer/src/lib.rs)
shows separate interface, provider and consumer crates.

`DependencyValues::prepare` groups setup before the environment's first lookup.
It is not a transaction: an error or panic does not undo earlier setup changes.
`reset_cache` clears cached defaults only. It does not revoke captured clients,
reopen registration or wait for an old provider to stop.

Before returning an effect or task, own the client selected from the callback
context. That context's borrow cannot escape into returned asynchronous work.
An owned dependency environment can lend values within its own asynchronous scope.
The work retains the selected client even after
the override scope ends. Replacing an environment value does not rewrite existing
captures. The [session refresh tests](../../rca-test/tests/session_client_refresh.rs)
distinguish captured clients from current service state.

## Does a shared reader follow rebinding?

Clones of one `Shared<T>` follow that handle's rebinding together. A reader or
projection captures the current value reference instead. It continues to access
that reference after the writable handle binds to another value. It is not a
snapshot: later mutations of its captured value remain visible.

For example, a reader captured from a shared value containing `2` still reads
that value after the writable handle rebinds to a different value containing `10`.
The [sharing doctest](../../rca-core/src/sharing.rs) and
[alias tests](../../rca-core/tests/shared_value.rs) demonstrate this distinction.

A reader exposes no mutable-borrow or rebinding method. It does not remove
interior mutability already available through `&T`, such as a `Cell` in the value.
Use an independent owned projection for historical assertions, not a cloned
shared handle. TestStore can track selected external values with
`with_capture`, as shown in the
[namespace tests](../../rca-test/tests/shared_namespaces.rs).

Shared access uses short synchronous callbacks. It reports `Busy` for overlapping
access instead of waiting. A panic inside value access poisons that value.
A missing optional, case or keyed projection reports `Absent` and does not
recreate the missing payload.

Mutation is in place, not rollback-based. An `Err` returned by a mutation retains
earlier writes. Every normally returning mutation invokes its explicit publication
callback after unlocking, including unchanged values and mutation errors.
The callback decides how to notify an external consumer. Across threads, callback
order need not match mutation order. If the notification needs that exact result,
return an owned snapshot from the mutation.

Retaining shared data does not retain permission to send to a removed child.
The [scope tests](../../rca-test/tests/shared_value_scopes.rs) show readable data
alongside rejected stale feedback.

## When is a shared-key cache appropriate?

If provider-backed values need explicit reuse, use `SharedKeyCache`. Create a
fresh cache for an independent session or test. Clone the cache to give consumers
deliberate access to the same namespace. Its entries are weak references:
existing handles, not the cache alone, keep values and subscriptions alive.

`SharedKeyIdentity` combines a resource address, codec type and codec configuration.
The address must distinguish different backends that use the same resource name.
Equal identity means compatible access to the same resource, not merely equal
current data.

| Operation | Use when | Important boundary |
| --- | --- | --- |
| `shared` or `reader` | A seed is valid when storage is absent | A compatible live hit skips seed, load and subscription work |
| `require` or `require_reader` | Stored data must exist | Storage is loaded even on a cache hit, but the live cached value can still win |
| `reload` or `reload_reader` | Refresh the captured resource | Storage absence retains its current value |
| `load` or `load_reader` | Change to another resource key | Rebinding precedes reload, so a reload error does not restore the old binding |
| `reset` | Stop future lookups from finding current entries | Existing handles, subscriptions, storage and in-flight constructors remain |

Only the latest started reload can replace its captured value. Failure or
cancellation of that reload does not restore an older request's permission to
write. A successful producer update also supersedes earlier reloads.

Concurrent cache misses can both load and subscribe before one published value
wins. Provider work is not guaranteed to run exactly once. Loading and subscribing
are separate calls. Providers that require gap-free updates need replay or another
explicit continuity policy.

`SharedKey::save` supplies a saving capability. Shared mutation does not call it
automatically. Use an explicit save policy or the separate persistence APIs for
coalescing, conflict handling and close acknowledgements. See the
[persistent counter](../../examples/persistent-counter/README.md) and
[shared-key cache tests](../../rca-core/tests/shared_key_cache.rs).
