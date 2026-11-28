# Measure runtime performance

Use the [qualification guide](../qualification.md) to choose the test before you make a change.
The runtime suite measures admission, committed actions, effect completion, scheduling and shutdown. The [composition suite](composition/README.md) measures feature composition and downstream builds.

## What counts as a comparison

Use the published baseline identified by the [release procedure](../qualification.md#native-review-and-release).
Record the candidate, reference, and controller separately. The controller supplies the common workload and the rules for evaluating it.
Keep the same toolchain, features, workload, input size, platform, and build settings on both sides. Keep each product’s own implementation and dependency lockfile.

Before the first full release there is no previous released runtime. Report absolute costs, controls, and initial measurements, not a cross-release improvement.
A pull request can still compare two identified runtime revisions for a specific change. Keep that result distinct from the release baseline.

| Measured cost | What the number means |
| --- | --- |
| Latency and throughput | Time per completed operation and completed work per second. Count the useful work, not only the number of outer loops. |
| Scheduler work | The `task_wakes` field counts poll activations. It is not an operating-system context-switch count. |
| CPU | The normal process supplies native CPU counters. A retired instruction count and a process cycle count are different measurements. |
| Allocation | Separate DHAT processes count allocated blocks, bytes, and peak live heap. Do not use an instrumented process to measure normal latency. |
| Memory | macOS reports maximum resident memory and peak footprint. Windows reports peak working set and peak commit charge. Do not compare these across platforms. |
| Build and size | Start the measured build from an empty target. Record the stripped executable's identity and size. See [macOS command counters](darwin-command-resource-accounting.md). |

Both run orders matter. The evaluator combines paired ratios separately by order and then takes their geometric mean.
The release limit is 1.10 for median and p99 latency, inverse throughput, allocation count and bytes, CPU cycles, poll count, and both process-memory measures.
DHAT peak live heap helps explain memory costs. It does not replace the process-memory checks.
A passing limit is not evidence of a speedup. Neither CPU counters nor wall time alone measure energy.

## Commands

On macOS, the [Cargo watchdog](../../CONTRIBUTING.md#build-memory-safety) owns the complete capture.
The [qualification script](../../scripts/qualify-performance.sh) checks the host, power settings, toolchain, clean checkout, and benchmark inputs.
Use new output paths. For a published reference, set `reference` to its verified commit or clean checkout.
For first-release enrolment, set it to the candidate's full commit ID. The collector builds the same product twice to measure repeatability.
Select a timeout before collection:

```sh
mkdir -p target/qualification-evidence
python3 scripts/guard_cargo_memory.py \
  --receipt target/qualification-evidence/qualification-memory.json \
  --timeout 3600 --qualify-performance -- \
  target/qualification-evidence/candidate.json "$reference" target/qualification-evidence/reference.json
```

Do not run the shell collector directly. If the watchdog refuses or stops the build, do not bypass it.
The watchdog uses one Cargo job and disables the compiler cache. A smaller memory ceiling must still leave the required host reserve.

After a successful capture, replay the result from the same controller checkout:

```sh
scripts/compare-performance.sh \
  target/qualification-evidence/reference.json target/qualification-evidence/candidate.json
```

For a past **published** release, check out its measured commit. Set `tag` to its signed release tag.
Keep the original watchdog receipt and measured files at their recorded paths. Run:

```sh
scripts/compare-performance.sh --replay-release "$tag" "$referenceJson" "$candidateJson"
```

This writes a separate `*-historical-comparison.json` report. It checks the measured release and its immediate published predecessor, not today's baseline.
For the first full release, replay checks two independent builds of the same source. There is no earlier runtime to compare.
Historical replay cannot qualify a new release or turn an unpublished PR capture into release evidence.

The fixed-count macOS capture uses ten normal and six allocation repetitions per product and workload. It alternates run order.
Keep the paired observations, source and guard receipts, profiles, and comparison together. The final guard receipt must match the output hashes.
A parser test, changed file, failed guard, or partial capture cannot establish a release pass.

## Windows-native capture

Run all 54 scenarios in the [workload list](../../benches/performance/scenarios.txt). Windows does not produce macOS measurements.
Only the measurement host operator runs these builds and captures.
Use the [specified Windows host](composition/README.md#windows-paired-measurements) and its operator-provided memory guard.
The guard limits the controller and its children to two GiB of committed memory. Closing the Job Object stops its processes.
Keep four GiB each of available physical memory and unused commit capacity for the host.
Check free space for outputs and temporary builds before capture.
The [Windows comparison instructions](../qualification.md#native-review-and-release) require more than 20 GiB free for cancellation and at least 24 GiB for cold builds.
Without the guarded host, request the build and capture receipts from its operator.

Pin the candidate, reference, controller, Rust tools, dependencies, effective source, and executable hashes before measurement.
Inside the guarded job, build the normal and allocation executables for both runtime configurations of each product:

- The native configuration measures the production runtime without test-only features.
- The deterministic configuration measures the test runtime with the features its harness requires.

Preserve the exact source and feature records for each build. Checked reference binaries can come from a local cache.
Check the enrolment hash from an independent record. Never reuse a candidate build for a measured cold build or cache a pass decision.

For each product and runtime, build the normal benchmark and its `qualification-dhat` version separately.
The native build excludes the `rca-test` development dependency from its measurement copy. The deterministic build keeps the original graph.
Keep the locked dependency versions and production Rust source unchanged. The source/build receipt must identify the four source inventories, eight executables, and Cargo transcripts.
The [input checker](../../scripts/capture-windows-performance.py) rejects unexpected manifests, harness changes, or binary identities.

The size checks require a stripped PE file without a debug directory or symbol table.
The [strip helper](../../scripts/windows_strip.py) lists the required Windows x64 LIEF wheel and its SHA-256 hash.
With x64 Python 3.12 or later, set `RCA_WINDOWS_LIEF_WHEEL` to the checked wheel and `RCA_WINDOWS_LIEF_SITE` to a new directory.
Install that local wheel without network access or dependencies:

```powershell
python -m pip install --no-index --no-deps --no-compile --target $env:RCA_WINDOWS_LIEF_SITE $env:RCA_WINDOWS_LIEF_WHEEL
```

The capture checks the installed files before import. LIEF is a measurement tool, not an RCA runtime dependency.

### Early-stop numerical checks

Supply the eight executables and their build receipt to the [Windows collector](../../scripts/capture-windows-performance.py).
Use fresh output and the reviewed controller. Inside the guarded job, set the Bash variables to the recorded paths:

```bash
python -B scripts/capture-windows-performance.py --experimental-rank \
  --candidate-normal "$candidateNormal" --candidate-allocation "$candidateAllocation" \
  --reference-normal "$referenceNormal" --reference-allocation "$referenceAllocation" \
  --candidate-deterministic-normal "$candidateDeterministic" --candidate-deterministic-allocation "$candidateDeterministicAllocation" \
  --reference-deterministic-normal "$referenceDeterministic" --reference-deterministic-allocation "$referenceDeterministicAllocation" \
  --metadata "$buildReceipt" --jq "$jqPath" --output "$native"
```

The collector stops each measurement only when the remaining valid observations cannot change the numerical result.
It retains both run orders. The maximum is 42 normal pairs, ten for the scheduler-budget workload, and six for allocations.
From the matching controller checkout, the coordinator can replay the saved data without another measurement:

```bash
python -B scripts/capture-windows-performance.py --verify-experimental-rank "$native" --release-reference --jq "$jqPath"
```

A valid replay can exit successfully for a failing measurement. Require `numerical_decision: pass`.
The `--release-reference` option also checks the source against the release policy.
For offline replay alone, omit that option. Offline replay does not establish that the reference qualifies for a new release.

For a past **published** Windows release, use its recorded controller and the retained runtime capture:

```sh
python -B scripts/capture-windows-performance.py --verify-experimental-rank "$native" --replay-release "$tag" --jq "$jqPath"
```

This checks the signed, published sources for that capture. Do not combine `--replay-release` with `--release-reference`.
The historical result reports the old numerical decision, not current release acceptance.

For Windows release qualification, also run and replay the [composition checks](composition/README.md#early-stop-windows-capture).
Use the [support collector](../../scripts/capture_windows_support.py) for a clean build, stripped PE sizes, idle working set, five benchmark suites, and the asynchronous-effect allocation profile.
Retain CPU samples from the measured process under both four-producer and eight-producer contention. A timing run cannot replace these two profiles.
Keep traces separate from uninstrumented latency measurements. Protect private paths and machine identifiers before sharing native traces.

Check the source, host, and toolchain across the three records:

```sh
python3 scripts/check_windows_performance_inputs.py \
  --native "$native" --composition "$composition" --support "$support" \
  --controller "$controllerSha" --subject "$subjectSha" --reference "$referenceSha"
```

This command checks identities. It does not produce an overall performance verdict. Require both numerical decisions, support evidence, profiles, and successful guard receipts.
If an input differs or a measurement is incomplete, keep that result open. Do not pool measurements across hosts, runs, or methods.

## Controlled instruction comparison

The [downstream profiling program](../../tests/performance-profile/README.md#controlled-store-costs) also runs nine fixed Store schedules.
It uses a manual runtime and checks action order, state, completed work, and owner destruction. It does not measure native contention.

Use an OS-limited x86-64 Linux host with Callgrind. Keep at least 20 GiB free on the output volume.
Build each product in its own target directory:

```sh
rustup run 1.100.0 cargo build --locked --release \
  --manifest-path tests/performance-profile/Cargo.toml --bin controlled_cost
```

Pin both executables, compiler, workload, and dependencies. Set `output` to a new directory.

```sh
python3 scripts/profile_controlled_cost.py \
  --reference "$referenceBin" --candidate "$candidateBin" --output "$output"
```

Each unchanged product must reproduce both instruction totals and function counts.
An added-work control must raise counts without changing the Store result. A failed control stops the comparison.
Use native measurements to establish latency or throughput. A lower instruction count alone proves neither.
