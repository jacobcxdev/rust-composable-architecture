# RCA support policy

RCA 0.1.0 uses Rust 1.99.0, Cargo 1.99.0, and Rust Edition 2024. RCA
does not require nightly Rust. Nightly 2026-08-23 is pinned for Miri,
unused-dependency analysis, fuzzing, and proc-macro API qualification only.

## Qualification status

Release qualification covers source review, tests, coverage, performance,
documentation, package consumers and the target matrix below.
Each result applies to the source, dependencies, toolchain and environment it covers.
Changes to those inputs require an applicability check and, where affected, new checks.
Local results do not establish that hosted checks passed.

For hosted results, use the [CI runs](https://github.com/jacobcxdev/rust-composable-architecture/actions/workflows/ci.yml)
for the exact commit under review. Later changes require their own applicable checks.
The [qualification guide](qualification.md) records evidence boundaries and historical results.
Passing checks do not publish packages or establish support beyond the tested configurations.

## Targets

| Target | Core and umbrella gate | Tokio adapter gate |
| --- | --- | --- |
| `x86_64-unknown-linux-gnu` | Required | Required |
| `aarch64-unknown-linux-gnu` | Required | Required |
| `x86_64-apple-darwin` | Required, compile-only | Required, compile-only |
| `aarch64-apple-darwin` | Required | Required |
| `x86_64-pc-windows-msvc` | Required | Required |
| `wasm32-unknown-unknown` | Required | Not applicable |

WebAssembly support covers runtime-neutral `rca-core` and umbrella APIs. It
remains experimental until RCA ships and qualifies a WebAssembly-compatible
`Send` runtime adapter.

The table defines required checks, not the result of any particular run.
Hosted results must identify the exact source revision.
Native qualification covers macOS ARM64, Windows x86-64 and Linux ARM64/x86-64.
Linux checks can use virtualised hosts. The macOS x86-64 check uses cross-compilation
on an ARM Mac. Cross-compilation does not establish native runtime support.

The four native-execution CI rows also execute Tokio's library tests (including
shared runtime and composition conformance on current-thread and multi-thread
executors), synchronous and thread-affine host integration and deterministic
conformance.
These tests use locked dependencies and run on the target host.
The WebAssembly check remains compile-only.
Release qualification must retain each host's successful run and exact source revision.
Platform I/O and thread-affine resource adapters need their own tests beyond runtime conformance.

Those native rows also run the persistent-counter package tests, covering real
file replacement/restart, process interruption, malformed-input preservation,
CLI behaviour and file-client quiescence. Unix-only directory-sync and symlink
tests remain conditional. Windows exercises its supported file-only durability mode.
The ten-minute CI step deadline bounds a stuck test process.
These tests do not establish power-loss durability.

## Features

The umbrella package is named `rust-composable-architecture` and imported as crate `rca`.
Default features are empty. RCA contains six headless crates and three maintained
transport dependencies. It does not include a renderer or native event-loop adapter.

| Feature | Adds |
| --- | --- |
| `tokio` | Tokio runtime adapter |
| `test-util` | Deterministic runtime and exhaustive `TestStore` |
| `local` | Owner-thread Store, runtime and task APIs for non-`Send` values |
| `macros` | Dependency, path, reducer-composition, snapshot and case-description macros |
| `diagnostics` | Reducer change tracing, lifecycle callbacks and bounded, caller-rendered diagnostics |
| `futures-stream` | `futures_core::Stream` for observations |

Features are additive. Runtime-neutral core has no Tokio, macro, test, or UI dependency.
Custom executors integrate through `RuntimeDriver`.
Adapter authors must run shared conformance tests before claiming support.

`rca-persistence` is selected directly rather than through a facade feature.
Its empty default feature set keeps generic codec/backend coordination runtime-neutral.
Its additive `file` feature enables Tokio and
temporary-file dependencies on supported native targets. Generic persistence is
also subject to the runtime-neutral target checks. WebAssembly has no file-adapter gate.
`insta` is development-only snapshot tooling, not a minimal production
dependency. Diagnostics select no logger or global sink. Reducer-change tracing
callbacks run only with debug assertions. Lifecycle callbacks run in debug and release builds.
Typed change events precede actor acknowledgement.

Registry-only qualification includes minimal core/facade consumers, a
diagnostics/client-clock/dependency/control/shared consumer, generic persistence
without a runtime feature and a native file consumer. Source validation checks
every resolved RCA dependency, exact versions and minimal dependency boundaries before the offline
consumer check/test/run steps. A persistence receipt acknowledges committed actor
state under the backend's own successful-save guarantee.
It does not establish that the backend stopped all earlier writes.
The file consumer explicitly closes the backend before restart.

Feature-combination checks qualify the configurations they exercise on that
host. They do not establish native support on other targets or test every
behaviour within each combination.

## Compatibility

### Public API paths in 0.1.0

Application code normally imports from `rca`.
Runtime-neutral integration crates can import the same core types directly from `rca_core`.
Ordinary authoring types
and functions live at the crate root, such as `Store`, `Feature`, `Runtime`,
`KeyedState` and `scope`. Implementation modules are not another public route.

Two namespaces describe distinct uses:

- `local` contains owner-thread types whose payloads need not implement `Send`.
  Shared identity and policy types, including `InputGate` and `RemovalOrigin`,
  remain at the root.
- `diagnostics` contains tracing, checkpoints and structural inspection. For
  example, use `rca::diagnostics::TraceChanges`, not `rca::TraceChanges`.

For example, import `rca_core::Clock` rather than an implementation-module path.
`ReactionChange` describes a state reaction. The separate `diagnostics::Change`
type records a traced state change. Direct-core and facade imports retain their
documented feature requirements.

The hidden `rca_core::testing` module is an inter-crate testing seam, not the
application testing interface. Use `rca_test::TestStore`, or `rca::TestStore`
with `test-util`, for application assertions.

### Plain persistence revisions in 0.1.0

`rca_persistence::NoBackendRevision` is a zero-sized struct with a private field.
External callers cannot construct it or obtain it through `Default`.
A plain document's `backend_revision()` returns `None`.
Only a revision-aware backend supplies a comparison identity.
The optional revision getter distinguishes plain storage from revision-aware storage.

### Version policy

RCA starts at 0.1.0. During pre-stable development,
MSRV and public API changes require explicit release notes and renewed qualification.
Stable 1.x releases will follow semantic versioning.

## Stable API and runtime contracts

The following policy takes effect with version 1.0.

Stable releases follow semantic versioning for public APIs and documented
runtime behaviour. Removing public APIs, strengthening required generic bounds,
or breaking the documented ordering, cancellation, observation or shutdown
contracts requires a major release. Additive features must preserve existing
signatures, bounds and behaviour when enabled or disabled. Internal details that
are not public contracts may change without a major release.

The [runtime contracts](runtime.md) describe RCA's observable guarantees.
Any contract-breaking repair before 1.0 requires updated documentation and qualification.
Earlier results do not qualify changed code without an applicability check.

## MSRV and tooling

The first stable release records its minimum supported Rust version (MSRV).
After 1.0, increasing that requirement needs a minor release and a migration
note, not a patch release. RCA 0.1.0 requires Rust 1.99.0.
Nightly-only test tools do not become requirements for library consumers.

## Deprecation and migration

Deprecation is a migration notice, not permission to break a stable API within
its current major version. Deprecated public APIs retain their documented
behaviour until a major release permits removal or incompatible replacement.
Deprecation notes identify the replacement when one exists.
Release notes and migration documentation explain required caller changes. A replacement must not
silently change runtime semantics behind a compatibility wrapper.

These rules do not promise a release schedule, a minimum support duration or
backports to every older release. Pre-stable changes follow the explicit
release-note and renewed-qualification requirements above.
