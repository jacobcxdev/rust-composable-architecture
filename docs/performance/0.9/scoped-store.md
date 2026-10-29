# Scoped Store and dependency characterisation

Historical excerpts use `<owner-home>` for the original user's home directory.
They abbreviate local machine paths and are not current copy-and-paste commands.
Raw receipts are retained unchanged.

This evidence characterises the 0.9 scoped Store and lexical dependency paths.
It is not canonical cross-revision qualification. The checkout was intentionally
dirty while the Task 5 property and benchmark sources were being completed, and
no alternating reference cohort was run. The measured base revision was
`a19e95b8547779de0047bafff08a35cc3526c062`; the original full
characterisation benchmark source had SHA-256
`96aee12b4245578d666ae37e00feae1f262e8937cc0a2b2ad4c6e0367fab4f5c`.
The post-review source, which excludes observation setup from the coalescing
timed batches, has SHA-256
`24e3bf9bf44ef701f1b98d86295108bcbc4c92b754194ccb0485be84fef4ecd0`.

## Environment and profile

- Host: `Mac15,9`, Apple silicon, 16 logical CPUs, 51,539,607,552 bytes
  memory, `arm64`.
- OS: macOS 26.6.2 build 25G83; Darwin 25.6.0.
- Rust: `rustc 1.98.0 (88d9e12ae 2026-08-18)`, compiler commit
  `88d9e12ae178fab0fb5cc050a94da85685d449ea`, LLVM 22.1.8,
  `aarch64-apple-darwin`.
- Cargo: `cargo 1.98.0 (797e8a9bc 2026-08-05)` with
  `CARGO_BUILD_JOBS=1` and `--locked`.
- Benchmark profile: workspace bench/release settings with `opt-level=3`,
  `codegen-units=1`, and `incremental=false`; this run overrode LTO to `false`,
  debug information to `false`, and stripping to `true`.
- Runtime: each normal fixture owns one four-worker `futures-executor` pool.
  The full-mailbox fixture uses a paused driver and a one-slot mailbox so full
  admission and recovery are deterministic.

## Timing method

Fixtures and initial warm-up are outside the timed closure where the operation
allows it. Store operations include admission, actor scheduling, reduction,
receipt, and task completion; the results do not isolate only the projection or
reducer body. Construction rows time construction but retain destruction until
after sampling. The full-mailbox row returns the owned action from
`TryScopedSendError`; the recovery row starts the paused driver, drains the
first receipt, then admits and finishes a second action.

Every scalar row used 50 samples and a 0.5-second maximum time. Scale rows used
five requested samples with one batch per sample and a 0.5-second maximum; the
10,000-handle row completed two samples and the 10,000-observation row completed
three. The corrected coalescing rerun completed all five samples at each scale.
Divan reported a 41 ns timer precision.

The exact timing commands were:

```bash
PATH=<owner-home>/.rustup/toolchains/1.98.0-aarch64-apple-darwin/bin:$PATH \
RUSTC=<owner-home>/.rustup/toolchains/1.98.0-aarch64-apple-darwin/bin/rustc \
RUSTDOC=<owner-home>/.rustup/toolchains/1.98.0-aarch64-apple-darwin/bin/rustdoc \
DYLD_LIBRARY_PATH=<owner-home>/.rustup/toolchains/1.98.0-aarch64-apple-darwin/lib \
CARGO_BUILD_JOBS=1 CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_DEV_STRIP=false \
CARGO_PROFILE_BENCH_LTO=false CARGO_PROFILE_BENCH_DEBUG=false \
CARGO_PROFILE_BENCH_STRIP=true \
cargo bench -p rca-core --bench composition --locked -- scoped_

# The same environment and profile were used here.
cargo bench -p rca-core --bench composition --locked -- dependency_lookup
```

### Scalar results

| Scenario | Median | Mean | Meaning |
| --- | ---: | ---: | --- |
| Required handle construction | 18.60 ns | 21.95 ns | Root-lifetime typed view; no actor registration |
| Identified optional handle construction | 7.603 us | 7.879 us | Mailbox-ordered generation registration |
| Keyed handle construction | 7.229 us | 8.520 us | Mailbox-ordered entry-generation registration |
| `send` | 8.103 us | 8.566 us | Accepted action through task completion |
| Accepted `try_send` | 8.228 us | 8.653 us | Immediate admission through task completion |
| Full `try_send` | 332.7 ns | 416.3 ns | Rejected admission and owned-action recovery |
| Full-mailbox recovery | 16.08 us | 17.25 us | Drain first action, admit and finish second action |
| `read` | 8.041 us | 8.411 us | Mailbox-ordered child projection |
| `observe`, equal commit | 16.58 us | 17.70 us | Commit plus retained latest-value take |
| `observe_distinct`, equal commit | 8.165 us | 8.486 us | Commit with suppressed publication |
| `observe_distinct`, changed commit | 16.39 us | 20.34 us | Commit plus published-value take |
| Generation invalidation | 10.39 us | 11.57 us | Removal commit plus observation closure |

### Scale results

The median is the whole batch; Divan's item rate uses the batch cardinality.
Observation coalescing performs the requested number of commits without taking,
then takes the one retained latest value.

| Scenario | 1 | 100 | 10,000 |
| --- | ---: | ---: | ---: |
| Identified optional handles | 8.916 us | 874.6 us | 250.2 ms |
| Observations registered | 9.415 us | 847.6 us | 212.1 ms |
| Commits coalesced into one latest value | 21.54 us | 811.7 us | 78.65 ms |

The coalescing row was rerun after moving fixture creation, required-handle
construction, observation registration, and initial-value draining into Divan's
untimed input factory. Its timed batch now contains exactly the requested
commits followed by the final retained-value take. The other rows retain the
original characterisation results.

Dependency rows perform one end-to-end Store commit and one lookup beneath the
requested number of nested feature overlays. Scheduler variation is larger
than the observed depth differences, so these measurements do not support a
claim of constant lookup time.

| Feature-overlay depth | Median | Mean |
| ---: | ---: | ---: |
| 0 | 8.270 us | 9.641 us |
| 1 | 8.103 us | 8.431 us |
| 4 | 7.958 us | 8.658 us |
| 16 | 7.999 us | 8.703 us |

## Allocation and retained-byte method

The allocation build enables the existing `allocation-measurement` feature and
uses Dhat as the global allocator. A fixture is fully constructed before a
testing profiler and before snapshot are created. Each direct executable run
measures only the named operation sequence, then reports the delta in allocation
operations, allocated bytes, current live bytes, and the observed process peak
while returned handles or observations remain alive. It drops measured outputs
after the snapshot.

This is end-to-end protocol allocation evidence, not a proof about an isolated
reducer call. Thread scheduling and deferred actor cleanup can vary counts and
retained bytes. The table is one local characterisation run and is not a stable
allocation budget.

The benchmark was built with:

```bash
PATH=<owner-home>/.rustup/toolchains/1.98.0-aarch64-apple-darwin/bin:$PATH \
RUSTC=<owner-home>/.rustup/toolchains/1.98.0-aarch64-apple-darwin/bin/rustc \
RUSTDOC=<owner-home>/.rustup/toolchains/1.98.0-aarch64-apple-darwin/bin/rustdoc \
DYLD_LIBRARY_PATH=<owner-home>/.rustup/toolchains/1.98.0-aarch64-apple-darwin/lib \
CARGO_BUILD_JOBS=1 CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_DEV_STRIP=false \
CARGO_PROFILE_BENCH_LTO=false CARGO_PROFILE_BENCH_DEBUG=false \
CARGO_PROFILE_BENCH_STRIP=true \
cargo build -p rca-core --bench composition --features allocation-measurement \
  --release --locked
```

Each row was then run as a separate process. For example:

```bash
RCA_SCOPED_ALLOCATION_SCENARIO=scoped_send \
RCA_SCOPED_ALLOCATION_SAMPLES=100 \
target/release/deps/composition-d932f0f27ec8692f
```

| Scenario | Samples | Allocations | Allocated bytes | Retained bytes | Peak live bytes |
| --- | ---: | ---: | ---: | ---: | ---: |
| Required construction | 100 | 301 | 9,600 | 9,600 | 9,600 |
| Optional construction | 1 | 19 | 1,980 | 1,860 | 1,860 |
| Optional construction | 100 | 1,825 | 193,724 | 31,808 | 32,544 |
| Optional construction | 10,000 | 181,605 | 19,994,772 | 3,330,104 | 3,472,296 |
| Keyed construction | 100 | 1,930 | 199,828 | 37,328 | 38,120 |
| `send` | 100 | 1,323 | 175,496 | 1,432 | 2,680 |
| Accepted `try_send` | 100 | 1,346 | 156,096 | 208 | 2,472 |
| Full `try_send` | 100 | 1,000 | 135,200 | 0 | 1,344 |
| Full-mailbox recovery | 1 | 32 | 4,152 | 3,272 | 3,624 |
| `read` | 100 | 1,519 | 180,640 | 1,384 | 2,760 |
| `observe`, equal commit | 100 | 2,866 | 336,584 | 2,864 | 4,224 |
| `observe_distinct`, equal commit | 100 | 1,312 | 153,624 | 1,352 | 2,464 |
| `observe_distinct`, changed commit | 100 | 3,064 | 358,744 | 2,904 | 4,184 |
| Generation invalidation | 1 | 11 | 1,448 | 1,328 | 1,368 |
| Observation construction | 1 | 25 | 2,476 | 652 | 2,236 |
| Observation construction | 100 | 2,351 | 246,628 | 54,456 | 56,104 |
| Observation construction | 10,000 | 232,744 | 24,689,588 | 5,372,600 | 5,373,472 |
| Coalesced commits | 1 | 12 | 1,456 | 1,336 | 1,376 |
| Coalesced commits | 100 | 1,321 | 154,360 | 1,312 | 2,424 |
| Coalesced commits | 10,000 | 133,430 | 15,610,560 | 2,808 | 4,128 |
| Dependency depth 0 | 100 | 1,228 | 154,180 | 1,540 | 2,652 |
| Dependency depth 1 | 100 | 1,250 | 155,752 | 1,352 | 2,464 |
| Dependency depth 4 | 100 | 1,231 | 154,368 | 1,432 | 2,464 |
| Dependency depth 16 | 100 | 1,271 | 158,448 | 1,704 | 3,840 |

The full-mailbox error path captures the canonical error backtrace, so its ten
allocations per rejected action must not be interpreted as mailbox bookkeeping
alone. Retained bytes for construction deliberately include the returned live
handles or observations. Coalescing retains one observation slot instead of one
value per commit; the 10,000-commit row retained 2,808 bytes at its snapshot.

## Binary size and limitations

The original full-characterisation Divan executable produced by the documented
debug-free, stripped, non-LTO profile was 1,960,352 bytes. It was a Mach-O
64-bit arm64 executable with SHA-256
`bb0a41e288933a8e65adec2db935cbcc08eb81168d1f65b2caa12cfb504962d1`.
The corrected coalescing rerun rebuilt a 1,960,352-byte Mach-O arm64 executable
with SHA-256
`429a73718bf4750662fcd377316fb856d3a6096da72b32e1ded93822e5f5fbf8`.
The differing hashes mean the executables are not byte-identical.
The symbol table contained only external/global runtime entries under `nm -g`;
the measurement did not compare a reference executable.

Binary size includes the complete existing composition benchmark, Divan, and
all linked benchmark support. It is not the incremental cost of ScopedStore.
Likewise, a single-host timing cohort cannot establish regression or improvement
against a prior revision. This run omitted the canonical alternating paired
cohorts, CPU-cycle counters, compile-time comparison, and cross-host evidence.
Run `scripts/qualify-performance.sh` after this workload is admitted to the
canonical scenario manifest to make a release-gate claim.
