# Rust Composable Architecture

[![Build status on main](https://github.com/jacobcxdev/rust-composable-architecture/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/jacobcxdev/rust-composable-architecture/actions/workflows/ci.yml?query=branch%3Amain)
[![Licence: MIT or Apache-2.0](https://img.shields.io/badge/licence-MIT_OR_Apache--2.0-blue)](#licence)
[![Latest rca version on crates.io](https://img.shields.io/crates/v/rca.svg)](https://crates.io/crates/rca)
[![API documentation on docs.rs](https://docs.rs/rca/badge.svg)](https://docs.rs/rca)

Build headless Rust applications from small, testable pieces. One Store owns
the state and processes changes in order. Your application owns the runtime,
the external services and any user interface.

[Quick start](#quick-start) · [Documentation](docs/README.md#start-here) ·
[Examples](#guides-and-examples) · [Platform support](docs/support.md)

Consider a service that manages several workers. Each worker can have its own
state and cancellable tasks. If you remove a worker and later reuse its ID,
an old task cannot use its expired context to update the replacement.
In a test, you can drive the same Store engine, control time and check each
state change without starting a real worker.

Use RCA when those rules matter. A command that reads one file and exits
does not need a Store. RCA supplies no renderer or event loop. The core does
not require Tokio, and state does not need to implement `Clone`.

## Quick start

From the repository root, run the counter with the [pinned Rust toolchain](rust-toolchain.toml). It sends one
action, checks that the count is one, then exits without printing anything. It uses the checkout, not a published RCA package.

**macOS** — use the repository's [build memory guard](CONTRIBUTING.md#build-memory-safety):

```sh
python3 scripts/guard_cargo_memory.py --receipt "$(mktemp -d)/counter.json" --memory-limit-gib 3 -- \
  run --manifest-path examples/counter/Cargo.toml
```

**Linux or Windows** — use Cargo from rustup. It reads the repository's toolchain file:

```sh
cargo run --manifest-path examples/counter/Cargo.toml
```

To use this checkout in a separate application, run the following commands **in the application directory**.
Replace the example path with the path to `crates/rca` in your checkout:

```sh
cargo add --path ../rust-composable-architecture/crates/rca --features tokio
cargo add tokio@1 --features macros,rt,time
```

Once [crates.io](https://crates.io/crates/rca) lists a release that includes `tokio`, you can install it by name instead:

```sh
cargo add rca --features tokio
cargo add tokio@1 --features macros,rt,time
```

The registry commands add a compatible version requirement. The manifests below show the release series, not an exact version.
RCA uses your Tokio runtime. It does not start a second one.

<details>
<summary><strong>Cargo.toml</strong> — package name and required features</summary>

```toml
[package]
name = "rca-quickstart"
version = "0.0.0"
edition = "2024"

[dependencies]
rca = { version = "0.1", features = ["tokio"] }
tokio = { version = "1", features = ["macros", "rt", "time"] }
```

</details>

A reducer changes state for one action and returns any effects. In `src/main.rs`, `Reduce::new` constructs a counter that returns no effects:

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

`send(action).await` returns after the state change commits. The returned
handle's `finish().await` also waits for work that RCA tracks for that action.
Neither wait proves that unrelated work has finished. [Choose the right wait](docs/receipt-lifetimes.md#choose-the-wait-that-answers-your-question).

### Keyed testing

What if two workers use the same logic? Route their actions by ID.
In this example, `TestStore` checks both the state change and the follow-up
action from the selected counter. RCA drives the test work, not the Tokio clock.

<details>
<summary><strong>See the complete keyed test and manifest</strong></summary>

```toml
[package]
name = "rca-keyed-quickstart"
version = "0.0.0"
edition = "2024"

[dependencies]
rca = { version = "0.1", features = ["test-util"] }
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

</details>

## Features and integration

The `rca` package has no default features.
The [`rust-composable-architecture` package](crates/rust-composable-architecture/README.md)
re-exports the same API and features from the matching `rca` release.

| Need | Cargo feature |
| --- | --- |
| Use the Tokio runtime that your application already owns | `tokio` |
| Check state, actions and unfinished work with `TestStore` | `test-util` |
| Keep non-`Send` state or tasks on the owner thread | `local` |
| Read observations as a `futures_core::Stream` | `futures-stream` |
| Generate dependency, path, reducer and snapshot code | `macros` |
| Trace reducer changes in debug builds or receive lifecycle events | `diagnostics` |

Dependencies, clocks and shared values do not need a Cargo feature.
For storage, add the separate [`rca-persistence` crate](crates/rca-persistence/src/lib.rs).
Store shutdown cancels work. It does not flush pending writes.
The [persistent counter](examples/persistent-counter/README.md) shows how to save before shutdown.

## Guides and examples

| Start with a question | Try this |
| --- | --- |
| How do I split an application into features? | [Authoring guide](docs/guides/authoring.md), including [when to use a reducer or a feature](docs/guides/authoring.md#choose-an-authoring-form) |
| How can a coding agent use RCA in my application? | [Install the optional RCA skill](docs/README.md#use-rca-with-a-coding-agent) |
| How do I test a request without a real server or a timer? | [Testing guide](docs/guides/testing.md) and [controlled clients](docs/guides/external-clients.md) |
| How do I stop a task or replace a child safely? | [Cancel a timer](examples/timer/src/main.rs) or [replace an upload worker](examples/composition-search/docs/uploader.md) |
| Which example fits my application? | [Runnable examples and tested recipes](examples/README.md) |
| How do I connect a native UI or another service? | [Host integration guide](docs/integration-boundaries.md) |
| How do I reduce work on a busy application path? | [Application performance](docs/guides/performance.md) |
| How do I choose between runtime speed and binary size? | [Application build profiles](docs/guides/authoring.md#choose-a-release-profile) |
| Where can I find a type or method? | [Choose the API reference for your version or checkout](docs/README.md#api-reference-and-support) |

## Requirements and compatibility

The [workspace manifest](Cargo.toml) declares the minimum Rust version.
The [support policy](docs/support.md) describes API compatibility and distinguishes native execution from
compile-only targets. The [qualification guide](docs/maintainers/qualification.md) explains
what the test and performance checks establish for a revision.

The six main libraries use safe Rust. The maintained transport dependencies
contain unsafe implementations with separate ownership and concurrency contracts.
See [Contributing](CONTRIBUTING.md) for build and review requirements and
[Security](SECURITY.md) to report a vulnerability privately.

## Licence

RCA is available under either the [MIT Licence](LICENSE-MIT.md) or the
[Apache License, Version 2.0](LICENSE-APACHE.md), at your option.

## Acknowledgements

RCA owes its architectural foundation to [Point-Free](https://www.pointfree.co),
especially [The Composable Architecture](https://github.com/pointfreeco/swift-composable-architecture).
RCA takes its approach to state, effects and exhaustive testing from TCA.
Rust ownership and traits shape the APIs. RCA does not reproduce SwiftUI bindings.
Thank you to their authors and contributors for the designs, explanations and
tests that inform this project.

Companion libraries also inform RCA's design and testing:

- [Dependencies](https://github.com/pointfreeco/swift-dependencies), [Clocks](https://github.com/pointfreeco/swift-clocks), [Combine Schedulers](https://github.com/pointfreeco/combine-schedulers) and [Concurrency Extras](https://github.com/pointfreeco/swift-concurrency-extras): dependency control and deterministic asynchronous work.
- [Case Paths](https://github.com/pointfreeco/swift-case-paths) and [Identified Collections](https://github.com/pointfreeco/swift-identified-collections): typed routing and stable collection identity.
- [Custom Dump](https://github.com/pointfreeco/swift-custom-dump) and [Issue Reporting](https://github.com/pointfreeco/swift-issue-reporting): readable diagnostics and test failures.
- [Sharing](https://github.com/pointfreeco/swift-sharing), [Navigation](https://github.com/pointfreeco/swift-navigation) and [Perception](https://github.com/pointfreeco/swift-perception): state, lifetime and observation contracts, adapted without a UI layer.
- [Macro Testing](https://github.com/pointfreeco/swift-macro-testing), [Snapshot Testing](https://github.com/pointfreeco/swift-snapshot-testing) and Compilation Testing: tests for generated code and compiler diagnostics.

RCA is an independent project. These acknowledgements do not imply endorsement
or compatibility with those libraries.

The maintained transport forks preserve their original licences and attribution.
Their provenance notices describe the upstream authors and RCA changes:
[async-channel](dependencies/rca-async-channel/UPSTREAM.md),
[event-listener](dependencies/rca-event-listener/UPSTREAM.md) and
[event-listener-strategy](dependencies/rca-event-listener-strategy/UPSTREAM.md).
