# RCA 0.1.0 qualification

This guide separates local qualification, hosted CI and publication.
Results apply only to their recorded source and configuration.
None of these checks establishes that software has no defects.

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
