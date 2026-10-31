# Composition performance qualification

The [v8 macOS protocol](composition-protocol-v8.json) and
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

Mac captures require [compiler memory protection](../../../CONTRIBUTING.md#build-memory-safety).
The Mac controller applies the memory watchdog to each owned command group.
Memory checks run separately from host telemetry. The measured command keeps its original timing boundary.
Each command records the watchdog version, memory peak, exit status and cleanup result.
Forced cleanup cannot count as successful natural completion.

This watchdog is not a kernel-enforced memory cap. Use an OS-limited remote job for large or unfamiliar builds.
Do not run `capture-composition-performance.sh` directly on the Mac. Use the guarded controller below.
The Windows paired route below applies an OS memory limit.

The v8 protocol requires macOS 27 or later, the named hardware and Rust 1.99.0,
twenty paired blocks per mode, the fixed ordering seed and confidence rule,
and the 1.10 relative limit. These host requirements do not set RCA's deployment support.
The runner records background CPU load and compiler processes without waiting for an idle host.
It still requires nominal thermal state, unchanged power settings and no competing composition or qualification benchmark.

With the exact reference available, capture the candidate and its reference/reference control together:

```sh
scripts/qualify-composition-performance.sh --mode candidate-reference <capture-output> <pinned-reference-worktree>
```

Use a new output path below `target/qualification-evidence/0.1.0/composition`.
Retain failed evidence outside the tracked tree.
The capture contains separate `candidate-reference` and `reference-reference-aa` directories.
Both modes use the same controller revision, scripts, protocol and exact host OS/build.
All relative gates and all 33 absolute gates remain required for each mode.

The top-level `complete.json` reports the combined result.
The single-mode evaluator remains available for historical v6 evidence, not current acceptance.
Binary-only measurements do not replace the paired timing run.

Each mode retains twenty blocks. The complete capture uses four independent benchmark builds.
Odd-numbered blocks run the A/A control first. Even-numbered blocks run the candidate comparison first.
The existing subject/control order stays unchanged inside each block.
Both modes bind to one capture manifest, which records the complete schedule before measurement.
Ordered start/finish receipts bind each block to that manifest.
Each finished block also records the hashes of its transcripts and resource counters.

A control from another capture cannot qualify the candidate.

Background load can affect latency, throughput and compile time. Process CPU time and instruction counts do not replace these measurements.
Interleaved controls expose interference during the capture, but they do not prove that observations are independent.
The confidence bound still assumes independent, identically distributed observations within each execution-order group.
Correlated load can violate that assumption. A passing A/A control is not proof of immunity to arbitrary load.

The runner never removes inconvenient samples or repeats a failed measurement.
A failed control makes the combined result fail. Thermal or malformed-evidence failures stop the capture and retain its incomplete evidence.
The unchanged absolute limits can still fail on a busy host.
Recorded telemetry supports review of interference. It does not convert CPU time into response time.

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
V8 applies those unchanged ceilings to the common workloads. It does not claim
that the historical measurements used this later harness. Each result separates
historical budget provenance from the effective measured workload.

Capture requires a working pinned stripping tool and successful
explicit stripping before recording each binary. Cargo success alone does
not prove that stripping succeeded. Receipts record the controller script
hash, subject revision, workload hashes and lockfiles.

Each compile sample uses a distinct clean target. After its transcript and
resource counters are complete, the runner deletes that check-only target.
Failed transcripts remain available. Runtime binaries remain available until
their final hash check completes. After each mode completes all source and binary checks,
the runner deletes its temporary source and build trees. Receipts and measurements remain.
These deletions occur outside measured intervals.

## Windows paired measurements

The [Windows protocol](composition-windows-paired-v1.json) runs the same 14
relative gates on H1 with Rust 1.99.0. It uses the same 20 paired blocks, fixed
orders, confidence rule and 1.10 limit. Every block contains both comparison
modes; their first mode alternates. All measurements and their hashes remain in the
capture, including failures. Windows results do not qualify any of the 33 macOS
absolute gates.

From a clean Windows checkout with the exact reference available, run:

```powershell
python scripts/composition_runner.py --platform windows <capture-output> <pinned-reference-worktree>
```

Keep Git line endings unchanged in both checkouts (`core.autocrlf=false`). The
runner checks the exact source bytes before and after capture. Use an unused
output path below `target/qualification-evidence/0.1.0/composition`.

Each command starts suspended, joins an owned Windows Job Object, then runs.
The job limits total committed memory across its process tree to 2 GiB.
Windows denies allocations above that limit. A nonzero exit or the fixed
30-minute timeout fails the capture. Failure cleanup terminates only that owned job.
The runner waits for the job to empty before deleting build output.
It does not wait for background load to fall.
The runner never increases the memory limit or retries a failed measurement.

Compile time ends when the pinned Cargo process exits successfully.
Only compiler calls can terminate a surviving `VCTIP.EXE` telemetry process.
The runner checks its exact image path, file hash and owned-job membership.
Unknown surviving children fail the capture. Receipts retain each requested
termination and the full elapsed time through cleanup, including on failure.
Runtime calls require natural descendant exit and include that wait in elapsed time.

CPU cycles, CPU time and memory counters cover only the direct process. They are kept as raw
diagnostics, not treated as compiler-child totals or instruction counts.

The retained [v7 protocol](composition-protocol-v7.json) describes historical
Rust 1.98 measurements. Current commands use Rust 1.99. The old protocol and its
evidence remain unchanged; they do not qualify the new toolchain.

## Test inputs are not release evidence

Historical parser inputs live in
[`scripts/tests/fixtures/composition-performance`](../../../scripts/tests/fixtures/composition-performance/README.md).
They test valid and invalid evidence, including rejection of the original
unstripped sizes and an obsolete protocol. They do not describe current RCA
performance. Superseded reports, experiments and capture logs are archived
outside the public source tree.
