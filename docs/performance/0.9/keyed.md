# Phase 3 keyed-composition characterisation

This evidence records the first safe keyed-composition baseline. It is local
characterisation, not canonical cross-revision qualification. The host matched
the documented hardware cohort, but the candidate checkout was intentionally
dirty and no alternating paired reference cohort was run. The canonical method
also requires ten normal and six Dhat paired processes, CPU and memory counters,
compile and binary-size evidence, and a clean candidate revision.

The measured source was based on
`708c687911ad17139005ec746ed6e2c126bc62a9`. The tracked workload diff had
SHA-256
`397121c73de05ed4b69da0d135108f52eefd126616b327919d1fe6471aea769f`;
the benchmark source itself had SHA-256
`629f097fae2a6ef6f45ca2b4401fcf2c9010063f4eb0d9470c957f53faa99b25`.
This identifies the local workload without presenting the dirty base revision
as the measured final commit.

## Environment and profile

- Host: `Mac15,9`, Apple M3 Max, 16 logical CPUs, 48 GiB memory, `arm64`.
- Power: AC attached; `pmset` power mode `2`.
- OS: macOS 26.6.2 build 25G83; Darwin 25.6.0.
- Rust: `rustc 1.98.0 (88d9e12ae 2026-08-18)`, compiler commit
  `88d9e12ae178fab0fb5cc050a94da85685d449ea`, LLVM 22.1.8,
  `aarch64-apple-darwin`.
- Cargo: `cargo 1.98.0 (797e8a9bc 2026-08-05)` with
  `CARGO_BUILD_JOBS=1` and `--locked`.
- Benchmark profile: workspace bench/release settings with `opt-level=3`,
  `codegen-units=1`, and `incremental=false`; this local run overrode LTO to
  `false`, debug information to `false`, and stripping to `true` to retain the
  documented low-memory profile.
- Executor: one four-worker `futures-executor` pool per Store fixture.

The exact command and complete latency/throughput transcript are in
[`keyed-divan.txt`](keyed-divan.txt). Serial allocation and clone-count output
is in [`keyed-allocation-clones.txt`](keyed-allocation-clones.txt).

## Workloads and sample policy

The collection workloads compare identical visible ordered-collection
operations. Lookup targets the middle ID. Insert uses one reserved slot and a
new terminal ID. Ordered and swap removal target the middle ID. Move shifts the
last entry to the first position. Clear removes every entry. The benchmark-only
controls are direct `IndexMap`, ordered `Vec` plus `HashMap<ID, index>`, and
ordered `Vec` plus `BTreeMap<ID, index>` representations. None enters RCA's
public API.

Each mutating collection sample receives a fresh pre-populated collection from
`with_inputs`; construction happens before the sample. The consuming closure
returns the collection as its benchmark output, so Divan defers its destruction
until after `sample_end`. `skip_ext_time=true` excludes that external work only
from minimum/maximum time accounting; it does not exclude closure-local work.
Mutations use ten requested samples, `sample_size=4`, and a 0.5-second maximum.
Lookup reuses one immutable collection and requests 50 samples under the same
maximum.

Runtime fixtures are warmed before measurement. Matching route targets the
middle child. Unrelated reconciliation sends a parent-only action. One-removal
cancellation mounts one pending child task before timing its removal. The
1,000-removal workload is one parent transaction retaining away the first 1,000
of 1,000 or 100,000 active keyed entries; those entries have no attached work.
Two-output mapping times one child action whose effect emits two parent outputs.
Lifecycle-changing fixtures are constructed before the timed region and
returned as benchmark outputs, which defers cleanup until after `sample_end`.
Reusable paths request 50 samples. Fresh cancellation and bulk-removal paths
request ten samples with `sample_size=1`, `skip_ext_time=true`, and a 0.5-second
maximum. The 100,000-entry bulk-removal row completed three samples before that
time bound. Its latency is a whole parent transaction; its reported item
throughput counts the 1,000 departed entries.

This bounded policy preserves useful steady-state medians without allowing the
known high-scale invalidation path or large fixture setup to dominate wall time.
Divan reported 41 ns timer precision. Four mutations per collection sample avoid
zero-valued medians, but the fastest size-10 and size-1,000 rows remain visibly
quantised and should not drive representation choices.

Independent review found that the first captured cohort returned `()` from
consuming benchmark closures. That placed fixture and collection destructors
inside the timed sample. The first cohort is superseded: the tracked raw file,
tables below, and source hashes describe the review-corrected cohort only.

## Runtime observations

These medians include mailbox admission, actor scheduling, reduction, receipt,
and returned task completion. They do not isolate the combinator body.

| Scenario | 10 entries | 1,000 entries | 100,000 entries |
| --- | ---: | ---: | ---: |
| Matching route | 11.45 µs | 32.72 µs | 3.210 ms |
| Unrelated root reconciliation | 9.812 µs | 34.85 µs | 3.235 ms |
| Remove one child with pending work | 12.64 µs | 46.97 µs | 4.141 ms |
| Map two child outputs | 28.08 µs | 91.27 µs | 9.571 ms |
| Remove 1,000 active task-free entries | — | 907.4 µs | 167.2 ms |

Matching and unrelated rows expose the expected O(n) `ForEach` scan: both are
about 3 ms at 100,000 entries and track each other across sizes. The much larger
167.2 ms bulk-removal row is not explained by collection removal or one O(n)
reconciliation scan.

## Ordered-collection controls

The table reports Divan medians. Each row is one operation on a collection of
the named initial size.

| Representation / operation | 10 | 1,000 | 100,000 |
| --- | ---: | ---: | ---: |
| `KeyedState` lookup | 8.428 ns | 8.265 ns | 8.670 ns |
| `KeyedState` reserved insert | 36.22 ns | 30.97 ns | 156.0 ns |
| `KeyedState` ordered remove | 30.97 ns | 968.4 ns | 255.1 µs |
| `KeyedState` swap remove | 41.47 ns | 41.34 ns | 468.4 ns |
| `KeyedState` move last to first | 41.47 ns | 2.739 µs | 216.4 µs |
| `KeyedState` clear | 10.22 ns | 30.97 ns | 1.822 µs |
| `IndexMap` lookup | 7.533 ns | 7.472 ns | 7.492 ns |
| `IndexMap` reserved insert | 30.97 ns | 30.97 ns | 88.22 ns |
| `IndexMap` ordered remove | 41.22 ns | 833.0 ns | 241.2 µs |
| `IndexMap` swap remove | 30.97 ns | 41.22 ns | 369.5 ns |
| `IndexMap` move last to first | 36.09 ns | 2.765 µs | 193.1 µs |
| `IndexMap` clear | 10.22 ns | 30.97 ns | 1.796 µs |
| `Vec` + `HashMap` lookup | 7.857 ns | 7.615 ns | 7.655 ns |
| `Vec` + `HashMap` reserved insert | 25.84 ns | 30.97 ns | 202.9 ns |
| `Vec` + `HashMap` ordered remove | 62.22 ns | 3.468 µs | 753.1 µs |
| `Vec` + `HashMap` swap remove | 46.59 ns | 51.97 ns | 447.7 ns |
| `Vec` + `HashMap` move last to first | 72.72 ns | 6.989 µs | 1.188 ms |
| `Vec` + `HashMap` clear | 10.22 ns | 30.97 ns | 2.030 µs |
| `Vec` + `BTreeMap` lookup | 2.488 ns | 8.632 ns | 12.49 ns |
| `Vec` + `BTreeMap` reserved insert | 10.22 ns | 20.47 ns | 692.4 ns |
| `Vec` + `BTreeMap` ordered remove | 41.47 ns | 5.145 µs | 1.752 ms |
| `Vec` + `BTreeMap` swap remove | 20.59 ns | 46.59 ns | 1.103 µs |
| `Vec` + `BTreeMap` move last to first | 51.72 ns | 10.14 µs | 3.430 ms |
| `Vec` + `BTreeMap` clear | 30.97 ns | 2.775 µs | 286.0 µs |

Across the 18 comparable operation/size medians, geometric-mean ratios against
`KeyedState` are 0.9050 for direct `IndexMap`, 1.4202 for `Vec` plus `HashMap`,
and 2.7361 for `Vec` plus `BTreeMap`. The direct `IndexMap` control
deliberately omits keyed lineage and revision semantics; it measures the private
backing cost rather than a valid replacement. Neither explicit parallel-index
control improves the complete workload. The safe private `IndexMap` baseline
therefore remains appropriate.

## Allocation and clone observations

Dhat processes ran alone with one test thread. The warmed matching and unrelated
`ForEach` reductions at 10, 1,000, and 100,000 entries each measured zero
allocation-operation and zero allocated-byte deltas. Borrowed lookup, reserved
insert, ordered remove, swap remove, move, and clear also measured zero deltas at
all three sizes when construction and teardown were excluded. These are narrow
warm/capacity-reuse observations: initial collection construction, lineage
allocation, first scope mounting, Store work, and effectful scheduling can
allocate and are not covered by the zero-allocation assertion.

After warm mounting, an effect that emits exactly two child outputs increased
the ID clone counter by exactly two. Warm matching and unrelated empty-effect
actions do not clone IDs. Each owned parent output therefore pays one compact-ID
clone, matching the public contract.

## Registry scaling and decision

This section records historical source and decision state at the Phase 3
baseline. Later reviewed work replaced site-wide registry scans with a
collision-safe index, replaced sibling-vector retention with an ordered linked
tree, compacted scope tokens, and added private keyed structural revisions.
Concrete identity equality remains mount-reuse authority; invalidation remains
iterative and deterministic.

The superseding three-cohort evidence is in
[`keyed-scaling.md`](keyed-scaling.md). At 100,000 entries, matching and
unrelated reconciliation changed from O(n) scans to O(1) unchanged-stamp checks
while structural changes retain a full O(n) refresh. The complete candidate
also preserved warmed zero-allocation gates and measured 24.404762% lower
direct-process RSS than its paired Task 1 baseline. These remain local results,
not canonical release qualification.

## Exact hash command and limitations

The workload digest excludes evidence documents and has no self-reference:

```bash
git diff --binary --no-ext-diff \
  708c687911ad17139005ec746ed6e2c126bc62a9 -- \
  Cargo.toml Cargo.lock \
  rca-core/benches/composition.rs \
  rca-core/src/composition/keyed.rs \
  rca-core/tests/keyed.rs \
| shasum -a 256
```

The result was
`397121c73de05ed4b69da0d135108f52eefd126616b327919d1fe6471aea769f`.
Using the review-correction commit as the second revision reproduces the same
byte stream. `Cargo.toml` and `Cargo.lock` were unchanged but remain explicit
protocol inputs.

This local characterisation cannot be passed to
`scripts/compare-performance.sh` and establishes no release qualification. A
future canonical workload amendment must add keyed rows to the scenario
manifest and capture clean alternating reference/candidate cohorts before the
10% paired gate can make a release decision.
