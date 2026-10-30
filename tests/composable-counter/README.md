# Counter comparison fixtures

The two locked workspaces exercise current RCA and upstream Composable
`0b5baa80246a625c1bb886e6df20dae27fc97c97`. The retired RCA fork is not a supported
comparison target.

## Allocation instrumentation

Build a fixture with `--features allocation-measurement` and run its binary
with `--measure-allocations`. Without the feature this mode is rejected.
Correctness and timing modes remain available; do not use instrumented binaries
for representative timing or binary-size evidence.

The optional global allocator is exactly DHAT 0.3.3. One process-wide profiler
captures five snapshots:

- A: before constructing the adapter and its owned host.
- B: after the constructor returns.
- C: after 20 committed warm-up actions.
- D: after 10,000 committed checked actions, including receipt cleanup.
- E: after state extraction and owned-host teardown return.

Parsing happens before profiling. The snapshots and work accounting remain on
the stack. Ledger validation, JSON formatting and output happen after profiling
ends. Failed work produces no measurement.

`composable-counter-allocations-v1` contains `schema`, `engine`, `instrument`,
`work`, `snapshots` and `phases`. Each named snapshot contains cumulative
`allocated_blocks`/`allocated_bytes` and current `live_blocks`/`live_bytes`.
Successful reallocation counts as one allocation event and the full new size
in cumulative bytes, not only growth. Live counts track the resulting block
and its new size. These are DHAT allocation events, not unique live objects.

Phases contain `operations`, `allocated_blocks` and `allocated_bytes`:
construction B−A (one), warm-up C−B (20), checked D−C (10,000), teardown E−D
(one), and setup C−A (one construction plus its 20 warm-up actions). Live counts
can decrease and are preserved directly; no saturating subtraction or phase
peak estimate is used.

Constructor return need not mean the worker has finished initialising.
Keep the warm-up and combined setup counts when interpreting that split.
Process-wide counting includes Rust worker-thread allocations but excludes
native allocations outside Rust's global allocator, worker-stack mappings,
allocator-resident memory and RSS. The different executor topologies remain
part of the adapter costs.

The `allocation_controls` target runs each control in a separate process without
a concurrent libtest receiver thread. This prevents test-harness allocations from
entering the global profiler. The controls cover worker allocations, phase
attribution, warm-up exclusion, teardown frees, allocation-free accounting,
reallocation semantics and rejected work. CLI tests execute the actual binary
and check instrumentation/mode admission and parse the emitted JSON.

Allocation children and CLI checks reuse the core bounded process helper.
Each direct child has a 60-second watchdog and a 1 MiB limit per output pipe.
The helper drains both pipes and reaps the child before joining readers.
These fixtures must not pass their pipe writers to surviving descendants.
The allocation runner requires exactly one passing, non-ignored child test.

Run complete suites under an outer process deadline for native joins and sync
calls, which have no intrinsic watchdog. A normal upstream suite stalled
in the unchanged lifecycle teardown test during qualification; its 120-second
timeout is retained in the instrumentation audit, not replaced by passing runs.

## Failed upstream reference

The upstream fixture pins Composable
`0b5baa80246a625c1bb886e6df20dae27fc97c97`. Its shutdown can stall after completed
counter work. The native adapter calls `Store::into_inner` without changing
the upstream runtime.

In [CI run 40850241800](https://github.com/jacobcxdev/rust-composable-architecture/actions/runs/40850241800/job/123367608381),
13 counter tests passed. These two tests remained active until the two-minute
deadline stopped the step:

- `enqueue_only_control_is_rejected_at_return`
- `extraction_observes_gated_reducer_teardown`

The pinned channel wakes its receiver before the last sender releases its shared
ownership. The receiver can observe that sender, wait again and receive no later
wake. `Store::into_inner` then cannot join the worker. This race is consistent
with the observed timeout. The hosted log does not identify its exact thread
interleaving.

CI retains upstream runtime outcomes as reference observations, not RCA acceptance
gates. Counter suites, including allocation instrumentation, keep their external
deadline and report their actual outcome. Formatting, lint and compilation remain
required for both engines. RCA runtime checks remain required.

The upstream reference remains failed even when a later observation passes.
No numerical comparison can depend on its successful completion without new
qualification. The [qualification boundary](../../docs/qualification.md) applies
to every fixture that uses this upstream runtime.

## Minimal application binaries

Each package also provides `--bin minimal-counter`, a separate application for
future clean-build and stripped-size measurements. It imports no benchmark
helpers or measurement modes and uses the default allocator. The literal input
trace is `[3, 1, 7, 2, 9, 4, 16]`; successful output is exactly `42\n` after
state extraction and owned-host teardown. Legacy applications use native
`Store::sync` and `into_inner`; RCA awaits committed sends, extracts with
`shutdown_into_state`, then drops its current-thread Tokio runtime.

The application tests run
the native workload, reject omitted/duplicate inputs, and check reducer release
using a witness compiled only for tests. That witness is not an independent
host-thread join proof. Existing gated benchmark proofs remain unchanged and
are not direct tests of these separate applications. Tests have a ten-second
result deadline; retain an external process deadline for the whole suite.
No build-time or binary-size qualification follows from adding these binaries.

Each package retains its benchmark as `default-run`; select the minimal
application explicitly with `cargo run --locked --bin minimal-counter
--manifest-path tests/composable-counter/<fixture>/Cargo.toml`. The existing
measurement controller also selects its original binary explicitly.
