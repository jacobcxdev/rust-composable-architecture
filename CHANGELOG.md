# Changelog

This file records notable changes to RCA.

## 0.1.0

### Application logic

- Added a common `Feature` interface. Action-based reducers implement it automatically,
  and both forms use the same Store and TestStore constructors.
- Added serial state updates, effects and typed tasks. Tasks start after the
  originating transaction commits and use typed state access, commands and outputs.
- Added required, optional, enum-case and keyed child composition with typed state
  and action paths.
- Added mounted child Features with installation identity, child-scoped work and
  rejection of stale handles.
- Added ordered, unique `KeyedState` collections and generation-aware child lifetimes.
- Added scoped Store handles for child sends, reads and observations.
- Added typed dependencies, lexical dependency overrides, clocks and shared values.
- Added bounded action queues, cancellation and `ActionTask` completion handles.
- Added a runtime-neutral core, an adapter for existing Tokio runtimes and an
  optional owner-thread API for non-`Send` state and work.
- Added persistence with explicit codecs and backends, plus an optional Tokio file adapter.

### Testing and diagnostics

- Added a deterministic runtime and exhaustive `TestStore` assertions that use the production Store engine.
- Added controlled futures, streams and gates for external-client tests.
- Added independent owned snapshots for state that cannot be cloned as a whole.
- Added focused receive operations with explicit omission reporting.
- Added scoped issue reporting, debug change tracing and lifecycle callbacks.
- Added compiler-contract tests, property tests and runtime conformance suites.

### Authoring and tools

- Added optional macros for dependencies, paths, composition, snapshots and case descriptions.
- Added headless application examples and guides for ownership, cancellation, composition and persistence.
- Added formatting, lint, feature, platform, package-consumer and concurrency checks.
- Added workload-specific performance and executable-production coverage requirements.
- Required Rust 1.99.0 and Rust Edition 2024.

The [support policy](docs/support.md) defines compatibility and target requirements.
The [qualification guide](docs/qualification.md) explains the scope of recorded evidence.
