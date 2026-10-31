# Concurrent ingress semantic fixture

Each existing engine package provides `--bin concurrent-ingress`. Run it under
an outer process deadline (for example, 30 seconds). Success prints exactly one
JSON object with schema `composable-concurrent-ingress-v1`; errors print a
diagnostic to stderr and return failure without a success ledger.

Four OS producer threads start from one cancellable gate and each submit one ID. A
separate gate action first reports reducer entry and blocks that reducer. The
gate is not one of the four measured IDs. No effect work is spawned.

- Original Composable shares its native Store through
  `Arc` (the Store itself is not Clone). Native `send` returns after unbounded
  enqueue: all four return while the independent reducer counter remains zero.
- Current RCA uses cloned native Stores with mailbox capacity two. Exactly two
  `try_send` calls succeed and two return Full with their original ID retained.
  Both admitted receipt futures are actually polled and must return Pending
  while the gate is held. Admission is not reported as commit.
- After releasing the gate, RCA awaits the initial commits, then explicitly
  retries the two rejected IDs using awaited sends. This retry is caller policy,
  not equivalent admission semantics. Which producers win admission is unspecified.

Final state is sorted only for checking and must be exactly
`[[0,7],[1,10],[2,13],[3,16]]`, with each ID appearing once. Values are
`3*id+7`. Inter-producer order and fairness are not asserted. Legacy uses a final
native sync barrier, then recovers sole Store ownership and calls into_inner,
which joins its host. RCA uses committed receipts and a final barrier action,
then extracts state, drops its runtime and joins the dedicated host thread.
Only after these boundaries may the executable print a ledger.

The ledger has exactly these fields: schema, engine, producers, policy,
capacity, initial_admitted, initial_full, pending_receipts,
committed_while_blocked, caller_retries, final_trace, state_extracted,
host_stopped. Engine is composable or rca. The upstream policy is
unbounded-enqueue with null capacity, counts 4/0/0 and zero retries. RCA policy
is bounded-try-send-retry with capacity 2, counts 2/2/2 and two retries.
All successful ledgers have four producers, zero blocked commits, the exact
final trace and both terminal booleans true. These literals are emitted only
after corresponding native observations and final-state checks succeed.
This schema is separate from the existing measurement protocols; no controller
admission or performance qualification is implied.

Tests run all native stages for successful, omitted-ID and duplicate-ID inputs.
The premature-completion negative chooses the returned-enqueue checkpoint as
though four reductions had finished; the independent live reducer counter is
still zero and rejects that claim. It does not overwrite the observed count.
RCA also independently rejects a receipt becoming Ready while gated.

Gate/channel/receipt waits have ten-second watchdogs, not timing assertions.
The shared producer guard waits for all four arrivals before normal release.
If only part of the group starts, guard destruction cancels the waiting producers.
Cancelled producers do not submit actions. The guard releases the reducer gate
and joins every launched producer, even if another producer panics.
On fallible controller exits, host shutdown/extraction and join run before an
error returns. Joins and legacy sync have no intrinsic deadline. The outer
process deadline remains mandatory for native runtime failures.
No success ledger is emitted after an error. These checks establish
this finite native overload policy, not sustained saturation, cancellation,
throughput, latency, allocation cost or general ordering equivalence.

Run the candidate and upstream fixture tests with the pinned Rust toolchain.
Use the current CI results for verification; archived development receipts do
not establish the status of a new source revision.
