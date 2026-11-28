# Packaged composition and compiler contracts

This consumer imports the public RCA crate as `architecture`. It uses no
workspace paths or RCA internals. From the repository root, run the following
check. It builds the RCA package archives, puts them in an isolated local
registry, then checks, tests and runs this consumer:

```sh
scripts/verify-packaged-downstream.sh
```

The `compiler_contracts` target uses `ui_test` only as a development dependency.
It checks that a scoped read can return an owned value but not a borrowed one.
It also checks both ways an action task can compile: with the expected warning
when ignored and without a warning when handled. The test compiles the fixtures
but does not run their code.

Within a consumer checkout with its `rca-local` registry configured and lockfile
resolved, run the narrow target with Rust 1.100.0:

```sh
cargo test --locked --test compiler_contracts -- --check
```

`pass/` expects successful compilation. Warning fixtures use source-line `WARN`
annotations and adjacent diagnostic snapshots. Cases that must compile without warnings use `check-pass`.
`fail/` expects rejection at the annotated operation. Missing or unexpected
diagnostics fail verification. Explicit local recording uses `--bless` instead
of `--check`. Review the diff, then run the check again. CI never records expectations.

When adapting this sample to an application, use that application's normal RCA
dependency and registry. Keep compiler tooling in `[dev-dependencies]`. See the
[compiler-contract guide](../../docs/compiler-contract-testing.md) for ownership,
toolchain and recording guidance. Runtime tests still establish cancellation,
ordering and effect completion. Compilation alone does not.
