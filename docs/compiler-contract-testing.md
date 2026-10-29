# Compiler-contract testing

Compiler tests protect RCA's ownership boundaries and developer-facing diagnostics.
Use behavioural tests for runtime semantics, rustdoc for usage examples, and
compiler fixtures for acceptance, rejection at an intended operation, or successful
compilation with useful warning guidance. Compilation cannot prove cancellation,
ordering, stale authority, performance or production coverage.

## Existing suites

| Owner | Contracts and harness |
| --- | --- |
| [Core](../rca-core/tests/compile.rs) | Shared transport, move-only domains, keyed paths and borrowed views: `trybuild` |
| [Typed tasks](../rca-core/tests/typed_compile.rs) | Retained captures and task domains: `trybuild` |
| [Removed children](../rca-core/tests/removal_compile.rs) | Authority versus owned output, with a [Local suite](../rca-core/tests/local_removal_compile.rs): `trybuild` |
| [Core diagnostics](../rca-core/tests/compiler_contracts.rs) | Receipt/task warnings and borrowed/owned reads: `ui_test` |
| [Macros](../rca-macros/tests/snapshot_compile.rs) | Rejection snapshots, warning-clean generics and renamed dependencies: `trybuild` and Cargo |
| [TestStore](../rca-test/tests/compiler_contracts.rs) | Shared/Local task warnings and borrowed/owned projections: `ui_test` |

Preserve existing harness ownership. Add a positive counterpart for a new rejection:
keep the operation, changing the property that violates its contract. Return an
owned `String` rather than `&str` from a scoped read, for example.
Keep invalid fixtures outside the test crate's compiled module tree.

No owned deprecated API exists in these selected families. When one is intentionally
deprecated, test successful compilation with its authored migration guidance and
a warning-free replacement; do not invent deprecations to demonstrate the harness.

## Run and record

Use Rust 1.98.0, edition 2024, matching required CI. These harnesses need Cargo and
rustc, not nightly or `rust-src`; quality CI installs the latter for other checks.

```sh
rustup run 1.98.0 cargo test --locked -p rca-core -p rca-test --all-features --test compiler_contracts -- --check
rustup run 1.98.0 cargo test --locked -p rca-core -p rca-test --no-default-features --test compiler_contracts -- --check
rustup run 1.98.0 cargo test --locked -p rca-core --no-default-features --test compile --test typed_compile --test removal_compile
rustup run 1.98.0 cargo test --locked -p rca-macros --test snapshot_compile
```

Append a filename substring after `--check` to select fixtures, for example
`-- --check task_warning`. Local-only TestStore fixtures run with `local`.
All-features testing does not replace the non-Local run: some existing rejection
snapshots depend on which APIs are available.

For an intentional diagnostic change, inspect the failure before recording:

```sh
rustup run 1.98.0 cargo test --locked -p rca-core --all-features --test compiler_contracts -- --bless task_warning
git diff -- rca-core/tests/compiler
rustup run 1.98.0 cargo test --locked -p rca-core --all-features --test compiler_contracts -- --check task_warning
```

Review new files too: ordinary `git diff` omits untracked snapshots.
For existing `trybuild` suites, explicit local `TRYBUILD=overwrite` records
expectations; unset it before verification. Never record in CI. Normal Cargo
test execution checks expectations; missing or changed output fails.

## Fixture conventions

Small private harnesses use upstream `ui_test` configuration, not an RCA public
compiler API. They build public normal dependencies with explicit matching
features and locked manifests, sharing Cargo's target directory. Fixture output
stays under `target/compiler-contracts`.

Leave `CARGO_TARGET_DIR` unset, or set it to an absolute path. These harnesses
reject relative overrides: Cargo starts tests in each package directory, which
would otherwise silently create separate caches. The dependency builder also
passes the host target triple, so its first build can populate a target-specific
cache even when the outer Cargo command used the host default.

- `tests/compiler/pass/` requires compiler success. Warning fixtures require
  line-associated annotations and the exact warning summary.
- Warning-free positives additionally use `//@ check-pass`; checked output must
  be empty. Do not put this directive on warning fixtures: this `ui_test`
  version forbids diagnostic annotations in that mode.
- `tests/compiler/fail/` requires rejection at the intended operation. Annotate
  that error and retain the adjacent `.stderr` snapshot.
- These targets emit metadata, never execute fixture binaries. Existing
  `trybuild` pass fixtures can execute; keep their `main` side-effect-free.

Example successful default-severity warning:

```rust
//@ error-in-other-file: 1 warning emitted
pub async fn ignored(store: &rca_core::Store<String, ()>) {
    store.send(()).await.unwrap();
    //~^ WARN: unused `ActionTask` that must be used
}

fn main() {}
```

Its snapshot checks severity, use-site span and RCA's authored note: dropping an
action task does not cancel causal work. The positive counterpart awaits
`finish()` when completion matters, or explicitly drops the task when detaching
is intentional. Dropping a receipt/task is not cancellation; dropping the
cancellation-on-drop wait has different semantics. Runtime tests prove those facts.

Unexpected warnings and missing expected warnings fail. A missing compiler,
broken dependency build or compiler crash is an infrastructure failure, never an
acceptable rejection snapshot. Do not record such output as an API contract.

Snapshots are toolchain-specific evidence, not a promise to preserve every rustc
word across releases. On compiler updates, check the intended cause and positive
counterpart before recording. Built-in path normalisation is retained. One exact
TestStore type-qualification normalisation handles a proven Local-feature display
difference without removing type identity, severity, location or authored guidance.
Never suppress the lint being tested or normalise away the tested distinction.

## Downstream applications

The [packaged composition consumer](../tests/downstream-composition) demonstrates
owned/borrowed reads and task-warning counterparts through its renamed public
`architecture` dependency. It does not access RCA internals.

```toml
[dev-dependencies]
ui_test = { version = "=0.30.6", default-features = false, features = ["rustc"] }

[[test]]
name = "compiler_contracts"
harness = false
```

Copy its small [harness](../tests/downstream-composition/tests/compiler_contracts.rs)
and adapt fixtures to your own public feature or wrapper. Keep compiler tooling in
development dependencies; do not re-export it from runtime libraries.

RCA 0.1.0 is not published yet. This consumer uses the disposable `rca-local`
registry, so direct invocation without that registry is unsupported. From the
repository root, the existing workflow stages actual archives, verifies provenance
and runs this sample with the other consumers:

```sh
rustup run 1.98.0 bash scripts/verify-packaged-downstream.sh
```

The script removes its temporary registry and does not publish packages or alter
personal Cargo/Git configuration. After publication, consumers use the public
package normally and run `cargo test --test compiler_contracts -- --check`.

## Required ownership

Update compiler witnesses alongside changed compiler-enforced public contracts.
Required CI runs all-feature tests, explicit non-Local contracts, native diagnostic
targets and the packaged consumer. Audit fixture scaffolding and failure assertions
separately from production coverage. No compiler-test result weakens existing
runtime, compatibility, performance or 100% executable-production coverage gates.

This adopts diagnostic locality from
[Point-Free CompilationTesting](https://github.com/pointfreeco/swift-compilation-testing),
using existing Rust tools instead of porting Swift compiler infrastructure.
