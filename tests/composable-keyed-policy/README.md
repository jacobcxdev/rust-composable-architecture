# Mutable per-key policy comparison

Two independent locked workspaces exercise historical RCA
`c6200234cebbbd9b0009cd5788736c78a47f32b0` and current RCA. Original Composable
`0b5baa80246a625c1bb886e6df20dae27fc97c97` has no native keyed collection/routing
API and is deliberately excluded, not assigned a vacuous passing adapter.

Run `cargo test --locked --manifest-path tests/composable-keyed-policy/candidate/Cargo.toml`
and substitute `historical`. Both packages have formatting and strict Clippy
checks in a dedicated two-engine CI job. Apply an external process deadline
(CI uses two minutes). Tests bound worker observation to ten seconds; this
detects but cannot terminate a stuck native worker. RCA also bounds the operation
loop and state extraction separately to three seconds.

## Native adapters

Historical RCA uses `KeyedState<u8, Child>`, `RecursiveReducer` derive and its
native keyed child effect scoping. Its macro calls `Reducer::reduce` on the
selected mutable child object and wraps emitted actions through `scope_keyed`
and the parent's `From<Keyed<...>>` conversion. The child object owns policy and
progress because historical reducer objects are state.

Current RCA uses native `KeyedState`, `for_each`, state/action paths and one
shared child reducer. Mutable policy and progress live in each child state.
The shared reducer's mutable test-only policy field is used only by the
deliberately incorrect shared-policy control. Child state is not Clone.

Historical derive runs the parent before the child; current `for_each` runs
the child before the parent. This workload does not equate those orderings:
the parent records completed payloads without inspecting child state and
replacement is a separate, non-child action.

## Literal workload and completion

Children start with `(next, step)` values `(10, 2)` and `(100, 10)`. Change only
key 1's step to 4, then step keys 1, 2 and 1. Replace key 1 with fresh `(7, 3)`,
then step keys 1 and 2. Each step emits a synchronous `Completed` child action.

The complete parent output trace must be exactly
`[(1,10), (2,100), (1,14), (1,7), (2,110)]`. Final child data is checked literally:

- Key 1: next 10, step 3, completed `[7]`.
- Key 2: next 120, step 10, completed `[100,110]`.

Final order is normalised by key because the historical default HashMap does
not promise RCA's insertion order. The parent must reduce exactly 12 actions:
seven external commands and five effect outputs.

Historical native `Store::sync` completes reduction and drains these synchronous
follow-up actions. RCA awaits each committed send and its `ActionTask::finish`.
An independent atomic completion counter must have literal values
`[0,1,2,3,3,4,5]` immediately after those operation boundaries, without an extra
Store query that could hide incomplete delivery. Extraction follows: historical
`into_inner` joins its LocalPool worker; RCA `shutdown_into_state` returns state,
then the owned current-thread Tokio runtime is dropped. Final domain assertions
run after these boundaries. There are no asynchronous domain effects.

## Negative controls

All controls run through the same native routing and require specific failures:

- Shared mutable step: use one policy across both keys; the final keyed output
  trace becomes wrong. Historical uses a shared atomic policy; current RCA uses
  the shared reducer field.
- Wrong-key embedding: alter only the parent's emitted-action conversion/path
  embedder. The native scoped output reaches the other existing child and the
  complete keyed trace is rejected.
- Retained replacement policy: create fresh progress but reuse the retired
  child's step. Outputs alone still match; the literal final child state fails.

The controls change fixture adapters only, not pinned engines or product code.
They do not introduce a per-entry reducer factory or imply that fields on RCA's
shared reducer are per-entry state.

This qualifies one mutable-policy migration pattern and its actual routing
glue. It does not qualify missing-key behaviour (historical ignores it; RCA
panics), concurrent stale outputs, replacement while work is pending, automatic
policy reset, arbitrary heterogeneous/opaque reducers, non-Send resources,
storage polymorphism, reducer-object identity, general ergonomic parity or
performance. Replacements happen only after prior work completes, and the
caller constructs the fresh replacement policy.

## Mounted allocation diagnostics

Each package also provides `mounted-allocations`. For example, run
`cargo run --locked --release --manifest-path tests/composable-keyed-policy/candidate/Cargo.toml --bin mounted-allocations -- 1000`
under an external process deadline. Supported cardinalities are 0, 2, 10 and
1000; run each in a fresh process. The integration test does this with a
15-second child deadline, kills and reaps a timed-out child, and drains both
output pipes concurrently. It reuses the core allocation-test capture helper,
which retains at most one MiB per pipe and rejects excess output after draining
it. Probes must not pass pipe writers to surviving child processes. Keep the
enclosing two-minute CI deadline too.
Neither measured driver adds internal timeout timers: the external deadline
bounds native operation completion and shutdown without candidate-only timer
bookkeeping in DHAT. The three-second RCA timeouts above apply only to the
separate, unmeasured policy tests.

The driver uses DHAT 0.3.3 and native routing, not the policy fixture's mutation
modes. Both engines have identical scalar child data: next value, step,
completion count and last output. There are no per-child vectors or shared
atomic handles. Each collection reserves its requested cardinality and the
parent trace reserves six entries. Extra children start at `(next=id, step=1)`
and must remain unchanged. Static output/child-drop/root-drop counters are
fixture instrumentation; their atomic operations are included in this workload.

The ten snapshots are baseline (before owned resource construction), populated
but unmounted, Store constructed, mounted by a non-child `Mount`, seven policy
commands completed, key 1 removed, fresh key 1 reinserted and stepped, all
children cleared, state extracted and owned host stopped, and all owned domain
state released. Runtime construction is included between population and Store
construction. Historical extraction joins its worker; RCA extraction is followed
by dropping its owned current-thread Tokio runtime. No extra Store read is used
to manufacture effect completion. Native `sync` or committed send plus
`ActionTask::finish` precedes every independent output-counter assertion.

Nonempty cases run the five-output trace above, then remove key 1, insert fresh
`(7,3)` and step it to produce a sixth output `(1,7)`. The complete trace must be
`[(1,10),(2,100),(1,14),(1,7),(2,110),(1,7)]`, with 18 parent reductions.
Scalar child rows `(id,next,step,completed,last)` must be
`[(1,10,3,1,7),(2,120,10,2,110)]` both before removal and before clearing;
every untouched row is checked too. The replaced row is checked before drop.
Exactly `cardinality+2` child drops and one root-ledger drop are required.
The empty control uses the same snapshot schema but skips policy work: it runs
only Mount and Clear, with two reductions and zero outputs/child drops.

JSON schema `mounted-keyed-allocations-v1` identifies `rca` or `rca-0.7`, records
cumulative and retained allocation blocks/bytes, peak bytes and the block count
at that byte peak at each boundary, and
records output and domain-drop counters alongside them. Snapshots use fixed-size
stack storage. JSON formatting and profiler teardown happen after the final
snapshot. Tests require the literal trace, state evidence, all ten boundaries
and release counters. Temporary source-mutation checks also rejected omitted
native output, duplicate native output and retaining the extracted root in both
engines; no mutation mode remains in the measured driver.

These are **semantic-boundary-not-quiescent** observations, explicitly emitted
with `comparison_qualified: false`. Effect completion does not promise that all
runtime bookkeeping has been freed at that instant, and historical worker
bookkeeping can overlap a snapshot. Final retained bytes may include process
statics or runtime thread-local allocations; they are reported, not asserted
zero or labelled a leak. DHAT is not RSS, allocator-reserved memory, timing or a
qualified memory comparison. This fixture has no capture controller and does
not establish repeatability, host admission or broad performance parity.

Verification: both standalone packages passed five tests each, strict all-target
Clippy and formatting. Parent independently reran both mounted integration
tests, each covering all four cardinalities. Independent review found no remaining
issues after removing candidate-only per-operation timeout instrumentation.
Logs: `/tmp/rca-mounted-{candidate,historical}-{full,clippy,fmt}.log` and
`/tmp/rca-mounted-candidate-external-{test,clippy,fmt}.log`. Six rejected mutation
controls are retained in `/tmp/rca-mounted-{candidate,historical}-{omitted,duplicate,release}.log`.
These checks execute debug-profile diagnostic drivers; they do not constitute
a frozen release allocation capture.
