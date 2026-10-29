# Composition performance protocol v5

V5 corrects six binary-size baselines whose historical Cargo builds returned
success despite `rust-objcopy` failures. The original raw logs, v2–v4 manifests,
and historical sizes remain preserved. See the
[corrected binary reference](composition-report.md#corrected-binary-only-reference-2026-09-29).

The reference revision remains `b9eb6e732d2b0cf6ce3cd7255a5958781587af6c`.
Its six executables were rebuilt on macOS 27.0 (build `26A428`) with pinned
Rust 1.98.0 and unchanged release profiles, then explicitly stripped with the
pinned `rust-objcopy --strip-all`. Budgets use those reference sizes plus 10%,
rounded up. Candidate sizes did not set thresholds. The budget document records
this separate binary-only provenance; the other 27 rows retain their historical
macOS 26 provenance and values.

The capture now requires a working pinned stripping tool and successful
explicit stripping before recording each binary. Cargo success alone does not
qualify a stripped-size measurement.
Both modes execute the controller's corrected capture script in the subject
checkout. Thus A/A measures unchanged reference sources without reusing its
faulty stripping logic. Receipts record the invoked controller script hash,
while source revision, workload hashes, and lockfiles identify the subject.

## Unchanged timing qualification

The [v5 manifest](composition-protocol-v5.json) retains v4's macOS 27 minimum,
hardware, toolchain, host-load limits, twenty-block schedule, seed, confidence
rule, and 1.10 relative limit. This minimum applies to measurement hosts, not
crate deployment support. CPU load constrains timing evidence, not independent
implementation or correctness work.

The new protocol and budget hashes invalidate earlier A/A receipts. Run fresh
reference/reference A/A before candidate/reference timing qualification:

```sh
scripts/qualify-composition-performance.sh --mode reference-reference-aa <aa-output> <pinned-reference-worktree>
scripts/qualify-composition-performance.sh --mode candidate-reference --aa-evidence <aa-output> <candidate-output> <pinned-reference-worktree>
```

Use distinct output paths and preserve failed evidence. The runner checks the
same controller revision, scripts, protocol, and exact host OS/build between
A/A and candidate runs. All relative gates and all 33 absolute gates remain
required. Binary-only measurements do not substitute for this timing cohort.
The manifest's frozen-before-measurement status applies to future paired timing
runs; the corrected reference binary measurements are explicitly recorded
inputs to this revision, not an unmeasured baseline.
