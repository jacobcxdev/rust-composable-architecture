# RCA qualification

Start with the change you need to qualify. A local test, a hosted CI result and
a published package answer different questions. Each result applies only to its
recorded source and configuration.

- For a pull request, [choose the checks for the change](#choose-performance-evidence-for-the-change).
- For a release, use the [native qualification instructions](#native-review-and-release).
- To reuse previous results, check [what the evidence covers](#what-the-evidence-covers).

## Choose performance evidence for the change

Start with the cost a caller can observe. A one-line change to the Store can affect every dispatch. A guide edit usually affects none.
Correctness, coverage and packaging keep their own checks. A performance result cannot prove ordering, cancellation safety or shutdown correctness.

| Change | Comparison and decision |
| --- | --- |
| Documentation or tests only | Run the applicable documentation or behaviour tests. Skip performance measurements only when the edit changes neither shipped behaviour nor the measurement itself. |
| Bug fix | Reproduce the bug. Compare the repaired path and its nearest caller with the parent revision. If the fix now completes work the old version skipped, report correctness first. Do not call the extra work a comparable slowdown. |
| New capability | Compare the existing workload with the capability off. With it on, measure useful work, including setup and cleanup. Give new behaviour an initial cost, not an invented historical result. |
| Optimisation | Before editing, name the workload, metric and likely trade-off. Compare with the parent. Confirm the gain on a representative host workload before claiming lower latency or higher throughput. |
| Shared runtime, build or dependency | Include affected consumers and interactions. For a compiler upgrade, run both products on the same compiler. Separately measure unchanged RCA on the old and new compilers to identify the toolchain effect. |
| Merge to `main` or release | Compare cumulative cost with the last full release. Before the first full release, measure the initial cost and run the controls. There is no previous released runtime to compare. |

### What runs on a pull request

When [CI selects a performance check](../CONTRIBUTING.md#documentation-and-checks), it compares the PR head with its branch point.
If neither the branch point nor a full release contains a comparable runtime, there is no relative PR result.
A documentation-only pass is not a new measurement.
On pushes to `main` or `dev/next`, the job selects the workloads that changed and measures them against the release commit.
If the previous push is unavailable, selection covers the full difference from the release.
Use the signed, published release as the reference. Its tag or a GitHub Release page alone does not prove publication on crates.io.
The snapshot-only preview has no runtime and is not a performance baseline.

To see which families a committed change selects, use both full commit IDs:

```sh
python3 scripts/performance_scope.py --base "$base" --head "$head"
```

Unknown paths select all families. If a change also affects a caller outside the selection, include that caller in the PR evidence.
For an extra allocation comparison, pass the affected runtime families to `--families` as in the optimisation example below.
For a change to native timing or build time, measure the affected workload on the host required by its protocol.
For selected runtime families, CI builds the same downstream profiling program against each product. It compares allocations for the same completed work.
The job retains the revisions, measurements, compiler and elapsed time. A docs-only skip is not a measured pass.
A cold-build-only selection has no runtime allocation rows. It needs separate native evidence for a compile-time claim.

| Workload | What to look for |
| --- | --- |
| Store dispatch | Allocations per completed action; native latency for an admission or reply change. |
| Keyed routing | Work per routed action, including replacement and stale identity. |
| Queue pressure | A full bounded queue must return the unaccepted payload. Measure throughput and end-to-end latency separately under contention. |
| Cancellation and shutdown | Wait for real work to stop. Short trials can miss a fault that starts only after hundreds of cancellations. |
| Downstream compilation | Time the caller's complete command from an empty target, not an incremental rebuild. Measure each affected consumer separately. |
| Combined headless worker | The existing `mixed-pool-shared-ordered` workload fills a one-slot mailbox, replaces workers, cancels work and finishes shutdown. |

Use counts to explain work and native time to measure speed. Neither allocations nor instructions establish energy use.
A change can save allocations and still make the slowest requests slower. Report both results when they affect callers. Do not average them into one score.

### Native review and release

For a Windows performance claim, use the hardware and limits in the [Windows protocol](performance/composition/README.md#windows-paired-measurements).
Review the exact source before you run it on a measurement host. Do not run unreviewed pull request code there.
Keep the absolute limits that the selected protocol requires and the 10% relative limit.
Each host has its own [composition policy and limits](performance/composition/README.md). Measurements from one host do not qualify another.
Before measuring, select the useful work, controls, stopping rule and permitted extension.
An inconclusive result does not pass. Extend only the affected measurement under its stated rule, or use the established qualification method.
Do not retry until a favourable result appears.

For a Windows cancellation change, use the two-GiB outer guard and both four-GiB host reserves.
Pin the build records for the candidate and two independent builds of the reference.
Before capture, verify the source revisions, image hashes, compiler, workload and build configuration.
The build bindings name these inputs. A hash of an unchecked bindings file does not verify the builds.

```powershell
python -B scripts/composition_cancellation_exposure.py capture $output --builds $bindings --builds-sha256 $trustedDigest
python -B scripts/composition_cancellation_exposure.py replay $output --builds $bindings --builds-sha256 $trustedDigest --capture-sha256 $captureDigest
```

Keep the capture digest outside the evidence directory. Replay needs that independent pin.
The screen checks median removal cost over 1,024 complete operations per process. It does not prove p99 latency.
Use `--witnessed-candidate` only for the deliberate delayed fault, on capture and replay.

For a compiler-facing change, use the same guard and a new output directory outside the checkout.
Choose the fixture that compiles the affected consumer. For generated child paths, use:

```powershell
python -B scripts/composition_compile_portfolio.py --base $base --candidate $head --fixture composed-macro --output $output
```

This comparison starts eight single-job Cargo commands from empty targets, including separate unchanged controls.
Use the outer guard receipt to verify that all processes stopped normally.
If the guard cannot retain a receipt for an expected nonzero exit, add `--collect-only` to the cold-build command.
Exit zero then means only that collection finished. After verifying the guard receipt, read the separate decision:

```powershell
$screen = Get-Content -Raw -LiteralPath (Join-Path $output 'screen.json') | ConvertFrom-Json
switch ($screen.decision) {
    'pass' { 'Selected cold-build screen passed. No release decision.' }
    'fail' { throw 'Selected cold-build cost exceeded the limit.' }
    default { throw 'Missing or inconclusive cold-build evidence.' }
}
```

Both screens use two run orders. A split decision or unstable control needs further evidence.

If the cold-build screen is inconclusive, keep its original result and its SHA-256 digest.
For the same fixture and revisions, run the bounded comparison once with the same Windows memory guard:

```powershell
python -B scripts/composition_compile_portfolio.py --base $base --candidate $head --fixture composed-macro --output $newOutput --collect-only --confirm $firstScreen --confirm-sha256 $firstDigest
```

Use a new output directory. The comparison starts fresh. It does not reuse the eight measurements from the first screen.
It uses the original order and limit for up to 20 blocks. It can stop early when the candidate can no longer pass.
An early rejection does not establish the cause while the unchanged control remains uncertain.
Read the saved decision after the guard finishes. An incomplete or inconclusive result does not pass.

The hosted job measures resource use for selected workloads. It does not measure native latency, cold-build time, energy or release readiness.
For a release, use the integrated candidate and the existing [native](performance/method.md) and [composition](performance/composition/README.md) checks.
Reuse a result only when its source, workload, dependencies, configuration, toolchain and host conditions still apply.
Missing evidence stays open. Never relabel a focused PR comparison as a release result.

Use a clean release checkout with the remote tags available. The resolver needs read access to GitHub and crates.io.
Before a release capture, identify the published baseline:

```sh
python3 scripts/release_baseline.py revision
```

If the result is `none`, report “no prior released runtime”. Do not substitute a development commit or call a same-source comparison a speedup.
Keep the absolute limits, independent controls, deliberate regression checks, and initial measurements.
After publication, [record the first full release](releasing.md#record-the-performance-baseline) as the reference for future work.

For a later release, use the returned commit. It identifies the signed tag, source tree, and dependency lock that the measurements must match.
If the resolver reports an error, stop. Missing evidence, a partial publication, or a mismatched tag is not an initial release.
If you qualify a signed release tag at the exact checkout tip, select its predecessor with `revision --candidate-tag "$tag"`.
For the native collector, set `RCA_QUALIFICATION_TAG` to that same tag. It cannot select another commit as the baseline.
Keep the source, workloads, toolchain, protocol, and hashes of the measurement files with the release record.
For replay, use that exact controller and those inputs. A new baseline does not change a previous result.

## Start with the changed behaviour

For a change to typed task names, run the Shared and Local behaviour and allocation tests together.
On macOS, use the memory guard with a new receipt. With the locked dependencies already downloaded, run:

```sh
mkdir -p target
receipt_dir=$(mktemp -d target/task-check.XXXXXX)
CARGO_PROFILE_TEST_DEBUG=0 CARGO_INCREMENTAL=0 python3 scripts/guard_cargo_memory.py \
  --receipt "$receipt_dir/memory.json" --memory-limit-gib 2 --timeout 300 -- \
  test --offline --locked -p rca-core --features local,allocation-measurement \
  --test named_operation_tasks --test local_named_operations \
  --test named_operation_allocations --test nested_task_allocations
```

This command checks cancellation, ownership and allocation assertions. It does not measure native latency or compile cost.
For a change to task registration, compare the four named and unnamed task workloads on the Linux profiling host.
Use the build artefact for the exact candidate and baseline commits. Set `output` to a directory that does not exist.
If your change must not increase instruction counts, run:

```sh
python3 scripts/profile_task_fold_cost.py "$artifact" "$candidate" "$baseline" "$output" --tasks-only &&
  jq -e '.complete == true and .scope == "tasks-only" and
  (.comparisons | length == 4 and all(.[]; .candidate <= .baseline))' "$output/comparison.json"
```

The profiler checks that the counts repeat. The second command fails if any task workload uses more instructions.
Neither command measures native timing or energy use.

For a scheduler change, test the poll limit before measuring its cost. On macOS, use a fresh memory receipt:

```sh
mkdir -p target
receipt_dir=$(mktemp -d target/budget-check.XXXXXX)
CARGO_PROFILE_TEST_DEBUG=0 CARGO_INCREMENTAL=0 python3 scripts/guard_cargo_memory.py \
  --receipt "$receipt_dir/memory.json" --memory-limit-gib 2 --timeout 240 -- \
  test --locked -p rca-test --test conformance_deterministic \
  failed_factory_advance_retains_work_without_committing_requested_time -- --exact
```

At 100,000 polls, the task must remain queued, own its value and leave virtual time unchanged.
The next poll must complete the task and release the value. The release benchmark measures the cost of reaching that boundary.

For a change to generated child paths, use the same memory guard with a new receipt and these Cargo arguments:

```sh
check --offline --locked --manifest-path benches/compile/Cargo.toml -p rca-compile-composed-macro
```

This fixture compiles required, optional and keyed children. Keep its build directory between edits.
Use runtime tests for routing behaviour. Use fresh build directories for the release measurement of cold compile time.

For release review, start with the last accepted evidence for each affected suite.
If its measured inputs, toolchain, runner conditions and contract still apply, reuse the complete evidence.
For example, `scripts/compare-performance.sh REFERENCE.json CANDIDATE.json` checks the native pair against the checkout and recomputes the result.
Use the checkout that matches the recorded collector. An earlier collector's result remains evidence for that earlier run.
Keep the accompanying source and guard receipts as specified in the [performance method](performance/method.md).
Replay checks a past measurement. It cannot establish how a changed executable performs.
If the evidence is missing, inapplicable or uncertain, run the affected qualification on the final candidate.
Do not substitute a focused test for a release measurement or combine rows from different runs.

## Examples: feature costs, optimisations and compiler upgrades

These examples measure allocations for fixed work. They do not measure elapsed application time, energy use or cold builds.
Use an isolated Linux runner with a memory limit, sufficient disk space and the locked dependencies available offline.
On macOS, use the [Cargo memory guard](../CONTRIBUTING.md#build-memory-safety) instead of running this build function.

Set `parent_src` and `candidate_src` to clean checkouts at the exact revisions under review.
Keep the workload, allocation instrument, target and profile identical for each comparison pair.
If the workload or instrument changed, first establish a common measurement on both products.
This Bash function keeps build outputs separate for each revision and compiler:

```bash
set -euo pipefail
results=$(mktemp -d)
measure() {
    local label=$1 src=$2 tool=$3 profile=$4 features=$5 revision
    shift 5
    revision=$(git -C "$src" rev-parse HEAD)
    (
        cd "$src"
        export CARGO_TARGET_DIR="$results/build/$revision/$tool"
        export RUSTC RUSTDOC
        RUSTC=$(rustup which --toolchain "$tool" rustc)
        RUSTDOC=$(rustup which --toolchain "$tool" rustdoc)
        rustup run "$tool" cargo run --offline --locked -j 1 --profile "$profile" \
            --manifest-path tests/performance-profile/Cargo.toml --bin rca-performance-profile \
            --no-default-features --features "$features" -- "$@"
    ) > "$results/$label.json"
}
```

**New feature.** For example, measure 64 completed dispatches with the Cargo feature `diagnostics` absent and present.
The workload does not install a diagnostic observer. This isolates the cost that existing dispatch callers pay:

```bash
common=deterministic,allocations
dispatch=(topology-deterministic 1 0 64)
measure parent-off "$parent_src" 1.100.0 release "$common" "${dispatch[@]}"
measure candidate-off "$candidate_src" 1.100.0 release "$common" "${dispatch[@]}"
measure candidate-on "$candidate_src" 1.100.0 release "$common,diagnostics" "${dispatch[@]}"
jq -es 'length == 3 and all(.[]; .empty_root_causal_completed == 64 and .owners_released == true)' \
    "$results"/{parent-off,candidate-off,candidate-on}.json
jq '.allocations.phases.work | {allocated_blocks, allocated_bytes}' "$results"/{parent-off,candidate-off,candidate-on}.json
```

Compare parent-off with candidate-off for existing callers. Compare candidate-off with candidate-on for the cost of enabling the feature.
Next, measure the capability in use. The following two workloads need the Cargo feature in **both** builds.
Use the debug profile because a normal release build removes the observed checkpoint callbacks.

```bash
measure disabled "$candidate_src" 1.100.0 dev "$common,diagnostics" diagnostic-disabled 2 64
measure observed "$candidate_src" 1.100.0 dev "$common,diagnostics" diagnostic-observed 2 64
jq -e '.mode == "explicit-disabled" and .work_facts.sink_calls == 0' "$results/disabled.json"
jq -e '.mode == "debug-observed" and .work_facts.sink_calls == 64' "$results/observed.json"
```

These last two measurements describe the cost of using the debug observer. They do not describe release-mode dispatch.
If the parent also supports the feature, repeat the enabled case on the parent for a like-for-like comparison.

**Optimisation.** For a dispatch change, name the intended saving before editing, such as fewer allocated blocks for 64 completed actions.
Use the revised implementation and its parent, not an older release with unrelated changes.
Build each profiler with the same `deterministic,local,allocations` configuration.
Pass their absolute executable paths to the PR comparison:

```sh
python3 scripts/compare_change_resources.py --baseline-bin "$parent_bin" \
  --candidate-bin "$candidate_bin" --families dispatch
```

The 10% limit detects adverse changes. Passing it does **not** establish an improvement.
Compare the reported blocks and bytes with the stated target. For a speed claim, also measure dispatch latency or throughput on the native host.

**Compiler upgrade.** Set `old_rust` and `new_rust` to the full installed compiler versions.
Select a parent revision that both compilers support. Do not lower the minimum compiler version to force compatibility.
Keep the previous workload and its Cargo features fixed:

```bash
measure old-parent "$parent_src" "$old_rust" release "$common" "${dispatch[@]}"
measure new-parent "$parent_src" "$new_rust" release "$common" "${dispatch[@]}"
measure new-candidate "$candidate_src" "$new_rust" release "$common" "${dispatch[@]}"
```

The first pair measures the compiler effect on unchanged RCA. The last pair measures the RCA change on the new compiler.
Record both compiler versions and both revisions with the results. Do not attribute the difference between old-parent and new-candidate to RCA alone.
These build directories contain reusable outputs. For a compile-time claim, use separate clean builds under an approved native protocol for each compiler.
The Windows protocol specifies the compiler for both builds. Its result does not qualify an unmeasured compiler.
Retain the small JSON results and discard the temporary build outputs after review.

## What the evidence covers

Record the revision, toolchain, features, platform and workloads with each result.
For a later change, identify which inputs still match. Run new checks for the inputs that changed.
If a crate packages its README without compiling it, a README edit changes the archive but not the library code.
Check the archive and its consumer. Do not report a new runtime measurement.

Coverage applies to the executable production code in the recorded configurations.
Audit tests, compile-failure fixtures and conformance-test bodies separately.
A passing test count or a link to a past run does not establish full coverage for a different revision.
See [coverage evidence](../CONTRIBUTING.md#coverage-evidence) for the required records.

Keep the original measurements and known failures with their release records.
Release-specific changes belong in the [changelog](../CHANGELOG.md), not in this procedure.

## Hosted checks and publication

Hosted CI is a separate requirement. A local result does not establish that GitHub
Actions passed for the public commit. The pull request records its current CI status.
See the [support policy](support.md#qualification-status) for the target matrix.

Merging a pull request, publishing packages and publishing documentation are separate actions.
Documentation publication requires its protected environment and signing configuration.
See [documentation publication](documentation-publishing.md).

Before release, package verification must cover the exact release source and archive bytes.
Any remaining hosted or publication check must retain its actual status.

## Repeating affected checks

Use [CONTRIBUTING.md](../CONTRIBUTING.md) for toolchain, formatting, lint and test commands.
Use [CI](../.github/workflows/ci.yml) for feature, platform and compiler-contract configurations.
Use [the compiler-contract guide](compiler-contract-testing.md) for compile-time checks.
The Tokio conformance tests live in `rca-tokio/src/lib.rs`.
The package-consumer runner is [verify-packaged-downstream.sh](../scripts/verify-packaged-downstream.sh).

The [runtime contract fixtures](../tests/runtime-contracts) exercise RCA counters, streams, timers, observation, and keyed children.
Keep their deterministic assertions and deliberate fault controls separate from native timings.
Passing an allocation test does not establish faster dispatch or lower latency.
