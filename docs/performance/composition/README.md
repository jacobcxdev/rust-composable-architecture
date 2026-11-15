# Composition performance qualification

The [v9 macOS protocol](composition-protocol-v9.json) and
[complete Windows policy](composition-windows-complete-v6.json) define separate qualification routes.
Their workloads, hashes and numerical limits are frozen before capture.
Complete Windows qualification uses the current release reference.
The older [macOS absolute budgets](composition-budgets.json) retain their original values and provenance.
Those older architectural ceilings are not additional release blockers for the current reference.
This directory contains the methods and historical receipts, not current-candidate captures.

The v9 macOS and v6 Windows methods use Rust 1.100.0. Their workloads,
ordering and acceptance limits match the preceding Rust 1.99 methods.
The frozen Rust 1.98 and 1.99 protocols and captures retain their original
identities. A new compiler requires a new matched capture, not relabelled evidence.

## Reference and availability

The release reference is `ab99e19ca772086f11e0a855f840f5cf9fae5d7f`, the implementation before the performance review.
It belongs to the initial pull request's history. Qualification requires a clean checkout of that exact revision.
The older `b9eb6e732d2b0cf6ce3cd7255a5958781587af6c` reference identifies historical budgets and protocols, not current paired measurements.
Those original files and results retain their hashes and values.
Fixture tests do not qualify candidate performance. A different release reference requires explicit review.

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

The v9 protocol requires macOS 27 or later, the named hardware and Rust 1.100.0,
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
and unchanged. Shared Git ancestry is not required.

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

If a mode fails its numerical gates, the runner retains both normal runtime binaries in `builds/failed-runtime-products/` before that cleanup.
The runner checks each retained copy against its executable hash in `metadata.json`.
Passing modes retain no runtime binaries.

## Windows paired measurements

The [Windows protocol](composition-windows-paired-v6.json) runs the same 14
relative gates on H1 with Rust 1.100.0. It uses the same 20 paired blocks, fixed
orders, confidence rule and 1.10 limit. Every block contains both comparison
modes; their first mode alternates. All measurements and their hashes remain in the
capture, including failures. Windows results do not qualify any of the 33 macOS
absolute gates.

Versions 2 and 3 set the minimum and maximum Divan sampling time to 0.5 seconds for every isolated runtime cell.
Version 4 keeps that duration for eight scenarios and uses two seconds for scoped concurrent sends.
Version 5 uses two seconds for every isolated runtime cell.
Version 6 keeps that duration and uses fixed primary-thread placement for normal Divan timing commands.
The process helper sets primary-thread mask `1` in processor group `0` before `ResumeThread`.
This rule applies to isolated cells and full-benchmark diagnostics, not Cargo, allocation or list commands.
The process mask stays equal to the full system mask. The helper performs no CPU search.
Each timing receipt records the previous and actual primary-thread masks, processor group, process mask and system mask.
The evaluator rejects missing records, changed masks and mixed method records.
These records do not observe individual worker-thread affinity. They show that the process still permits the full system CPU set.
The Windows unit test separately observes four new worker-thread masks. This finite check does not measure every qualification worker.
The sample size, work per operation, pair counts, order and acceptance limits do not change.
Before a full capture uses a new sampling policy, its readiness control must pass.
That control must use independently built reference products, not two copies of one executable.
Longer collection does not prove immunity to interference or guarantee that the full control will pass.
Failed captures retain their original method and result. They supply no rows to a new capture.

Version 6 can reject an irrecoverable failure after a sealed mode block.
Each order stratum must contain at least two observations before this rule applies.
The current second-largest ratio cannot decrease as later observations enter that stratum.
The geometric mean of both such ratios is a lower bound on the final upper-confidence bound.
If that lower bound exceeds `1.10`, the runner stops and retains all four normal runtime images.
The prefix receipt records a rejected, incomplete capture, not a confidence interval for the prefix.
The runner checks sealed file hashes before each prefix calculation. It does not read in-flight observations.
This rule never accepts early. Successful qualification still requires all twenty pairs, both modes and all absolute gates.

Collection can continue past the workload sample count to reach that duration.
The final sample can cross the time limit. This limit is not a process deadline.
For `scoped_concurrent_senders_32`, the two seconds measure workload execution.
Store and producer-thread setup and cleanup occur outside that measured interval.
The process can therefore take much more wall time. The fixed process deadline stays unchanged.

Sample size and per-operation work stay unchanged. Full-benchmark diagnostics retain their workload defaults.
Receipts record the actual arguments, sample counts and iteration counts.
Earlier captures keep their original policies. No result is relabelled.

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

## Complete Windows qualification

The [complete Windows policy](composition-windows-complete-v6.json) adds native
absolute measurements to the paired protocol above. It does not change that
protocol or claim that Windows measurements satisfy Mac limits.

```powershell
python scripts/composition_runner.py --platform windows --windows-complete <capture-output> <pinned-reference-worktree>
```

Both paired modes and all 33 native absolute gates must pass. The thirteen
allocation limits keep their original values, including three exact-zero contracts.
The capture runs all forty-seven allocation workloads and both exact zero-test hosts.

Timing and binary sizes need native reference values. The runner uses all twenty
reference-b observations from the same A/A capture to set each timing limit.
Maximum limits are the reference median times 1.10. The throughput minimum is the
reference median divided by 1.10. Each of the six size limits is the freshly built,
stripped reference PE size times 1.10, rounded up. Candidate values never set limits.
The runner seals these reference-only limits before evaluating the candidate.

The capture retains the policy, reference observations, derived limits and both
absolute reports with their hashes. Mac protocols, limits and failed observations
remain unchanged. The Windows result explicitly states `mac_absolute_qualified: false`.

Complete mode permits pinned `cargo build` and `cargo test --no-run` calls to use
the same checked compiler-telemetry cleanup as the paired builds. Tests execute
separately and must exit naturally. All process memory limits remain unchanged.

The retained [v7 protocol](composition-protocol-v7.json) describes historical
Rust 1.98 measurements. Current commands use Rust 1.100.0. The old protocol and its
evidence remain unchanged; they do not qualify the new toolchain.

## Test inputs are not release evidence

Historical parser inputs live in
[`scripts/tests/fixtures/composition-performance`](../../../scripts/tests/fixtures/composition-performance/README.md).
They test valid and invalid evidence, including rejection of the original
unstripped sizes and an obsolete protocol. They do not describe current RCA
performance. Superseded reports, experiments and capture logs are archived
outside the public source tree.
