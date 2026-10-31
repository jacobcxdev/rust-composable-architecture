# Rust Composable Architecture

RCA is a Rust library for composable, exhaustively testable application logic.
It takes architectural ideas from Point-Free's Composable Architecture and
adapts them to Rust ownership, traits and explicit runtimes.

A `Feature` defines application behaviour. A `Reducer` is a feature that returns
`Effect` values for follow-up work. Reducers become features automatically, without
an adapter. Direct feature implementations can also start typed tasks that read
or update state through the Store. Both forms use the same store constructors
and support installed children, private commands and declared outputs.

Start with `Reducer` for action-based inputs and results, including asynchronous
effects. Use direct `Feature` implementations when tasks need typed reads or
updates of current State between awaits. Neither callback itself is asynchronous.
The [authoring choices](docs/guides/authoring.md#choose-an-authoring-form) explain
the trade-offs, composition rules and examples.

The Store owns state and processes each change in order. Applications select the
runtime and supply external services.

RCA is headless-first: it supplies no UI framework, renderer or native event-loop
adapter. A native UI can use RCA application logic through host-owned integration.
RCA does not require UI concepts in a service, worker or command-line application.
See the [integration boundaries](docs/integration-boundaries.md) for the host's responsibilities.

Rust-specific design choices include:

- Exclusive state mutation without a global `Clone` requirement.
- Portable `Send` APIs and optional owner-thread APIs for non-`Send` values.
- A runtime-neutral core with Tokio and deterministic test adapters.
- Bounded action queues that make senders wait or report full capacity.
- Keyed child instances with stable identity and rejection of stale task contexts.
- Typed cancellation and `ActionTask` handles that track work caused by an action.
- Observations that select values without cloning the whole state and keep the latest update.
- Exhaustive deterministic tests that use the production Store engine.

The six main libraries use safe Rust. The maintained transport dependencies
contain unsafe implementations with separate ownership and concurrency contracts.

## Quick start

For local macOS repository builds, follow the
[build memory-safety instructions](CONTRIBUTING.md#build-memory-safety).
Run the Cargo command below through that watchdog, not directly.
Other hosts can use the direct command from the repository root:

```sh
rustup run 1.99.0 cargo run --manifest-path examples/counter/Cargo.toml
```

The following manifest and source show a standalone Tokio application.
Repository checks run the README examples against locally packaged RCA crates.
RCA uses the caller's Tokio runtime. It does not create one.

```toml
[package]
name = "rca-quickstart"
version = "0.0.0"
edition = "2024"

[dependencies]
rca = { package = "rust-composable-architecture", version = "0.1.0", features = ["tokio"] }
tokio = { version = "1", features = ["macros", "rt", "time"] }
```

```rust
use rca::{Effect, Reduce, Store, TokioRuntime};

enum Action {
    Increment,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let counter = Reduce::new(|state: &mut i64, action: &Action, _| {
        match action {
            Action::Increment => *state += 1,
        }
        Effect::none()
    });
    let store = Store::new(0, counter, TokioRuntime::current()?);
    store.send(Action::Increment).await?.finish().await;
    assert_eq!(store.read(|state| *state).await?, 1);
    store.shutdown().await?;
    Ok(())
}
```

`TokioRuntime::current()` reports a missing host or timer driver. The direct Tokio
features above provide the entry-point macro, current-thread runtime and timers.
`shutdown` orders Store cleanup and cancellation. It does not flush persistence
or prove that an external service stopped its background work.

### Keyed testing

This example routes actions to child reducers by ID and checks their state and
effects with `TestStore`. Identity paths select the entire root collection.
The Tokio runtime only runs the async entry point. RCA's deterministic runtime
drives the test work.

The standalone test application uses this manifest and source.

```toml
[package]
name = "rca-keyed-quickstart"
version = "0.0.0"
edition = "2024"

[dependencies]
rca = { package = "rust-composable-architecture", version = "0.1.0", features = ["test-util"] }
tokio = { version = "1", features = ["macros", "rt", "time"] }
```

```rust
use rca::{
    Effect, KeyedAction, KeyedState, Reduce, TestStore, for_each, identity_keyed_action_path,
    identity_keyed_state_path,
};

enum Action {
    Increment,
    Completed,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let child = Reduce::new(|state: &mut i64, action: &Action, _| match action {
        Action::Increment => {
            *state += 1;
            Effect::send(Action::Completed)
        }
        Action::Completed => Effect::none(),
    });
    let parent = Reduce::new(
        |_: &mut KeyedState<String, i64>, _: &KeyedAction<String, Action>, _| Effect::none(),
    );
    let reducer = for_each(
        parent,
        identity_keyed_state_path(),
        identity_keyed_action_path(),
        child,
    );
    let expected_id = String::from("counter");
    let rows = [(expected_id.clone(), 0)].into_iter().collect();
    let mut store = TestStore::projected(rows, reducer, |rows| {
        rows.iter()
            .map(|(id, value)| (id.clone(), *value))
            .collect::<Vec<_>>()
    });
    let _ = store
        .send_with(
            KeyedAction::new(expected_id.clone(), Action::Increment),
            |rows| rows[0].1 = 1,
        )
        .await?;
    let _ = store
        .receive(|action| {
            action.id() == &expected_id && matches!(action.action(), Action::Completed)
        })
        .await?;
    store.finish().await?;
    Ok(())
}
```

The matcher is an ordinary Rust closure that borrows `expected_id` and the
received action. `Action` does not need `Clone`. `finish` rejects unasserted
actions, state changes and unfinished work.

## Features and integration

The `rust-composable-architecture` package is imported as `rca`. Its default
features are empty. Enable only the features that the application needs:

- `tokio`: an adapter for an existing Tokio runtime.
- `test-util`: the deterministic runtime and `TestStore`.
- `diagnostics`: reducer change tracing in debug builds, plus lifecycle callbacks in debug and release builds.
- `local`: owner-thread Store, runtime and task APIs for non-`Send` values.
- `futures-stream`: `futures_core::Stream` support for observations.
- `macros`: macros for reducers, dependencies, paths, snapshots and case descriptions.

Typed dependencies, clocks, shared values and scoped issue reporters are core
capabilities. Controlled futures, streams and gates are available directly from
`rca-test`. `SharedReducer` borrows separate private and parent-owned state during
each reduction. It does not require shared mutable handles.

Persistence uses a separate `rca-persistence` dependency with an explicit codec
and backend. Its default features select no runtime, filesystem or serialisation
format. The `file` feature adds an adapter for an existing Tokio runtime.
A flush or close receipt acknowledges the persistence result published to state.
Before restart, wait for the backend to stop all earlier writes.
Store shutdown cancels work rather than flushing pending writes.
The [persistent counter](examples/persistent-counter) demonstrates this sequence.

## Guides and examples

Start with the [authoring guide](docs/guides/authoring.md) for a workflow that
covers state, effects, dependencies, child identity and exhaustive tests.
Its [decision index](docs/guides/authoring.md#find-a-decision) links common API choices.

- [Testing](docs/guides/testing.md) explains assertions, snapshots, virtual time and controlled clients.
- [Dependencies and shared state](docs/guides/dependencies-and-shared-state.md) explains where data belongs and how overrides work.
- [Diagnostics](docs/guides/diagnostics.md) distinguishes change tracing, lifecycle facts and application logging.
- [Runtime contracts](docs/runtime.md) and [receipt lifetimes](docs/receipt-lifetimes.md) explain scheduling, cancellation and completion.
- [Reducer composition](docs/composition.md) and [structural Features](docs/structural-features.md) explain the two composition routes.
- [Headless integration boundaries](docs/integration-boundaries.md) describe native hosts and external services.
- [Compiler-contract testing](docs/compiler-contract-testing.md) explains tests for compiler-enforced API rules.
- [Support policy](docs/support.md) records target requirements and public API paths.
- [Contributing](CONTRIBUTING.md) describes formatting, tests and review requirements.

Executable examples include [counter](examples/counter), [search](examples/search),
[timer](examples/timer) and [composition search](examples/composition-search).
The [owned-resource guide](docs/guides/owned-resources.md) explains move-only
payloads and client cleanup responsibilities.

## Requirements and compatibility

RCA 0.1.0 requires Rust 1.99.0. The public API can change before version 1.0.
The [support policy](docs/support.md) defines platform requirements and compatibility rules.
The [qualification guide](docs/qualification.md) explains how to assess test and performance evidence for a specific revision.

## Relationship to Swift TCA

RCA follows TCA's approach to state, effects, cancellation, dependencies,
composition and exhaustive testing. Rust ownership, traits and thread bounds
determine how RCA expresses those ideas. Installed Features, typed tasks and
lifetime-bound access support headless application logic. The
[structural Feature guide](docs/structural-features.md) explains RCA's current
ownership and task contracts.

TCA is a design reference, not a source-level compatibility target.
RCA does not reproduce SwiftUI bindings or promise every Swift API in Rust.

## Licence

Distributed under MIT or Apache-2.0. See [LICENSE-MIT](LICENSE-MIT.md) and
[LICENSE-APACHE](LICENSE-APACHE.md).

## Acknowledgements

RCA owes its architectural foundation to [Point-Free](https://www.pointfree.co),
especially [The Composable Architecture](https://github.com/pointfreeco/swift-composable-architecture).
Thank you to their authors and contributors for the designs, explanations and
tests that inform this project.

Companion libraries also inform RCA's design and testing:

- [Dependencies](https://github.com/pointfreeco/swift-dependencies), [Clocks](https://github.com/pointfreeco/swift-clocks), [Combine Schedulers](https://github.com/pointfreeco/combine-schedulers) and [Concurrency Extras](https://github.com/pointfreeco/swift-concurrency-extras): dependency control and deterministic asynchronous work.
- [Case Paths](https://github.com/pointfreeco/swift-case-paths) and [Identified Collections](https://github.com/pointfreeco/swift-identified-collections): typed routing and stable collection identity.
- [Custom Dump](https://github.com/pointfreeco/swift-custom-dump) and [Issue Reporting](https://github.com/pointfreeco/swift-issue-reporting): readable diagnostics and test failures.
- [Sharing](https://github.com/pointfreeco/swift-sharing), [Navigation](https://github.com/pointfreeco/swift-navigation) and [Perception](https://github.com/pointfreeco/swift-perception): state, lifetime and observation contracts, adapted without a UI layer.
- [Macro Testing](https://github.com/pointfreeco/swift-macro-testing), [Snapshot Testing](https://github.com/pointfreeco/swift-snapshot-testing) and [Compilation Testing](https://github.com/pointfreeco/swift-compilation-testing): tests for generated code and compiler diagnostics.

RCA is an independent project. These acknowledgements do not imply endorsement
or compatibility with those libraries.

The maintained transport forks preserve their original licences and attribution.
Their provenance notices describe the upstream authors and RCA changes:
[async-channel](dependencies/rca-async-channel/UPSTREAM.md),
[event-listener](dependencies/rca-event-listener/UPSTREAM.md) and
[event-listener-strategy](dependencies/rca-event-listener-strategy/UPSTREAM.md).
