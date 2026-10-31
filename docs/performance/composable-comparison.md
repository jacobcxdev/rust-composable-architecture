# Composable comparison method

This page defines the current RCA/upstream comparison method, not a performance
result for the current release. The retired fork fixtures and development
results are archived outside the public source tree. [Current qualification](../qualification.md)
records release gates and available evidence.

## Status and purpose

Local capture controllers require [compiler memory protection](../../CONTRIBUTING.md#build-memory-safety).
The current controllers create separate process groups and do not protect
their memory use. Do not run their capture commands locally until each compiler
process has equivalent protection.

This method starts with an executable correctness prerequisite, not a performance
result. RCA must demonstrate equivalent completed work before comparing costs.
The first fixture covers one effect-free counter. It does not establish overall
superiority, production readiness or completion of the roadmap's comparison
matrix. Existing runtime and composition qualification protocols remain unchanged.

## Sources and dependency fidelity

- Original Composable: `0b5baa80246a625c1bb886e6df20dae27fc97c97`.
- Candidate RCA: the exact clean revision recorded by each future capture.

The fixtures compare current RCA with pinned upstream Composable. Do not patch
a baseline to make it pass: a patched variant is a separate implementation.
An upstream timeout remains a failed baseline, not a successful comparison or
an RCA release failure. RCA's own correctness, coverage and performance gates
remain unchanged.

Upstream has no repository lockfile at the pinned revision. Each fixture freezes
its own lockfile. This is a new dependency resolution, not a reconstruction of
the captured MCMaitreD graph. Record the resolved graph and differences from the
captured consumer before attributing performance changes to architecture.
Use Rust, Cargo and rustdoc 1.99.0, empty compiler wrappers and locked builds.
Fixture locks must not change the product workspace lock or package versions.
The v2 protocols use Rust 1.99.0 for both engines. Their workloads, schedules and
limits match v1. Retain v1 captures with their original Rust 1.98 identities;
do not combine their results with a new capture. The toolchain change does not
require another run of the failed upstream baseline.

## First common workload

State is a `u64` initially zero. One synchronous producer sends increments of
one, with at most one operation in flight. There are no effects, observers,
dependencies, child reducers or timers. Perform 20 warm-up increments followed
by 10,000 checked increments; the final value must be 10,020.

Composable uses its original `Store::sync`, worker thread and `LocalPool`.
Candidate RCA uses its normal Store with mailbox capacity one and a current-thread Tokio
runtime. Each candidate operation crosses `block_on(store.send(...))` and drops the
successful receipt before returning. No effect exists to await in this fixture.
These are equivalent domain operations, not identical executor topologies.
Upstream's unbounded queue does not confer an overload comparison with one
operation in flight.

Final-state retrieval is outside the operation boundary. Original Composable uses
`into_inner`, which joins its worker thread. The documented
best-effort limitation concerns asynchronous effects, which this fixture excludes.
Candidate RCA uses `shutdown_into_state`, then drops its owned Tokio runtime. Final state
must be checked after successful extraction. An error, panic or process timeout
invalidates the run; never reduce the denominator to hide failed work.

## Correctness evidence

Both fixtures emit the same versioned JSON work ledger, distinguished by engine.
Record expected, attempted and completed counts separately for warm-up and
checked operations, initial and final values, error count, and successful state
extraction and host teardown. Counts are domain actions, not loop or benchmark
iterations. The ledger is correctness evidence only; it contains no timing,
performance pass or equivalence classification.

Use the real production adapters in completion tests. A reducer-entry handshake
and controlled release must show that the call cannot complete before reduction.
An enqueue-only control must be rejected by that same proof. Bound test waits
and clean up workers even when assertions fail. A final total alone cannot prove
the per-operation completion boundary. Test omitted and duplicate increments,
wrong warm-up accounting, errors, missing teardown and malformed ledger fields.
For indistinguishable increments, the total does not establish action identity;
future distinct-action workloads need a trace contract.

## Raw counter timing interface

Both fixtures accept no arguments for the original
correctness ledger, or `--measure` for a `composable-counter-timing-v1` object.
Unknown or repeated arguments exit unsuccessfully. Timing is opt-in and changes
neither the pinned engine nor its committed-send adapter. Standard allocators,
dependency locks and product features remain unchanged.

The measured output embeds the original validated ledger as `work`, declares
`clock: "std::time::Instant"` and `unit: "ns"`, and retains `samples_ns` with
exactly 10,000 integer operation latencies plus `loop_ns`. After the 20 warm-up
actions, sample storage is allocated before the loop timer. Each latency starts
immediately before the committed adapter call and ends after it returns,
including candidate receipt cleanup. It includes timestamp instrumentation;
there is no overhead subtraction. Loop time includes work accounting, timestamp
collection and sample recording, not just the sum of operation latencies.
Warm-up, construction, sample-buffer allocation, extraction, host teardown and
JSON formatting are outside that interval. This mode does not measure lifecycle,
CPU, allocation or memory costs.

No object is printed until final state, completed work and owned-host teardown
have passed the original checks. The existing evaluator's
`capture_composable_counter_timing(command, expected_engine)` additionally
requires successful process exit within 60 seconds, matching engine identity,
valid nested work, exact sample count, unsigned integer nanoseconds and a
positive loop duration covering all non-overlapping samples. Zero-resolution
individual samples are retained as zero, not adjusted with pseudocounts.

This is a measurement interface, not an admitted comparative cohort. Debug-mode
test/smoke output cannot establish performance. The frozen cohort/controller,
host qualification, A/A sensitivity and uncertainty obligations below still
apply before collecting or interpreting comparative results. The old frozen
composition protocol and its evaluator thresholds are unchanged.

### Raw counter lifecycle interface

Both adapters also accept `--measure-lifecycle`, mutually exclusive with
`--measure`. This is one cold lifecycle per invocation: construct the adapter
and its owned host, commit one increment, extract final state, and release the
owned host. There is no warm-up or repeated-work loop. The monotonic timer starts
immediately before construction and stops only after consuming `Adapter::finish`
returns. Argument parsing, JSON formatting, process startup and process exit are
outside the interval.

The output is a separate `composable-counter-lifecycle-v1` object, not a
steady-state sample. It carries engine identity, `clock: "std::time::Instant"`,
`unit: "ns"`, `elapsed_ns`, expected/attempted/completed operation counts of one,
initial state zero, final state one, zero errors, and true extraction/host-stopped
flags. Failed construction, send or extraction, or an incorrect final state,
produces no measurement. The evaluator's
`capture_composable_counter_lifecycle(command, expected_engine)` accepts output
only after successful process exit and strict field/type/engine validation.

For example, an instrumentation smoke check uses:

```sh
rustup run 1.99.0 cargo run --locked --manifest-path tests/composable-counter/candidate/Cargo.toml -- --measure-lifecycle
```

The existing adapters define host ownership: current RCA owns a current-thread
Tokio runtime, while upstream Composable owns a worker thread joined by
extraction. Thus this interval includes different host costs; it is not an
isolated reducer measurement. These effect-free fixtures do not prove general
asynchronous-effect draining.

This interface alone does not provide cohort qualification, A/A sensitivity,
allocation/RSS evidence or a performance verdict. The frozen steady-state v2
cohorts below reject this different schema. Correctness smoke timings are not
comparative evidence.

### Frozen lifecycle cohort analysis

The separate [lifecycle protocol](composable-counter-lifecycle-protocol-v2.json)
predeclares two engine-specific A/A cohorts and one candidate comparison,
each with 24 pairs, balanced 12 per execution order. Each invocation constructs
a cold adapter and performs one complete lifecycle; there is no warm-up. The
only metric is `elapsed_ns`. The controller uses four fresh release builds and
the existing source/dependency/tool/binary authentication, host checks, machine
lock and retained failure receipts. It does not silently retry failed or
unsuitable programmes.

The estimator is the geometric mean of the two order-stratum median paired
ratios. Bounds are the geometric means of stratum minima and maxima. Under
the stated IID continuous-positive-ratio assumptions, each 12-pair stratum's
extreme interval misses its population median with probability `2 / 2^12`.
The current schedule has three metrics (one per cohort) and two strata. The
protocol conservatively retains its earlier nominal family coverage lower bound
of 0.9951171875; removing the retired cohorts does not relax it. This is a
within-programme, unconditional model bound, not confidence conditional on
passing diagnostics, proof of IID observations, or joint coverage across all
other benchmark programmes.

A/A intervals must include one and have at most 1.10 upper/lower width. Order
and within-stratum chronological half-drift factors must be at most 1.05.
Failures remain visible beside estimates; they prevent a qualified directional
claim even when a candidate comparison looks favourable. No universal
performance-regression allowance is implied by diagnostic thresholds.

```sh
python3 scripts/composition_runner.py --mode counter-lifecycle-cohorts \
  target/qualification-evidence/0.1.0/composition/counter-lifecycle-v2
```

The lifecycle and steady-state entry points reject one another's protocols and
raw schemas. Neither numerical evaluator sets `provenance_qualified`; only the
controller's completed capture receipt can attest its checked capture process.
Implementation tests do not establish a successful native comparative run.

### Frozen counter cohort analysis

The [counter protocol](composable-counter-protocol-v2.json) freezes three cohorts:
upstream/upstream and RCA/RCA A/A controls, plus RCA/upstream.
Each has 24 paired processes, balanced across execution order by a fixed seed.
The evaluator admits every complete raw work ledger before calculating p50,
p95, p99 and amortised loop cost. Zero percentiles retain their observations
but make that metric's multiplicative analysis unavailable.

Ratios are subject/control (lower cost is better). Each estimate is the
geometric mean of the two order-stratum sample medians; bounds use the geometric
means of stratum minima and maxima. Across the 12 metric/cohort combinations,
the protocol conservatively retains its earlier nominal simultaneous coverage
bound of at least 0.98046875 under IID continuous positive ratios within each
order stratum. This is unconditional model coverage,
not a guarantee conditional on passing diagnostics, and not a pooled-median interval.

Every A/A interval must include 1 and have width at most 1.10. Order and
within-stratum first-half/last-half median factors must be at most 1.05.
These diagnose measurement suitability, not product equivalence. Failed
diagnostics do not hide estimates or interval direction. The numerical evaluator
always returns `provenance_qualified: false`; authenticated capture remains
separate.

The controller command is:

```sh
python3 scripts/composition_runner.py --mode counter-cohorts \
  target/qualification-evidence/0.1.0/composition/counter-v2-<revision>
```

Run once from a clean checkout after independent review. Do not reuse an output
name or retry a failed/noisy programme without a new documented pre-observation
decision. Upstream dependencies remain pinned by the fixture manifests and
locks. Four fresh release targets supply independent A/B builds,
then the same binaries serve A/A and comparisons. Builds use Rust 1.99.0,
default features, native aarch64-apple-darwin, jobs 1, incremental off, opt-level 3,
16 codegen units, no LTO/debug assertions/overflow checks/debug info/stripping,
and unwind panic behaviour. Builds use a fresh private Cargo home and neutral
temporary working directory. Ambient Cargo configuration in those locations
is rejected and rechecked after builds and capture; user configuration is untouched.
Resolved Git dependency files must match their locked commits. Registry files
must match crate archives authenticated by lockfile checksums. Those dependency
bytes are hashed before building and rechecked through final admission.

The controller binds tracked source and locks, tools, resolved metadata, build
commands, binaries, raw output and host telemetry. Missing/changed evidence
cannot yield a qualified comparison. Failed work remains in the `.incomplete`
directory. Telemetry subprocesses share the remaining measurement deadline,
so a stalled probe cannot keep the measured process running indefinitely.
Successful raw capture can establish provenance while failed
diagnostics still prevent comparison qualification. The numerical report itself
never attests provenance. These checks detect changed inputs; they are not a
cryptographic defence against a malicious local operator rewriting all receipts.

### Counter allocation capture

The frozen `composable-counter-allocation-protocol-v2.json` runs three cycles.
Each cycle runs upstream Composable and current RCA using fresh build A, then
reverses that order using fresh build B: six observations per engine, twelve
processes in total. Cargo metadata and all four release builds enable `allocation-measurement`;
normal timing binaries remain separate. Instrumentation is process-wide DHAT
0.3.3, including worker-thread allocations, not a thread-local allocator count.

Run the existing controller with `--mode counter-allocation-cohorts`. It retains
the source, lock, dependency, tool, binary and protocol identities, raw JSON,
command, stderr and exit receipts. Each process has a 60-second deadline and
failure stops capture without retry. Busy CPU is allowed: allocation capture
does not instantiate the timing host monitor or wait for idle. The canonical
host identity is still checked before and after capture. This is not a restart
of the timing programme, and previously retained upstream timeouts remain failures.

Admission requires the exact `composable-counter-allocations-v1` schema and
matching engine/work identity. Snapshots A–E separate construction (B−A),
20 warm-up actions (C−B), 10,000 committed actions including receipt cleanup
(D−C), and consuming teardown (E−D). Setup is C−A with denominator one: one
construction plus 20 warm-up actions, not one action. Each snapshot retains
cumulative allocated blocks/bytes and current live blocks/bytes. Only cumulative
counts must increase monotonically; live counts may fall. A successful DHAT
reallocation counts one allocation event and the full new requested byte size,
not just growth. Allocated blocks therefore do not mean distinct live objects.

The report retains every raw and per-operation count, observed minimum/maximum,
and paired absolute differences against upstream. With control/subject
both zero it reports observed zeros, not parity; zero control and positive
subject has no ratio; positive control and zero subject has ratio zero; two
positive counts permit their ordinary ratio. No pseudocounts are used. Unlike
timing log-ratio admission, a zero subject allocation count is valid. No confidence,
equivalence, superiority or unconditional successor claim follows from these
observations. `allocation_capture_complete` and `provenance_qualified` attest
successful authenticated capture; `qualified_comparison` remains false.

### Comparative cohort prerequisites

The semantic fixture is not a timing programme. Extend the existing controller
and evaluator when these prerequisites are complete; do not create a parallel
qualification framework. Freeze the following before collecting candidate timing:

1. Source, adapter and protocol hashes; both complete locks and dependency graphs;
   exact compiler paths and commits; features, target, allocator, profile, jobs
   and incremental policy. Fetch dependencies outside compile timers.
2. Steady-state operation latency includes caller integration and receipt cleanup.
   Setup and extraction stay outside that timer. Throughput includes the whole
   measured loop and sample-recording overhead, with completed actions as its
   numerator. Retain raw samples and report p50, p95 and p99 separately.
3. Lifecycle timing includes construction, one increment, extraction and complete
   owned-host teardown. Prove teardown before admitting that distinct metric.
   Process CPU/RSS include a different interval; ready-idle RSS, live heap and
   whole-process peak RSS are not interchangeable.
4. Separate normal and allocation-instrumented binaries. Declare allocation
   intervals and denominators. Missing counts are not zero. Zero/zero reports
   observed zeros; zero/positive has an absolute delta and no ratio;
   positive/zero has ratio zero; positive/positive permits a ratio. No pseudocounts.
5. Equivalent minimal application fixtures for clean check, clean release build
   and successfully stripped binary size. Use fresh targets for compile samples;
   exclude measurement instrumentation from representative application size.
   Preserve [Darwin command resource accounting](darwin-command-resource-accounting.md)
   limits: maximum reported process RSS is not summed process-tree memory, and
   quantised zero CPU/wall readings do not support multiplicative comparisons.
6. Fresh upstream/upstream and candidate/candidate A/A controls. Predeclare
   sample counts, balanced seeded order, drift/order diagnostics, sensitivity
   rules, uncertainty model and conditions that make a result unqualified.
   Recording dispersion alone is not a sensitivity rule. No new statistical
   classification is enabled by this document.
7. Metric-specific interpretation and rerun policy. The old 1.10 bound is not an
   equivalence margin, superiority proof or simultaneous multi-metric guarantee.
   No product-backed equivalence margin has been selected. Preserve all failed
   and noisy cohorts; document and freeze any rerun decision before observing it.

Timing uses macOS 27+ with the exact OS build recorded. Busy-host timing remains
unqualified; it does not block correctness work or other roadmap implementation.
Archived captures retain their original host labels and are not current results.
These requirements do not change the crate's deployment floor.

## Other workloads and interpretation

The [ready-future fixture](../../tests/composable-stream/README.md#ready-future-allocation-instrumentation)
runs 64 serial native future effects. RCA's causal finish is stronger than
upstream result acknowledgement; the allocation report keeps that difference
visible. Its [protocol](composable-ready-future-allocation-protocol-v2.json)
runs three forward/reverse cycles with two independent builds per engine and
six observations per engine. The initial 20 and checked 44 operations do not
establish steady state. Run `--mode ready-future-allocation-cohorts`.

The [common-outcome protocol](composable-common-outcome-allocation-protocol-v2.json)
uses result/drop notices and an effect-free checkpoint for each of 64 operations.
It records the whole lifetime from before host construction through host teardown
and release of the probes. Run `--mode common-outcome-allocation-cohorts`.
It retains all twelve observations and six paired differences. It does not claim
equal native task-join semantics, heap quiescence or general performance parity.

All allocation modes include fixture coordination, use process-wide DHAT 0.3.3
and stop after a failed or timed-out process without retrying it. Busy CPU is
allowed for allocation counts, but host and input identities are still checked.
Neither numerical analysis nor successful capture grants statistical confidence
or performance superiority. The exact protocol byte/document hashes have changed
with removal of the retired fork: old three-engine captures cannot be admitted
as new two-engine captures.

The [mounted keyed diagnostic](composable-mounted-keyed.md) measures current RCA
at four cardinalities. Upstream has no equivalent native keyed API and is not
given an artificial zero-cost result. Collection, mounting, clearing and release
costs have separate boundaries; completion is not heap quiescence.

The [minimal application size diagnostic](composable-minimal-size.md) uses four
fresh builds and checks original and stripped executables. The [compile diagnostic](composable-minimal-compile.md)
keeps six samples per engine/workload, balanced across the two engine orders.
Neither page claims current comparative results.

The [concurrent-ingress fixture](../../tests/composable-counter/concurrent-ingress.md)
checks four producers, gated admission, exact final state and host teardown.
It preserves upstream unbounded enqueue versus bounded RCA admission with explicit
caller retry. It is not a sustained-overload or performance result.

[Timer](../../tests/composable-timers/README.md) and [observation](../../tests/composable-observation/README.md)
fixtures retain their own semantic boundaries. Observation includes an
application-written upstream publisher, not a native subscription API.
Async tasks, streams, timers, cancellation, keyed composition, observation,
saturation, move-only payloads and downstream applications each need their own
contracts. Include application glue on both sides and label stronger RCA-only
guarantees separately. A justified architectural difference need not preserve
every capability of the retired fork.
