Ergonomic dependency handling.

Dependencies represent resources needed by application logic that are outside its direct control:
networking, storage, clocks, randomness, environment values, and so on.

Dependency injection, dependency mocking, and other patterns and frameworks are all designed to avoid
the problems that application dependencies cause for testing — reliability and performance both
suffer.

This module attempts to make dependency handling easy, encouraging its use.

## The model

This module implements *dynamic scoping* for dependencies using per-thread storage.

- Dependencies are supplied for the duration of a closure using
  [`with_dependency`](crate::dependencies::with_dependency) or
  [`with_dependencies`](crate::dependencies::with_dependencies).
- Code that needs a dependency constructs a [`Dependency<T>`](crate::dependencies::Dependency) and
  reads from it.
- Dependencies can be optional. If nothing was provided, `Dependency<T>` behaves like "no value
  present".
- Some dependencies can have a default implementation via
  [`DependencyDefault`](crate::dependencies::DependencyDefault), but defaults are intentionally
  **forbidden in tests** unless the dependency is explicitly provided.

This approach keeps reducer logic testable without forcing dependency values through every function
signature.

## Supplying dependencies

Use `with_dependency` for a single value:

```rust
use composable::dependencies::{with_dependency, Dependency};

#[derive(Default)]
struct ApiClient;

with_dependency(ApiClient::default(), || {
    let api = Dependency::<ApiClient>::get();
    assert!(api.is_some());
});
```

Use `with_dependencies` for multiple values:

```rust
use composable::dependencies::{with_dependencies, Dependency};

#[derive(Default)]
struct ApiClient;
#[derive(Default)]
struct UuidGenerator;

with_dependencies((ApiClient::default(), UuidGenerator::default()), || {
    let api = Dependency::<ApiClient>::get();
    let uuid = Dependency::<UuidGenerator>::get();
    assert!(api.is_some());
    assert!(uuid.is_some());
});
```

Scopes are stack-like: inner scopes shadow outer ones for the same type.

## Using dependencies in reducers

Typical usage inside a reducer looks like:

```rust
use composable::{Effects, Reducer};
use composable::dependencies::Dependency;

#[derive(Default)]
struct Clock;

#[derive(Default)]
struct State;

#[derive(Clone)]
enum Action {
    Tick,
}

impl Reducer for State {
    type Action = Action;
    type Output = Self;

    fn reduce(&mut self, _action: Action, _send: impl Effects<Action>) {
        let _clock = Dependency::<Clock>::get();
        // use _clock.as_deref(), _clock.is_some(), etc.
    }
}
```

## Defaults and tests

If a dependency type implements [`DependencyDefault`](crate::dependencies::DependencyDefault), then
`Dependency<T>` can lazily create a default value **in production**.

In tests, attempting to use a default will panic with an explanation. Tests must explicitly provide
every dependency they rely on — either by wrapping the test body in `with_dependency(…)` /
`with_dependencies(…)`, or by using a `TestStore` mechanism if/when one is added for dependency
registration.

This rule prevents tests from accidentally using real "production" behaviour.

<!--

# Registering dependencies

# Using dependencies

# Designing dependencies

# Testing with dependencies

-->
