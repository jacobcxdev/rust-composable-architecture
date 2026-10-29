# Phase 1 composition-path measurements

Historical excerpts use `<owner-home>` for the original user's home directory.
They abbreviate local machine paths and are not current copy-and-paste commands.
Raw receipts are retained unchanged.

These results qualify the safe Phase 1 implementation at core revision
`cc1d14e88c3209797d9e19e848cacf5bea6a2717`. The measurement worktree also
contained uncommitted Task 5 conformance changes in `rca-test` and `rca-tokio`;
neither crate is in the `rca-core` benchmark dependency graph.

The figures are measured evidence, not API guarantees, regression budgets, or
cross-host performance promises.

## Environment

- Host architecture: `arm64` (`aarch64-apple-darwin`).
- Operating system: macOS 26.6.2, build 25G83; Darwin 25.6.0.
- Compiler: `rustc 1.98.0 (88d9e12ae 2026-08-18)`.
- Compiler commit: `88d9e12ae178fab0fb5cc050a94da85685d449ea`.
- LLVM: 22.1.8.
- Cargo jobs: 1.
- Bench profile: workspace `release` profile with `opt-level=3`,
  `codegen-units=1`, `incremental=false`, debug information disabled, stripping
  requested, and `lto=false` overriding the normal fat-LTO setting.

The exact compiler description was captured with:

```text
rustc 1.98.0 (88d9e12ae 2026-08-18)
binary: rustc
commit-hash: 88d9e12ae178fab0fb5cc050a94da85685d449ea
commit-date: 2026-08-18
host: aarch64-apple-darwin
release: 1.98.0
LLVM version: 22.1.8
```

## Timing method

The Divan fixture warms one Store and performs end-to-end, effect-free action
commits. `direct` measures the leaf reducer without Scope. A matching action
traverses every requested Scope. A nonmatching action misses at the deepest
Scope. The results therefore include mailbox, actor scheduling, receipt, and
task-completion costs; they do not isolate nanosecond combinator overhead.

The exact command was:

```bash
RUSTC=<owner-home>/.rustup/toolchains/1.98.0-aarch64-apple-darwin/bin/rustc \
RUSTDOC=<owner-home>/.rustup/toolchains/1.98.0-aarch64-apple-darwin/bin/rustdoc \
CARGO_BUILD_JOBS=1 \
CARGO_PROFILE_BENCH_LTO=false \
CARGO_PROFILE_BENCH_DEBUG=false \
CARGO_PROFILE_BENCH_STRIP=true \
<owner-home>/.rustup/toolchains/1.98.0-aarch64-apple-darwin/bin/cargo \
bench -p rca-core --bench composition
```

Each Store-path case used 50 samples and 50 measured iterations. Divan reported
a 41 ns timer precision. The observed fastest-to-slowest range is included as
the available dispersion evidence. Raw output is stored in
[`composition-paths-divan.txt`](composition-paths-divan.txt).

| Path | Depth | Fastest | Slowest | Median | Mean |
| --- | ---: | ---: | ---: | ---: | ---: |
| Direct | 1 | 3.707 µs | 28.70 µs | 5.791 µs | 6.928 µs |
| Direct | 4 | 4.166 µs | 15.91 µs | 7.187 µs | 7.284 µs |
| Direct | 16 | 3.874 µs | 12.66 µs | 6.666 µs | 6.971 µs |
| Matching Scope | 1 | 3.499 µs | 12.58 µs | 6.687 µs | 6.650 µs |
| Matching Scope | 4 | 4.124 µs | 9.791 µs | 6.083 µs | 6.453 µs |
| Matching Scope | 16 | 3.457 µs | 11.49 µs | 6.353 µs | 6.422 µs |
| Nonmatching Scope | 1 | 3.457 µs | 10.79 µs | 6.145 µs | 6.424 µs |
| Nonmatching Scope | 4 | 4.166 µs | 10.74 µs | 6.270 µs | 6.687 µs |
| Nonmatching Scope | 16 | 3.999 µs | 30.99 µs | 7.145 µs | 7.662 µs |

Scheduler variance and outliers make the depth ordering non-monotonic. No
per-Scope delta should be inferred from this first end-to-end baseline.

## Allocation method

The Dhat fixture warms matching and deepest-miss paths, starts an isolated test
profiler, snapshots cumulative block and byte counters around each reduction,
and compares the before and after values. It exercises the direct reducer API so
Store setup, mailbox, and receipt allocations cannot obscure the synchronous
Scope path.

The exact command was:

```bash
RUSTC=<owner-home>/.rustup/toolchains/1.98.0-aarch64-apple-darwin/bin/rustc \
RUSTDOC=<owner-home>/.rustup/toolchains/1.98.0-aarch64-apple-darwin/bin/rustdoc \
CARGO_BUILD_JOBS=1 \
CARGO_PROFILE_DEV_DEBUG=false \
CARGO_PROFILE_DEV_STRIP=false \
<owner-home>/.rustup/toolchains/1.98.0-aarch64-apple-darwin/bin/cargo \
test -p rca-core --lib --features allocation-measurement \
scope_none_paths_do_not_allocate_after_warm_up -- --nocapture
```

Raw test output is stored in
[`composition-paths-dhat.txt`](composition-paths-dhat.txt).

| Path | Depth | Dhat blocks | Dhat bytes |
| --- | ---: | ---: | ---: |
| Matching Scope | 1 | 0 | 0 |
| Matching Scope | 4 | 0 | 0 |
| Matching Scope | 16 | 0 | 0 |
| Nonmatching Scope | 1 | 0 | 0 |
| Nonmatching Scope | 4 | 0 | 0 |
| Nonmatching Scope | 16 | 0 | 0 |

These counts apply only after warm-up when the child returns `Effect::none()`.
Effectful mapping owns a shared mapper and is not covered by the zero-allocation
claim.

## Binary size and caveats

An isolated detached worktree at exact revision
`cc1d14e88c3209797d9e19e848cacf5bea6a2717` produced a 1,144,528-byte stripped
benchmark executable under the documented one-job, debug-free, non-LTO
profile. Its SHA-256 digest is
`23c86bc888b0b83e8b87608a7b9ece97227d3460fc1b039568bad0ec9e809f3c`.

The Rust 1.98 `rust-objcopy` links `@rpath/libLLVM.dylib`, but its installed
relative runtime path does not resolve the toolchain-level library. The command
therefore sets `DYLD_LIBRARY_PATH` to the Rust 1.98 toolchain's `lib` directory.
The exact binary build command was:

```bash
DYLD_LIBRARY_PATH=<owner-home>/.rustup/toolchains/1.98.0-aarch64-apple-darwin/lib \
RUSTC=<owner-home>/.rustup/toolchains/1.98.0-aarch64-apple-darwin/bin/rustc \
RUSTDOC=<owner-home>/.rustup/toolchains/1.98.0-aarch64-apple-darwin/bin/rustdoc \
CARGO_BUILD_JOBS=1 \
CARGO_PROFILE_BENCH_LTO=false \
CARGO_PROFILE_BENCH_DEBUG=false \
CARGO_PROFILE_BENCH_STRIP=true \
<owner-home>/.rustup/toolchains/1.98.0-aarch64-apple-darwin/bin/cargo \
bench -p rca-core --bench composition --no-run
```

The archived transcript records the revision, clean detached state, compiler,
`rust-objcopy` and `libLLVM.dylib` identities, successful build without a strip
warning, executable path, byte count, digest, and stripped symbol table in
[`composition-binary-size.txt`](composition-binary-size.txt).

Task 4's 1,144,560-byte value remains an unqualified historical observation;
it is not used as exact-revision evidence. Binary byte counts can vary with the
temporary worktree path and other linker inputs even when source, dependencies,
toolchain, target, and profile match.

The normal workspace fat-LTO benchmark link exceeded available host memory
during Task 4. Disabling LTO changed only the measurement build profile; source
coverage remained enforced by the full semantic test, documentation, and lint
gates. Binary sizes and timings can change with compiler, dependency, target,
linker, profile, and host load.
