# Measure composition cost

These checks answer three questions: how fast composed features process work, how much they allocate, and what they cost a downstream build.
The [runtime suite](../method.md) measures the Store and scheduler separately. Neither suite replaces the other.

The [macOS policy](../../../benches/performance/composition-macos.json), [Windows timing policy](../../../benches/performance/composition-windows.json), and [complete Windows policy](../../../benches/performance/composition-windows-complete.json) define the inputs for each platform.
The [budget table](../../../benches/performance/composition-budgets.json) covers ten runtime timings, four compile times, six binary sizes, and thirteen allocation limits.
Three allocation limits require exactly zero. Keep machine inputs with the harness and raw output outside Git.

## Choose the comparison

Use the [release procedure](../../qualification.md#native-review-and-release) to identify the reference.
For the first full release, use a clean checkout of the same signed candidate for the reference. Build each control independently.
This records repeatability and absolute costs, not performance relative to an earlier release.
For a relative claim, measure a candidate against the reference, and two independent builds of the reference against each other.
The second comparison is a control: a difference there cannot come from changes to RCA.

Use the same compiler, workloads, run order, host, and build settings throughout the capture.
Never change a limit or choose a different stopping rule after seeing a result.
Start timed cold builds from empty targets. A warm compiler cache does not measure the cost of a clean build.
Build and explicitly strip each measured executable. A successful Cargo build does not prove that stripping succeeded.

## Windows paired measurements

From a Visual Studio Developer shell, use the host and toolchain specified by the [Windows policy](../../../benches/performance/composition-windows.json).
The policy fixes the CPU, operating system, compiler, thread placement, and memory limits.
Keep the fixed two-second window for each isolated runtime workload. Retain the actual sample and iteration counts.
A process can also spend time on setup and cleanup outside that window.

The collector starts each command in its own Windows Job Object. The job limits committed memory across the command and its children to two GiB.
The runner checks the path, version, and hash of the Visual Studio telemetry image under `VSINSTALLDIR` before stopping it.
The image and path must match across the paired capture and replay. Other child processes must exit normally.
Use the guards specified by the collector. A guard failure or an incomplete command is not a passing result.

### Early-stop Windows capture

Use a clean subject checkout and a clean reference checkout. Preserve Git line endings in both (`core.autocrlf=false`).
Set `$subjectSha` and `$controllerSha` to the full hashes selected before capture. Use a new output path.
On Windows, run the following from the controller checkout:

```powershell
python -B scripts/composition_runner.py --platform windows --windows-complete --experimental-rank --subject $subject --expected-subject $subjectSha --expected-controller $controllerSha $output $reference
python -B scripts/composition_runner.py --verify-experimental-rank $output --expected-subject $subjectSha --expected-controller $controllerSha
```

The collector runs each workload until the remaining valid samples cannot change its decision, up to the fixed limit.
It does not shorten a timed workload or discard a slow observation. Each new block includes the independent reference control.
Run both fixed-count modes if the selected protocol requires all observations:

```powershell
python -B scripts/composition_runner.py --platform windows --windows-complete $output $reference
```

Check the replay's `decision` as well as its exit status. A correct replay of a failed capture can exit successfully.
Acceptance requires both comparison modes and every product check to pass. The `release_authorised: false` field distinguishes this result from approval to release.

To replay a past **published** Windows capture, use the clean checkout of its original controller.
Set `$tag` to the measured subject's signed release tag. Use the exact controller and subject commit IDs recorded for that capture:

```powershell
python -B scripts/composition_runner.py --verify-experimental-rank $output --replay-release $tag --expected-controller $controllerSha --expected-subject $subjectSha
```

The controller and subject can be different commits. Replay checks the tagged subject and its immediate published predecessor.
For the first full release it checks two independent builds of the same source. Read `historical_decision`; `current_qualification` is false.
This route cannot qualify another candidate. An offline numerical replay alone does not authenticate the published sources.

### Interpret the limits

The Windows evaluator derives timing and size limits from the independent reference measurements, not from the candidate.
For time, the upper limit is the reference median times 1.10. For throughput, the lower limit is the reference median divided by 1.10.
The size limit is the stripped reference PE size times 1.10, rounded up to the next whole byte.
The allocation limits are fixed. Run the allocation workloads and both zero-allocation test hosts.

Both run orders matter. A failed control or missing record invalidates the comparison.
A control can detect interference. It does not prove that all samples are independent or that latency is insensitive to load.
Combine this result with the [native runtime, CPU profiles, and input checks](../method.md#early-stop-numerical-checks). A Windows pass does not qualify macOS.

## macOS capture

The [macOS policy](../../../benches/performance/composition-macos.json) sets the required host conditions, absolute limits, and paired decision rule.
From the candidate checkout, select a new output path under `target/qualification-evidence/composition`. Use the [guarded build route](../../../CONTRIBUTING.md#build-memory-safety):

```sh
scripts/qualify-composition-performance.sh --mode candidate-reference "$output" "$reference"
```

Do not run the underlying capture script directly. The controller guards each command, including preparation and absolute measurements.
The guard requires its selected memory ceiling plus four GiB of available host memory.
Both modes use the same controls, preserve their work order, and record all twenty paired blocks.
Require the combined `complete.json` and all relative and absolute checks. A binary-only result does not establish compile or runtime speed.

## Keep the evidence

Store the protocol, source and binary identities, ordered samples, allocation and size reports, and guard receipts with the qualification record.
If you reuse reference binaries, check their hashes against a trusted record kept outside the cache. Build the candidate afresh.
Replay with the controller recorded in the capture. An older run cannot qualify changed inputs.
