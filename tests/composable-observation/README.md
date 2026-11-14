# Paced state publication correctness fixtures

Two independent locked workspaces compare a finite application workload on
current RCA and upstream Composable `0b5baa80246a625c1bb886e6df20dae27fc97c97`.

The upstream engine exposes no native observation. Its adapter therefore
adds explicit application glue: a reducer-owned channel sender, registration
and unregistration actions, and publication after each state update. RCA uses
native `Store::observe`. This is **legacy application publication versus native
RCA observation**, not equivalent native subscription APIs or a timing result.

Run `cargo test --locked --manifest-path tests/composable-observation/candidate/Cargo.toml`
and substitute `upstream`. Use an external process deadline
(CI uses two minutes). Receive waits are two seconds, candidate scenario/shutdown
waits are bounded, and each adapter test waits at most ten seconds for its worker. The
worker bound detects a stuck host but cannot terminate a native worker thread;
the outer process deadline remains necessary.

The pinned upstream reference has a known shutdown race. On 29 October 2026,
its all-feature suite failed locally with Rust 1.99.0 after three tests passed. The
`previous_value_publication_is_rejected` and `omitted_unregistration_is_rejected`
tests reported `publication host must finish: Timeout`.
The adapter received no result from its worker before the ten-second deadline.
A later run with pinned Rust 1.98.0 passed all 14 tests.

These failures remain part of the reference record, even when later runs pass.
CI reports upstream runtime outcomes separately from RCA acceptance gates.
The [failed-reference policy](../../docs/qualification.md) prohibits numerical
comparisons that depend on successful upstream completion.

The literal publication trace is initial `0`, then `3, 1, 1, 4`. Registration
must publish the existing state. Each update commits and its publication is
consumed before sending the next update; the repeated `1` must still publish.
Legacy `sync` waits for reduction. RCA awaits the committed send before `next`.
The trace and five publication/selector calls are checked independently.

Unregistration is a committed legacy action, but a dropped observation on RCA.
RCA then awaits an ordered `read`, which adds no selector invocation. Both
adapters commit one final update to `9` and verify that publication/selector
activity has stopped. Legacy also checks that its channel disconnected after
unregistration. This does not ascribe a channel-disconnection API to RCA or
claim equivalent handling of pending values: the observation was fully consumed
before it was dropped.

Final extracted state must have value `9`, previous value `4`, and exactly five
domain updates, separately from publication counts. Both adapters extract state
and release their owned host even after a deliberate negative-control failure.
Legacy `into_inner` joins its worker; RCA uses `shutdown_into_state` and drops
its owned current-thread Tokio runtime. There are no asynchronous domain effects.

Three negative controls exercise the same adapters:

- Suppress the repeated publication: legacy application deduplication versus
  RCA `observe_distinct`; both must report a missing publication.
- Publish the previous state: legacy publishes before mutation; RCA's native
  selector deliberately selects the saved previous value. Both must report an
  incorrect publication.
- Omit unregistration/drop: the final committed update must detect continued
  publication/selector activity.

These are intentional adapter errors, not mutations to pinned dependencies or
RCA product code. The controls require their specific error result, not merely
a panic or failed process.

This qualifies one subscriber with paced delivery only. It does not compare
coalescing, slow consumers, fan-out, distinct observation policy, dynamic
selector lifetimes, callback reentrancy or queued-value disposal. The legacy
unbounded channel is application glue; pacing avoids
using its buffering policy as a claim about native RCA observation.

## Allocation diagnostic

Build each workspace with `cargo build --locked --release --features
allocation-measurement --manifest-path tests/composable-observation/candidate/Cargo.toml`
(substitute the engine). Run its `observation-allocations` executable in a fresh
process with an external two-minute deadline. Use separate target directories
for each engine: the executable names are identical. This optional feature adds
DHAT 0.3.3 only to these fixtures, not the product.

The fixed workload remains the five-value publication trace above. One
process-wide profiler captures seven cumulative snapshots, including allocations
on the legacy worker thread. Reporting happens after profiling stops. Errors,
missing boundaries or incorrect boundary order produce no success JSON.

| Window | Completed work |
| --- | --- |
| A–B construction | Runtime/host, store and adapter glue constructed |
| B–C registration | Subscriber registered; initial `0` consumed and checked |
| C–D initial | Updates `3, 1` committed; publications consumed and checked |
| D–E checked | Updates `1, 4` committed; complete trace and count checked |
| E–F unregistration | Subscription removed; final `9` committed; no publication verified |
| F–G teardown | Final state extracted and checked; owned host and adapter dropped |

Initial means the first two updates, not a steady-state warm-up. Unregistration
includes its verification update and RCA read barriers. Both adapters use the
same trace-vector growth pattern. Adapter bookkeeping, publication glue, timeout
machinery and host costs remain included; these are application-workload totals,
not isolated subscription costs. Caller-visible checkpoints do not guarantee a
quiescent worker: legacy startup allocations may cross construction/registration
boundaries. Do not interpret individual windows as exclusive causal attribution.

Each snapshot reports cumulative allocated blocks/bytes and currently live
blocks/bytes. Phase differences report allocation traffic; retained memory comes
from live counts, not total bytes. G includes any process-lifetime allocations
left after host teardown; nonzero G alone does not establish a leak. DHAT does
not measure RSS or allocator-reserved memory. Four paced updates do not qualify
throughput, latency, steady-state costs or overall superiority. These diagnostics
are not a frozen multi-build comparison cohort.

`cargo test --locked --all-features` runs the adapter controls, isolated
profiler controls and real-reporter integration tests. CI also lints all targets
with this feature enabled.
The `allocation_controls` target uses the shared synchronous runner. Its child
process has no concurrent libtest receiver thread, but still counts workload
allocations from every thread.

These diagnostics do not establish a current numerical comparison.
See the [qualification boundary](../../docs/qualification.md).

### Reporter contract checks

The shared `reporter_tests.rs` launches each engine's actual allocation binary
in a fresh process with a ten-second execution deadline, drains both pipes,
and requires successful exit with no diagnostics. It reuses the core process
helper and rejects output larger than 1 MiB per pipe. The helper reaps a failed
or timed-out child before joining its readers. These fixture children must not
pass their pipe writers to surviving descendants.

The isolated profiler controls use the same helper with its 60-second watchdog.
They require the final harness summary to show exactly one passing test.
The reporter regression invokes an ignored child entry point that emits excess
output and requires the specific output-limit error. It is not a skipped scenario.

The reporter suite parses the entire stdout
as one strict report: exact engine, schema, instrument and literal workload;
all A–G snapshots; unsigned cumulative/live counters; six phases with literal
operation counts; monotonic totals and exact adjacent-snapshot subtraction.
It imposes no target-specific allocation budget and no zero-retention claim.

Typed deserialisation reads the original bytes, preserving rejection of duplicate,
unknown or missing fields. Separate object-shape validation rejects positional
arrays that Serde otherwise accepts as structs. A positional-work negative
control failed before this guard and passes after it. A temporary reporter
mutation claiming three checked operations instead of two also failed the
integration test at the expected comparison; the original reporter was restored.

The existing isolated-profiler child now additionally feeds the real adapter's
omitted-unregistration mode through the reporter and requires its specific
semantic error, not a success report. Existing boundary order and synthetic
allocation controls remain. Adapters, callback positions and reporter production
logic are unchanged. Only the fixture test manifests gain explicit development
dependencies on already-locked Serde/JSON packages; no dependency version moved.

These checks qualify report plumbing and validation, not a release-mode allocation
comparison or performance claim. The CI matrix runs the current RCA and upstream
fixtures with their separate acceptance policies.
