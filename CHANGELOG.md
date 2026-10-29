# Changelog

All notable RCA changes are documented here. Dates describe published
releases; unreleased candidates have no date.

## [0.1.0] - Unreleased

### Added

- Added `Feature` and typed tasks alongside `Reducer` and `Effect`. Tasks start
  after their originating transaction commits and use typed state access,
  commands and outputs.
- Added mounted child features with installation identity, stale-handle
  rejection and child-scoped work. Optional and keyed children can be composed
  into a parent without routing every child operation through a parent action.
- Added owner-thread Store, runtime, feature and task APIs under `local` for
  state and work that need not implement `Send`.
- Added independent owned snapshots and the `Snapshot` derive for testing
  state that cannot be cloned as a whole.
- Added focused `TestStore` operations: `receive_until`,
  `receive_until_with_captures` and `receive_omitting`. Tests explicitly choose
  omission reporting; selected assertions remain strict, and virtual time does
  not advance implicitly.

### Changed

- Replaced the empty `NoBackendRevision` enum with a private-field zero-sized
  struct. Plain documents still expose no backend revision, and external code
  cannot construct or default the marker. Earlier `match value {}` eliminations
  no longer compile. See the
  [migration note](docs/support.md#plain-persistence-revisions-in-010).
- Allowed borrowed arguments and results in `DependencyClient` endpoints,
  including higher-ranked signatures. Rust checks their lifetimes; endpoint
  captures retain their declared `Send + Sync` requirements. Removed the
  redundant syntax scan that rejected these valid signatures.
- Corrected failure messages that overstated lifecycle state. A failed scoped
  receipt does not prove the root Store stopped; a test gate can remain reserved
  by unfinished work after its actor detaches. Error types and recovery stay
  unchanged, but consumers comparing the old diagnostic strings must update.
- Consolidated ordinary core imports at the crate root. Implementation modules
  are private; `local` and `diagnostics` remain purposeful public namespaces.
  Renamed reaction `feature::Change` to root `ReactionChange`. Diagnostic types
  now use only `diagnostics`, and shared `InputGate` and `RemovalOrigin` use only
  the root. Completed the facade's mount, fold and removal exports. Hidden
  inter-crate testing helpers now use `rca_core::testing`. See the
  [migration notes](docs/support.md#public-api-paths-in-010).
- Reset the unpublished new-product package line from `0.9.0-alpha.1` to
  `0.1.0`, including exact internal dependency bounds and downstream fixtures.
- Preserved legacy tags and earlier qualification receipts under their original
  identities. This is not a downgrade of a published package or a stable release.
- Retained the composition design described below without changing runtime or
  public API source during the version transition.
- Replaced the boxed `ActionReceiptFuture` alias with a concrete, allocation-free
  commit-wait future. `receipt.await` and `receipt.finish().await` retain their
  result and cancellation semantics. Code relying on the former boxed alias
  must explicitly use `Box::pin(receipt.into_future())` where a boxed future is
  required; arbitrary boxed futures no longer have type `ActionReceiptFuture`.
- Kept type-generic path and case-catalogue namespaces type-only, without a
  `Debug` implementation that has no valid receiver. This supersedes the earlier
  pre-release namespace `Debug` promise; remove that bound from namespace types.
  Actual path values, case tags and borrowed views retain their diagnostics.
  Marker aliases remain unavailable for tuple construction or patterns, and the
  `__rca_path_namespace_` sibling prefix remains reserved.

### Qualification

- Completed local release-readiness qualification for the recorded candidate.
  Evidence remains tied to the source, dependencies, toolchain and environment
  it covers. Hosted PR CI has not yet run, and no package has been published.
  See the [qualification record and limits](docs/qualification.md).

## [0.9.0-alpha.1] - Unreleased

Historical development candidate, superseded by the new `0.1.0` package line.

### Added

- Added borrowing state and action paths with handwritten constructors.
- Added required, optional, enum-case, and keyed reducer composition.
- Added ordered unique `KeyedState` collections and generation-aware child
  lifetimes.
- Added typed `ScopedStore` handles for child sends, reads, and observations.
- Added lexical dependency overlays for Store, feature, effect, and test scopes.
- Added optional derives for state paths, action paths, dependency keys, and
  reducer-tuple syntax.

### Changed

- Changed reducers to borrow actions during serial reduction instead of taking
  ownership or requiring action clones.
- Tied child effects, queued outputs, scoped handles, and observations to their
  mounted child generation.
- Required Rust 1.98.0 and Rust Edition 2024.

### Removed

- Removed prototype recursive reducer derives and implicit clone-based routing.

### Qualification

- This development line remained local and unpublished. RCA 0.1.0 supersedes
  its historical checklist. See the [current qualification boundary](docs/qualification.md).
