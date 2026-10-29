# Composition performance protocol v4

Historical protocol. The runner now uses [v5](composition-protocol-v5.md),
which corrects binary-size baseline evidence while preserving these timing
controls. The v4 manifest remains unchanged.

V4 raises the measurement-host minimum to macOS 27, following approval on
2026-09-26. It does not change the crates' deployment support. The host now runs
macOS 27.0, build `26A428`; v3 rejected that OS before creating evidence or taking
measurements. V4 records the actual OS version and build rather than treating
all supported versions as one interchangeable measurement environment.

The evaluator checks the hash of [the v4 manifest](composition-protocol-v4.json).
Its runner identity is `mac15-9-m3-max-16c-48gb-macos27plus-ac-high`. Hardware,
toolchain, AC power, high-power mode, and the [v3 host controls](composition-protocol-v3.md)
stay unchanged. The twenty-block schedule, seed, confidence rule, workload
hashes, allocation policy, 1.10 relative limit, and all 33 absolute budgets also
stay unchanged. The timing and isolation method remains that of
[v2](composition-protocol-v2.md), with v3's 20% CPU and 6.0 load limits.

## Preserve the historical baseline

The budget document remains byte-for-byte unchanged, with SHA-256
`22572ea0c98f78fe7412bb80d7c57940d15b2843256d15ac04d889adb70c63cb`.
Its `mac15-9-m3-max-16c-48gb-macos26-ac-high` labels describe the historical
macOS 26 baseline, not a new measurement. The evaluator keeps that identity
separate from the current runner and continues to restrict unpaired historical
absolute evidence to macOS 26. V2/v3 manifests and retained evidence remain
historical; no old receipt qualifies a v4 run.

V4 absolute timing gates use the twenty subject-arm observations. The unchanged
legacy capture script supplies fresh allocation, binary and diagnostic evidence
under the whole-run lock and monitoring. Its recorded OS version and build must
exactly match the paired metadata. Absolute reports identify both the current
runner and historical baseline, and retain the actual capture OS/build.

## Requalify before candidate measurement

First run two independent builds of reference
`b9eb6e732d2b0cf6ce3cd7255a5958781587af6c` under v4. Require every relative A/A
gate (ten runtime gates, including throughput, and four compile gates) and all
33 unchanged absolute budgets to pass. Then supply that published A/A receipt
to a fresh candidate/reference run. The runner re-evaluates its raw evidence
and requires the same protocol, controller revision, script hashes and exact
host identity, including OS version and build. Host identity must also remain
unchanged through each run. An OS update therefore requires fresh A/A even if
both OS versions meet the minimum.

Use distinct new paths below
`target/qualification-evidence/0.9.0-alpha.1/composition`:

```sh
scripts/qualify-composition-performance.sh --mode reference-reference-aa <aa-output> <pinned-reference-worktree>
scripts/qualify-composition-performance.sh --mode candidate-reference --aa-evidence <aa-output> <candidate-output> <pinned-reference-worktree>
```

Compare candidate telemetry with A/A telemetry before accepting the candidate.
A/A does not establish macOS 26/27 performance equivalence or validate conditions
outside its observed range. Retain failed evidence and investigate before any
further method change. Candidate results cannot set acceptance thresholds.
No v4 measurement informed this protocol; real A/A qualification remains required.
