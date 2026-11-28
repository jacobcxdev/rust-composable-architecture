# Packaged composition and compiler contracts

This consumer imports the public RCA crate as `architecture`. It uses no workspace paths or RCA internals.
The package check builds RCA archives, puts them in a temporary registry, then checks, tests, and runs the consumers.

On an isolated Linux runner with an OS memory limit, run the package check from the repository root:

```sh
scripts/verify-packaged-downstream.sh
```

On a shared Mac, use that runner for the package check. The [Mac memory guard](../../CONTRIBUTING.md#build-memory-safety) cannot run this shell script.

The package check also [extracts the library's facade examples](../../scripts/facade_doc_examples.py)
and tests them against the staged `rca` crate. It tests each combination of features and
direct dependencies separately, including macro examples with a renamed dependency.
Examples marked `no_run` only compile. Rejection examples must fail to compile,
and separate positive checks verify that their imports exist. The compiler tests below
check the exact diagnostics for their own fixtures.

The `compiler_contracts` target uses `ui_test` only as a development dependency.
It checks that a scoped read can return an owned value but not a borrowed one.
It also checks both ways an action task can compile: with the expected warning
when ignored and without a warning when handled. The test compiles the fixtures
but does not run their code.

To run only the compiler test, start in a consumer checkout with a configured `rca-local` registry and a resolved lockfile.
On macOS, set `rca_root` to the absolute path of the RCA checkout:

```sh
python3 "$rca_root/scripts/guard_cargo_memory.py" --receipt "$(mktemp -d)/compiler-contracts.json" -- \
  test --locked --test compiler_contracts -- --check
```

On an isolated runner with an OS memory limit, run:

```sh
rustup run 1.100.0 cargo test --locked --test compiler_contracts -- --check
```

`pass/` expects successful compilation. Warning fixtures use source-line `WARN`
annotations and adjacent diagnostic snapshots. Cases that must compile without warnings use `check-pass`.
`fail/` expects rejection at the annotated operation. Missing or unexpected
diagnostics fail verification. Explicit local recording uses `--bless` instead
of `--check`. Review the diff, then run the check again. CI never records expectations.

When adapting this sample to an application, use that application's normal RCA
dependency and registry. Keep compiler tooling in `[dev-dependencies]`. See the
[compiler-contract guide](../../docs/maintainers/compiler-contract-testing.md) for ownership,
toolchain and recording guidance. Runtime tests still establish cancellation,
ordering and effect completion. Compilation alone does not.
