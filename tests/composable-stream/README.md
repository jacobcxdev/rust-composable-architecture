# Finite effect correctness fixtures

Three independent workspaces exercise current RCA, upstream Composable
`0b5baa80246a625c1bb886e6df20dae27fc97c97`, and historical RCA
`c6200234cebbbd9b0009cd5788736c78a47f32b0`. Each has its own lockfile.
These are correctness fixtures, not a performance protocol.

Run `cargo test --locked --manifest-path tests/composable-stream/candidate/Cargo.toml`
and substitute `upstream` or `historical` for the other engines. `cargo run`
with the same manifest emits two JSON lines: empty, then ordered `[3, 1, 4, 2]`.
The optional `--ready-future` mode is described below; other arguments are rejected.
Run subprocesses with an external deadline: the internal
ten-second watchdog bounds completion observation, not a wedged host join.

Native main-test cases run individually in child processes with a 60-second
deadlock watchdog. This includes stream delivery, ready futures and task ownership
in all three engines. The parent requires exactly one passing test, captures at
most 1 MiB from each output pipe, and attempts to kill and reap a timed-out child.
A timeout fails the test; it does not trigger a retry or qualify native teardown.
These children own their pipe writers and do not spawn surviving child processes.
The existing common-outcome tests keep their separate ten-second deadline.
Executable workloads still require the external controller described above.

Each case supplies an always-ready `futures::Stream` yielding distinct values
and one terminal action. The reducer acknowledges the terminal only after
checking the exact ordered trace. Extraction follows acknowledgement; the final
trace is checked again after joining the host, detecting subsequent duplicate
terminals or values. Stream exhaustion (`poll_next` returned `None`) and exactly
one stream destruction are checked independently after teardown.

Current RCA consumes the stream in `Effect::run`, awaiting each send's commit,
and waits for the causal task to finish before checking acknowledgement and
extracting state. Legacy adapters use `effects.stream`; detached producer tasks
have no equivalent independently joined handle. `producer_exhausted` reports
stream exhaustion, **not** equivalent producer-task completion semantics. Legacy
host teardown joins the worker only after terminal acknowledgement was received.

All engines reject missing, reordered or duplicate values, an early terminal,
and duplicate terminal actions through their real stream adapters. RCA also
tests the same delivery loop with an effect sender retained beyond causal
completion and store shutdown:
delivery fails, the stream is destroyed without exhaustion, and no valid result
can be reported. This does not isolate store closure from retired lineage.

Additional tests put the same finite stream behind a test-only initial gate.
Its first poll records the current waker, signals a controller thread and returns
`Pending`. The controller releases the gate and wakes the recorded waker; all
three adapters then verify empty and ordered delivery, exhaustion and destruction.
A deterministic counting-waker test checks that release wakes the most recently
registered waker. Omitting that wake fails this test; no test assumes executors
are forbidden from making spurious polls. Run integration tests with an external
deadline as above. The executable ledger still describes always-ready streams.

Another test snapshots producer progress at entry to the first value's reducer,
before updating the trace, for the always-ready ordered workload. Both pinned
legacy engines have yielded all five actions (four values plus terminal), reached
EOF and destroyed the stream. Current RCA has yielded only the first value,
without reaching EOF or destroying the stream. This is an exact event-boundary
observation, not progress inferred from elapsed time or concurrent execution.

The legacy task loop (`effects/task.rs`, upstream lines 47–48 and historical
lines 50–51) polls and synchronously enqueues every ready item in one task poll.
Its single `LocalPool` worker cannot enter the reducer until that poll returns.
RCA's delivery loop awaits each commit before polling the next item. Both hosts
remain unchanged; no blocked reducer or extra producer thread is introduced.
An eager-collection mutation in RCA's delivery loop is rejected by the entry
snapshot test even though the final ordered trace still succeeds.

Candidate-only lifecycle tests cancel the root `ActionTask` after the stream
has signalled `Pending`, without releasing its gate. Waiting for causal finish
observes one stream destruction, no exhaustion, no values and no terminal.
The external controller still owns its gate/waker at that observation; the test
then drops the controller explicitly and does not call that resource ownership
equivalent to stream destruction. A separate test drops every observation
handle, releases the stream and waits for terminal acknowledgement and natural
stream destruction before extracting the complete trace. Neither test equates
these semantics with legacy `Task` cancellation-on-drop.

Test-only finite-future ownership cases now characterise that difference directly.
One real future waits behind a controlled gate, then returns `37`; its sole
`Completed(37)` output is also its reducer terminal acknowledgement. Both pinned
legacy adapters use `effects.task(once(future))`, retain the native `Task` inside
the reducer and perform cancel, drop or detach in a reducer action. Explicit
cancel and drop destroy the still-pending future before host teardown, without
Ready or output. Detach leaves it alive: releasing/waking the gate produces the
exact terminal output and one future destruction before state extraction.

RCA explicitly cancels the root `ActionTask` and awaits causal finish, or drops
every observation handle and releases the gate. Dropping those handles preserves
the work. Successful delivery checks future destruction, exact reducer output
and an effect-side notice after `send` acknowledges commit, before extraction.
That post-commit notice is not a task join; the no-handle case does not claim
effect termination before shutdown. Legacy host completion still comes from
joined extraction, not a public causal-finish API. External controller gates and
stored wakers remain separately owned throughout the destruction observations.

These tests use the same three pinned workspaces without changing executable
ledgers. Ownership-inverting mutations fail: replacing legacy cancel/drop with
detach misses pre-teardown destruction, replacing detach with drop loses Ready,
omitting RCA cancellation misses destruction, and cancelling before its handle
drop loses successful output. A counting-waker check rejects omitted wake calls.
This qualifies the specified root-owned lifecycle policies, not equivalent
cancellation scopes, queued-output revocation or general future parity.

Pending-stream ownership tests use the existing gated `Finite` producer directly
through legacy `effects.task(stream)`. They retain its native handle in the
reducer: explicit cancellation and handle drop each enter the stream's own `Drop` while
the gate remains withheld, before host teardown, with no exhaustion, values or
terminal. A bounded synchronous notice comes from the stream's own `Drop`;
a subsequent live reducer inspection and final extraction both verify the empty
trace. The controller's separately owned gate/waker is not claimed to be freed.

Another legacy test queues two values before first reducer entry, then cancels
the native task inside that first reduction. Both pins still commit the queued
second value, but destroy the pending producer before teardown without yielding
the remaining suffix or terminal. A first-entry probe proves both values were
already yielded; live and extracted state agree. Replacing cancel with detach
fails the destruction check. This eager legacy boundary differs from RCA's
commit-paced stream adapter; it does not establish cross-engine equivalence.

Legacy detach releases the same stream only after the reducer acknowledges detach.
Both empty and ordered cases require natural entry into the stream's `Drop`, exhaustion and
the exact terminal-acknowledged trace before extraction. This deliberately uses
`effects.task`, not the automatically detached `effects.stream`, and is not a
task-join observation. RCA's existing pending-stream tests retain their different
contract: explicit root cancellation plus causal finish, or dropping all
observation handles while work continues. No new executable protocol is added.
Mutating legacy cancel/drop into detach misses the pre-teardown drop notice;
mutating detach into drop fails the exhaustion check. These are narrow ownership
characterisations, not cancellation equivalence or scoped-lifetime qualification.

Partial-delivery tests now pause the shared producer after yielding `3` from
`[3, 1, 4, 2]`. Each adapter inspects live state and requires `[3]` before the
ownership operation, rather than treating a yielded item as a committed value.
Explicit cancellation in all three engines destroys the still-pending stream
before teardown, preserves `[3]` and emits no terminal. Legacy handle drop has
the same bounded outcome; legacy detach followed by gate release delivers the
complete remainder. These cases do not exercise cancellation with already
queued later outputs. The executable ledgers remain unchanged.
The historical partial-delivery receipt remains in the local audit records.

This proves finite Send-stream delivery and one controlled Pending/wakeup
transition for these workloads. It does not qualify arbitrary stream scheduling,
general buffering capacity or behaviour under a gated consumer,
legacy cancellation equivalence, local/non-Send streams, generic stream parity, allocation/memory
costs, throughput or latency. Host models intentionally remain their normal
supported models; this fixture does not make them equivalent.

## Ready-future ledger

Pass `--ready-future` to exercise 64 serial operations through each engine's
native future adapter. Each `Start` launches one always-ready future; its
`Completed(id, value)` result must match the operation's identity and `3 * id + 7`.
The final state must contain 64 results with checksum 6496, exactly 64 future
polls and destructions, and no errors. This mode emits one
`composable-ready-future-v1` JSON object, not the default stream ledgers.

Each iteration observes both the reducer's result acknowledgement and the input
future's destruction before advancing. Current RCA additionally awaits causal
finish; its ledger labels completion `causal-finish`. The legacy adapters use
their native detached future effects and label completion
`result-acknowledgement`. Acknowledgement from inside a reducer does not itself
prove the reducer has returned. Input-future destruction does not prove its
surrounding task allocation has been reclaimed or provide a producer-task join.
Final state extraction and native host teardown remain separate checks.

The Python evidence helper validates exact fields, types, counts and the
engine-specific completion label. It rejects duplicate JSON fields and failed
subprocesses. This is a correctness prerequisite for future measurements, not
an allocation or timing protocol. It does not exercise `Pending`, wakeups, I/O,
parallel operations or equivalent completion guarantees across engines.

## Ready-future allocation instrumentation

Build with `--features allocation-measurement`, then pass the exclusive
`--measure-ready-future-allocations` executable flag. Without that feature the
flag is rejected. The optional allocator is pinned to DHAT 0.3.3; it observes
all threads in one process-wide session. Default builds keep their normal
allocator. Do not use instrumented binaries for timing comparisons.

The same native adapters and 64-operation trace produce the nested correctness
ledger. Five snapshots retain cumulative allocation events/bytes and currently
live blocks/bytes:

| Snapshot | Boundary |
| --- | --- |
| A | Before creating the host, store or shared future probes |
| B | After construction, before preparing operation 0 |
| C | After operation 19's completion boundary and caller-side temporary cleanup |
| D | After operation 63's completion boundary and caller-side temporary cleanup |
| E | After state extraction, host teardown and release of the probes |

Reported phases are construction (A→B, one host/store), initial (B→C, 20
operations), checked (C→D, 44 operations), teardown (D→E, one host/store) and
setup (A→C, construction plus initial work). The initial phase separates early
work; neither it nor the short checked phase proves steady state. Output uses
`composable-ready-future-allocations-v1` and is formatted after the final snapshot
and profiler shutdown.

All fixture coordination costs are included: action preparation, notice channels,
probe ownership, result checks and each engine's completion mechanism. No costs
are subtracted. C/D are the documented completion observations, not proof of
executor quiescence or complete reclamation of legacy reducer/task temporaries.
Native worker scheduling can move cleanup costs across adjacent windows. Final
live values are observations, not an automatic leak verdict.
Likewise, B records constructor return: a legacy worker may still be initialising,
so later startup allocations can appear in the initial phase.

The Python allocation validator checks exact nested work, all five snapshots,
integer ranges, cumulative monotonicity, live counts and recomputed phase deltas.
This instrumentation alone does not qualify provenance, comparative cohorts or
performance. Counter protocols and their previous captures remain unchanged.

The separately frozen
`docs/performance/composable-ready-future-allocation-protocol-v1.json` schedules
three forward/reverse cycles, six samples per engine from two fresh release
builds per engine. Run `scripts/composition_runner.py` with
`--mode ready-future-allocation-cohorts` and a new output directory. The existing
controller authenticates source, dependencies, toolchain, builds, binary hashes,
commands and completed raw ledgers. Busy CPU is allowed for allocation capture;
failed or timed-out processes stop the programme without retries. The analysis
retains completion differences and never marks a qualified performance comparison.
Temporary build binaries are removed after capture; retained hashes are receipts,
not a claim that those binaries remain available for independent replay.

## Common-outcome correctness fixture

The separate `outcome` integration target uses shared asynchronous oneshot notices.
Each of 64 ready futures produces one checked result and one input-future drop notice.
The caller then sends an effect-free scalar checkpoint. Legacy `Store::sync`
waits for reducer return. RCA awaits the checkpoint commit and drops its task
handle, without requesting causal completion. RCA awaits the notices inside its
current-thread runtime. No extra driver thread or blocking receive is necessary.

The final ledger requires 64 starts, results, completions, checkpoints, polls and
drops, checksum 6496, and no errors. Controls reject wrong identities, wrong values,
duplicate results, missing notices and checkpoints before the result.
Native tests run in separate child processes with a ten-second failure deadline.
The parent kills and reaps a timed-out child, reports failure, and does not retry.
The watchdog is outside the workload.

Run `cargo test --locked --manifest-path tests/composable-stream/candidate/Cargo.toml --test outcome`.
The `upstream` and `historical` manifests expose the same integration target.
The existing CI fixture matrix includes this target through its normal Cargo test command.

The feature-gated `common-outcome-allocations` binary measures the complete
64-operation lifetime. Run it with `cargo run --locked --manifest-path
tests/composable-stream/candidate/Cargo.toml --features allocation-measurement
--bin common-outcome-allocations`. The other two manifests provide the same binary.
The original executable remains each package's default.

Snapshot A precedes runtime, store and probe construction. Snapshot E follows
native extraction, host teardown and caller-owned probe release. All coordination
costs stay inside this interval. Only scalar results survive E; JSON formatting
occurs after the profiler stops. There are no intermediate or per-operation windows.
`scripts/common_outcome_allocations.py` checks the exact ledger and A-to-E deltas.
It does not authenticate provenance or qualify a comparison. The binary has no
internal watchdog; an external controller must impose a failure deadline and
retain failures without retries.

Five allocation controls run in the `outcome` target with `allocation-measurement`.
CI runs these controls and the strict decoder tests.
The historical instrumentation receipt remains in the local audit records.
Checkpoint completion does not prove task-wrapper reclamation or executor quiescence.
Successful final extraction proves fixture host teardown, not process-global allocator quiescence.
The pinned upstream reference has a shutdown race and a failed broader suite.
Its implementation receipt remains in the local audit records.
See the [qualification boundary](../../docs/qualification.md) for the failed-reference policy.
The existing frozen v1 workload, protocol and captures remain unchanged.

## Common-outcome allocation capture

The [separate protocol](../../docs/performance/composable-common-outcome-allocation-protocol-v1.json)
fixes three forward/reverse cycles, six fresh release builds and 18 processes.
It was frozen after exploratory debug instrumentation checks, before the first
qualified capture. Every initial A counter must be zero. The report retains all
whole-lifetime allocation totals and final live counts, plus candidate-minus-control
differences paired by cycle and build role. No per-operation estimate is reported.

Run `python3 scripts/composition_runner.py --mode common-outcome-allocation-cohorts
target/qualification-evidence/0.1.0/composition/<new-capture-name>` from a clean checkout.
The controller uses the machine lock, private Cargo cache, fresh build targets,
source/dependency/tool/binary hashes, exact command receipts and a 60-second process
deadline. Busy CPU does not block allocation capture. Failure stops the programme;
partial stdout, stderr and exit evidence remain in its incomplete directory.
Never remove or retry a failed attempt to replace it with a successful cohort.

Numerical analysis alone sets both qualification flags to false. A completed
controller receipt authenticates provenance, not general performance superiority.
The upstream shutdown race remains part of the pinned baseline. Neither shared
completion notices nor final live counts prove global allocator quiescence.
The first capture receipt records the clean revision, retained evidence and
remaining allocation disadvantage. That receipt remains local. It does not
establish a numerical comparison for the current public snapshot.
