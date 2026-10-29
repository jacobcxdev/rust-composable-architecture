# Phase 2 structural-lifetime characterisation

Historical excerpts use `<owner-home>` for the original user's home directory.
They abbreviate local machine paths and are not current copy-and-paste commands.
Raw receipts are retained unchanged.

This evidence characterises Phase 2 lifecycle paths and cancellation work. It
is not canonical cross-revision qualification. Although the host matches the
documented hardware cohort, the Task 5 checkout was intentionally dirty and no
alternating paired reference cohort was run. `docs/performance/method.md`
requires both a clean candidate and the full paired protocol before a release
gate can be claimed.

The measured source was based on
`9ad4e5c5704420d637bf1f5de69cf6282ba3f55c`. The tracked dirty-worktree
workload diff had SHA-256
`77a61bfaee8e5326349d8d4e3760f491854be6a6e6ed4898b2e18bc3f60e864a`;
the benchmark source itself had SHA-256
`5e6d93172d9ec6ffee4cd0d63fa5d61e4fc3814f5767e023a91d1de70a7abdf2`.
This identifies the local workload without presenting the dirty base revision
as the measured final commit.

## Environment and profile

- Host: `Mac15,9`, Apple M3 Max, 16 logical CPUs, 51,539,607,552 bytes
  memory, `arm64`.
- Power: AC attached, `pmset` power mode `2`.
- OS: macOS 26.6.2 build 25G83; Darwin 25.6.0.
- Rust: `rustc 1.98.0 (88d9e12ae 2026-08-18)`, compiler commit
  `88d9e12ae178fab0fb5cc050a94da85685d449ea`, LLVM 22.1.8,
  `aarch64-apple-darwin`.
- Cargo: `cargo 1.98.0 (797e8a9bc 2026-08-05)` with
  `CARGO_BUILD_JOBS=1` and `--locked`.
- Benchmark profile: workspace bench/release settings with `opt-level=3`,
  `codegen-units=1`, `incremental=false`; this local run overrode LTO to
  `false`, debug information to `false`, and stripping to `true`. The existing
  Phase 1 evidence had already established fat-LTO link pressure on this host.
- Executor: one four-worker `futures-executor` pool per Store fixture. Task
  futures use the pool directly; no benchmark-owned mutex or OS thread per
  descendant is present.

The complete command, environment, latency, and item-throughput transcript is
in [`lifetimes-divan.txt`](lifetimes-divan.txt). The complete serial Dhat test
transcript is in [`lifetimes-allocation.txt`](lifetimes-allocation.txt).
Review-corrected transition and cancellation rows are in
[`lifetimes-drop-correction.txt`](lifetimes-drop-correction.txt).

## Review correction

Independent Task 5 review found that the original `optional_enter`,
`optional_leave`, `case_switch`, and `descendant_cancellation` closures returned
`()`. Consuming a fixture and returning `()` dropped that fixture before Divan's
`sample_end`, so those original medians included deterministic Store shutdown.
`skip_ext_time=true` affects Divan's minimum/maximum time accounting only and
does not exclude closure-local destruction.

The corrected closures return their fixture as benchmark output. Divan stores
that output and drops it after `sample_end`. The affected rows in the table
below come from the review-corrected cohort; their original raw rows are
superseded. Unaffected rows retain the original cohort. This correction changes
no production source or allocation claim.

## Scenario definitions

- `matching_scope/1` is one required child Scope and one effect-free matching
  child action. `matching_scope/16` nests the same required path 16 levels.
- `optional_hit` and `optional_miss` use one warmed, present optional child.
  The hit reduces child then parent; the miss reduces only parent.
- `optional_enter` and `optional_leave` construct a fresh absent or present
  fixture outside the timed region and time only the lifecycle action commit
  and returned task completion.
- `case_hit`, `case_miss`, and `case_switch` use a present detail case. Switch
  replaces it with the list case. Fresh switch fixtures are prepared outside
  the timed region.
- `descendant_cancellation/N` registers `N` pending tasks beneath a required
  Scope nested inside an optional child. Registration is outside the timed
  region. The timed operation is the leave commit that removes the optional
  child and cancels its descendants. Task completion and Store shutdown are
  deterministic cleanup outside the timed region.

All lifecycle rows used one logical transition per iteration, black-boxed
inputs, 50 samples, a 0.5-second maximum measurement time, and 50 reported
iterations. Divan timer precision was 41 ns. Transition fixtures with changing
presence used `sample_size=1`; `with_inputs` constructs them before the sample,
and returning the fixture from the consuming closure defers destruction until
after `sample_end`. `skip_ext_time=true` excludes that external work only from
minimum/maximum time accounting.

## Observations

The table reports Divan medians and mean item throughput. Fastest, slowest,
median, mean, and their corresponding throughput values remain available in
the raw transcript.

| Scenario | Median latency | Mean throughput |
| --- | ---: | ---: |
| Required Scope, depth 1 | 8.395 µs | 108.4 Kitem/s |
| Required Scope, depth 16 | 8.603 µs | 110.8 Kitem/s |
| Optional hit | 7.687 µs | 112.2 Kitem/s |
| Optional miss | 7.312 µs | 113.1 Kitem/s |
| Optional enter | 7.479 µs | 124.6 Kitem/s |
| Optional leave | 10.12 µs | 91.91 Kitem/s |
| Case hit | 6.583 µs | 132.1 Kitem/s |
| Case miss | 9.957 µs | 93.62 Kitem/s |
| Case switch | 9.020 µs | 99.59 Kitem/s |
| Cancel 1 descendant | 9.207 µs | 82.67 Kitem/s |
| Cancel 100 descendants | 47.56 µs | 19.4 Kitem/s |
| Cancel 10,000 descendants | 327.6 µs | 2.937 Kitem/s |

These are whole Store-operation measurements, including mailbox admission,
actor scheduling, reduction, receipt, and task completion. They do not isolate
the combinator body. The local cohorts show scheduler variation, so small
differences between steady paths are not evidence of an improvement or
regression.

## Allocation observations

The Dhat tests warm each reducer, snapshot cumulative heap statistics around
one subsequent reduction, and require both allocation-operation and allocated-
byte deltas to remain zero. Tests ran serially.

| Warmed empty-effect path | Allocation operations | Allocated bytes |
| --- | ---: | ---: |
| Required Scope hit, depths 1/4/16 | 0 | 0 |
| Required Scope miss, depths 1/4/16 | 0 | 0 |
| Optional hit and miss | 0 | 0 |
| Identified optional hit and miss | 0 | 0 |
| Absent optional parent path | 0 | 0 |
| Case hit and miss | 0 | 0 |
| Identified case hit and miss | 0 | 0 |

This is the warmed empty-path gate. First registration of a structural scope
may allocate and is deliberately not described as allocation-free. Effectful
task registration and descendant cancellation are also outside this Dhat
claim. No `dhat-heap.json` remained after the serial checks.

## Exact commands and limitations

The workload digest was captured from
`<owner-home>/.codex/worktrees/rca-runtime-foundation` while `HEAD` was the
base commit. The exact measurement-time pipeline was:

```bash
git diff --binary -- \
  Cargo.toml \
  Cargo.lock \
  rca-core/benches/composition.rs \
  rca-core/tests/lifecycle_properties.rs \
  rca-test/src/conformance/lifecycle.rs \
| shasum -a 256
```

At capture time `rca-core/tests/lifecycle_properties.rs` was untracked, so Git
emitted no bytes for it despite the pathspec. `Cargo.lock` was unchanged and
also emitted no bytes. The three `lifetimes*` evidence files were untracked and
were not pathspecs, so the digest excludes them and has no self-reference. The
same byte stream and digest can be reproduced from the committed range with:

```bash
git diff --binary --no-ext-diff \
  9ad4e5c5704420d637bf1f5de69cf6282ba3f55c \
  ac131936ecd8c4b6e7411c6bf15bba264c16e430 -- \
  Cargo.toml \
  Cargo.lock \
  rca-core/benches/composition.rs \
  rca-test/src/conformance/lifecycle.rs \
| shasum -a 256
```

This produces
`77a61bfaee8e5326349d8d4e3760f491854be6a6e6ed4898b2e18bc3f60e864a`.
The digest therefore identifies the tracked benchmark, dependency-manifest,
and shared-conformance changes used by the local run; it does not identify the
untracked property-model source. The separate benchmark-source digest above
identifies the exact measured benchmark source.

The latency and throughput command was:

```bash
toolchain_bin=$(dirname "$(rustup which --toolchain 1.98.0 cargo)")
toolchain_root=$(dirname "$toolchain_bin")
DYLD_LIBRARY_PATH="$toolchain_root/lib" \
RUSTC="$toolchain_bin/rustc" \
RUSTDOC="$toolchain_bin/rustdoc" \
CARGO_BUILD_JOBS=1 \
CARGO_TARGET_DIR=target/task5-bench \
CARGO_PROFILE_BENCH_LTO=false \
CARGO_PROFILE_BENCH_DEBUG=false \
CARGO_PROFILE_BENCH_STRIP=true \
"$toolchain_bin/cargo" bench -p rca-core --bench composition --locked
```

The allocation commands were:

```bash
PATH="$toolchain_bin:$PATH" CARGO_BUILD_JOBS=1 \
CARGO_PROFILE_DEV_DEBUG=false CARGO_PROFILE_DEV_STRIP=false \
cargo test -p rca-core --lib --features allocation-measurement --locked \
lifecycle_hot_paths_do_not_allocate_after_warm_up -- \
--nocapture --test-threads=1

PATH="$toolchain_bin:$PATH" CARGO_BUILD_JOBS=1 \
CARGO_PROFILE_DEV_DEBUG=false CARGO_PROFILE_DEV_STRIP=false \
cargo test -p rca-core --lib --features allocation-measurement --locked \
scope_none_paths_do_not_allocate_after_warm_up -- \
--nocapture --test-threads=1
```

This local characterisation did not run the canonical ten normal and six Dhat
paired repetitions, a clean reference checkout, CPU-cycle counters, resident
memory capture, compile-time comparison, or binary-size comparison. It cannot
be fed to `compare-performance.sh` and does not change the 10% release budget.
Run `scripts/qualify-performance.sh` from a clean candidate after the Phase 2
workload has been admitted to the canonical scenario manifest to obtain
release-qualified evidence.
