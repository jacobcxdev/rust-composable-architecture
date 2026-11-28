# Choose checks for an RCA change

Qualification means collecting enough evidence to accept a change or release.
Use this guide for changes to RCA itself.
For an application that uses RCA, see [application testing](../usage/testing.md) or [application performance](../usage/performance.md).

## Start with the changed behaviour

1. Name the result a caller expects. Include what the caller must see on failure.
2. Find a test that distinguishes the old behaviour from the intended result. For a bug, reproduce the failure first.
3. Run the focused test. Then exercise the nearest caller that depends on the changed behaviour.
4. Identify which other checks the change needs. See the table below.

| Question | Where to start |
| --- | --- |
| Does the API accept valid Rust and reject invalid Rust? | Use a [compiler test](compiler-contract-testing.md). Successful compilation does not establish runtime behaviour. |
| Does the Store deliver the correct state, action order, and cancellation? | Use the [runtime fixtures](../../tests/runtime-contracts). The Tokio runtime tests live in `crates/rca-tokio/src/lib.rs`. |
| Does the change work for each supported feature or platform? | Use the [support matrix](../support.md#qualification-status) and [CI workflow](../../.github/workflows/ci.yml). A cross-build checks compilation, not execution. |
| Can a separate application use the packaged crate? | Use the [package consumer check](../../scripts/verify-packaged-downstream.sh). Test the archive, not only the workspace. |
| Does the change alter performance? | Choose evidence in the next section. Use the [benchmark guide](benchmarking.md) for commands and limits. |

For example, a scheduler edit can change when a task yields.
The [conformance test](../../crates/rca-test/tests/conformance_deterministic.rs) checks the boundary at 100,000 polls.
A poll asks a task whether it can make progress.
At the limit, the task still owns its value and the virtual clock remains unchanged.
The next poll completes the task. A fast benchmark cannot prove any of these facts.

## Choose performance evidence for the change

A workload exercises a defined amount of useful work. For example, it can ask a Store to process 64 actions and check the result.
Choose the workload that measures what the caller needs.
Latency is the time to one result. Throughput is the amount of completed work per unit of time.

| Change | Evidence |
| --- | --- |
| Guidance or tests | Run the applicable documentation or behaviour checks. If neither product behaviour nor the measurement changed, no new performance measurement is necessary. |
| Bug fix | Compare the repaired path and its caller with the parent revision. If the old path skipped required work, report the correction. Those costs do not describe equivalent operations. |
| New capability | Compare old and new code with the capability off. On the new code, measure it enabled but unused, then in use. Include setup and cleanup. |
| Optimisation | Before editing, name the expected saving and a possible cost. Compare against the parent. For a speed claim, measure completed work on a representative native host. |
| Shared runtime, build, or dependency | Include the callers that use the changed component together. For a compiler upgrade, first measure unchanged RCA with both compilers. Then measure both RCA revisions with the new compiler. |
| Release | Compare cumulative cost with the last full release. Before the first full release, measure initial costs and repeatability instead. |

For example, an allocation reduction can save memory but increase the time taken by the slowest requests.
Report both results. Do not average different costs into a score.

### What runs on a pull request

Continuous integration (CI) runs checks on GitHub. When CI selects a performance check, it compares the proposed commit with its branch point.
On a push to `main` or `dev/next`, it selects the affected workloads and compares against the release reference.
If the previous push is unavailable, selection covers the difference from that reference.
If neither the branch point nor a release contains a comparable runtime, CI cannot give a relative result.

To see the selection locally, set `base` and `head` to the full commit IDs before and after the change:

```sh
python3 scripts/performance_scope.py --base "$base" --head "$head"
```

Unknown paths select all workload families. Include any affected caller the selection misses.
For runtime changes, CI compares allocations for the same completed work. A compile-only change has no runtime allocation result.
Native time, energy, and clean build costs require separate measurements. See [how CI selects checks](../../CONTRIBUTING.md#how-ci-selects-checks).

| Selected family | Example of useful work |
| --- | --- |
| Store dispatch | Count allocations per completed action. Measure the time to the reply separately from the time to admission. |
| Keyed routing | Deliver to the intended child. Check that a replacement cannot receive work for its predecessor. |
| Queue pressure | Fill a bounded queue. Check that a rejected send returns its original payload. |
| Cancellation and shutdown | Wait for the work to stop. A fault that starts after repeated cancellations can escape a short run. |
| Downstream compilation | Time the caller's full Cargo command from an empty target directory. |
| Combined worker | The `mixed-pool-shared-ordered` workload fills a one-slot queue, replaces workers, cancels work, and shuts down. |

## Native review and release

For a release, use the final reviewed candidate and the [release benchmark procedure](benchmarking.md#prepare-a-release-comparison).
Both macOS and Windows need runtime and composition results. A Linux instruction count or a green CI run cannot replace them.
The first full release needs independent controls and absolute checks even though no earlier release exists.

Before accepting a release:

1. Check the exact source, dependencies, toolchain, workloads, and runner requirements against the retained results.
2. Require the performance decisions, controls, and guard receipts in the benchmark procedure. A guard receipt records how a protected command finished.
3. Check [production coverage](../../CONTRIBUTING.md#coverage-evidence) in the required configurations. The requirement is 100% of executable production code. Audit test code, compile-failure fixtures, and conformance test bodies separately.
4. Check hosted CI on the exact public commit. Check the release source and the exact package archives separately.
5. Record failures, missing evidence, and the checks that still need to run. An inconclusive result cannot count as a pass.

Review, merge, package publication, and documentation publication are separate decisions.
See the [crate release procedure](releasing.md) and the [documentation publication procedure](documentation-publishing.md).
Record release-specific changes in the [changelog](../../CHANGELOG.md).

## What the evidence covers

Reuse a result only while its source, dependencies, configuration, tools, workload, environment, and observable behaviour still apply.
If an input changed or its identity is uncertain, repeat the affected check on the final candidate.
Do not combine incompatible measurements or turn a replay into evidence for a different executable.

For example, a crate can package its README without compiling it. A README edit then changes the archive but not the runtime.
Check the new archive and an application that consumes it. Do not claim a new runtime measurement.

Keep original source records, measurements, and guard receipts together, including failures.
For formatting, lint, and test commands, see [contributing](../../CONTRIBUTING.md#documentation-and-checks).
