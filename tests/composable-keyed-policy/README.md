# Mutable per-key policy fixture

The candidate workspace exercises current RCA's keyed routing and child state.
Upstream Composable `0b5baa80246a625c1bb886e6df20dae27fc97c97` has no native keyed
collection API. It has no adapter in this fixture.

Run `rustup run 1.98.0 cargo test --locked --manifest-path tests/composable-keyed-policy/candidate/Cargo.toml`.
CI also runs formatting and strict Clippy checks. Its two-minute deadline bounds
the complete process. Individual policy tests bound the operation loop and state
extraction separately to three seconds.

## Native adapter

RCA uses `KeyedState`, `for_each`, state/action paths and one shared child reducer.
Mutable policy and progress live in each child state. Child state is not `Clone`.
The reducer's mutable test-only policy field belongs only to the incorrect
shared-policy control.

The child reduces before the parent. The parent records completed payloads
without inspecting child state. Replacement uses a separate, non-child action.

## Literal workload and completion

Children start with `(next, step)` values `(10, 2)` and `(100, 10)`.
Change only key 1's step to 4, then step keys 1, 2 and 1.
Replace key 1 with fresh `(7, 3)`, then step keys 1 and 2.
Each step emits a synchronous `Completed` child action.

The complete parent output trace is
`[(1,10), (2,100), (1,14), (1,7), (2,110)]`.
Final child data must match:

- Key 1: next 10, step 3, completed `[7]`.
- Key 2: next 120, step 10, completed `[100,110]`.

The fixture normalises final rows by key. It does not test insertion order.
The parent reduces exactly 12 actions: seven external commands and five effect outputs.

The adapter awaits each committed send and its `ActionTask::finish`.
An independent completion counter must equal `[0,1,2,3,3,4,5]` after those boundaries.
No extra Store query can hide incomplete delivery.
Then `shutdown_into_state` returns state and the adapter drops its owned Tokio runtime.
Final domain assertions follow both boundaries. There are no asynchronous domain effects.

## Negative controls

All controls use the same native routing and require specific failures:

- Shared mutable step: one policy across both keys produces the wrong keyed output trace.
- Wrong-key embedding: the emitted-action path sends output to the other child.
- Retained replacement policy: fresh progress retains the retired child's step, which fails the final state assertion.

These controls change fixture adapters, not product code.
They do not introduce a per-entry reducer factory or make shared reducer fields per-entry state.

This fixture checks one mutable-policy pattern and its routing.
It does not qualify missing-key behaviour, concurrent stale outputs, pending-work replacement,
heterogeneous reducers, non-`Send` resources, storage polymorphism or general performance.
Replacement follows completed work. The caller constructs the new policy.

## Mounted allocation diagnostics

The workspace also provides `mounted-allocations`.
Run
`rustup run 1.98.0 cargo run --locked --release --manifest-path tests/composable-keyed-policy/candidate/Cargo.toml --bin mounted-allocations -- 1000`
under an external process deadline.
Supported cardinalities are 0, 2, 10 and 1000. Each run needs a fresh process.

The integration test imposes a 15-second child deadline.
It kills and reaps a timed-out child and drains both output pipes concurrently.
The core capture helper retains at most 1 MiB per pipe and rejects excess output.
Children must not pass pipe writers to surviving descendants.
The enclosing two-minute CI deadline remains necessary.

The measured driver adds no internal timeout timers.
The three-second timeouts apply only to the separate policy tests.

DHAT 0.3.3 records allocations from native routing.
Each child holds a next value, step, completion count and last output.
There are no per-child vectors or shared atomic handles.
The collection reserves its requested cardinality and the parent trace reserves six entries.
Extra children start at `(next=id, step=1)` and must remain unchanged.
Fixture output and drop counters use atomics whose costs remain included.

The ten snapshots record these boundaries:

1. Before owned resources exist.
2. Populated state, before mounting.
3. Store construction.
4. Mounting through a non-child `Mount` action.
5. Completion of seven policy commands.
6. Removal of key 1.
7. Reinsertion and stepping of fresh key 1.
8. Removal of all children.
9. State extraction and owned-host shutdown.
10. Release of all owned domain state.

Runtime construction lies between population and Store construction.
Each output-counter assertion follows committed send and `ActionTask::finish`.
No extra Store read manufactures effect completion.

Nonempty cases run the five-output trace, remove key 1, insert fresh `(7,3)`,
and step it to produce `(1,7)`.
The complete trace is `[(1,10),(2,100),(1,14),(1,7),(2,110),(1,7)]`, with 18 parent reductions.
Rows `(id,next,step,completed,last)` must equal
`[(1,10,3,1,7),(2,120,10,2,110)]` before removal and before clearing.
Every untouched row is checked too.
The driver requires exactly `cardinality+2` child drops and one root-ledger drop.

The empty control uses the same schema but skips policy work.
It runs only Mount and Clear, with two reductions and no outputs or child drops.

Schema `mounted-keyed-allocations-v1` identifies engine `rca`.
It records cumulative and retained allocation blocks/bytes, peak bytes,
the block count at that byte peak, and domain counters.
Snapshots use fixed-size stack storage.
JSON formatting and profiler teardown follow the final snapshot.
Tests require the literal trace, state evidence, all ten boundaries and release counters.

These are `semantic-boundary-not-quiescent` observations, with `comparison_qualified: false`.
Effect completion does not promise that every runtime allocation is already freed.
Final retained bytes can include process statics or runtime thread-local allocations.
They are reported, not asserted zero or labelled a leak.
DHAT does not measure RSS, allocator-reserved memory or timing.
This fixture has no capture controller and does not establish repeatability or broad performance parity.
