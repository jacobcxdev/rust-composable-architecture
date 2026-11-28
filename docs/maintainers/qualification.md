# RCA qualification

Use this guide to review changes to RCA itself. To measure an app that uses RCA, see the [app performance guide](../guides/performance.md).

- For a pull request, [choose the checks for the change](#choose-performance-evidence-for-the-change). The [benchmark guide](../performance/README.md) lists the portable workloads.
- For a release, [select the published reference and qualify the candidate](#native-review-and-release).
- Before you reuse a result, check [what the evidence covers](#what-the-evidence-covers).

## Choose performance evidence for the change

Start with the cost a caller can observe. A one-line change to the Store can affect every dispatch. A guide edit usually affects none.
Correctness, coverage and packaging keep their own checks. A performance result cannot prove ordering, cancellation safety or shutdown correctness.

| Change | Comparison and decision |
| --- | --- |
| Documentation or tests only | Run the applicable documentation or behaviour tests. Skip performance measurements only when the edit changes neither shipped behaviour nor the measurement itself. |
| Bug fix | Reproduce the bug. Compare the repaired path and its nearest caller with the parent revision. If the fix now completes work the old version skipped, report correctness first. Do not call the extra work a comparable slowdown. |
| New capability | Compare the existing workload with the capability off. With it on, measure useful work, including setup and cleanup. Give new behaviour an initial cost, not an invented historical result. |
| Optimisation | Before editing, name the workload, metric and likely trade-off. Compare with the parent. For a speed claim, measure latency or throughput on a representative host workload. |
| Shared runtime, build or dependency | Include affected consumers and interactions. For a compiler upgrade, run both products on the same compiler. Separately measure unchanged RCA on the old and new compilers to identify the toolchain effect. |
| Merge to `main` or release | Compare cumulative cost with the last full release. Before the first full release, measure the initial cost and run the controls. There is no previous released runtime to compare. |

### What runs on a pull request

When [CI selects a performance check](../../CONTRIBUTING.md#how-ci-selects-checks), it compares the PR head with its branch point.
If neither the branch point nor a full release contains a comparable runtime, there is no relative PR result.
On pushes to `main` or `dev/next`, the job selects the workloads that changed and measures them against the release commit.
If the previous push is unavailable, selection covers the full difference from the release.

To see which families a committed change selects, use both full commit IDs:

```sh
python3 scripts/performance_scope.py --base "$base" --head "$head"
```

Unknown paths select all families. If a change also affects a caller outside the selection, include that caller in the PR evidence.
For an extra allocation comparison, pass the affected runtime families to `--families` as in the [optimisation example](#examples-feature-costs-optimisations-and-compiler-upgrades).
For a change to native timing or build time, measure the affected workload on the host required by its protocol.
For selected runtime families, CI builds the same downstream profiling program against each product. It compares allocations for the same completed work.
The job retains the revisions, measurements, compiler and elapsed time. A skipped check does not give a new result.
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

For a release, use the integrated candidate and both the [runtime protocol](../performance/method.md) and the [composition protocol](../performance/composition/README.md).
A hosted allocation comparison does not qualify native latency, cold builds, energy use or a release.

From a clean checkout with the remote tags available, identify the last full published release. The resolver reads GitHub and crates.io:

```sh
python3 scripts/release_baseline.py revision
```

- If the resolver returns `none`, report “no prior released runtime”. Keep the initial measurements, absolute limits, independent controls and deliberate regression checks.
  A development commit, a snapshot without a runtime or a same-source comparison cannot establish a release speedup.
  After publication, [record the first full release](releasing.md#record-the-performance-baseline) as the reference.
- If the resolver returns a commit, use its signed tag, source tree and dependency lock.
  If the checkout tip is the signed candidate tag, use `revision --candidate-tag "$tag"` to select its predecessor.
  For the native collector, set `RCA_QUALIFICATION_TAG` to the same tag.
- If the resolver reports an error, stop. Missing evidence, a partial publication or a mismatched tag does not count as a first release.

Before capture, review the exact source. Do not run unreviewed PR code on a measurement host.
Use the enrolled host and its private runner profile. Keep the protocol’s absolute limits and 10% relative limit.
Compare both products on the same host under the same conditions. Results on one host do not qualify another.
Before measurement, select the useful work, controls, stopping rule and permitted extension.
If a result is inconclusive, use only its permitted extension or the full qualification method. Do not retry for a favourable result.

Keep the runner profile, protocol, source identities, toolchain, workload and evidence hashes with the private release record.
To replay a result, use its original controller and inputs. A new baseline does not change an earlier result.

#### Focused Windows screens

These two comparisons guide changes to cancellation or cold builds. A passing screen does not replace full release qualification.
Use the [Windows protocol](../performance/composition/README.md#windows-paired-measurements) for host requirements.

Only the measurement host operator runs the Windows cancellation and compile comparisons below.
The operator supplies a launcher that runs the controller and its children in a Windows Job Object.
The job must cap total committed memory at two GiB and terminate its children on close.
Keep at least four GiB of available physical memory and four GiB of unused commit capacity on the host.
Keep the launcher's receipt for the complete process tree. The Python commands do not start the launcher.
Without that launcher and the specified host, request a capture from the operator.

For a cancellation capture, choose a new output directory on a volume with more than 20 GiB free.
Pin the build records for the candidate and two independent builds of the reference.
Check the source revisions, image hashes, compiler, workload, and build configuration against the bindings.
Run this command inside the guarded job:

```powershell
python -B scripts/composition_cancellation_exposure.py capture $output --builds $bindings --builds-sha256 $trustedDigest
```

Keep the capture digest outside the evidence directory. To replay the saved data without another measurement, run:

```powershell
python -B scripts/composition_cancellation_exposure.py replay $output --builds $bindings --builds-sha256 $trustedDigest --capture-sha256 $captureDigest
```

The screen checks median removal cost over 1,024 complete operations per process. It does not prove p99 latency.
Use `--witnessed-candidate` only for the deliberate delayed fault, on capture and replay.

For a compile comparison, keep at least 24 GiB free on the output volume throughout collection.
Choose a new output directory outside the checkout. For generated child paths, run the following inside the guarded job:

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
For the same fixture and revisions, run the comparison once inside the same guarded job configuration.
Use a new directory with the same 24 GiB disk reserve:

```powershell
python -B scripts/composition_compile_portfolio.py --base $base --candidate $head --fixture composed-macro --output $newOutput --collect-only --confirm $firstScreen --confirm-sha256 $firstDigest
```

The comparison starts fresh. It does not reuse the eight measurements from the first screen.
It uses the original order and limit for up to 20 blocks. It can stop early when the candidate can no longer pass.
An early rejection does not establish the cause while the unchanged control remains uncertain.
Read the saved decision after the guard finishes. An incomplete or inconclusive result does not pass.

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

## Examples: feature costs, optimisations and compiler upgrades

These examples measure allocations for fixed work. They do not measure elapsed application time, energy use or cold builds.
Use an isolated Linux runner with a memory limit, sufficient disk space and the locked dependencies available offline.
On macOS, use the [Cargo memory guard](../../CONTRIBUTING.md#build-memory-safety) instead of running this build function.

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

**New feature.** Compare 64 completed dispatches with and without the Core `diagnostics` feature.
Use the native runtime. The `deterministic` feature pulls in `rca-test`, which already enables Core diagnostics.
Check both builds' resolved Core features. The workload installs no diagnostic observer:

```bash
common=allocations
dispatch=(topology-native 1 0 64)
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

Keep the original source and guard receipts with each result, including failures.
Reuse complete evidence only while the source, dependencies, configuration, toolchain, workload, runner conditions and contract still apply.
If evidence is missing or uncertain, run the affected qualification on the final candidate.
Do not combine measurements from different runs or replace a release measurement with a focused test.

For example, the [runtime replay command](../performance/method.md#commands) checks a previous capture using its original controller.
It cannot measure a changed executable.
If a crate packages its README without compiling it, a README edit changes the archive but not the library code.
Check the archive and its consumer. Do not report a new runtime measurement.

Coverage applies to the executable production code in the recorded configurations.
Audit tests, compile-failure fixtures and conformance-test bodies separately.
A passing test count or a link to a past run does not establish full coverage for a different revision.
See [coverage evidence](../../CONTRIBUTING.md#coverage-evidence) for the required records.

Release-specific changes belong in the [changelog](../../CHANGELOG.md), not in this procedure.

## Hosted checks and publication

Hosted CI is a separate requirement. A local result does not establish that GitHub
Actions passed for the public commit. The pull request records its current CI status.
See the [support policy](../support.md#qualification-status) for the target matrix.

Merging a pull request, publishing packages and publishing documentation are separate actions.
Documentation publication requires its protected environment and signing configuration.
See [documentation publication](documentation-publishing.md).

Before release, package verification must cover the exact release source and archive bytes.
Any remaining hosted or publication check must retain its actual status.

## Repeating affected checks

Use [CONTRIBUTING.md](../../CONTRIBUTING.md) for toolchain, formatting, lint and test commands.
Use [CI](../../.github/workflows/ci.yml) for feature, platform and compiler-contract configurations.
Use [the compiler-contract guide](compiler-contract-testing.md) for compile-time checks.
The Tokio conformance tests live in `crates/rca-tokio/src/lib.rs`.
The package-consumer runner is [verify-packaged-downstream.sh](../../scripts/verify-packaged-downstream.sh).

The [runtime contract fixtures](../../tests/runtime-contracts) exercise RCA counters, streams, timers, observation, and keyed children.
Keep their deterministic assertions and deliberate fault controls separate from native timings.
Passing an allocation test does not establish faster dispatch or lower latency.
