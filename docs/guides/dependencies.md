# Supply a dependency

A dependency gives a feature access to something it does not own, such as a
clock, a file service, or a network client. Each Store has its own dependency
environment. Import the types from `rca`; there is no separate dependency crate.
For data that changes as part of a decision, see [where a value belongs](authoring.md#where-does-this-value-belong).

## Define a key and supply a client

Use an application-owned `DependencyKey` to name the service. Its `Value` can be a
concrete client, an owned function, or an `Arc` handle. Both the key and the value
must be `Send + Sync + 'static`, even in a Local Store.

With the `macros` feature, prefer `#[derive(DependencyKey)]` on a unit struct with a live default.
The [search example](../../examples/search/src/main.rs) shows how.
For clients with named `Arc<dyn Fn(...) + Send + Sync>` endpoints,
`#[derive(DependencyClient)]` supplies `Client::test_value()`. Calling an endpoint
without a test implementation panics. The derive does not build a live client or register a key.
To use these defaults for the key, set `test = Client::test_value()` in its
`#[dependency(...)]` attribute. This expression must compile in normal builds.
If a helper exists only as a dev-dependency, inject it from the test instead.

Without macros, implement the key trait and construct the client directly.
Read the value during reduction with `context.dependency::<Key>()`.
Use `Store::builder(...).with_dependency::<Key>(client)` to supply a specific
instance. A TestStore builder accepts an override in the same way. For a worked
example that tests requests and a stream without real I/O, see [owned external clients](external-clients.md).

## Which dependency value does the feature receive?

Lookup uses the first matching value in this order:

1. An explicit TestStore override.
2. An action override from `ReduceContext::with_dependency_override`.
3. A feature override from a reducer wrapped with `with_dependency`.
4. An explicit value in the Store's environment.
5. The default for the environment, created on first use and then cached.

Nested overrides at the same level use the innermost matching key. An override
for another key leaves this one unchanged. A feature override applies only while
its wrapped reducer runs. A private action sent directly to an
[installed child](../structural-features.md) does not run its parent reducer.
To share a service across both, supply it to the Store. To override it only in a
child reducer, wrap that reducer. The [scope tests](../../crates/rca-test/tests/dependency_scope.rs)
exercise each level, including a client used after reduction.

Ordinary stores use live defaults. TestStore, including projected and
non-exhaustive tests, uses test defaults. A missing test default panics with the key
name. Preview policy uses the preview default, falling back to the test default.
Neither falls back to live. Enabling `test-util` does not change ordinary stores.
A test that explicitly supplies a live client can still perform real I/O.

For an optional service, use `Option<Client>`. Supplying `None` disables it and
masks lower-priority values. It does not request fallback. RCA does not expose
whether a lookup used a binding or a default, or let you skip the default factory.
If your application must distinguish unconfigured, disabled, and enabled, use
an enum with those three cases. See the [optional dependency tests](../../crates/rca-test/tests/optional_dependencies.rs).

## When are clients created or replaced?

The environment creates a default lazily and caches a successful result. It has
no global or thread-local dependency lookup. Independent environments have
independent override and cache maps. They can still share an explicitly supplied
client, such as a cloned `Arc`.

If an interface crate must not depend on its provider, use `DependencyDefaults`
and `register_defaults`. Registration supplies lazy constructors, not a running
service. Register before the first default attempt for that key. A failed attempt
also closes registration for that key. The
[modular dependency fixture](../../crates/rca-core/tests/fixtures/modular-dependencies/consumer/src/lib.rs)
shows separate interface, provider and consumer crates.

`DependencyValues::prepare` groups setup before the environment's first lookup.
It is not a transaction: an error or panic does not undo earlier setup changes.
`reset_cache` clears cached defaults only. It does not revoke captured clients,
reopen registration or wait for an old provider to stop.

Before returning an effect or task, give it an owned client or a cloned client
handle. It cannot keep a borrow from the reducer callback. An owned dependency
environment can lend values within its own asynchronous scope.

For example, a request that captures the old session client keeps that client
after sign-in selects a new one. An override changes future lookups, not work
that already holds the client. See the
[session refresh tests](../../crates/rca-test/tests/session_client_refresh.rs).
