# Packaged composition and compiler contracts

This consumer imports the public facade under the name `architecture`. It does
not use workspace paths or RCA internals. The packaged-downstream check builds
the RCA package archives and stages them in an isolated local registry.
It then checks, tests and runs this consumer:

```sh
scripts/verify-packaged-downstream.sh
```

The `compiler_contracts` target uses `ui_test` only as a development dependency.
It demonstrates an owned scoped read, its rejected borrowed alternative, a
successful default-severity causal-task warning, and warning-free task handling.
The fixtures are compiled but never executed.

Within a consumer checkout with its `rca-local` registry configured and lockfile
resolved, run the narrow target with Rust 1.98.0:

```sh
cargo test --locked --test compiler_contracts -- --check
```

`pass/` expects successful compilation. Warning fixtures use source-line `WARN`
annotations and adjacent diagnostic snapshots; clean positives use `check-pass`.
`fail/` expects rejection at the annotated operation. Missing or unexpected
diagnostics fail verification. Explicit local recording uses `--bless` instead
of `--check`; review the diff, then verify again. CI never records expectations.

When adapting this sample to an application, use that application's normal RCA
dependency and registry; keep compiler tooling in `[dev-dependencies]`. See the
[compiler-contract guide](../../docs/compiler-contract-testing.md) for ownership,
toolchain and recording guidance. Runtime tests still establish cancellation,
ordering and effect completion; compilation alone does not.
