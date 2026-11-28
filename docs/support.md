# RCA support policy

RCA supports Rust programs that use the standard library. This page explains which platforms, features, and public APIs you can rely on.
You do not need a nightly compiler to use RCA. Cargo, the Rust package tool, reads the minimum supported Rust version from the [workspace manifest](../Cargo.toml).
The libraries use Rust edition 2024. The [toolchain file](../rust-toolchain.toml) selects the compiler for work on this repository.

## Targets

A **target** names the processor, operating system, and tool environment for a build.
**Compile-only** means that the compiler accepts the code. It does not mean that the test program ran on that platform.
The table defines required checks, not the result of any particular run.

| Target | Checks for `rca-core` and `rca` | Checks for `rca-tokio` and `rca` with `tokio` |
| --- | --- | --- |
| `x86_64-unknown-linux-gnu` | Required | Required |
| `aarch64-unknown-linux-gnu` | Required | Required |
| `x86_64-apple-darwin` | Required, compile-only | Required, compile-only |
| `aarch64-apple-darwin` | Required | Required |
| `x86_64-pc-windows-msvc` | Required | Required |
| `wasm32-unknown-unknown` | Required, compile-only | Not applicable |

RCA runs native tests on Linux (Arm and x86-64), macOS (Apple silicon), and Windows (x86-64).
Linux tests can run on virtual machines. The macOS check for Intel processors uses a compiler on an Arm Mac.

WebAssembly support is experimental. The check compiles the core and public package without default features.
It does not run a WebAssembly program. RCA does not yet supply a tested WebAssembly runtime adapter for work that can move between threads.

Disabling default features does not remove the standard library requirement. RCA does not support bare-metal firmware.
The [roadmap](roadmap.md) describes the proposed embedded core.

## Features

Start with the `rca` package. Cargo **features** turn optional capabilities on at build time.
Neither `rca` nor its alternative package name, `rust-composable-architecture`, enables features by default.
The long name exposes the same types and features as the matching `rca` version.

| To use… | Enable |
| --- | --- |
| An existing Tokio runtime to drive asynchronous work | `tokio` |
| A test Store with time and work that the test controls | `test-util` |
| State or tasks that must stay on one thread | `local` |
| Generated code for dependencies, child paths, reducers, snapshots, or enum cases | `macros` |
| Reports about state changes and the lifetime of work | `diagnostics` |
| A stream of observed values through `futures_core::Stream` | `futures-stream` |

The core does not require Tokio, macros, test tools, or a user interface.
A **runtime** drives asynchronous work. An application can supply its own runtime through `RuntimeDriver`.
Before claiming support for an adapter, its author must run the common tests for runtime behaviour.

With `diagnostics`, changes to reducer state trigger tracing callbacks only in builds with debug assertions.
Callbacks for lifecycle events also run in release builds. Typed change events occur before the Store acknowledges the action.
RCA does not select a global logger. The [diagnostics guide](usage/diagnostics.md) explains how to collect a report.

For [persistence](usage/persistence.md), add the separate `rca-persistence` package. Without extra features, it does not select a runtime or storage provider.
Its `file` feature adds Tokio and temporary-file support on native targets. WebAssembly has no file-adapter check.
The test tool `insta` is not a dependency of a minimal production build.

## Compatibility

### Public API paths

An API is the set of types, functions, and behaviours that your program can use.
Application code normally imports types such as `Store`, `Feature`, and `KeyedState` from the root of `rca`.
A library that integrates directly with the core can import the same types from `rca_core`.
For example, import `rca_core::Clock` without naming its implementation module.

- `local` contains types for work that stays on one thread. Their values need not implement `Send`, Rust's requirement for transfer between threads.
  Common types, including `InputGate` and `RemovalOrigin`, remain at the crate root.
- `diagnostics` contains tools for tracing and inspection. For example, use `rca::diagnostics::TraceChanges`.
  `ReactionChange` describes a state reaction. The separate `diagnostics::Change` records a traced change.

For application tests, use `rca::TestStore` with `test-util`, or depend on `rca-test` and import `rca_test::TestStore`.
The hidden `rca_core::testing` module connects RCA's own crates. It is not a public testing interface.
Direct imports and imports through `rca` retain their documented feature requirements.

### Version policy

Before version 1.0, a change to the public API or the minimum compiler version requires release notes and new qualification.
A fix that changes a documented guarantee also requires updated documentation.

From version 1.0, RCA uses semantic versioning. In a version such as `1.2.3`, the numbers denote major, minor, and patch releases.

| Change | Policy from version 1.0 |
| --- | --- |
| Remove a public API, tighten its type requirements, or break a documented behaviour | A major release is required. This includes ordering, cancellation, observation, and shutdown. |
| Add an optional feature | Existing signatures, type requirements, and behaviour must still hold with the feature on or off. |
| Raise the minimum supported Rust version (MSRV) | Use at least a minor release. Include a migration note. Do not raise it in a patch release. |
| Change a private implementation detail | No major release is necessary if the public behaviour stays the same. |

The [runtime guide](usage/runtime.md) explains what applications can observe.
The [manifest](../Cargo.toml) records the compiler minimum. Nightly tools for library checks do not change that minimum.

### Deprecation and migration

Deprecation tells callers to prepare for a change. It does not remove the API or change its behaviour within a stable major version.
If a replacement exists, the deprecation notice names it. Release notes explain what callers must change.
A compatibility wrapper must not silently change runtime behaviour.

This policy does not promise a release date, a minimum support period, or fixes for every older release.

## Qualification status

Release qualification covers source review, tests, coverage, performance, documentation, package consumers, and the targets listed here.
Each result applies to the source, dependencies, toolchain and environment it covers.
Local results do not establish that hosted checks passed.
Read the [CI runs](https://github.com/jacobcxdev/rust-composable-architecture/actions/workflows/ci.yml) for the exact commit under review.
Later changes require their own applicable checks. Passing checks do not publish packages or establish support beyond the tested configurations.

On the four native targets, tests use the locked dependencies and run programs on the target.
They exercise Tokio on one or multiple threads, synchronous callers, thread-bound work, and deterministic tests.
They also exercise the persistent counter with real files, failed input, process interruption, and restart.
Unix tests cover directory sync and symbolic links. Windows tests use its file-only sync mode.
These tests do not prove that a file survives power loss. A platform-specific input or output adapter needs its own tests.

The package checks also build independent consumers against packages from the registry, not just the workspace.
They check exact dependency versions and features for minimal imports, optional services, and persistence with and without file support.
A feature combination that passes on one host does not qualify a different host.
The [qualification guide](development/qualification.md) covers evidence, when to repeat a check, and what must pass for release.
