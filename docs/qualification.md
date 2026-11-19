# RCA 0.1.0 qualification

This guide separates local qualification, hosted CI and publication.
Results apply only to their recorded source and configuration.
None of these checks establishes that software has no defects.

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
| Merge to `main` or release | Compare cumulative cost with the last release. Before 0.1.0, use `ab99e19`. Several small regressions can exceed the limit together even though each PR passed. |

### What runs on a pull request

The hosted CI job compares the PR head with its branch point. For the initial metadata-only branch, it uses the agreed product reference.
On pushes to `main`, the job selects the affected workloads from that push but measures them against the release commit.
If the previous push is unavailable, selection covers the full difference from the release.
CI resolves the last published stable GitHub release to its exact commit. Drafts, prereleases and unpublished tags are not baselines.

To see which families a committed change selects, use both full commit IDs:

```sh
python3 scripts/performance_scope.py --base "$base" --head "$head"
```

Unknown paths select all families. If a change also affects a caller outside the selection, include that caller in the PR evidence.
For an extra allocation comparison, pass the affected runtime families to `--families` as in the optimisation example below.
For a change to native timing or build time, use the H1 check for the affected workload.
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

Use H1 for Windows performance claims. Review the exact source before it runs on the LAN host. Public PR jobs do not dispatch code to H1.
Keep the existing absolute limits and the 10% relative limit. Before measuring, select the useful work, controls, stopping rule and permitted extension.
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
For the same fixture and revisions, run the bounded comparison once under the same H1 guard:

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

Use a release checkout with current Git tags and read access to GitHub through `gh`.
Before a release capture, check the published baseline:

```sh
bash scripts/hosted_change_performance.sh check-release-baseline
```

The command prints the commit only when the published baseline matches all current protocol pins.
If a pin differs, stop before capture. Review the new reference and enrol its controls without changing the limits.
Run the command again before accepting release evidence. Use its commit as the reference for the input check.
Historical replays keep their original reference.

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
The current H1 protocol pins Rust 1.100.0. Its result does not qualify an unmeasured compiler.
Retain the small JSON results and discard the temporary build outputs after review.

## Historical local results

Local release-readiness qualification completed on 29 October 2026 for the recorded
source. The retained source envelope identifies it as
`58e860271a7a88b079274541956ddd5100244c69c5dc4b9399bdff6aadbf8cd2`.
This identifier is a source-record identifier, not a public Git commit.

The recorded results include:

- Workspace tests, strict Clippy checks and compiler-contract tests.
- 236 workspace feature configurations and 128 facade test configurations.
- Public and private documentation checks, 204 documentation tests and rendered-link checks.
- 17 property tests with 10,000 cases each, plus transport and concurrency checks.
- 25 strict-provenance Miri checks and four Tokio conformance checks.
- Nine package archives and 13 consumers built from those archives.
- Performance checks with workload-specific allocation, instruction-count and native evidence.

The executable-production coverage record accounts for 5,131 bodies, 49,080 regions
and 3,432 branch sides. Native coverage covers 5,050 bodies.
Complete finite-operation checks account for 81 additional bodies.
Test-only code, compile-failure fixtures and conformance-test bodies have a separate audit scope.
This is not a claim that every test-only failure path executed.

The retained coverage result has SHA-256
`fed723d58529b722e2df99c8a9d2ff98150b9c5d2ead1f74fececa096ecaf912`.
The retained release-gate record has SHA-256
`989b1a282734f1a3b89970d478d25ffa89d1896673faad0542b2451c9a78d9d7`.
These hashes identify local records. They do not substitute for accessible logs or independent reproduction.

## Public snapshot boundary

The first public snapshot preserved the locally qualified implementation.
Later changes require checks for the affected source and contracts.
Changes to README prose also change package archive bytes.
Earlier results do not establish that later package archives passed their checks.

Raw profiles, temporary agent records and private upstream research remain outside
the public repository. Historical local audit receipts are not public reproduction
instructions. Current tests, required regression inputs and maintained development
tools remain part of the snapshot.

Archived development reports and retired-fork comparisons are not release inputs.
Small historical samples remain only where maintained tooling tests require them.
Their fixture documentation distinguishes parser and provenance checks from current
RCA performance evidence. Removing an archive from the final tree does not erase
it from earlier public Git commits.

## Later regression and macro changes

The 30 October regression changes added child-routing and named-task checks without
runtime changes. They check replacement before routing, rejection of old child
authority, private child-state lifetimes and task replacement across handlers.
Both Shared and Local paths retain their existing contracts. Each explicit replacement
still replaces earlier work. Coexistence remains a separate choice.

Macro fixes prevent caller type parameters from hiding source types or generated
companion types. Private type aliases preserve caller bounds, embedded type macros
and capture expressions. Compiler fixtures cover conditional domains, independent
binding generics, function-local types, renamed dependencies and library-name collisions.
Existing non-colliding expansion snapshots remain unchanged.

These changes require their own focused tests, compiler checks, strict lint,
independent review and coverage evidence. The earlier coverage counts above remain
historical, not a recalculated total for the amended source. Unchanged runtime
performance evidence retains its original scope. These changes make no new speed claim.
The pull request records the checks for the delivered head.

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

Performance comparisons need matching workloads and applicable measurement conditions.
The pinned upstream Composable runtime can hang during shutdown.
It remains a failed reference baseline, not an RCA failure or a valid numerical comparison.
RCA's own correctness, coverage and performance requirements remain unchanged.
CI reports upstream runtime observations without using them as RCA acceptance gates.
Formatting, lint and compilation remain required for all reference fixtures.
RCA runtime checks remain required. A later passing upstream
observation does not qualify that reference or erase its recorded failures.
The [counter](../tests/composable-counter/README.md#failed-upstream-reference),
[stream](../tests/composable-stream/README.md) and
[publication](../tests/composable-observation/README.md) fixtures record this boundary.

The public benchmark tooling does not maintain retired Composable releases.
Archived comparisons retain their original revisions and limits outside the
maintained tree. They are not evidence for later source revisions.
