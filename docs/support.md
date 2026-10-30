# RCA support policy

RCA 0.1.0 uses Rust 1.98.0, Cargo 1.98.0, and Rust Edition 2024. RCA
does not require nightly Rust. Nightly 2026-08-23 is pinned for Miri,
unused-dependency analysis, fuzzing, and proc-macro API qualification only.

## Qualification status

Local release-readiness qualification was completed on 29 October 2026 for the
recorded 0.1.0 candidate. This includes source review, tests, coverage,
performance checks, documentation, package consumers and retained platform
evidence. Each result applies to the source, dependencies, toolchain and
environment it covers. Changes to those inputs require an applicability check
and, where affected, new verification.
Local results do not establish that hosted checks passed.

All 18 hosted checks passed for `796032f234095596c822ba48f4a54e96d5c3c67a` on
30 October 2026. The [recorded run](https://github.com/jacobcxdev/rust-composable-architecture/actions/runs/40994124660)
qualifies that revision. Later changes require their own applicable checks.
The [review PR](https://github.com/jacobcxdev/rust-composable-architecture/pull/1)
records the current head and its results. RCA 0.1.0 remains unpublished.
Qualification does not establish a stable-support promise or publication receipt.

## Targets

| Target | Core and umbrella gate | Tokio adapter gate | Hosted CI at `796032f` |
| --- | --- | --- | --- |
| `x86_64-unknown-linux-gnu` | Required | Required | Passed |
| `aarch64-unknown-linux-gnu` | Required | Required | Passed |
| `x86_64-apple-darwin` | Required, compile-only | Required, compile-only | Passed |
| `aarch64-apple-darwin` | Required | Required | Passed |
| `x86_64-pc-windows-msvc` | Required | Required | Passed |
| `wasm32-unknown-unknown` | Required | Not applicable | Passed |

WebAssembly support covers runtime-neutral `rca-core` and umbrella APIs. It
remains experimental until RCA ships and qualifies a WebAssembly-compatible
`Send` runtime adapter.

The table defines the required target scope and the status of hosted CI, not a
claim that every local platform check is pending. Hosted results must identify
the exact candidate revision. They do not replace the retained local evidence.

Per the 1 October scope decision, existing devices are sufficient for native
qualification: macOS ARM64, Windows x86-64 and Linux ARM64/x86-64, including
the existing virtualised Linux hosts. No Intel Mac is required. The macOS
x86-64 row retains cross-compilation coverage on an ARM Mac, not a claim of
native runtime qualification. Earlier receipts retain their historical limits.

Foreign-target compilation does not establish native runtime support on those
hosts.

The four native-execution CI rows also execute Tokio's library tests (including
shared runtime and composition conformance on current-thread and multi-thread
executors), synchronous and thread-affine host integration and deterministic
conformance.
These use locked dependencies and run on the target host; the WebAssembly row
remains compile-only. Adding these commands is not evidence that hosted runs
have passed. Release qualification must retain each host's successful run and
exact candidate revision. Real platform I/O and thread-affine resource adapters
need their own tests beyond runtime conformance.

Those native rows also run the persistent-counter package tests, covering real
file replacement/restart, process interruption, malformed-input preservation,
CLI behaviour and file-client quiescence. Unix-only directory-sync and symlink
tests remain conditional; Windows exercises its supported file-only durability
mode. The ten-minute CI step deadline bounds a stuck test process. This is
configured coverage, not a successful Windows/Linux execution receipt or proof
of power-loss durability.

## Features

The umbrella candidate is packaged as `rust-composable-architecture` and
imported as crate `rca`. Default features are empty. The nine-package 0.1.0 candidate
contains six headless crates and three transport dependencies.
Historical renderer and callback-adapter prototypes are not RCA packages.
No current release package depends on them.

| Feature | Adds |
| --- | --- |
| `tokio` | Tokio runtime adapter |
| `test-util` | Deterministic runtime and exhaustive `TestStore` |
| `local` | Owner-thread Store, runtime and task APIs for non-`Send` values |
| `macros` | Dependency, path, reducer-composition, snapshot and case-description macros |
| `diagnostics` | Reducer change tracing, lifecycle callbacks and bounded, caller-rendered diagnostics |
| `futures-stream` | `futures_core::Stream` for observations |

Features are additive. Runtime-neutral core has no Tokio, macro, test, or UI
dependency. Custom executors integrate through `RuntimeDriver`; adapter authors
must run shared conformance tests before claiming support.

`rca-persistence` is one of the six headless candidate crates, selected directly
rather than through a facade feature. Its empty default feature set keeps generic codec/backend
coordination runtime-neutral. Its additive `file` feature enables Tokio and
temporary-file dependencies on supported native targets. Generic persistence is
also subject to the runtime-neutral target checks; WebAssembly has no file-adapter
gate. `insta` is development-only snapshot tooling, not a minimal production
dependency. Diagnostics select no logger or global sink. Reducer-change tracing
callbacks run only with debug assertions; lifecycle callbacks run in debug and
release builds. Typed change events precede actor acknowledgement.

Registry-only qualification now includes minimal core/facade consumers, a
diagnostics/client-clock/dependency/control/shared consumer, generic persistence
without a runtime feature and a native file consumer. Source validation checks
every resolved RCA dependency, exact versions and minimal dependency boundaries before the offline
consumer check/test/run steps. A persistence receipt acknowledges committed actor
state under the backend's own successful-save guarantee; it does not imply generic
backend quiescence. The file consumer explicitly closes the backend before restart.

Feature-combination checks qualify the configurations they exercise on that
host. They do not establish native support on other targets or test every
behaviour within each combination.

## Compatibility

### Public API paths in 0.1.0

Application code normally imports from `rca`; runtime-neutral integration crates
can import the same core types directly from `rca_core`. Ordinary authoring types
and functions live at the crate root, such as `Store`, `Feature`, `Runtime`,
`KeyedState` and `scope`. Implementation modules are not another public route.

Two namespaces describe distinct uses:

- `local` contains owner-thread types whose payloads need not implement `Send`.
  Shared identity and policy types, including `InputGate` and `RemovalOrigin`,
  remain at the root.
- `diagnostics` contains tracing, checkpoints and structural inspection. For
  example, use `rca::diagnostics::TraceChanges`, not `rca::TraceChanges`.

For a checkout that used earlier 0.1.0 development APIs, replace ordinary
implementation-module imports with root imports: `rca_core::runtime::Clock`
becomes `rca_core::Clock`. Replace `feature::Change` with root `ReactionChange`;
the distinct tracing record remains `diagnostics::Change`. Direct-core and
facade imports retain their existing feature requirements. These path changes
do not change runtime ownership, scheduling or cancellation.

The hidden `rca_core::testing` module is an inter-crate testing seam, not the
application testing interface. Use `rca_test::TestStore`, or `rca::TestStore`
with `test-util`, for application assertions.

### Plain persistence revisions in 0.1.0

`rca_persistence::NoBackendRevision` is now a zero-sized struct with a private
field, not an empty enum. External callers still cannot construct it or obtain
it through `Default`. A plain document's `backend_revision()` still returns
`None`; only a revision-aware backend supplies an actual comparison identity.

This is an intentional source change from earlier unpublished candidates. Code
that eliminated a `NoBackendRevision` value with `match value {}` must remove
that assumption. Use the document's optional revision getter to distinguish
plain storage from revision-aware storage. Internally, the private marker lets
plain and revisioned operations carry a concrete completion value without
impossible missing-revision branches or backend ownership in document state.

### Version policy

RCA starts at 0.1.0. It does not maintain earlier Composable releases or promise
source compatibility with development prototypes. During pre-stable development,
MSRV and public API changes require explicit release notes and renewed qualification.
Stable 1.x releases will follow semantic versioning.

## Stable API and runtime contracts

The following policy takes effect with the first qualified 1.0 release. The
current candidate is not a stable release, and this policy does not declare any
outstanding qualification gate complete.

Stable releases follow semantic versioning for public APIs and documented
runtime behaviour. Removing public APIs, strengthening required generic bounds,
or breaking the documented ordering, cancellation, observation or shutdown
contracts requires a major release. Additive features must preserve existing
signatures, bounds and behaviour when enabled or disabled. Internal details that
are not public contracts may change without a major release.

The [runtime contracts](runtime.md) describe the current candidate's observable
guarantees. Any contract-breaking repair before 1.0 requires updated
documentation and qualification; passing older receipts does not qualify the
repaired candidate.

## MSRV and tooling

The first stable release records its minimum supported Rust version (MSRV).
After 1.0, increasing that requirement needs a minor release and a migration
note, not a patch release. The candidate currently requires Rust 1.98.0 as
listed above; its final stable MSRV remains subject to release qualification.
Nightly-only test tools do not become requirements for library consumers.

## Deprecation and migration

Deprecation is a migration notice, not permission to break a stable API within
its current major version. Deprecated public APIs retain their documented
behaviour until a major release permits removal or incompatible replacement.
Deprecation notes identify the replacement when one exists; release notes and
migration documentation explain required caller changes. A replacement must not
silently change runtime semantics behind a compatibility wrapper.

These rules do not promise a release schedule, a minimum support duration or
backports to every older release. Alpha changes continue to follow the explicit
release-note and renewed-qualification requirements above.
