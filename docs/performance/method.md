# Performance qualification

RCA performance evidence is a release gate, not an informal benchmark snapshot. The canonical suite records latency, throughput, allocation, scheduler work, CPU, memory, compile time, and binary size from one pinned runner. Semantic and concurrency conformance must already pass before performance results are accepted.

## Canonical runner

The `mac15-9-m3-max-16c-48gb-macos26-ac-high` runner is:

- `Mac15,9`, Apple M3 Max, 16 logical CPUs, and 48 GiB memory;
- macOS 26 on AC power with high-power mode selected; and
- exact Rust and Cargo 1.98.0.

`scripts/qualify-performance.sh` validates this cohort, a clean checkout, release-profile settings, and absence of another RCA benchmark process. Results from a different runner are not comparable. New runner cohorts require a separately reviewed baseline instead of relabelling existing evidence.

## Commands

From a clean checkout:

```bash
./scripts/qualify-performance.sh \
  target/qualification-evidence/0.8.0-alpha.1.json \
  /tmp/rca-performance-reference \
  target/qualification-evidence/baseline.json
./scripts/compare-performance.sh \
  target/qualification-evidence/baseline.json \
  target/qualification-evidence/0.8.0-alpha.1.json
```

After review, replace canonical JSON, comparison, Divan log, and profile report together. In the canonical candidate JSON, normalise the Divan log and profile-report fields to their tracked `docs/performance` paths; measured rows and cohort metadata remain byte-for-byte unchanged. Qualification stages all four artefacts and refuses existing or aliased output paths. Failure removes only artefacts published by that invocation.

Reference worktree must be clean and checked out at revision recorded by `docs/performance/baseline-revision.txt`. Its ignored `Cargo.lock` must byte-match `docs/performance/baseline-Cargo.lock`; candidate uses tracked root `Cargo.lock`. Every Cargo invocation uses `--locked`. Qualification also requires exact ordered scenario manifest in `docs/performance/scenarios.txt` and rejects changed candidate/reference workload sources before measurement. Protocol identity hashes benchmark source, scenario manifest, and qualification script together, so sample or capture-policy changes invalidate stale evidence.

Qualification runner builds four distinct bench executables, two from each revision, and balances execution order within every scenario:

- normal system allocator for latency, throughput, CPU, scheduler, and resident-memory evidence, repeated ten times per revision; and
- `qualification-dhat` build for allocation counts, allocated bytes, and peak live heap, repeated six times per revision.

Odd repetitions run reference then candidate; even repetitions run candidate then reference. Each scenario retains raw paired processes. For each metric, gate computes median pair ratio separately for each order, then combines two order strata with geometric mean. This symmetric log-scale estimator keeps pair coupling while cancelling systematic first/second-process bias. Aggregate row medians remain reporting fields only. Keeping DHAT out of normal executables prevents allocator instrumentation from biasing latency. All builds forbid unsafe code in RCA's qualification harness.

## Stable result schema

Schema version 4 rejects single-process, unbalanced, stale, or unpaired evidence. Both files record identical cohort identity, environment, protocol hash, lockfile hashes, reference/candidate revision pair, raw process pairs, and fixed repetition policy.

Each row contains these required fields:

```json
{
  "scenario": "external_commit/multi/4_producers/saturated",
  "revision": "full-git-sha",
  "runner": "stable-runner-id",
  "samples": 10000,
  "normal_repetitions": 10,
  "allocation_repetitions": 6,
  "throughput_per_second": 1.0,
  "latency_ns": {"p50": 1, "p95": 1, "p99": 1},
  "cpu_cycles": 1,
  "allocations": 1,
  "allocation_operations": 1,
  "allocated_bytes": 1,
  "task_wakes": 1,
  "peak_bytes": 1,
  "pairs": {
    "normal": [{"repetition": 1, "order": "reference-candidate", "reference": {}, "candidate": {}}],
    "allocation": [{"repetition": 1, "order": "reference-candidate", "reference": {}, "candidate": {}}]
  }
}
```

All values shown as `1` are placeholders; captured values must be non-zero. Pair arrays above are abbreviated; actual rows require ten normal and six allocation pairs. Rows also retain operation count, retired instructions, maximum resident set size, peak memory footprint, and optional reviewed regression rationale.

Top-level metadata records cohort ID, compiler and Cargo versions, compiler commit, OS, non-sensitive runner hardware, power mode, protocol, qualification-source, qualification-script, scenario-manifest and lockfile hashes, commands, compile evidence, binary sizes, idle Store RSS, Divan log hash, canonical supporting-artefact paths, and local raw-profile paths. Raw Instruments traces stay under ignored `target/qualification-profiles`: trace packages contain host identifiers and must be redacted before publication.

## Measurements

- Latency uses `Instant` around each completed operation after up to 20 warm-up operations. For `n` sorted samples and percentile `p`, the runner selects zero-based index `ceil((n - 1) * p / 100)`. This upper indexed percentile is not the nearest-rank method. p50, p95, and p99 describe whole-operation latency. Every normal scenario uses 10,000 outer samples so each process has 100 observations at or beyond p99. Aggregate fields use medians of ten process-level results; acceptance uses geometric mean of two order-stratified medians from ten paired candidate/reference ratios.
- Throughput divides completed logical operations by measured sample-loop time. Producer and fan-out scenarios report all inner operations, not only outer iterations.
- CPU cycles and retired instructions come from macOS `/usr/bin/time -lp` for the complete scenario process. These counters aggregate runtime threads.
- Allocations and allocated bytes come from six separate DHAT processes. Aggregate allocation counts, allocated bytes, and peak live heap are their medians; allocation acceptance uses geometric mean of two three-pair order-stratified medians. Odd stratum sizes preserve reciprocal symmetry. DHAT uses a separate bounded sample policy recorded by `allocation_samples`; deterministic budget exhaustion uses five samples per repetition because each one already performs 100,000 non-allocating self-wake polls.
- `task_wakes` is the historical schema name for measured future poll activations. Tokio scenarios include the top-level workload future and RCA runtime futures instrumented by `tokio-metrics`; deterministic scenarios report driver poll work. Counting the workload future preserves real zero-work cancellation cases without inventing a non-zero floor.
- Maximum resident set and peak memory footprint come from normal processes and are strict paired gates. Idle RSS is sampled after a Store reaches its ready state. DHAT peak live heap remains diagnostic: concurrent scheduling changes allocation lifetimes enough to make it unsuitable as a cross-process 10% release gate, while total allocation count/bytes and process memory retain strict coverage.
- Clean compile time uses a fresh target directory and locked workspace `--all-features --release` build. Compile cycles and peak RSS are retained beside elapsed time.
- Binary size is a stripped, fat-LTO, locked workspace probe that constructs an RCA `Effect`. Qualification-binary size is retained separately for diagnostics. The runner supplies pinned toolchain library directory to `rust-objcopy` and fails if Cargo reports unsuccessful stripping, preventing an unstripped binary from being labelled as release-size evidence.
- Divan evidence covers every phase benchmark except the custom qualification harness. Each Divan benchmark uses 50 samples and a 0.5-second maximum measurement time.

Normal samples are 10,000. Earlier 1,000-sample cohorts left only ten observations at or beyond p99 and produced unrelated scheduler-tail reversals across repeated cohorts; 10,000-sample targeted checks cleared every flagged scenario in both execution-order strata. DHAT samples default to 1,000. High-fan-out, high-timer-count, mailbox, and shutdown cases use 200 or 500 DHAT samples, while deterministic budget exhaustion uses five. Qualification and comparison validate these exact counts.

## Matrix

The 50 stable scenarios cover:

- current-thread and multi-thread external commit;
- 1, 2, 4, and 8 producers with empty and saturated mailboxes;
- half-full and full mailbox admission;
- 1, 2, 4, and 8 independent Stores;
- small action/state, large action, and large state payloads;
- immediate and asynchronous effects;
- observation fan-out, slow consumer, handoff, and cancellation;
- cancellation insertion, replacement, and churn;
- action-task fan-out, finish, and cancellation;
- timer insertion, wake, and removal;
- query, graceful shutdown, and abortive shutdown; and
- deterministic ready work, timer work, and step-budget exhaustion.

## Profiling rule

Every qualification records:

- Time Profiler trace for saturated four-producer commit;
- Time Profiler trace for saturated eight-producer contention; and
- DHAT heap profile for asynchronous effect completion.

Profile before changing a hot path. If safe implementation remains within budget, make no optimisation-only code change. If a budget fails, compare safe alternatives first and rerun relevant runtime conformance, race, lifecycle, and benchmark checks. Unsafe is not an optimisation option for this gate.

## Regression gate

`scripts/compare-performance.sh` requires the measured candidate revision to be an ancestor of the review checkout and rejects any committed, staged, or unstaged change since that revision to runtime source, runtime manifests, workspace release profile, lockfile, or size-probe source. This lets evidence and release documentation be committed without falsely relabelling later runtime code as measured. The comparator also rejects evidence not matching the tracked candidate lockfile, pinned baseline revision and lockfile, current workload protocol, exact 50-scenario manifest, or identical cohort metadata. It rejects changed samples, operations, allocation samples and operations, raw pairs, exact alternating order, or repetition counts.

Comparator joins rows by scenario and rejects order-stratified paired ratios above `1.10` for median latency, p99 latency, inverted throughput, allocations, allocated bytes, peak memory footprint, maximum resident memory, CPU cycles, or task wakes. Throughput is inverted so every ratio uses same direction: greater than one is worse. Comparison also reports DHAT peak-live-heap ratio without using it for release acceptance.

A deliberate regression is accepted only when both conditions hold:

1. candidate row has a non-null `rationale_id`; and
2. candidate top-level `accepted_regressions` contains the same ID with a non-empty review record.

Comparison fails on missing scenarios, duplicate scenarios, zero metrics, schema mismatch, stale evidence, workload drift, lockfile drift, protocol drift, or cohort mismatch.

## Bootstrap reference

`docs/performance/baseline.json` records pinned first accepted 0.8 safe-runtime source revision and is marked `reference_kind: "paired-reference"`. Its measurements are regenerated beside each candidate, in alternating order, on canonical runner. Source revision never changes without reviewed baseline replacement; paired measurements and candidate evidence change together.

Do not silently replace baseline source revision. Replacement needs documented runner or methodology change, review of raw evidence, and retained prior reference. Commit both paired result files together.

## Variance and interpretation

One process can expose large regression but cannot prove tiny improvement. Qualification publishes aggregate medians from ten normal and six DHAT processes per revision, while release decisions use order-stratified paired ratios. Keep runner, power, OS major, compiler, dependency locks, workload, pairing policy, repetition policy, and sample policy fixed. Treat changes near 10% as prompts for another full paired cohort and profile inspection, not universal hardware claims. Compile and process counters include toolchain and OS effects; row metrics describe this qualification pair only.
