# Rust Composable Architecture

Rust Composable Architecture (RCA) is a library for building headless Rust
applications from composable features. Your application supplies the async
runtime, external services and any user interface.

## First release

RCA 0.1.0 is under review. The pull request contains the implementation, guides,
runnable examples and tests. [Read pull request #1](https://github.com/jacobcxdev/rust-composable-architecture/pull/1).

- A `Store` owns state and processes actions in order. Reducers update state
  synchronously and return effects. Features can also run typed async tasks.
- Compose features by routing actions into child state. Keyed children have
  distinct lifetimes, so a task from a removed child cannot update its replacement.
- `TestStore` can drive the same Store engine in tests. Control time and external
  services, then check state changes, follow-up actions and unfinished work.
- Inject dependencies and clocks, observe state changes, and opt into shared
  state, persistence, macros or a Tokio runtime adapter as needed.

The candidate includes examples for counters, timers, search, persistence and
service lifecycles. It targets Rust 1.100.0. [Browse the candidate guides and API reference](https://github.com/jacobcxdev/rust-composable-architecture/blob/jacobcxdev/rca-0.1.0/docs/README.md).

## Licence

RCA is available under either the [MIT Licence](LICENSE-MIT.md) or the
[Apache License, Version 2.0](LICENSE-APACHE.md), at your option.

## Acknowledgements

RCA takes architectural inspiration from Point-Free's
[Composable Architecture](https://github.com/pointfreeco/swift-composable-architecture).
RCA adapts these ideas to Rust ownership, traits, and explicit runtimes.
