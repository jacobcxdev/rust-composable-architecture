# Compile time and binary size

These four small applications measure the cost of building a program that uses RCA. They do not measure how fast the program runs.

| Package | What it builds |
| --- | --- |
| `rca-compile-direct` | A Store with one reducer and an owned action. |
| `rca-compile-composed-manual` | Required, optional, and keyed children with hand-written action and state paths. |
| `rca-compile-composed-macro` | The same child structure using the facade and macros. The build includes their dependencies, not just macro expansion. |
| `rca-compile-scoped` | A scoped Store handle that sends a child action and reads child state. |

All four use the executor in [support](support/src/lib.rs). This workspace has its own [lockfile](Cargo.lock) and [release settings](Cargo.toml).
Applications that use RCA choose their own settings.

## Check a fixture

From the repository root on a Mac, use the [Cargo memory watchdog](../../CONTRIBUTING.md#build-memory-safety):

```sh
mkdir -p target
python3 scripts/guard_cargo_memory.py --receipt target/compile-direct-check.json -- \
  check --locked --manifest-path benches/compile/Cargo.toml -p rca-compile-direct
```

Choose a new receipt path for each run. On a runner with an operating-system memory limit, use Cargo directly:

```sh
rustup run 1.100.0 cargo check --locked --manifest-path benches/compile/Cargo.toml --workspace
```

A successful check confirms that the fixture compiles. It does not produce a linked binary or measure a cold build.
For release build times and binary sizes, follow the [composition procedure](../../docs/performance/composition/README.md). To measure an application's runtime instead, see [measure your app](../../docs/guides/performance.md).
Keep generated binaries, timings, and raw command output outside the source tree.
