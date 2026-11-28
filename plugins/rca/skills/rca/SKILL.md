---
name: rca
description: Write, test, or review an application built with Rust Composable Architecture (the rca crate). Not for changes to RCA itself, Swift TCA, or root cause analysis.
license: MIT OR Apache-2.0
---

# Write a headless RCA feature

For a new application, use the `rca` package and import from `rca`.
Some applications use the `rust-composable-architecture` re-export. Its default Rust import is `rust_composable_architecture`.
If the application renames either dependency, use its existing alias. Import derive macros from the same crate as the types.
Use the application's existing runtime and Cargo features. Keep test-only features in development dependencies.

Use the API that the application builds. Read its `Cargo.lock`, not the latest release notes.
For a Git dependency, use the resolved commit. For a path dependency, use the actual checkout.
With the app's lockfile, run `cargo metadata --locked --format-version 1`. Find the resolved package's `manifest_path`.
Read the guides and examples named below at the matching [RCA repository](https://github.com/jacobcxdev/rust-composable-architecture) revision.
Start with `docs/guides/authoring.md`.
Registry packages need not include repository guides. Read the resolved source or build its API reference with `cargo doc -p <package> --no-deps`.
For this command, use the Cargo package name, not the Rust import alias.

## Choose the smallest authoring form

- For actions that describe state changes, use `Reduce::new` or implement `Reducer`.
  Return `Effect::none()` for a synchronous change. Every reducer already implements `Feature`. Do not implement both traits on one type.
- For a single async result, `Effect::future` accepts a future that returns `Option<Action>`.
  For multiple responses, use `Effect::run` and its sender.
  Capture owned inputs and the injected client, not a borrow of state.
- If work needs **current** state after an await, use a direct `Feature` and a typed `Task`.
  If a decision depends on current policy, check the policy and change state in the same update.
- Keep state and work in one Store when they need one transaction order. The host owns the runtime and any UI.
  Use the `local` feature for values that must stay on one thread, such as `Rc`. The owner must still drive the Store.
  A successful `try_send` returns a receipt, not a committed result. Dependency values still require `Send + Sync`.

Put domain facts in state. Put external capabilities behind a `DependencyKey`. Use `Shared` for intentional shared ownership, not as a second Store.
Default Cargo features are empty. `tokio` adapts an existing runtime. `test-util` supplies test tools.

For a new dependency key with a live default, prefer `#[derive(DependencyKey)]` with the `macros` feature.
For a client with named closure endpoints, prefer `#[derive(DependencyClient)]` to create its test configuration.
The client derive does not register a key or build a live client. Read the client through `context.dependency::<Key>()`.
For several child routes, prefer `StatePaths` and `ActionPaths` over repeated field and variant closures.
If the app omits `macros` or needs custom behaviour, use the corresponding traits and explicit paths.

Inside a reducer, capture the request before starting the effect. This example uses an injected search client:

```rust
let client = context.dependency::<SearchClientKey>().clone();
let query = query.clone();
Effect::future(async move { Some(Action::Response(client.results(query).await)) })
    .cancellable_in_flight(SearchRequestId)
```

Here `query` starts as a borrowed `String`. The future owns its copy and its client.
For a controlled test, inject a client that you can complete. Do not wait for a real network reply.

## Follow the contract that matters

- **Child identity, cancellation, or late replies:** read [Child work and completion](references/child-work.md).
- **Tests, virtual time, or move-only state:** read [Deterministic tests](references/testing.md).
- **Decide where a value belongs:** `docs/guides/dependencies-and-shared-state.md` explains when to use feature state, a shared value, or an injected dependency.
- **Connect an external service:** `docs/guides/external-clients.md` shows how to inject a client and control its responses in tests.
  See `examples/search/src/main.rs` for a feature that replaces a pending search.
- **Keep a stale save from overwriting newer data:** read about `RevisionBackend` in the `rca-persistence` crate guide (`crates/rca-persistence/src/lib.rs`).
- **Connect a host:** `examples/counter/src/main.rs` shows one Store on an existing Tokio runtime.
  For a callback that needs a committed reply, read “Synchronous hosts” in `docs/runtime.md` at the matching revision.
- **Read a total from child features:** `docs/structural-features.md` explains scoped folds and when to maintain the total in state.
- **Avoid a copy when loading or saving a document:** `docs/guides/dependencies-and-shared-state.md` explains when a codec can reuse its input buffer.
