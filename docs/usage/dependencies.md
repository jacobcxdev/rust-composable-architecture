# Supply a dependency

A dependency gives application code a service that another part of the program supplies.
For example, a search feature needs a client to find results. A running app can supply a network client.
A test can supply a client that returns known results without a network connection.
The feature uses the same code in both cases.

The Store owns the feature's state and processes its actions. It also has a dependency environment: a set of services named by keys.
Import the dependency types from `rca`. Basic lookup needs no extra crate or Cargo feature.
If you need to decide between a service and state, see [where a value belongs](authoring.md#where-does-this-value-belong).

## Define a key and supply a client

1. Define a `DependencyKey` in your application. Its `Value` type tells Rust which client this key selects.
2. Supply the client with `Store::builder(...).with_dependency::<Key>(client)`. The test Store, `TestStore`, accepts a client the same way.
3. In the reducer, read the client with `context.dependency::<Key>()`. A reducer processes an action and updates the feature's state.

A key can name a concrete client, an owned function, or a shared handle such as `Arc<Client>`.
The key and its value must satisfy `Send + Sync + 'static`, even for a Store that runs on one thread.
They must support transfer and shared access across threads. Neither can borrow short-lived data.

If you enable the `macros` feature, use `#[derive(DependencyKey)]` on a struct with no fields to declare a live default.
A live default supplies the client for a running application. The [search example](../../examples/search/src/main.rs) declares `SearchClientKey` this way.
Without the feature, implement the trait directly.

For clients with named `Arc<dyn Fn(...) + Send + Sync>` fields, `#[derive(DependencyClient)]` generates `Client::test_value()`.
Each field represents one operation the caller can request. The test client panics on a call to an operation without a test implementation.
The macro neither constructs a live client nor registers a key.
To use this test default, set `test = Client::test_value()` in the key's `#[dependency(...)]` attribute.
This expression must compile in normal builds. If a helper exists only as a development dependency, supply it from the test instead.
See [external clients](external-clients.md) for a worked request and event stream.

## Which dependency value does the feature receive?

An override supplies a replacement for a key. RCA uses the first matching entry in this list:

1. An explicit override in `TestStore`.
2. An override for the action from `ReduceContext::with_dependency_override`.
3. An override for the feature from a reducer wrapped with `with_dependency`.
4. An explicit value in the Store's environment.
5. The default for that environment.

If several overrides at the same level name the same key, the innermost one wins.
An override for a different key leaves this value unchanged.
A feature override applies only while its reducer runs. An action sent directly to an [installed child](structural-features.md) bypasses the parent reducer.
To supply one client to both, set it on the Store. To override it only in the child, wrap the child reducer.
The [scope tests](../../crates/rca-test/tests/dependency_scope.rs) show each level.

| Environment | Default |
| --- | --- |
| Ordinary Store | The live default. Enabling `test-util` does not change it. |
| `TestStore`, including projected and non-exhaustive tests | The test default. If none exists, lookup panics with the key's name. |
| Preview | If the key defines no preview default, RCA uses the test default. Otherwise it uses the preview default. |

Neither a test nor a preview falls back to live services. If a test explicitly supplies a live client, that client can perform real I/O.

For an optional service, use `Option<Client>`. Supplying `None` disables the service and masks all lower-priority values.
Lookup does not report whether it found an explicit value or called the default constructor.
Without an explicit value, you cannot skip the constructor.
If the app distinguishes unconfigured, disabled, and enabled services, give each case its own enum variant.
See the [optional dependency tests](../../crates/rca-test/tests/optional_dependencies.rs).

## When are clients created or replaced?

The first lookup that needs a default calls its constructor. The environment keeps a successful result for later use.
RCA does not search a global or thread-local registry. Separate environments keep separate overrides and caches.
To share the same client between environments, give them clones of one `Arc`.

Sometimes one crate defines a service interface and a different crate supplies its implementation.
Use `DependencyDefaults` and `register_defaults` to register the implementation's constructors without starting its services.
Register them before the first attempt to construct a default for that key. Even a failed attempt closes registration for that key.
The [modular example](../../crates/rca-core/tests/fixtures/modular-dependencies/consumer/src/lib.rs) wires the two crates together.

`DependencyValues::prepare` groups setup before the environment's first lookup. An error or panic does not undo earlier setup changes.
`reset_cache` clears only the cached defaults. It does not revoke clients, reopen registration, or wait for an old service to stop.

An effect runs work after the reducer returns. A task or effect cannot keep a borrow from the reducer's callback.
Give it an owned client or a clone of the handle. An owned dependency environment can also lend its values within its asynchronous scope.
For example, after the user signs in again, an earlier request still holds its original session client.
Replacing a client affects future lookups, not work that already holds it. See the [session tests](../../crates/rca-test/tests/session_client_refresh.rs).
