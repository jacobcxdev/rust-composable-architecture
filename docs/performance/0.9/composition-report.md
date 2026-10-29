# Composition performance qualification

## Decision

Safe composition qualifies on the canonical
`mac15-9-m3-max-16c-48gb-macos26-ac-high` runner at committed revision
`b9eb6e732d2b0cf6ce3cd7255a5958781587af6c`. Required scope, all supported
`Combine` arities, optional and case routing, keyed composition, dependencies,
scoped handles, observations, cancellation, effect mapping, and 32 concurrent
senders completed without a correctness failure. Warmed synchronous direct,
matching required-scope, and empty-effect `Combine` paths allocated zero.

This is a first same-cohort composition baseline, not a claim about other
machines, languages, or candidate improvement. Runtime used the documented
benchmark override (`lto=false`, no debug information, stripped), so it is not
directly comparable with the root fat-LTO release profile or earlier
dirty/profile-overridden 0.9 snapshots.

## Provenance and method

- Measured revision: `b9eb6e732d2b0cf6ce3cd7255a5958781587af6c`,
  with a clean working tree at capture start.
- Runner: `Mac15,9`, Apple M3 Max, 16 logical CPUs, 48 GiB, arm64,
  macOS 26.6.2 (`25G83`), AC power, power mode 2.
- Toolchain: rustc and rustdoc 1.98.0
  (`88d9e12ae178fab0fb5cc050a94da85685d449ea`), Cargo 1.98.0, and
  `cargo-bloat` 0.12.1. The harness resolved all three Rust tools through
  `rustup`, exported their exact paths, disabled both Cargo compiler-wrapper
  variables, and put that toolchain immediately after the pinned task tools on
  `PATH`.
- Runtime: ten independent complete Divan processes from one committed
  executable. Every numbered transcript has exactly 153 timing rows and
  retains fastest, slowest, median, mean, sample count, iteration count, and
  throughput when defined.
- Allocation: one isolated process per scenario, with explicit samples and
  logical-operation counts. Snapshot setup and reporting are outside the
  measured region.
- Compile: one untimed warm-up, then five independent fresh targets and five
  paired mtime-only incremental checks per fixture. All builds used one Cargo
  job and the nested locked workspace. Each verbose transcript proves
  `RUSTC=<toolchain>/rustc` and `RUSTDOC=<toolchain>/rustdoc`; none contains a
  bare compiler variable or Homebrew Rust sysroot.
- Binary: one stripped fat-LTO release build for each fixture and one stripped
  root-profile build for each application artefact. Symbol attribution is a
  separate unstripped build and is diagnostic.
- Monomorphisation counts are unavailable on the pinned stable toolchain; the
  report does not substitute an invented proxy.

Evidence commit `57043f88224e108b452228cacb072eb2ab538dae`
post-processed captured transcripts only by removing terminal padding and
trailing blank lines. Commit
`7724dbd2b36c78e538791e8f172f67153320fd50` made the identical transform part
of future captures. The measured runner remains revision
`b9eb6e732d2b0cf6ce3cd7255a5958781587af6c` with source hash
`5ad4c3093fabc4200e89c4b8742f0445b9d74e5e112349dc45dc8f6d4f42e755`;
the current script hash therefore differs intentionally.

The first capture attempt at
`95722d1afb35c90b6779aa0a133d0ec4dcd6c653` invoked Divan without `--bench`,
so its scenario-only runtime output and entire cohort were rejected. The later
evidence commit `abbb01411653e2640315d15559abe9a4e2e680df` used one correct
Divan process, but review found that it could not support absolute runtime
gates under the repository's process-level variance policy. It is superseded
in Git history and retained in the ignored task evidence. The repeated cohort
measured at `6150b640424a67bcfbd9bc0bf6444fb2957da45c` and published by
`d184a91cf0587285dcba48bab4189d59ae6cae5f` was also superseded: its Cargo
transcripts selected ambient Homebrew compiler executables rather than fixed
rustup paths. No value from any superseded cohort is mixed into this one.

## Runtime

The table reports the median across ten per-process row medians. The numbered
transcripts are authoritative for each within-process distribution:
[01](raw/runtime-divan-01.txt), [02](raw/runtime-divan-02.txt),
[03](raw/runtime-divan-03.txt), [04](raw/runtime-divan-04.txt),
[05](raw/runtime-divan-05.txt), [06](raw/runtime-divan-06.txt),
[07](raw/runtime-divan-07.txt), [08](raw/runtime-divan-08.txt),
[09](raw/runtime-divan-09.txt), and [10](raw/runtime-divan-10.txt).

| Scenario | Median of process medians | Process-median range |
| --- | ---: | ---: |
| Direct, depths 1 / 4 / 16 | 8.4470 / 8.1140 / 8.0095 us | 6.937-9.228 / 7.583-8.374 / 7.228-8.437 us |
| Matching required scope, depths 1 / 4 / 16 | 8.2595 / 8.3635 / 9.8015 us | 7.145-8.562 / 7.645-8.957 / 8.187-10.200 us |
| Nonmatching scope, depths 1 / 4 / 16 | 8.0830 / 7.9265 / 9.8015 us | 6.708-8.291 / 7.395-8.499 / 7.999-10.060 us |
| `Combine`, arities 1 / 4 / 8 / 12 | 8.1345 / 8.0820 / 7.9990 / 8.0410 us | 6.645-8.729 / 6.478-8.270 / 6.541-8.186 / 6.207-9.041 us |
| Optional hit / miss / enter / leave | 8.0305 / 8.0825 / 11.180 / 11.370 us | 6.957-8.895 / 6.833-8.811 / 8.771-11.680 / 8.312-12.470 us |
| Case hit / miss / switch | 8.4055 / 8.0100 / 11.380 us | 6.812-9.270 / 6.832-8.812 / 8.479-11.890 us |
| Dependency depth 0 / 1 / 4 / 16 | 7.9475 / 8.0095 / 7.8635 / 7.9785 us | 6.791-8.124 / 6.624-8.457 / 6.604-8.957 / 6.687-9.104 us |
| Keyed matching, 10 / 1,000 / 100,000 entries | 8.6870 / 8.0930 / 8.1975 us | 6.978-9.562 / 7.082-8.749 / 7.020-9.082 us |
| Keyed unrelated, 10 / 1,000 / 100,000 entries | 8.1345 / 8.0095 / 8.6655 us | 6.833-8.582 / 6.666-8.645 / 7.979-11.160 us |
| Keyed initial mount, 10 / 1,000 / 100,000 | 15.515 us / 324.800 us / 34.690 ms | 13.950-21.830 us / 313.5-367.8 us / 33.02-35.49 ms |
| One keyed removal, 10 / 1,000 / 100,000 | 12.600 us / 31.225 us / 2.980 ms | 8.395-15.410 us / 25.990-34.430 us / 2.623-3.330 ms |
| Descendant cancellation, 1 / 100 / 10,000 | 11.025 / 42.710 / 231.300 us | 8.166-11.990 / 29.850-44.490 / 221.6-273.0 us |
| Effect map, depths 1 / 4 / 16, one output | 20.975 / 19.905 / 21.670 us | 19.240-24.140 / 14.740-21.620 / 19.330-23.740 us |
| Effect map, 1 / 16 / 256 outputs | 17.690 / 49.985 / 542.450 us | 13.950-26.430 / 45.540-53.830 / 491.9-562.4 us |
| 32 scoped concurrent senders | 256.400 us; 124.8 K actions/s | 216.2-282.9 us; 113.1-148.0 K actions/s |

Scope depth 16 adds about 1.44 us over depth 4 for a matching route. `Combine`
does not trend upward across representative arities 1 through 12, and
dependency overlays are similarly flat through depth 16. Keyed matching stays
effectively independent of collection size, while initial mount and removal
scale with changed lineage. The concurrent workload verifies 32 accepted
actions, serial final state, joined sender tasks, and clean Store shutdown
outside its timed region.

Concurrent throughput divides 32 accepted actions by each process median and
rounds to the nearest 100 actions/s before cohort aggregation.

All selected machine-gated runtime rows satisfy the process-variance and inner
sample policies in this cohort. Stable gates therefore cover direct reduction,
matching and nonmatching Scope, `Combine`, keyed routing, effect mapping,
descendant cancellation, and 32 concurrent senders. Broader descriptive rows
remain visible with their full ranges; no favourable single process is promoted
over the retained distribution.

## Allocations

Exact zero gates passed for warmed direct reduction, matching required scope,
and static `Combine` tuples at all supported arities 1 through 12. They use
`opt-level=1` and `debug=0`; the default-profile test link was not retried after
the previously documented host SIGKILLs.

The isolated snapshots in [`raw/allocations.txt`](raw/allocations.txt) contain
47 rows. Representative totals are:

| Scenario | Logical operations | Allocations | Allocated bytes | Retained bytes | Peak live bytes |
| --- | ---: | ---: | ---: | ---: | ---: |
| Effect map depth 1, one output | 100 | 4,244 | 437,512 | 1,608 | 5,896 |
| Effect map depth 16, one output | 100 | 5,747 | 474,192 | 1,608 | 5,040 |
| Effect map depth 4, 256 outputs | 2,560 | 56,363 | 6,657,000 | 1,608 | 415,168 |
| Keyed initial lineage, 100,000 | 100,000 | 200,081 | 53,049,396 | 29,324,668 | 29,885,836 |
| Keyed matching route, 100,000 | 50 | 609 | 76,608 | 1,256 | 2,368 |
| Remove 1,000 active entries from 100,000 | 1,000 | 1,029 | 86,644 | 1,328 | 36,904 |
| Descendant task creation, 10,000 | 10,000 | 108,155 | 10,734,692 | 4,363,112 | 5,980,808 |
| Descendant cancellation, 10,000 | 10,000 | 236 | 130,988 | 122,672 | 124,128 |
| 32 scoped concurrent senders | 32 | 458 | 57,352 | 1,232 | 39,664 |

Allocation count and allocated-byte budgets use 10% headroom. Retained and
peak-live values remain diagnostic because one snapshot does not establish
scheduling-sensitive lifetime variance. Zero gates remain exactly zero.

## Compile time

| Fixture | Clean seconds, median (range) | Incremental seconds, median (range) | Maximum clean RSS | Maximum clean peak footprint |
| --- | ---: | ---: | ---: | ---: |
| Direct | 6.02 (5.92-6.43) | 0.10 (0.09-0.11) | 298,631,168 B | 15,073,832 B |
| Composed manual | 6.44 (6.04-6.91) | 0.11 (0.10-0.11) | 298,795,008 B | 14,975,528 B |
| Composed macro | 9.31 (9.05-10.42) | 0.12 (0.11-0.13) | 308,805,632 B | 16,400,912 B |
| Scoped | 6.15 (6.00-6.68) | 0.10 (0.10-0.11) | 298,860,544 B | 14,942,784 B |

Handwritten composition adds 0.42 seconds (7.0%) to the direct clean median.
The macro fixture adds 3.29 seconds (54.7%) to direct because the isolated graph
compiles the proc-macro dependency stack. Its incremental median adds 0.02
seconds over direct, but 0.01-second reporting resolution makes that comparison
diagnostic. Verbose logs retain and validate exact rustc and rustdoc paths.

## Binary size and symbols

Qualification correction (2026-09-29): the historical fixture and Tokio build
logs contain `rust-objcopy` stripping failures caused by a missing LLVM library
search path. Cargo still returned success. The sizes below therefore do not
prove stripped output. The corrected reference measurements below supersede
only the six binary-size baselines. Historical raw measurements remain
preserved; candidate measurements do not replace the reference baseline.

| Binary | Historical bytes (stripping unverified) | SHA-256 |
| --- | ---: | --- |
| Direct fixture | 614,720 | `f20ba0014f867303668d0089c69a46a89aa314cdbbaa257d35fb5a3335856e0b` |
| Composed manual fixture | 682,224 | `8a59bac40b0c093d08865c88719abb8cc777b3a63db5ae78be2a73fdc5b4f228` |
| Composed macro fixture | 681,848 | `a0e26c941e553ed507719e005b53c75c28c52b2f03967dff598f15ad630821df` |
| Scoped fixture | 657,312 | `c30d5f2c96ab7465b0fba8551100e50901a3009c249dc5dbf98673d75af46303` |
| Deterministic composition test | 1,549,512 | `6c1caba46f4cba73dc43ffebd47052e55a2599a5f07187b8f11adf6046891937` |
| Tokio application | 1,083,696 | `e24c48b6f896a0ff15b94b9ea741d683ecb8f5ef0eed4cae6cbe3ede557e33f9` |

Manual and macro fixtures run the same graph with the same `"composition"`
input and no `Clone` action requirement. Their historical byte counts are
within 376 bytes (0.06%); distinct hashes and the small size difference follow
from package and symbol identity. In the separate unstripped attribution build,
`rca_core` is 84.4 KiB for both and the fixture crate is 10.9 KiB manual versus
10.8 KiB macro. `cargo-bloat` calls its attribution guesswork, so those values
are diagnostic.

All four fixture binaries deliberately include the same support exercise and
runtime cost. Attribution shows 488 B for the support crate and 9.9 KiB for
`futures_executor` in each. The comparison does not subtract that common cost.
The deterministic test and Tokio application use the root release profile and
remain separate from fixture sizes.

### Corrected binary-only reference (2026-09-29)

The clean original reference `b9eb6e732d2b0cf6ce3cd7255a5958781587af6c`
was rebuilt on macOS 27 with the same pinned Rust 1.98.0 toolchain and fixture
and root release profiles. Each executable then passed an explicit pinned
`rust-objcopy --strip-all` invocation before its size and hash were recorded.
This separate binary-only cohort does not relabel the historical macOS 26
runtime, allocation, or compile-time measurements.
The retained [reference build log](raw/binary-corrected-reference-verification.txt),
[driver](raw/binary-corrected-reference-verify.sh), and
[size receipt](raw/binary-corrected-reference-sizes.txt) identify the clean
revision, toolchain, OS, workload hashes, and successful stripping steps.

| Binary | Corrected reference bytes | Candidate bytes | Reference plus 10% |
| --- | ---: | ---: | ---: |
| Direct fixture | 452,544 | 452,528 | 497,799 |
| Composed manual fixture | 502,224 | 502,208 | 552,447 |
| Composed macro fixture | 502,224 | 502,208 | 552,447 |
| Scoped fixture | 485,632 | 485,616 | 534,196 |
| Deterministic composition test | 1,133,776 | 1,133,792 | 1,247,154 |
| Tokio application | 767,408 | 750,880 | 844,149 |

Candidate measurements use preserved outputs from the clean
`a20ca75971ba7746d847c9a598c2831cd9f62289` build. Their input hashes were
checked against the original receipt, then copies were explicitly stripped;
the original binaries were preserved. All six sizes fall within corrected
reference headroom. These results qualify binary size only, not timing,
compile-time performance, or the full Task 5 comparison cohort.
The [candidate stripping receipt](raw/binary-correction-20260929/candidate-stripped.txt)
and [driver](raw/binary-correction-20260929/strip-candidate.sh) retain input
hashes and the explicit stripping step.

## Budgets and comparison policy

[`composition-budgets.json`](composition-budgets.json) is the machine-readable
baseline. Every row carries its scenario, statistic, units, sample policy,
baseline, tolerance, absolute budget, runner, toolchain, measured revision,
workload path/hash, and diagnostic decision.

Runtime tolerance is the larger of 10% and the upper excursion from the median
of ten process medians to the maximum process median, rounded up to five
percentage points. Allocation count/bytes and binary size use 10%. Clean
compile time uses the same upper-excursion rule over five independent samples.
Incremental compile time, compile memory, retained/peak-live allocation bytes,
symbol attribution, low-sample runtime rows, timer-resolution rows, and runtime
rows needing more than 50% headroom remain diagnostic. None of the selected
runtime budget rows triggered those exclusions in this cohort.

These absolute values bootstrap the first composition baseline on this exact
runner, toolchain, profile, workload hashes, and protocol. They do not establish
a candidate-versus-reference improvement. Task 5 must perform candidate
acceptance with ten paired, order-balanced processes per revision and the
repository's canonical paired comparison policy.

## Optimisation audit

- **Effect-map fusion:** rejected for now. Output count dominates measured work,
  while map depth 1-16 has no monotonic latency or peak-live trend.
- **Mutation journal:** rejected for now. Initial lineage and bulk removal
  scale, but matching routes remain collection-size independent. A journal adds
  rollback and cancellation complexity without an accepted limiting product
  workload.
- **Lower tuple-arity specialisation:** rejected. Static arities 1-12 are
  allocation-free and show no arity-related aggregate growth.
- **Explicit composition-erasure adapter:** deferred to separate API design,
  not an optimisation. The macro cost is clean compilation, not runtime or
  stripped binary size; erasure would trade static optimisation and diagnostics
  for dynamic dispatch without a measured consumer need.
- **Unsafe inline storage:** rejected categorically. No measurement justifies
  weakening safe ownership or concurrency.

No runtime implementation change is recommended from this cohort.

## Raw evidence

The accepted directory contains environment and exact command metadata, ten
complete numbered Divan transcripts, 47 isolated allocation rows, both
zero-allocation logs, five clean/incremental compile pairs and verbose rustc
arguments for four fixtures, six stripped binary identities, build logs, and
six crate-attribution reports. Paths are sanitised to `<repo>`, `<tmp>`, and
`<toolchain>` before `<home>`. Terminal progress padding and trailing blank
lines are removed without changing measured fields. No username, home path,
hostname, temporary host path, or Homebrew Rust sysroot is published.
