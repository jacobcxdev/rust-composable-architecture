# Composable comparison method

This page records development history, not results for the current release
candidate. The supporting audit receipts and development commits remain in the
local archive. A fresh public clone does not include that archive.
[Current qualification](../qualification.md) records the release gates and
available evidence.

## Status and purpose

This method starts with an executable correctness prerequisite, not a performance
result. RCA must demonstrate equivalent completed work before comparing costs.
The first fixture covers one effect-free counter. It does not establish overall
superiority, production readiness or completion of the roadmap's comparison
matrix. Existing runtime and composition qualification protocols remain unchanged.

## Sources and dependency fidelity

- Original Composable: `0b5baa80246a625c1bb886e6df20dae27fc97c97`.
- Historical RCA v0.7: `c6200234cebbbd9b0009cd5788736c78a47f32b0`.
- Candidate RCA: the exact clean revision recorded by each future capture.

The fixture compares original Composable, historical RCA v0.7 and candidate RCA
as three distinct engines. The historical fixture at
`tests/composable-counter/historical` pins its own source and lock and emits
`rca-0.7`, not `composable` or `rca`. Do not modify baselines to make them pass.
Any necessary patch creates a separately labelled variant, not original evidence.

Upstream has no historical repository lockfile. Each fixture therefore freezes
its own lockfile. This is a new dependency resolution, not a reconstruction of
the captured MCMaitreD graph. Record the resolved graph and differences from the
captured consumer before attributing performance changes to architecture.
Use Rust, Cargo and rustdoc 1.98.0, empty compiler wrappers and locked builds.
Fixture locks must not change the product workspace lock or package versions.

## First common workload

State is a `u64` initially zero. One synchronous producer sends increments of
one, with at most one operation in flight. There are no effects, observers,
dependencies, child reducers or timers. Perform 20 warm-up increments followed
by 10,000 checked increments; the final value must be 10,020.

Composable uses its original `Store::sync`, worker thread and `LocalPool`.
Historical RCA uses the corresponding APIs from its pinned v0.7 source. Its
adapter is deliberately kept identical to the original fixture apart from the
ledger identity, so the common workload does not silently change between them.
Candidate RCA uses its normal Store with mailbox capacity one and a current-thread Tokio
runtime. Each candidate operation crosses `block_on(store.send(...))` and drops the
successful receipt before returning. No effect exists to await in this fixture.
These are equivalent domain operations, not identical executor topologies.
Upstream's unbounded queue does not confer an overload comparison with one
operation in flight.

Final-state retrieval is outside the operation boundary. Original Composable and
historical RCA use `into_inner`; both implementations join the worker thread. The documented
best-effort limitation concerns asynchronous effects, which this fixture excludes.
Candidate RCA uses `shutdown_into_state`, then drops its owned Tokio runtime. Final state
must be checked after successful extraction. An error, panic or process timeout
invalidates the run; never reduce the denominator to hide failed work.

## Correctness evidence

All three fixtures emit the same versioned JSON work ledger, distinguished by engine.
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

The historical fixed-counter correctness receipt
records the tested locked fixture commands, process-checked ledgers, source and
dependency provenance, and limits. It is not a timing qualification.
The historical RCA counter receipt
adds the separately pinned v0.7 fixture and its executed completion proof. It
does not extend the workload or establish comparative costs.

## Measurement obligations still open

The historical ready-future ledger extends
correctness coverage to 64 serial native future effects per engine. It retains
RCA's stronger causal-finish boundary explicitly rather than treating legacy
result acknowledgement as a task join. All three executable ledgers passed;
an existing upstream malformed-stream test hit its host-join deadline and
remains recorded. Effect allocation instrumentation now has a separately frozen
[protocol](composable-ready-future-allocation-protocol-v1.json); timing remains
unqualified. The same 64 operations are split into 20 initial and 44 checked
operations, without a steady-state claim. See the
[fixture boundaries](../../tests/composable-stream/README.md#ready-future-allocation-instrumentation).

Run the existing controller with `--mode ready-future-allocation-cohorts` and a
fresh output directory. Its three forward/reverse cycles use six independently
built binaries and retain six observations per engine. All fixture coordination
costs remain included, and the analysis exposes different completion guarantees.
Neither a successful capture nor allocation ratios establish equivalent task
joins, overall superiority or timing qualification. Existing counter protocol
hashes and captures are unchanged.

The historical first release allocation capture
completed all 18 processes at `9400687`. Current RCA used 29 allocations / 4,304
bytes per checked ready effect; both legacy engines used 11 / 3,104. Fixed costs
were also higher. These observed gaps need attribution and a justified
disposition; stronger completion guarantees alone do not resolve them.

The historical stack control-listener repair
and repeat frozen capture at `3a1a18e` reduced RCA to 26 allocations / 4,136
bytes per checked ready effect across all six samples. Both legacy baselines
remained at 11 / 3,104. The repair also reduced the measured actor frame by 112
bytes. The remaining gap and completion-boundary differences remain open;
these results are not timing or categorical superiority evidence.

The historical singleton terminal-collection repair
and frozen capture at `1ef3b09` further reduced RCA to 25 allocations / 4,104
bytes per checked ready effect, with unchanged fixed and retained-memory costs.
All six RCA samples agree; both legacy engines remain at 11 / 3,104. The
14-allocation / 1,000-byte gap at that checkpoint was not waived.

Later repairs culminate in the historical inline terminal result capture
at `d44acaa`: all six RCA samples report 20 allocations / 3,808 bytes per
checked operation; both legacy arms remain 11 / 3,104. This supersedes the
earlier candidate allocation rows without changing their historical receipts.
The remaining nine-allocation / 704-byte gap still needs disposition, and
different completion guarantees still prevent an equivalent-work claim.

The later historical inline effect acknowledgement diagnostic
measures 19 allocations / 3,544 bytes per checked operation in three fresh
processes, versus 20 / 3,808 on its immediate RCA baseline. Fixed and retained
costs are unchanged. It records an eight-byte public send-future growth and
unchanged full-mailbox heap cost rather than claiming every path improved.
This RCA-only diagnostic does not replace the frozen three-engine cohort or
waive the remaining arithmetic eight-event / 440-byte legacy difference.
The subsequent historical frozen three-engine capture
at signed `3aeb892` reproduces 19 / 3,544 across all six RCA observations;
both legacy arms remain 11 / 3,104. All six fresh builds and 18 processes pass
without retries. This replaces the candidate row of the previous frozen cohort,
but does not waive that gap, fixed/retained costs or the documented trade-offs.

### Raw counter timing interface

The historical 1 October admission attempt
at clean `289e5a5` built all six binaries but stopped after five upstream A/A
processes when sustained admission timed out. Evidence is retained unchanged;
there is no qualified timing comparison or automatic retry.

Each of the three existing fixtures accepts no arguments for the original
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
The historical instrumentation receipt
records boundary controls, process checks, independent review and remaining limits.

### Raw counter lifecycle interface

All three adapters also accept `--measure-lifecycle`, mutually exclusive with
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
cargo run --locked --manifest-path tests/composable-counter/candidate/Cargo.toml -- --measure-lifecycle
```

The existing adapters define host ownership: current RCA owns a current-thread
Tokio runtime, while upstream Composable and historical RCA own worker threads
joined by extraction. Thus this interval includes their different host costs;
it is not an isolated reducer-kernel measurement. The
historical teardown proof and lifecycle instrumentation receipt
record the source and test boundaries. These effect-free fixtures do not prove
general asynchronous-effect draining.

This interface alone does not provide cohort qualification, A/A sensitivity,
allocation/RSS evidence or a performance verdict. The frozen steady-state v1
cohorts below reject this different schema. Correctness smoke timings are not
comparative evidence.

### Frozen lifecycle cohort analysis

The separate [lifecycle protocol](composable-counter-lifecycle-protocol-v1.json)
predeclares three engine-specific A/A cohorts and two candidate comparisons,
each with 24 pairs, balanced 12 per execution order. Each invocation constructs
a cold adapter and performs one complete lifecycle; there is no warm-up. The
only metric is `elapsed_ns`. The controller uses six fresh release builds and
the existing source/dependency/tool/binary authentication, host checks, machine
lock and retained failure receipts. It does not silently retry failed or
unsuitable programmes.

The estimator is the geometric mean of the two order-stratum median paired
ratios. Bounds are the geometric means of stratum minima and maxima. Under
the stated IID continuous-positive-ratio assumptions, each 12-pair stratum's
extreme interval misses its population median with probability `2 / 2^12`.
The union bound over five metrics (one per cohort) and two strata gives nominal
family coverage at least `1 - 5 * 2 * 2 / 2^12 = 0.9951171875`. This is a
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
  target/qualification-evidence/0.1.0/composition/counter-lifecycle-v1
```

The lifecycle and steady-state entry points reject one another's protocols and
raw schemas. Neither numerical evaluator sets `provenance_qualified`; only the
controller's completed capture receipt can attest its checked capture process.
Implementation tests do not establish a successful native comparative run.

### Frozen counter cohort analysis

The [counter protocol](composable-counter-protocol-v1.json) freezes five cohorts:
three engine-specific A/A controls, candidate/Composable and candidate/RCA 0.7.
Each has 24 paired processes, balanced across execution order by a fixed seed.
The evaluator admits every complete raw work ledger before calculating p50,
p95, p99 and amortised loop cost. Zero percentiles retain their observations
but make that metric's multiplicative analysis unavailable.

Ratios are subject/control (lower cost is better). Each estimate is the
geometric mean of the two order-stratum sample medians; bounds use the geometric
means of stratum minima and maxima. Across all 20 metric/cohort combinations,
nominal simultaneous coverage is at least 0.98046875 under IID continuous
positive ratios within each order stratum. This is unconditional model coverage,
not a guarantee conditional on passing diagnostics, and not a pooled-median interval.

Every A/A interval must include 1 and have width at most 1.10. Order and
within-stratum first-half/last-half median factors must be at most 1.05.
These diagnose measurement suitability, not product equivalence. Failed
diagnostics do not hide estimates or interval direction. The numerical evaluator
always returns `provenance_qualified: false`; authenticated capture remains
separate. The historical qualification receipt records that distinction.

The controller command is:

```sh
python3 scripts/composition_runner.py --mode counter-cohorts \
  target/qualification-evidence/0.9.0-alpha.1/composition/counter-v1
```

Run once from a clean checkout after independent review. Do not reuse an output
name or retry a failed/noisy programme without a new documented pre-observation
decision. Original and historical dependencies remain pinned by their fixture
manifests and locks. Six fresh release targets supply independent A/B builds,
then the same binaries serve A/A and comparisons. Builds use Rust 1.98.0,
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

The frozen `composable-counter-allocation-protocol-v1.json` runs three cycles.
Each cycle runs Composable, historical RCA 0.7 and current RCA using fresh build
A, then reverses that order using fresh build B: six observations per engine.
Both Cargo metadata and all six release builds enable `allocation-measurement`;
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
and paired absolute differences against each baseline. With control/subject
both zero it reports observed zeros, not parity; zero control and positive
subject has no ratio; positive control and zero subject has ratio zero; two
positive counts permit their ordinary ratio. No pseudocounts are used. Unlike
timing log-ratio admission, a zero subject allocation count is valid. No confidence,
equivalence, superiority or unconditional successor claim follows from these
observations. `allocation_capture_complete` and `provenance_qualified` attest
successful authenticated capture; `qualified_comparison` remains false.

### Comparative cohort prerequisites

The first historical allocation capture
completed at `450f991`. Current RCA used 12 allocation events and 1,512 bytes
per checked action versus roughly four events and 237–240 bytes for the two
baselines. Setup and teardown costs were also higher, and 56 bytes remained
live at the final snapshot. These adverse observations require attribution and
a repair or justified disposition; they are not a timing or superiority result.

The historical commit-reply repair at
`6b27d07` reduced checked costs to three events and 208 bytes in all six new
candidate samples, below every baseline sample in that capture. Fixed
construction, retained footprint and teardown disadvantages remain unresolved.
The original adverse evidence is preserved; no timing claim follows.

The historical implicit-root repair at
`5e664c2` removes one fixed allocation and 460 bytes in all six current samples.
Retained heap is now 34 blocks / 12,744 bytes; checked work remains three events
/ 208 bytes per action. Higher fixed memory and teardown costs remain open.

The historical actor-capture repair at
`048c8e9` removes another 896 fixed bytes without adding an allocation. All
six current samples retain 34 blocks / 11,848 bytes; checked and teardown
costs are unchanged. This removes duplicate inline actor storage, not runtime
functionality. The remaining fixed-cost disadvantage is still counted.

The semantic fixture is not a timing programme. Extend the existing controller
and evaluator when these prerequisites are complete; do not create a parallel
qualification framework. Freeze the following before collecting candidate timing:

1. Source, adapter and protocol hashes; all three complete locks and dependency graphs;
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
Historical cohorts retain their original host labels. These requirements do not
change the crate's deployment floor.

## Remaining workload coverage

The [mounted keyed diagnostic](composable-mounted-keyed.md) executes current and
historical native keyed lifecycles at four cardinalities. It separates collection,
mounting, clearing and release costs, with exact output/state checks. The observed
mounted-memory disadvantage remains open; asynchronous bookkeeping and differing
lifecycle guarantees prevent treating these snapshots as qualified parity.

The [minimal native application size capture](composable-minimal-size.md)
records six fresh release builds and successful original/stripped executions.
Candidate RCA's stripped counter is 757,296 bytes versus 414,320 bytes for
either legacy baseline. The 82.8% size disadvantage remains open; this result
does not qualify compile time, runtime performance or the broader size matrix.

The [finite concurrent-ingress fixture](../../tests/composable-counter/concurrent-ingress.md)
now checks four native producers, gated admission, exact eventual state and
host teardown across all three engines. It preserves unbounded legacy enqueue
versus bounded RCA admission with explicit caller retry. This is semantic
coverage, not a sustained-overload or performance result.

The historical paced observation allocation diagnostic
adds lifecycle allocation/live-memory snapshots to the exact five-publication
contract. All nine processes passed; checked RCA updates allocated 15 blocks
and 1,776 bytes each versus 4/240 for both legacy application-publisher adapters.
The subsequent historical one-shot take reply change
reduced RCA to 6 allocations/464 bytes per update in three fresh processes.
The historical registration reply change
then removed 9 allocations/1,288 bytes from registration, without changing
per-update costs or retained memory.
The historical uncontended take fast path
further reduces checked updates to 5 events/320 bytes; whole-workload allocation
falls to 114 events/43,365 bytes. Scoped takes retain baseline allocation costs;
the follow-up removes redundant ingress retention and restores the original
72-byte observation handle on macOS arm64.
The remaining adverse difference stays open. Four paced updates and caller-visible
phase boundaries do not establish steady-state behaviour or isolate native
subscription costs.
The historical remaining-cost attribution
reproduces current RCA and upstream costs by allocation source and records why
the pinned channel's stream interface cannot remove its receive-listener cost.

The separate historical scoped action embedding change
reuses each scope's existing embedder Arc instead of allocating a wrapper on
each deferred admission. Its diagnostic compares RCA revisions only; it does
not change the root observation workload above or establish legacy parity.
Likewise, the historical idle observation wait storage change
removes a redundant listener box on waits entered before publication, a path
outside the paced checked-update window above.
The historical mapped sender forwarding diagnostic
measures 4 allocations / 456 bytes before and 2 / 296 after for a two-layer
mapped send. It does not change the separate unmapped ready-future comparison.

Async tasks, streams, timers, cancellation, keyed composition, observation,
saturation, move-only payloads and representative downstream applications need
separate semantic contracts. Include application glue on both sides. Label
stronger RCA-only guarantees separately. Unsupported work is a capability gap,
not a zero-cost result. Use the successor difference ledger to accept justified
departures on their merits, not to impose unconditional Composable parity.
