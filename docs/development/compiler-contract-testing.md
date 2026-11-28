# Test what the Rust compiler accepts

Compiler tests show which API calls Rust accepts and which it rejects.
A *fixture* is a small Rust program that makes one such call.
A test runner compiles it and compares errors or warnings with expected output. A saved copy of that output is a *snapshot*.

For example, a Store can lend its state to a callback. The callback can return an owned string, but not a reference into that state.
See the [accepted call](../../crates/rca-core/tests/compiler/pass/owned_read.rs) and the [rejected call](../../crates/rca-core/tests/compiler/fail/borrowed_read.rs).
Neither fixture proves what happens after an action runs. Use a behaviour test for ordering, cancellation, and stale results.
Use [documentation tests](../../CONTRIBUTING.md#documentation-and-checks) to teach normal API use.

## Existing suites

RCA uses two Rust test tools, `trybuild` and `ui_test`. Use the existing suite for the API you changed:

| What changed? | Where to put the case |
| --- | --- |
| Which values can cross threads, which child an action selects, or how long a borrow lasts | [Core compiler tests](../../crates/rca-core/tests/compile.rs) (`trybuild`) |
| What a named task can retain or which task name it can use | [Typed task tests](../../crates/rca-core/tests/typed_compile.rs) (`trybuild`) |
| What a caller can still access after removing a child | [Removed-child tests](../../crates/rca-core/tests/removal_compile.rs) or the [single-threaded counterpart](../../crates/rca-core/tests/local_removal_compile.rs) (`trybuild`) |
| Store warnings or the difference between owned and borrowed reads | [Core diagnostics](../../crates/rca-core/tests/compiler_contracts.rs) (`ui_test`) |
| Macro errors, warnings on generic types, or use with a renamed dependency | [Macro tests](../../crates/rca-macros/tests/snapshot_compile.rs) (`trybuild` and Cargo) |
| Task warnings or values selected from TestStore state | [TestStore diagnostics](../../crates/rca-test/tests/compiler_contracts.rs) (`ui_test`) |

For each new rejection, add an example that compiles. Change only the property responsible for the error.
Keep invalid fixtures outside the test crate's module tree so that normal tests still compile.
For a deprecated API, check that a valid call compiles with the intended migration warning. Check that the replacement compiles without warnings.

## Run and record

Use the [repository toolchain](../../rust-toolchain.toml), edition 2024, and its `rust-src` component. Nightly Rust is not necessary.
Some snapshots include excerpts from the standard library. Without its source, the compiler omits those excerpts and the comparison fails.

Run these commands from the repository root in one POSIX shell.
On macOS, use the [memory watchdog](../../CONTRIBUTING.md#build-memory-safety). This function creates a new receipt for each run:

```sh
compiler_test() {
  python3 scripts/guard_cargo_memory.py --receipt "$(mktemp -d)/compiler-test.json" -- test "$@"
}
```

On an isolated runner with an operating-system memory limit, use this function instead:

```sh
compiler_test() { rustup run 1.100.0 cargo test "$@"; }
```

Run the command for the changed API:

```sh
compiler_test --locked -p rca-core -p rca-test --all-features --test compiler_contracts -- --check
compiler_test --locked -p rca-core -p rca-test --no-default-features --test compiler_contracts -- --check
compiler_test --locked -p rca-core --no-default-features --test compile --test typed_compile --test removal_compile
compiler_test --locked -p rca-macros --test snapshot_compile
```

For `ui_test`, append part of a fixture filename after `--check` to run that case, for example `-- --check task_warning`.
The `local` feature enables additional single-threaded cases. Keep the run without that feature: the available APIs also affect the rejection messages.

**If a diagnostic changes, read the difference before updating its snapshot.** A compiler crash or a missing dependency is not an expected rejection.
Only for an intentional change, use `--bless` to write the new expected text. For example:

```sh
compiler_test --locked -p rca-core --all-features --test compiler_contracts -- --bless task_warning
git diff -- crates/rca-core/tests/compiler
compiler_test --locked -p rca-core --all-features --test compiler_contracts -- --check task_warning
```

Inspect new files with `git status` as well. An ordinary `git diff` omits untracked snapshots.
For `trybuild`, `TRYBUILD=overwrite` records the expected output locally. Unset it before the next check.
Never record snapshots in CI. Normal checks fail on missing or changed output.

## Fixture conventions

The `ui_test` runner builds public dependencies with a locked manifest and the features under test.
Leave `CARGO_TARGET_DIR` unset or choose an absolute path.
Cargo starts each test in its package directory. The runner refuses relative paths to avoid separate caches for different packages.
The dependency build selects the host target explicitly. Its first run can create a target-specific cache.
The fixture output goes under `compiler-contracts` in Cargo's target directory.

| Case | Expected result |
| --- | --- |
| Warning in `tests/compiler/pass/` | Compiler succeeds. Annotate the line that causes the warning and keep the expected warning summary. |
| No warning in `tests/compiler/pass/` | Add `//@ check-pass`. The expected output is empty. Do not combine this directive with warning annotations. |
| Rejection in `tests/compiler/fail/` | Compiler rejects the intended operation. Annotate its error and keep the adjacent `.stderr` snapshot. |

The following [fixture](../../crates/rca-core/tests/compiler/pass/task_warning.rs) compiles with one warning. The first comment expects the compiler's warning count.
The `//~^` comment matches the warning on the preceding line:

```rust
//@ error-in-other-file: 1 warning emitted
pub async fn ignored(store: &rca_core::Store<String, ()>) {
    store.send(()).await.unwrap();
    //~^ WARN: unused `ActionTask` that must be used
}

fn main() {}
```

The [expected output](../../crates/rca-core/tests/compiler/pass/task_warning.stderr) names the call and explains why dropping the task handle does not cancel the work.
The [accepted alternatives](../../crates/rca-core/tests/compiler/pass/task_handled.rs) wait for completion or discard the handle intentionally.
[Action receipts](../usage/receipt-lifetimes.md) explain which wait *does* cancel work on drop.

The `ui_test` suites compile these fixtures without running them. A `trybuild` pass fixture can run: keep its `main` free of side effects.
Unexpected warnings and missing warnings both fail the test.
Snapshots describe a particular compiler. After a toolchain update, review the cause of each changed message and its passing counterpart.
Keep built-in path normalisation. The TestStore runner also normalises one feature-dependent spelling of a type.
Do not suppress the lint under test or remove the type, severity, location, or migration advice from the comparison.

## Downstream applications

The [packaged consumer](../../tests/downstream-composition) tests RCA as a dependency, not as part of the workspace.
It imports the public crate under the name `architecture` and uses no RCA internals.
For a library that wraps RCA, the same pattern can protect its own public API:

```toml
[dev-dependencies]
ui_test = { version = "=0.30.6", default-features = false, features = ["rustc"] }

[[test]]
name = "compiler_contracts"
harness = false
```

Copy the [test runner](../../tests/downstream-composition/tests/compiler_contracts.rs). Adapt its dependencies and fixtures to your library.
Keep compiler tooling in development dependencies, not runtime libraries.
From your application checkout, run `cargo test --test compiler_contracts -- --check`.

For RCA itself, the packaging script creates a temporary local registry, builds the archives, and runs independent consumers.
It does not publish packages or change personal Cargo or Git configuration. The script removes its temporary registry.
The macOS watchdog cannot guard its nested builds. On an isolated Linux runner with a memory limit, run:

```sh
rustup run 1.100.0 bash scripts/verify-packaged-downstream.sh
```

## Required ownership

If a public compiler guarantee changes, update the matching fixtures with it.
Required CI checks all-feature and no-default-feature configurations, compiler diagnostics, and the packaged consumer.
Review the test runner and rejection assertions separately from executable production coverage.
A passing compiler test does not establish runtime behaviour, performance, compatibility across targets, or 100% production coverage.

Point-Free's [CompilationTesting](https://github.com/pointfreeco/swift-compilation-testing) inspired the practice of checking a diagnostic at the call that caused it.
