# Benchmark RCA

A benchmark measures the cost of a specific piece of work.
Use this guide to measure RCA itself. To improve an application, start with [application performance](../usage/performance.md).

| Your task | Where to start |
| --- | --- |
| Try a workload on your computer | [Run one workload](#run-one-workload). No release equipment is necessary. |
| Investigate a change to RCA | [Compare a change](#compare-a-change). First establish that both versions return the right result. |
| Check a release candidate | [Prepare a release comparison](#prepare-a-release-comparison), then use the [macOS](#measure-on-macos) and [Windows](#measure-on-windows) procedures. |

Run commands from the root of the relevant RCA checkout.

## Run one workload

Start with the [effects example](../../tests/performance-profile/README.md#run-one-workload).
It sends 100 actions to a Store, the part of RCA that owns the program's state.
Each action produces 16 replies.
The program checks the result and prints one JSON record.
The example gives commands for a guarded Mac build and an isolated Linux build. Neither needs a private machine profile.

For a different API, [choose a workload and its Cargo features](../../tests/performance-profile/README.md#choose-a-workload-and-its-features).
For the cost of compiling RCA in an application, try a [small consumer build](../../benches/compile/README.md).
Running a workload once tells you whether it works, not whether RCA got faster.

## Compare a change

Use the [qualification guide](qualification.md#choose-performance-evidence-for-the-change) to choose an appropriate comparison.
The **candidate** is the code under review. The **reference** is the code you compare it with.
The **controller** runs the same workload on both and evaluates the results.

1. Identify the result that matters to the caller. For example, count completed imports, not requests to start an import.
2. Run both revisions on the same machine. Use the same workload, inputs, compiler, Cargo features, and build settings.
3. Keep each revision's own implementation and locked dependencies. Check that both products do the same useful work.
4. Alternate which revision runs first. Report how much the results vary.
5. If you claim a speedup, measure the time to complete the work or the completed work per second.

Do not scale times between different machines. A busy host can affect the two runs differently.
An unchanged reference compared with an independent build of itself acts as a **control**.
A difference in this comparison cannot come from the proposed code.

**Example: fewer allocations per action.** Build the [profiling program](../../tests/performance-profile/README.md) for both revisions with `deterministic,local,allocations`.
Set `parent_bin` and `candidate_bin` to the absolute paths of the two executables. Run:

```sh
python3 scripts/compare_change_resources.py --baseline-bin "$parent_bin" \
  --candidate-bin "$candidate_bin" --families dispatch
```

Compare the number and size of memory allocations for the same completed actions.
Passing the limit for an increase does not establish an improvement.
Run native latency measurements separately. Counts of allocations or instructions do not measure elapsed time or energy.

### Count instructions

Use this optional comparison to explain a change in Store work. It controls the schedule, not the timing of real concurrent callers.
Use an isolated x86-64 Linux host with an operating-system memory limit and Callgrind.
Keep at least 20 GiB of disk space free.
From each product checkout, build the [nine fixed Store workloads](../../tests/performance-profile/README.md#controlled-store-costs) in separate target directories:

```sh
rustup run 1.100.0 cargo build --locked --release \
  --manifest-path tests/performance-profile/Cargo.toml --bin controlled_cost
```

Set `referenceBin` and `candidateBin` to the two executable paths. Set `output` to a new directory. Run:

```sh
python3 scripts/profile_controlled_cost.py \
  --reference "$referenceBin" --candidate "$candidateBin" --output "$output"
```

The program checks action order, final state, completed work, and destruction of the owners.
Each unchanged executable must reproduce its instruction totals and function counts.
A control adds work without changing the answer. If the counts do not rise, the comparison fails.
For controlled task costs, use the [separate task profiler](../../tests/performance-profile/README.md#native-task-and-fold-costs).

### Investigate cancellation or a slow build on Windows

Use these optional checks to investigate a change before the full release capture.
Use the [guarded Windows host](#measure-on-windows). Retain the guard's receipt for the controller and all children.
Both checks use both run orders and an unchanged control. Neither can qualify a release.

**Cancellation.** Set `output` to a new directory on a volume with more than 20 GiB free.
Set `bindings` to the reviewed [build record](../../scripts/composition_cancellation_exposure.py) for the candidate and two independent reference builds.
Set `trustedDigest` to its independently recorded SHA-256 digest. Inside the guarded job, run:

```powershell
python -B scripts/composition_cancellation_exposure.py capture $output --builds $bindings --builds-sha256 $trustedDigest
```

Record the capture digest outside the output directory. Set `captureDigest` to that digest for replay:

```powershell
python -B scripts/composition_cancellation_exposure.py replay $output --builds $bindings --builds-sha256 $trustedDigest --capture-sha256 $captureDigest
```

The result measures median removal cost over 1,024 operations per process, not the slowest requests.
Use `--witnessed-candidate` on capture and replay only for the deliberately delayed control.

**Clean build.** Set `base` and `head` to the full signed reference and candidate commits.
Set `output` to a new directory outside the checkout. Keep at least 24 GiB free throughout collection.
To compare the example that generates child paths with macros, run:

```powershell
python -B scripts/composition_compile_portfolio.py --base $base --candidate $head --fixture composed-macro --output $output --collect-only
```

The check starts eight single-job Cargo commands from empty targets. With `--collect-only`, exit zero means only that collection finished.
After the guard finishes normally, read `screen.json`. Require `decision: pass`. A missing or inconclusive decision does not pass.

If the first result is inconclusive, the [compile comparison](../../scripts/composition_compile_portfolio.py) permits one fresh confirmation.
Keep the same fixture, revisions, guard settings, 24 GiB reserve, and limit. Set `newOutput` to a new directory.
Set `firstScreen` to the original `screen.json` and `firstDigest` to its independently recorded digest:

```powershell
python -B scripts/composition_compile_portfolio.py --base $base --candidate $head --fixture composed-macro --output $newOutput --collect-only --confirm $firstScreen --confirm-sha256 $firstDigest
```

The confirmation runs up to twenty fresh blocks. It can stop once the candidate can no longer pass.
A failed candidate with an unstable control does not establish the cause. Read the saved decision after the guard finishes.

## Prepare a release comparison

Release checks cover the runtime and composition suites on both macOS and Windows.
The runtime suite measures the Store, effects, scheduling, and shutdown.
The composition suite measures the cost of combining features, including the cost to build applications.
A platform result cannot qualify another platform.

1. Use clean checkouts. Before running any code on a release machine, review the exact source.
2. Use the measured candidate as the controller for this procedure.
   Record the full signed commit for each role. Release-bound Windows runtime replay requires the measured candidate checkout.
3. From a checkout with the remote tags available, find the previous full release:

   ```sh
   python3 scripts/release_baseline.py revision
   ```

   The resolver checks GitHub and crates.io. If it returns a commit, use that signed release and its lockfile as the reference.
   If the checkout is the signed candidate tag, pass `--candidate-tag "$tag"`, where `tag` names that tag.
   Set `RCA_QUALIFICATION_TAG` to the candidate tag for that capture.

   If the resolver returns `none`, report “no prior released runtime”. Use the signed candidate as its own reference.
   Use independent builds for the controls. Keep the absolute measurements and deliberate regression checks.
   Do not describe the first full release as faster than an earlier runtime. If resolution fails, stop.

4. Before measurement, record the workloads, useful results, limits, run order, and stopping rules.
   If a result is inconclusive, use only the extension its rules permit. Do not discard slow runs or retry for a pass.

### Supply the private runner profile

The public rules specify the work, comparisons, allocation limits, and required measurements.
A **runner profile** identifies the approved machine. The macOS profile also holds its measured timing, build, and size limits.
The macOS runtime, both composition suites, and the Windows support collector check the relevant profile.
The Windows runtime collector instead records the machine. A final comparison matches its record to the other two Windows records.

Set `RCA_RUNNER_PROFILE_DIR` to an absolute directory outside the checkout.
Use the operator's approved `macos.json` or `windows.json`. The controller checks the file's SHA-256 digest, a fingerprint of its exact bytes.
If the profile is missing or changed, stop. Do not use local timings as release evidence.
Keep the profile, raw output, and traces outside Git.

### Measure composition

The [macOS rules](../../benches/performance/composition-macos.json) and [Windows rules](../../benches/performance/composition-windows.json) fix the workloads and run order.
The [budget table](../../benches/performance/composition-budgets.json) covers ten runtime timings, four compile times, six executable sizes, and thirteen allocation limits.
Three allocation limits require zero. Run the allocation workloads and both Windows zero-allocation test programs.

The composition controller compares the candidate with the reference and two independent reference builds with each other.
Both comparisons and every absolute check must pass. A missing sample or a failed control invalidates the comparison.
Clean builds start from empty targets. An empty target does not clear the operating system's file cache.
Measure the size of the stripped executable, not the unstripped build.

The [complete Windows rules](../../benches/performance/composition-windows-complete.json) derive time and size limits from the reference measured in that run.
Time cannot exceed the reference median times 1.10. Throughput cannot fall below the reference median divided by 1.10.
For stripped executable size, the limit is the reference size times 1.10, rounded up to a whole byte.

For either composition suite, use a fresh shell without compiler wrappers, profile overrides, or Cargo target settings.
The [controller](../../scripts/composition_runner.py) rejects inherited settings such as `RUSTC`, `RUSTDOC`, and `RUSTFLAGS`.
Choose a **new** output directory below `target/qualification-evidence/composition`. It must not exist when the run starts.

### Measure on macOS

Run the commands from the candidate checkout. Use Xcode Instruments for the CPU profiles and the [Cargo memory guard](../../CONTRIBUTING.md#build-memory-safety).
The guard runs the complete runtime capture with one Cargo job and no compiler cache.
Before you start, select a timeout and allow for the guard's memory reserve. Do not bypass a stopped guard.

**Runtime.** Set `reference` to the verified reference commit. Use fresh receipt and result paths:

```sh
mkdir -p target/qualification-evidence
python3 scripts/guard_cargo_memory.py \
  --receipt target/qualification-evidence/qualification-memory.json \
  --timeout 3600 --qualify-performance -- \
  target/qualification-evidence/candidate.json "$reference" target/qualification-evidence/reference.json
```

The [collector](../../scripts/qualify-performance.sh) checks the machine, power settings, tools, source, and inputs.
For each workload and product, it alternates the order of ten normal measurements and six separate allocation measurements.
It also collects the CPU profiles for four and eight concurrent producers. Do not run the underlying shell collector directly.

After the guard succeeds, replay the comparison from the same controller checkout:

```sh
scripts/compare-performance.sh \
  target/qualification-evidence/reference.json target/qualification-evidence/candidate.json
```

**Composition.** Set `referenceCheckout` to a separate clean checkout at the reference commit, not the commit ID.
Set `output` to the new composition directory. Run:

```sh
scripts/qualify-composition-performance.sh --check-prerequisites
scripts/qualify-composition-performance.sh --mode candidate-reference "$output" "$referenceCheckout"
```

The first command checks the required tool versions, including the checkout's [cargo-bloat installation](../../CONTRIBUTING.md#coverage-evidence).
The second guards preparation and measurement and runs both comparisons in one capture.
It records all twenty paired blocks. Require `passed: true` in the combined `complete.json` and matching guard receipts.

### Measure on Windows

Use a Visual Studio Developer shell on the approved Windows host. Only its operator runs these captures.
Use the operator's Windows Job Object: it limits the controller and its children to two GiB of committed memory.
Closing the job stops its processes. Reserve four GiB each of available physical memory and unused commit capacity for the host.
The commands below do not supply this outer guard. Without it, request the operator's results instead.

**Tools.** The size check uses a Windows x64 Python 3.12 or newer and the [pinned LIEF wheel](../../scripts/windows_strip.py).
LIEF removes debug information from an executable. It is not an RCA dependency.
Set `RCA_WINDOWS_LIEF_WHEEL` to the wheel with the specified digest. Set `RCA_WINDOWS_LIEF_SITE` to a new installation directory.

```powershell
python -m pip install --no-index --no-deps --no-compile --target $env:RCA_WINDOWS_LIEF_SITE $env:RCA_WINDOWS_LIEF_WHEEL
python -B scripts/composition_runner.py --platform windows --windows-complete --check-prerequisites
```

**Runtime.** Prepare normal and allocation-counting executables for both the native runtime and the deterministic test runtime.
Each product needs all four executables, even for the first release.
The [collector](../../scripts/capture-windows-performance.py) consumes eight executable paths and a source/build receipt. It does not create those inputs.
The receipt identifies the four source inventories, exact Cargo commands, transcripts, toolchain, and executable hashes.
For native measurement copies, exclude the `rca-test` development dependency. For the deterministic runtime, retain the original dependency graph.
Do not change production Rust source or the locked dependency versions.

Set each executable variable below to its corresponding full path. Set `buildReceipt` to the checked receipt.
Set `jqPath` to the path to the `jq` executable. Set `native` to a new output directory.
From the controller checkout, run these commands inside the guarded job:

```powershell
python -B scripts/capture-windows-performance.py --experimental-rank `
  --candidate-normal $candidateNormal --candidate-allocation $candidateAllocation `
  --reference-normal $referenceNormal --reference-allocation $referenceAllocation `
  --candidate-deterministic-normal $candidateDeterministic --candidate-deterministic-allocation $candidateDeterministicAllocation `
  --reference-deterministic-normal $referenceDeterministic --reference-deterministic-allocation $referenceDeterministicAllocation `
  --metadata $buildReceipt --jq $jqPath --output $native
python -B scripts/capture-windows-performance.py --verify-experimental-rank $native --release-reference --jq $jqPath
```

The collector runs all [54 workloads](../../benches/performance/scenarios.txt).
It stops a measurement early only if the remaining permitted observations cannot change the decision.
The limits are 42 normal pairs, ten pairs for the scheduler-budget workload, and six pairs for allocations.
Require `numerical_decision: pass`. A successful replay of a failing result is not a pass.

**Composition.** Set `subject` and `referenceCheckout` to the clean candidate and reference checkout paths. Preserve Git line endings in both (`core.autocrlf=false`).
Set `subjectSha` and `controllerSha` to the recorded candidate and controller commit IDs respectively.
Set `composition` to the new composition directory. From the controller checkout, run:

```powershell
python -B scripts/composition_runner.py --platform windows --windows-complete --experimental-rank --subject $subject --expected-subject $subjectSha --expected-controller $controllerSha $composition $referenceCheckout
python -B scripts/composition_runner.py --verify-experimental-rank $composition --expected-subject $subjectSha --expected-controller $controllerSha
```

Each block includes an independent reference control. Isolated runtime workloads retain their two-second measurement window.
Require `decision: pass` for the complete capture. The result cannot authorise a release by itself.
The final input check requires the records from both `--experimental-rank` commands. Fixed-count output is not a substitute.

**Additional measurements.** Use the [support collector](../../scripts/capture_windows_support.py) with the candidate's original build inputs and executable paths.
Use its `--help` for the source manifest, build receipt, tools, revision, and output parameters.
For separate source builds, also supply the independently recorded `--source-build-sha256`.
Require a clean build, stripped executable sizes, idle working set, five benchmark suites, and the asynchronous-effect allocation profile.

The collector writes two commands to `cpu_profiles.required_commands` in `support.json`. It does **not** capture their CPU samples.
Run those commands separately under Windows Performance Recorder. One uses four concurrent producers and the other uses eight.
Keep CPU samples attributed to each measured process. A trace hash alone does not identify whose CPU time it contains.
Keep profiling runs separate from normal latency measurements.

**Match all three records.** Set `support` to the **file** `support.json`, not its directory.
Set `referenceSha` to the full reference commit. Reuse the native and composition directories from this run:

```powershell
python -B scripts/check_windows_performance_inputs.py --native $native --composition $composition --support $support --controller $controllerSha --subject $subjectSha --reference $referenceSha
```

This check compares source, host, and toolchain identities. It does not decide whether all costs passed or whether both CPU profiles are present.
Require both numerical passes, the support measurements, both CPU profiles, and successful guard receipts.

## Read and retain the result

The [runtime evaluator](../../scripts/native_performance_rows.jq) checks the applicable metrics against the 1.10 ratio.
It combines paired results separately for each run order, then takes their geometric mean.
The median describes a typical observation. The 99th percentile (p99) describes the slow end of the distribution.

| Measurement | What it does and does not show |
| --- | --- |
| Latency and throughput | Time per completed operation and completed work per second. Count the useful work, not just outer loops. |
| CPU and scheduler | Cycles and retired instructions count different things. `task_wakes` counts scheduler poll activations, not operating-system context switches. |
| Allocation | Separate instrumented runs use DHAT to count allocated blocks, bytes, and peak live heap. They do not measure normal latency. |
| Process memory | macOS reports maximum resident memory and peak footprint. Windows reports peak working set and peak commit charge. These measures are not equivalent across platforms. |
| Shutdown | The fixed-live and bounded-burst cases check memory. Rapid create/drop checks the other costs. Peak live heap does not replace process memory. |
| Build | On macOS, `/usr/bin/time -lp` separates elapsed time, user CPU time, and system CPU time. Maximum resident size is the largest process, not the concurrent total for Cargo and rustc. |

Record a rounded zero as zero, not a positive value chosen for a ratio.
For an incremental build, record the edit and the existing build state. For a clean build, start with an empty target.
Keep the private profile and digest, signed source identities, tool versions, workload rules, ordered samples, and guard receipts together.
Retain failures. Before sharing traces, remove private paths and machine details from the copy you share.
Reuse reference binaries only with independent records of their provenance and hashes. Do not cache a release decision.

**Historical replay.** Replay reproduces a decision from saved data. It does not measure new code.
Fetch the signed, published release tag. Set `tag` to its name. Unset `RCA_QUALIFICATION_TAG` before historical replay.
Use the original inputs, approved profile, and guard receipt at their recorded paths.
For runtime replay, use the measured candidate checkout. For Windows composition replay, use its recorded controller checkout.
On macOS, set `referenceJson` and `candidateJson` to their recorded file paths:

```sh
scripts/compare-performance.sh --replay-release "$tag" "$referenceJson" "$candidateJson"
```

On Windows, keep the native and composition directories and their recorded commit IDs:

```powershell
python -B scripts/capture-windows-performance.py --verify-experimental-rank $native --replay-release $tag --jq $jqPath
python -B scripts/composition_runner.py --verify-experimental-rank $composition --replay-release $tag --expected-controller $controllerSha --expected-subject $subjectSha
```

Historical replay checks the published sources for the old comparison, not the current candidate.
Do not use `--release-reference` and `--replay-release` together.
