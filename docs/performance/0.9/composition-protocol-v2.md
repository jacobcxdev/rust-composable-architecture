# Composition performance protocol v2

Historical protocol. The runner now uses [v4](composition-protocol-v4.md); the commands below describe the superseded v2 procedure and must not be used for current qualification.

This protocol replaces the noisy, ten-pair full-suite timing comparison. It does not change the 33 accepted absolute budgets or the 1.10 regression limit.

The reference remains `b9eb6e732d2b0cf6ce3cd7255a5958781587af6c`. No v2 measurement contributed to this protocol or its thresholds.

## Frozen inputs

`composition-protocol-v2.json` fixes the seed, twenty-block order, sample counts, host policy and statistical method. The evaluator checks its exact SHA-256.

Before measurement, each run records the protocol, expanded plan, revisions, source hashes, lockfiles, toolchain binaries and host identity. Each arm has a separate clean build target. The runner records executable hashes and identical benchmark lists before the first timing observation.

The runner requires clean source checkouts, exact Rust 1.98.0 tools, pinned cargo-bloat 0.12.1 and the accepted workload hashes. It checks source and input hashes again before publication. Ambient benchmark and compiler overrides cause failure.

## Observations and gates

Each of twenty blocks contains nine isolated runtime pairs and four clean compile pairs. There are 360 runtime processes and 160 compile processes. Each order stratum contains ten blocks. The SHA-256 ordering algorithm uses the recorded seed, not measurement results.

Each runtime process selects exactly one scenario with `--exact`. The workload source fixes Divan sample counts, sample sizes and time limits. The transcript retains the actual samples and iterations. Each compile process uses a previously absent target directory, `--locked`, one Cargo job and `CARGO_INCREMENTAL=0`.

Both relative and absolute timing gates use these same observations. The absolute timing statistic is the median of the twenty subject-arm observations. The existing throughput conversion remains unchanged. Allocation and binary gates use the unchanged historical capture script. Its full-suite runtime and compile timings are diagnostic only.

Each arm also produces one complete 153-row diagnostic transcript. The historical capture produces additional diagnostics. The evaluator never substitutes a diagnostic timing row for an isolated gate observation.

Every v2 process records `/usr/bin/time -lp` counters and a separate raw counter transcript. The legacy capture has aggregate counters, in addition to its original internal compile counters. Its internal processes do not receive new individual counters.

## Confidence bound

For latency and compile time, each ratio is subject/control. For throughput, each ratio is control/subject. A ratio greater than one always indicates regression.

Let `m1` and `m2` denote the two population medians, one for each order stratum. The target is `sqrt(m1 × m2)`. It is not a pooled median or a mean ratio.

Within each stratum, the ninth ordered observation is an upper bound for its median. For ten independent, identically distributed continuous observations, its coverage is:

`1 − (C(10,9) + C(10,10)) / 2^10 = 0.9892578125`.

The geometric mean of both stratum bounds has coverage at least `1 − 2 × 11/1024 = 0.978515625`. This Bonferroni bound exceeds 95%. It does not require independence between the two strata. The point estimate uses the geometric mean of their sample medians.

Each relative gate passes only when its upper bound is at most 1.10. All gates must pass. The coverage statement applies per metric, not simultaneously to every reported interval. The all-gates acceptance rule is an intersection-union test.

The assumptions require independent observations and a stable distribution within each stratum. Host controls reduce interference but do not prove these assumptions. Persistent drift or serial dependence can invalidate the coverage claim. Rounded Divan values can create ties. The order-statistic bound remains conservative for a specified population median with ties.

## Host controls and retained evidence

An atomic directory lock covers the entire run on the machine, including builds and historical capture. The runner never steals a stale lock. The owner record contains a PID and timestamp.

Initial and block-boundary checks require seven consecutive samples, with five seconds between samples. This gives at least thirty seconds of sustained quiescence. CPU use must not exceed 5%. One-minute load must not exceed 2.0. These conservative limits reserve most capacity on the fixed sixteen-core host. They do not measure per-core placement.

Each process has an idle check before it and a strict check after it. A failed pre-process sample restarts sustained quiescence, with a five-minute timeout. A failed active or post-process check aborts the run. No measured observation is retried or removed.

Active checks run between one-second process waits. They check competing processes, thermal state and AC power. Their execution time adds to the interval. They record load, but do not apply idle CPU/load limits to the benchmark itself. Monitoring adds small common overhead and cannot detect every short-lived competitor.

The process filter excludes the complete owned process tree. It detects Cargo, Rust, Go, Swift, C-family compilers, linkers, build tools and composition benchmarks outside that tree. Other applications remain subject to the idle CPU/load checks.

Current `NSProcessInfo.thermalState` must equal nominal (`0`). Unknown output and non-nominal states cause failure. The fixed modern Mac supports this API. Apple documents that unsupported systems can return nominal, so this check is not portable thermal proof. `pmset` warning history supplies an additional conservative check, not proof of current nominal state.

The runner retains telemetry, commands, exit status, raw counters and failed transcripts in `<output>.incomplete`. Successful evaluation writes all reports before an atomic Darwin `renamex_np(RENAME_EXCL)` publication. A complete gate failure also publishes its failed reports. An existing destination, including an empty directory, never receives replacement evidence.

The nominal idle overhead is at least 10.5 minutes at block boundaries, plus about two seconds per process. Actual duration also includes builds, historical capture, telemetry and cooldown. The runner makes no duration promise and does not weaken controls to meet a deadline.

## Two modes

`candidate-reference` compares the current candidate with the pinned reference. Its subject arm supplies all absolute timing observations and its own allocation/binary evidence.

`reference-reference-aa` independently builds the pinned reference twice. Its reference-only subject arm also checks all 33 absolute budgets. Candidate source and measurements never enter this mode. There is no separate reference-only capture mode.

If A/A fails an absolute timing budget, the result does not authorise threshold changes. It provides reference-only evidence for a separate methodology decision. Candidate measurements cannot establish reference budgets. Prior failed cohorts remain retained evidence, not a source for threshold tuning.

## Run procedure

After independent review, reserve a quiet host and use a new output path under `target/qualification-evidence/0.9.0-alpha.1/composition`.

```sh
scripts/qualify-composition-performance.sh --mode reference-reference-aa <new-output> <pinned-reference-worktree>
scripts/qualify-composition-performance.sh --mode candidate-reference <different-new-output> <pinned-reference-worktree>
```

If the machine lock remains after a crash, inspect its owner before manual recovery. Preserve incomplete evidence before any recovery action.

The Python evaluator supports offline arithmetic checks. Offline report labels alone do not establish source provenance. Qualification requires the runner-produced input manifest, build records and telemetry.

The historical absolute parser remains for the tracked Task 4 evidence. The superseded v1 paired evaluator and its CLI were removed. Git retains the old implementation.
