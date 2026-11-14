# Performance qualification

RCA performance evidence is a release gate, not an informal benchmark snapshot.
Each paired capture uses one identified host, toolchain and workload.
The suite records latency, throughput, allocation, scheduler work, CPU and memory.
Platform-specific evidence also records compile time, binary size and profiles.
Semantic and concurrency conformance must pass before performance acceptance.

## Canonical Mac runner

The `mac15-9-m3-max-16c-48gb-macos27-ac-high` runner is:

- `Mac15,9`, Apple M3 Max, 16 logical CPUs, and 48 GiB memory;
- macOS 27 on AC power with high-power mode selected; and
- exact Rust and Cargo 1.100.0.

`scripts/qualify-performance.sh` validates this cohort, a clean checkout, release-profile settings, and absence of another RCA benchmark process. Results from a different runner are not comparable. New runner cohorts require a separately reviewed baseline instead of relabelling existing evidence.

On 30 October 2026, the canonical runner changed to the installed macOS 27 host.
The first host version is macOS 27.0, build `26A428`. Hardware requirements remain unchanged.
Rust 1.100.0 is required for both revisions. Earlier Rust 1.98 and 1.99 captures remain historical evidence and do not qualify the current toolchain.
New qualification captures both revisions on the same runner with Rust 1.100.0.
The release baseline is `ab99e19ca772086f11e0a855f840f5cf9fae5d7f`, the implementation before the performance review.
Earlier architecture comparisons remain historical evidence, not release baselines. Numerical regression limits remain unchanged.
Retain macOS 26 evidence under its original runner and protocol identities. It does not qualify a macOS 27 pair.
The qualifier's new source hash gives this cohort a new protocol identity. The comparator rejects evidence with the previous protocol hash.

## Commands

Local captures require [compiler memory protection](../../CONTRIBUTING.md#build-memory-safety).
The fixed `--qualify-performance` route runs the canonical script in the watchdog's owned process group.
It selects the pinned Rust tools. The route disables compiler wrappers.
Cargo uses one build job. The shell rejects direct invocation of `qualify-performance.sh`.

Choose a timeout before the run. The example allows 3600 seconds.
The watchdog does not extend that timeout or retry a failed capture.
This polling watchdog is not a kernel-enforced memory cap.
Native Instruments services can start processes outside the owned group. The watchdog does not contain those processes.

From a clean checkout:

```bash
mkdir -p target/qualification-evidence
python3 scripts/guard_cargo_memory.py \
  --receipt target/qualification-evidence/qualification-memory.json \
  --timeout 3600 --qualify-performance -- \
  target/qualification-evidence/candidate.json \
  /tmp/rca-performance-reference \
  target/qualification-evidence/baseline.json
```

After the watchdog exits with status 0, run the comparison:

```bash
./scripts/compare-performance.sh \
  target/qualification-evidence/baseline.json \
  target/qualification-evidence/candidate.json
```

The comparator requires the final watchdog receipt named in both JSON files.
That receipt binds both files by path and hash, after successful process exit and cleanup.
Missing receipts, forced descendant cleanup and changed output files fail comparison.

Retain the watchdog receipt, paired JSON, comparison, Divan log and profile report together.
Keep these qualification artefacts outside the tracked source tree. Do not change measured
rows or cohort metadata. Qualification stages all four artefacts and refuses
existing or aliased output paths. If shell capture fails, it removes only artefacts published by that invocation.
A later watchdog or output-binding failure preserves completed files as failed evidence.

The reference argument accepts a clean worktree or the exact revision from `docs/performance/baseline-revision.txt`.
For either argument, the controller repository must contain that Git object. The comparator checks its provenance without an external worktree.
For a worktree argument, its `Cargo.lock` must match `docs/performance/baseline-Cargo.lock` exactly.
The candidate uses its tracked root `Cargo.lock`. Every Cargo command uses `--locked`.

The qualifier exports each exact product tree into a temporary directory.
It preserves each product's manifests and dependency lockfile, then copies the controller's two qualification source files into both directories.
These files are `qualification.rs` and `qualification/shutdown.rs`. Staging checks both hashes and rejects every other source change.
The original worktrees remain unchanged. Staging rejects links, submodules, wrong hashes and unexpected source changes.
Before measurement and publication, the qualifier checks the staged product files, lockfiles and common harness again.

Both results record the original product trees, manifest hashes and original harness hashes.
`qualification_source_sha256` identifies the common harness that actually ran.
Build metadata records each temporary source directory and its normal and DHAT executables. These directories are removed after capture.
The comparator checks original identities against the archived Git objects and checks the common harness against the current controller.
Protocol identity includes both harness files, the exact ordered scenario manifest, the qualification script and the memory watchdog.
Changes to measurement boundaries or capture policy require fresh paired evidence, not new labels on historical captures.

The common harness retains the current mailbox admission and final-state checks.
Its poll counts exclude native warm-up and final processing, as documented in the harness.
The budget row checks the exact poll budget and one ready task on both products.
Candidate tests separately check live-task diagnostics in `rca-test/tests/deadline_draining.rs`.
The four bounded-shutdown rows require a fresh 54-scenario pair. Existing numerical gates remain unchanged.

The reference belongs to the initial pull request's history.
The revision pin and lockfile identify the exact implementation and dependencies.
Earlier reference objects and results retain their original identities outside the public source tree.
Parser tests do not qualify candidate performance. A different release baseline requires explicit review.

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

## Windows-native capture

Windows provides a separate native measurement route when the Mac is busy.
It does not require an idle host or a Linux virtual machine.
Its results never share a cohort with Mac results.
It uses the same reference revision, common workload, dependency locks, 54 scenarios, samples, balanced pairs and `1.10` regression limit.

`scripts/capture-windows-performance.py` consumes eight prebuilt executables and their source/build receipt.
Each product has normal and DHAT builds for two configurations:

- The 48 native-runtime rows use `rca-core` with exactly its `default` feature.
- The six `deterministic/` rows use the original test-runtime dependency graph, including `test-util` and any `diagnostics` feature that graph enables.

Cargo merges features requested by development dependencies. Building the original
combined harness would enable test-runtime code in the native rows, even though
those rows do not use that runtime. Its results would not describe the default
production configuration.

The native source copy therefore removes only the `rca-test` development dependency
from `rca-tokio/Cargo.toml` and its edge in `Cargo.lock`. The native harness omits the
six deterministic rows and their test-runtime imports. Native workload bodies and
all production source stay unchanged. Dependency versions and checksums stay unchanged.
The deterministic copy retains the original manifests, lockfile and common harness.

The receipt binds each executable to its complete effective source inventory,
package manifest, dependency lock, harness and exact Rust 1.100.0 toolchain.
Its `harness_module_sha256` binds the common shutdown module in all four source inventories.
It records before-and-after hashes for the three changed native files.
Cargo's retained build transcripts must identify the exact Core features and
optimised benchmark executable. Normal and DHAT builds must use the same source
configuration, and both products must use the same native harness.
The capture checks binary hashes before and after measurement.
Both result files retain the exact `source-build.json` receipt beside them.
The comparator checks its hash, embedded copy, executable roles and source/toolchain identities.
It retains process output, raw counters, pairs and incomplete evidence after failure.
It does not retry processes, remove outliers or choose successful subsets.

After all eight builds pass their source checks, run the capture on Windows:

```bash
python scripts/capture-windows-performance.py \
  --candidate-normal C:/rca/candidate-normal.exe \
  --candidate-allocation C:/rca/candidate-allocation.exe \
  --reference-normal C:/rca/reference-normal.exe \
  --reference-allocation C:/rca/reference-allocation.exe \
  --candidate-deterministic-normal C:/rca/candidate-deterministic-normal.exe \
  --candidate-deterministic-allocation C:/rca/candidate-deterministic-allocation.exe \
  --reference-deterministic-normal C:/rca/reference-deterministic-normal.exe \
  --reference-deterministic-allocation C:/rca/reference-deterministic-allocation.exe \
  --metadata C:/rca/source-build.json \
  --output C:/rca/new-capture
```

Use the actual executable paths from the build receipt.
The output directory must not exist.
Keep the complete output directory together when transferring it to the review checkout.
Then run `scripts/compare-performance.sh <capture>/reference.json <capture>/candidate.json`.

Schema version 6 identifies the `win32-process-v1` provider and the
`split-native-deterministic-v1` configuration.
All 54 rows belong to one complete cohort, with unchanged samples,
balanced pairs and numerical limits. Each row and process observation identifies
its native or deterministic configuration.
The protocol hash includes the source/build receipt hash as well as the common
harness files, scenario manifest and capture script. The comparator validates the same
build schema as the capture, then checks source identities, feature routing,
protocol, sample counts, paired observations and numerical limits.
Earlier combined-harness captures remain historical evidence. They cannot qualify
the default production runtime or be relabelled as a schema 6 pass.
The memory fields have explicit Windows definitions:

| Field | Windows measurement | Acceptance |
| --- | --- | --- |
| `cpu_cycles` | `QueryProcessCycleTime`, summed across process threads | Paired ratio at most `1.10` |
| `peak_working_set_bytes` | `PeakWorkingSetSize`, peak resident working set | Paired ratio at most `1.10` |
| `peak_commit_bytes` | `PeakPagefileUsage`, peak process commit charge | Paired ratio at most `1.10` |
| `KernelTime100ns`, `UserTime100ns` | `GetProcessTimes`, process CPU time | Supporting evidence |
| `instructions_retired` | Unavailable from this provider | Explicit `null`, never an invented count |

The capture reads lifetime counters from the retained process handle after exit.
Current private commit is not a peak and is not an acceptable substitute.
Peak commit charge is not macOS physical footprint.
Both hosts retain separate memory measurements and definitions.
The API definitions come from [Microsoft process counters](https://learn.microsoft.com/windows/win32/api/psapi/ns-psapi-process_memory_counters), [process cycles](https://learn.microsoft.com/windows/win32/api/realtimeapiset/nf-realtimeapiset-queryprocesscycletime) and [process times](https://learn.microsoft.com/windows/win32/api/processthreadsapi/nf-processthreadsapi-getprocesstimes).

Mac instruction counts, profiles, Mach-O sizes and compile evidence remain separate supporting records when their measured inputs remain unchanged.
A Windows pass does not relabel a failed Mac observation or establish identical behaviour on every host.
Instruction counts from Callgrind can supplement this evidence, but they are not native hardware timings.
No Windows value is compared with a Mac value.

Release review names the native host selected for the relative gates.
When Windows is selected, its complete pair supplies latency, throughput, CPU cycles, polls, allocation and Windows memory acceptance.
The selected host must also supply the supporting measurements.
On Windows, `scripts/capture_windows_support.py` captures the clean-target compile,
stripped PE sizes, live idle working set, five Divan suites and asynchronous-effect DHAT profile.
Qualification also requires two WPR contention traces with retained CPU-sample analysis.
Missing profiles leave qualification incomplete.

Run the support capture inside the reviewed Windows memory guard.
Every child job retains the two-GiB limit.

Windows support and complete composition captures also require LIEF 1.0.0 for PE stripping.
LLVM removes COFF symbols but can retain an embedded debug directory in `.rdata`.
The shared helper removes that directory with LIEF's `clear_debug()` API.
The strict PE validators still reject any remaining debug directory or symbol table.
This is a capture-tool dependency, not an RCA runtime dependency.

Use standard x64 Windows CPython 3.12 or later.
Download `lief-1.0.0-cp312-abi3-win_amd64.whl` from the [official release files](https://pypi.org/project/lief/1.0.0/#files).
Check its SHA-256: `ca7774b0d88f4528a1c153d7a9d2798800d6389847b966861fad9f2edb17a593`.
The release does not include a publishing attestation.
Choose a new private directory for the installation. Then set these paths and install the retained wheel:

```powershell
$env:RCA_WINDOWS_LIEF_WHEEL = 'C:\RCA-Tools\lief-1.0.0-cp312-abi3-win_amd64.whl'
$env:RCA_WINDOWS_LIEF_SITE = 'C:\RCA-Tools\lief-1.0.0-site'
python -m pip install --no-index --no-deps --no-compile --target $env:RCA_WINDOWS_LIEF_SITE $env:RCA_WINDOWS_LIEF_WHEEL
```

The capture never installs or downloads tools. Preflight checks the wheel and both executable package files before any LIEF import.
It rejects extra Python bytecode or native libraries in that package.
An isolated child process disables site startup hooks and records the Python, LIEF and helper identities.
Each result retains the original, LLVM intermediate and final PE bytes.
The helper permits only debug removal and zeroed file-alignment padding outside each section's `VirtualSize`.
Mapped code, section layout, imports, relocations and all other non-debug bytes must remain unchanged.
The existing measured-source identity stays attached to the binaries; a new helper does not establish a new Rust build.

Windows native CPU evidence uses process cycles and sampled CPU profiles.
Retired instructions remain `null`: neither cycles nor profile samples are instruction counts.
Retired instructions are supporting measurements, not a numerical gate in the comparator.
This Windows route removes the need for a separate Mac capture without changing any
of the 54 scenarios, samples or numerical regression limits.
Each reused record still needs an applicable source, dependency, configuration and toolchain identity.
Any failed Mac timing observation remains explicit, with its review disposition and limits.
Composition retains its separate platform-specific absolute and relative gates.
Windows completion requires the [complete composition policy](composition/composition-windows-complete-v3.json),
not a paired-only capture.

## Measurements

- Latency uses `Instant` around each completed operation after up to 20 warm-up operations. For `n` sorted samples and percentile `p`, the runner selects zero-based index `ceil((n - 1) * p / 100)`. This upper indexed percentile is not the nearest-rank method. p50, p95, and p99 describe whole-operation latency. Every normal scenario uses 10,000 outer samples so each process has 100 observations at or beyond p99. Aggregate fields use medians of ten process-level results; acceptance uses geometric mean of two order-stratified medians from ten paired candidate/reference ratios.
- Throughput divides completed logical operations by measured sample-loop time. Producer and fan-out scenarios report all inner operations, not only outer iterations.
- On Mac, CPU cycles and retired instructions come from `/usr/bin/time -lp` for the complete scenario process. These counters aggregate runtime threads. Windows uses the separate definitions above.
- Allocations and allocated bytes come from six separate DHAT processes. Aggregate allocation counts, allocated bytes, and peak live heap are their medians; allocation acceptance uses geometric mean of two three-pair order-stratified medians. Odd stratum sizes preserve reciprocal symmetry. DHAT uses a separate bounded sample policy recorded by `allocation_samples`; deterministic budget exhaustion uses five samples per repetition because each one already performs 100,000 non-allocating self-wake polls.
- `task_wakes` is the historical schema name for measured future poll activations. Tokio scenarios include the top-level workload future and RCA runtime futures instrumented by `tokio-metrics`; deterministic scenarios report driver poll work. Counting the workload future preserves real zero-work cancellation cases without inventing a non-zero floor.
- On Mac, maximum resident set and peak memory footprint come from normal processes and are strict paired gates. Windows instead gates peak working set and peak commit charge. Idle RSS is sampled after a Store reaches its ready state. DHAT peak live heap remains diagnostic: concurrent scheduling changes allocation lifetimes enough to make it unsuitable as a cross-process 10% release gate, while total allocation count/bytes and process memory retain strict coverage.
- Clean compile time uses a fresh target directory and locked workspace `--all-features --features critical-section/std --release` build. The explicit `std` provider supports the critical-section backend enabled by the all-features build. Portable libraries do not select this host implementation. Compile cycles and peak RSS are retained beside elapsed time.
- Binary size is a stripped, fat-LTO, locked workspace probe that constructs an RCA `Effect`. Qualification-binary size is retained separately for diagnostics. The runner supplies pinned toolchain library directory to `rust-objcopy` and fails if Cargo reports unsuccessful stripping, preventing an unstripped binary from being labelled as release-size evidence.
- The support capture collects the five Divan suites named by its protocol. Each uses 50 samples and a 0.5-second maximum measurement time.

The separate `task_access` Divan target measures successful root task reads,
updates and sends through a manually polled actor. Its lifecycle row also
declares, registers, polls and collects a completed task. It checks actual
results, committed revisions and final state while the originating task remains live.
Setup, cancellation and shutdown stay outside timing. It does not measure
contention or executor scheduling. Use paired results from this target when
changing typed task admission; they supplement, not replace, the native gates.
Pass `--sample-size 256` to both binaries in a paired run. A fixed batch size
limits timer rounding and keeps automatic batch choices out of the comparison.
Enable `allocation-measurement` for separate allocation counts through Divan's
profiler. Do not use those instrumented timings as normal timing evidence.

Normal samples are 10,000. Earlier 1,000-sample cohorts left only ten observations at or beyond p99 and produced unrelated scheduler-tail reversals across repeated cohorts; 10,000-sample targeted checks cleared every flagged scenario in both execution-order strata. DHAT samples default to 1,000. High-fan-out, high-timer-count, mailbox, and shutdown cases use 200 or 500 DHAT samples, while deterministic budget exhaustion uses five. Qualification and comparison validate these exact counts.

## Matrix

The 54 stable scenarios cover:

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
- query, graceful shutdown, abortive shutdown speed, fixed-live-store memory and bounded-burst memory; and
- deterministic ready work, timer work, and step-budget exhaustion.

## Profiling rule

Every Mac qualification records:

- Time Profiler trace for saturated four-producer commit;
- Time Profiler trace for saturated eight-producer contention; and
- DHAT heap profile for asynchronous effect completion.

Windows qualification records the same two contention workloads with WPR and
retains the asynchronous-effect DHAT profile. Review must confirm CPU samples
belong to the measured process, not unrelated host activity. Keep the trace,
analysis, exact command, binary hash and collector result together.
Normal latency measurements do not run under Instruments, WPR or DHAT.
Fresh CPU Profiler captures can supplement the retained Time Profiler traces.
They do not replace uninstrumented latency evidence.

Profile before changing a hot path. If safe implementation remains within budget, make no optimisation-only code change. If a budget fails, compare safe alternatives first and rerun relevant runtime conformance, race, lifecycle, and benchmark checks. Unsafe is not an optimisation option for this gate.

## Regression gate

`scripts/compare-performance.sh` requires the measured candidate revision to be an ancestor of the review checkout.
It rejects later changes to measured source, manifests, maintained transport forks, examples, benchmark sources, build configuration, lockfiles and the size probe.
It also rejects new untracked files within those input paths. Unrelated root documentation can change without invalidating the measurements.
Both evidence files must match the original product identities, current common harness, protocol, lockfiles and exact scenario manifest.
The comparator rejects changed samples, operations, allocation samples and operations, raw pairs, alternating order or repetition counts.

Comparator joins rows by scenario and rejects order-stratified paired ratios above `1.10` for median latency, p99 latency, inverted throughput, allocations, allocated bytes, both process-memory metrics, CPU cycles, or task wakes. Mac uses peak memory footprint and maximum resident memory. Windows uses peak commit charge and peak working set. Throughput is inverted so every ratio uses same direction: greater than one is worse. Comparison also reports DHAT peak-live-heap ratio without using it for release acceptance.

A deliberate regression is accepted only when all conditions hold:

1. candidate row has a non-null `rationale_id`; and
2. candidate top-level `accepted_regressions` contains the same ID and exact scenario;
3. that record contains a written review and a non-empty `metrics` array of gated ratio names; and
4. every ratio above the limit appears in that array.

For example, `metrics: ["task_wakes_ratio"]` accepts only the reviewed poll-count cost.
It cannot accept a CPU, latency, allocation or memory regression in the same scenario.
The comparison output lists all failed ratios in `regressed_metrics`.

Comparison fails on missing scenarios, duplicate scenarios, zero metrics, schema mismatch, stale evidence, workload drift, lockfile drift, protocol drift, or cohort mismatch.

## Bootstrap reference

The generated baseline JSON records the pinned reference revision and is
marked `reference_kind: "paired-reference"`. Its measurements are regenerated
beside each candidate, in alternating order, on the canonical runner. The
source revision never changes without a reviewed baseline replacement;
paired measurements and candidate evidence change together. Old captures
are archived outside the public source tree.

Do not silently replace the baseline source revision. Replacement needs a
documented runner or method change, review of raw evidence, and a retained
prior reference. Retain both paired result files together.

## Variance and interpretation

One process can expose large regression but cannot prove tiny improvement. Qualification publishes aggregate medians from ten normal and six DHAT processes per revision, while release decisions use order-stratified paired ratios. Keep runner, power, OS major, compiler, dependency locks, workload, pairing policy, repetition policy, and sample policy fixed. Treat changes near 10% as prompts for another full paired cohort and profile inspection, not universal hardware claims. Compile and process counters include toolchain and OS effects; row metrics describe this qualification pair only.

Background load is telemetry, not a reason to stop all qualification work.
CPU time excludes time when a process waits for CPU scheduling.
Other work still affects shared caches, memory bandwidth and CPU frequency.
CPU cycles and instruction counts explain execution cost, but cannot replace latency or throughput checks.
Shared interference can also compress candidate/reference ratios towards one.
Balanced order and reference/reference controls reduce some bias but do not prove immunity to interference.

For CPU optimisation on supported Apple hardware, prefer CPU Profiler with deferred recording.
Its cycle-based samples account for different core frequencies.
Time Profiler remains useful, but its periodic samples do not measure each function call.
See [Apple's CPU profiling guidance](https://developer.apple.com/videos/play/wwdc2025/308/).
Profile instrumentation stays outside the uninstrumented latency capture.
