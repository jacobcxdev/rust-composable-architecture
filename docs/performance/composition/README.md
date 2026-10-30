# Composition performance qualification

The [v6 protocol](composition-protocol-v6.json) and
[absolute budgets](composition-budgets.json) define the composition performance
gate. Their contents, hashes and thresholds are frozen before capture. This directory holds
the maintained method and the three receipts needed to validate those budgets,
not captures of the current candidate.

## Reference and availability

The reference revision is
`b9eb6e732d2b0cf6ce3cd7255a5958781587af6c`. It belongs to RCA's archived
development history and is not available from the public repository. Running
the full paired gate therefore requires a clean copy of that exact reference
from the retained archive. A fresh public clone alone cannot reproduce it.
Do not substitute another revision or claim that fixture tests qualify a
candidate. A replacement reference needs a reviewed protocol and baseline
change.

The retained [v5 protocol](composition-protocol-v5.json) corrected six binary-size baselines. The original builds had reported
success despite `rust-objcopy` failures. The reference binaries were rebuilt
on macOS 27.0 (build `26A428`) with Rust 1.98.0 and unchanged release profiles,
then explicitly stripped with the pinned `rust-objcopy --strip-all`. Each
binary budget is the corrected reference size plus 10%, rounded up. Candidate
measurements did not set these limits. The other 27 absolute rows retain their
original macOS 26 measurements.

The files under `raw/` are exact, hash-checked receipts for that correction:
the measured sizes, verification output and capture driver. The driver is a
historical receipt, not the current capture command. Its original host paths
and the verification output remain unchanged so that their hashes still
identify the recorded evidence.

## Run the gate

The v6 protocol requires macOS 27 or later, the named hardware and toolchain,
host-load checks, twenty paired blocks, the fixed ordering seed and confidence
rule, and the 1.10 relative limit. These host requirements do not set RCA's
deployment support. Host load constrains timing evidence, not independent
implementation or correctness work.

With the exact reference available, first qualify a reference/reference
run, then measure the candidate against it:

```sh
scripts/qualify-composition-performance.sh --mode reference-reference-aa <aa-output> <pinned-reference-worktree>
scripts/qualify-composition-performance.sh --mode candidate-reference --aa-evidence <aa-output> <candidate-output> <pinned-reference-worktree>
```

Use distinct output paths and retain failed evidence outside the tracked tree.
The runner checks the same controller revision, scripts, protocol and exact
host OS/build between both runs. All relative gates and all 33 absolute gates
remain required. Binary-only measurements do not replace the paired timing
run.

Both modes use the controller's capture script and the same hash-pinned consumer
workloads. Each arm starts from an archive of its exact product revision.
The runner copies only the listed benchmarks, compile fixtures, example workload
and test-only support. It never replaces library implementation with another
revision's implementation. In particular, `scope.rs` and `combine.rs` remain unchanged.

The runner records the original Git revision and tree, original file hashes,
effective file hashes, and applied adapters. Both source checkouts remain clean
and unchanged. Shared Git ancestry is not required: the approved clean-slate
history does not contain the archived reference as an ancestor.

The deterministic binary fixture retains the historical lifecycle trace. The
candidate constructor uses a unit projection because its `TestStore` requires
an explicit snapshot policy. Both arms retain the same actions, waits and final
state assertions. This adapter's runtime and compiled costs remain part of the
measurement. It does not replace the current example's exhaustive correctness tests.

The zero-allocation host runs the same seven direct/scope cases and combine
widths 1–12 on both products. A real Store supplies each private reduction
context through its public reducer boundary. Admission and shutdown remain
outside each allocation window. The shared synchronous runner creates no
libtest receiver thread. None-effect checks use public Debug output after the
allocation snapshot, rather than private implementation fields.

All 33 numerical ceilings retain their historical values and source hashes.
V6 applies those unchanged ceilings to the common workloads. It does not claim
that the historical measurements used this later harness. Each result separates
historical budget provenance from the effective measured workload.

Capture requires a working pinned stripping tool and successful
explicit stripping before recording each binary. Cargo success alone does
not prove that stripping succeeded. Receipts record the controller script
hash, subject revision, workload hashes and lockfiles.

Each compile sample uses a distinct clean target. After its transcript and
resource counters are complete, the runner deletes that check-only target.
Failed transcripts remain available. Runtime binaries remain available until
their final hash check completes. These deletions occur outside measured intervals.

## Test inputs are not release evidence

Historical parser inputs live in
[`scripts/tests/fixtures/composition-performance`](../../../scripts/tests/fixtures/composition-performance/README.md).
They test valid and invalid evidence, including rejection of the original
unstripped sizes and an obsolete protocol. They do not describe current RCA
performance. Superseded reports, experiments and capture logs are archived
outside the public source tree.
