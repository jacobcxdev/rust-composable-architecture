# Fuzz keyed child lifetimes

A child can leave a collection while it still has work in flight. If another child takes the same key, an old sender must not reach it.

The [fuzz target](fuzz_targets/keyed_lifecycle.rs) runs generated actions against a real Store and an [independent model](../crates/rca-core/tests/support/keyed_model.rs).
After each action, it compares child order, values, accepted outputs, commit counts, and active tasks. At shutdown, every outstanding task must finish.
Each input describes at most 512 operations on eight keys. This standalone package is not part of a normal RCA build.

The [seed](seeds/keyed_lifecycle/reused-key) inserts a child, captures its sender, and sends an output.
It removes the child, inserts a replacement with the same key, and captures the new sender.
The Store must reject the old sender and accept the new one.

## Replay the seed

On macOS, run these commands from the repository root. They replay one input without coverage instrumentation or random mutations.
The [memory watchdog](../CONTRIBUTING.md#build-memory-safety) protects the compiler and the replay.

```sh
rustup run 1.100.0 cargo fetch --locked --manifest-path fuzz/Cargo.toml
mkdir -p target
run_dir=$(mktemp -d target/fuzz-replay.XXXXXXXX)
CARGO_PROFILE_DEV_DEBUG=0 CARGO_INCREMENTAL=0 \
  python3 -B scripts/guard_cargo_memory.py --memory-limit-gib 2 --timeout 240 \
  --receipt "$run_dir/memory.json" -- run --locked --offline \
  --manifest-path fuzz/Cargo.toml --bin keyed_lifecycle -- fuzz/seeds/keyed_lifecycle/reused-key
```

## Search for new failing inputs

Use an isolated Linux job with an OS memory limit for the entire build and run.
The macOS watchdog does not support `cargo fuzz`. Do not bypass it on a shared Mac.

Install `cargo-fuzz 0.13.2` and `nightly-2026-08-23` in the job. From the repository root, run:

```sh
set -eu
nightly=nightly-2026-08-23
rustup run "$nightly" cargo fetch --locked --manifest-path fuzz/Cargo.toml
run_dir=$(mktemp -d)
mkdir "$run_dir/corpus" "$run_dir/artifacts"
RUSTUP_TOOLCHAIN="$nightly" RUSTC="$(rustup which --toolchain "$nightly" rustc)" \
  RUSTDOC="$(rustup which --toolchain "$nightly" rustdoc)" CARGO_NET_OFFLINE=true \
  CARGO_BUILD_JOBS=1 rustup run "$nightly" cargo fuzz run keyed_lifecycle \
  "$run_dir/corpus" fuzz/seeds/keyed_lifecycle -- \
  -runs=1000000 -seed=1 -max_len=2048 -artifact_prefix="$run_dir/artifacts/"
git diff --exit-code -- fuzz/Cargo.lock
```

The first directory receives new inputs. The second supplies the checked-in seed.
A completed run checks only the inputs it tried. If it fails, keep the failing input and add a regression test before fixing the defect.
Keep generated inputs and logs outside Git. See the [Rust Fuzz Book](https://rust-fuzz.github.io/book/cargo-fuzz/tutorial.html) for replay and investigation.

## Why one target?

Keyed child lifetimes combine collection edits, effect senders, and cancellation. A long sequence can expose a defect that a single operation cannot.
Other boundaries have different tests: [property tests](../crates/rca-core/tests/scoped_properties.rs) generate scoped operations, and [Loom tests](../crates/rca-core/tests/loom.rs) explore competing thread schedules.
Add a fuzz target when it has a distinct input space and an independent way to tell right from wrong.
