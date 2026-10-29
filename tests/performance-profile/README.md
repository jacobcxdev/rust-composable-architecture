# Exact-work performance harness

Standalone, unpublished profiling input; not a product benchmark or a timing
claim. Run `rca-performance-profile WORKLOAD SIZE ITERATIONS`. Every counter/effect
prefix below has `-native` and `-deterministic` names. Direct shared-cache names
are listed separately. The original `store` and
`effects` names retain their meanings. The deterministic names require the opt-in
`deterministic` feature; without it they fail during argument parsing, before
host construction or allocation measurement.

Direct keyed collection modes need no optional features: `collection-insert-head`,
`collection-insert-middle`, `collection-insert-tail`, `collection-sort-sorted`,
`collection-sort-reversed`, and `collection-sort-ties`. SIZE is exactly 0, 1, 16,
or 1024; ITERATIONS is exactly 1 or 256. Each iteration owns a separate prebuilt
fixture and performs one operation inside `profile_collection_insert_work` or
`profile_collection_sort_work`. Insertion reserves N+1 entries, removes the
incoming pair during construction, and reuses that owned value and warm lineage;
it does not measure first-insertion lineage creation. Sorting never resets or
re-sorts a fixture inside the work window. `collection-insert-control` and
`collection-sort-control` visit equivalent fixtures but perform **zero** RCA
operations; they describe driver cost, not an automatic subtraction baseline.

The collection record checks independent full order and lookup oracles, literal
comparison counts, completion even at N=0/1, and every move-only value's release.
Tie order is an implementation-compatibility oracle, not a public stability
guarantee. Comparator counting, dispatch, and completion flags are visible harness
costs. Normal and `allocations` builds execute the same body; DHAT's five snapshots
have no retained-cache allowance. Whole-run peak includes all prebuilt fixtures
(at most 262,400 values), not just operation scratch. `value_size` reports the
instrumented payload size. These checks make no CPU-speed claim: normal-allocator
instruction qualification must attribute the named work boundary and retain
controls, since hashing and allocation can vary between processes.

Direct issue reporting has five synchronous public paths, available without
`deterministic`: `issue-direct` (SIZE=1), and `issue-owned`,
`issue-owned-disabled`, `issue-borrowed`, `issue-borrowed-failed` (SIZE=0/1/16/64).
ITERATIONS is 1..=1000 actual reports; each owns a 4096-byte message and
4096-byte cause. The first report is the `cold` phase and the rest are `work`;
there is no warm-up, Store, executor, recorder or simulated Closed result.
The issue schema is `rca-issue-performance-profile-v1`. It validates every
payload byte, ordered callbacks/recoveries, typed failure owners and final
release; `source_expectations.issue_clones` is a source-derived count, not an
observed allocation count. Callback byte scans and failure probes are visible
harness costs. Allocation builds use unchanged six-snapshot DHAT measurement
with no accepted retained cache.
Empty selections return one disabled outcome containing the original issue per
report: callbacks, endpoint owners, faults and issue clones are zero, but full
payload recovery still runs. Owned/borrowed empty modes exercise two public
implementations; their disabled/failed labels do not create missing endpoints.

For the counter/effect modes below, `ITERATIONS` is a positive count of committed root actions. Each root finishes
its complete causal task before the next root. Let `N=SIZE`, `R=ITERATIONS`, and
`Q` be responses per root:

| Prefix | SIZE meaning | Q | Exact `work_facts` per root |
| --- | --- | --- | --- |
| `store` | Must be 0 | 0 | All zero |
| `effects` | Positive immediate-send chain length | N | All zero |
| `future-chain` | Deferred response chain length | N | N future entries and captures |
| `run-chain` | Run/sender response chain length | N | N run entries, completed sends and captures |
| `merge-dense` | Concurrent deferred branch count | N | N future entries and captures |
| `concatenate` | Ordered deferred branch count | N | N future entries and captures |
| `merge-sparse` | Input width, first send then empty inputs | 0 if N=0, otherwise 1 | All zero |
| `merge-empty` | All-empty input width | 0 | All zero |
| `map-deferred` | Nested mapper depth over one deferred response | 1 | 1 future entry, N mapper calls, N+1 captures |
| `cancellation-depth` | Distinct nested cancellable identities over one send | 1 | All zero |
| `timers-at` | Actor-owned one-shot timers at one absolute deadline | N | N submitted timer registrations |
| `timers-after` | Actor-owned one-shot timers after a 1 ms delay | N | N submitted timer registrations |

New modes accept zero; widths are bounded at 256 and map/cancellation depths at
16. Total reducer calls are `R * (Q + 1)`, responses `R * Q`, causal completions
`R`, and checksum `calls * (calls + 1) / 2`. Multiply table counters by R;
every created capture must also be dropped. Omitted counter kinds are zero.
Actions carry consecutive values starting at one; state counts reductions and
accumulates their sum. Overflow, invalid inputs, missing responses, causal
cancellation, duplicate identities, wrong mapped payload, wrong promised order,
wrong state/checksum and retained owners fail the
process without a success record.

Dense merge and timers require exact per-root membership but impose no concurrent
order. Concatenate and chains require consecutive order. Deferred mapping adds
one to a separate payload per mapper and validates that payload equals N, even
at depth zero. Cancellation responses return `Effect::none`, exercising inherited
nonempty generations without adding a cancellation race. Sparse/all-empty merge
calls the production constructor directly with its original input iterator;
production preallocation is capped at 12 and all-empty merge allocates no
retained effect buffer. No unbounded width-proportional allocation is claimed.

Absolute timers share `context.now() + 1 ms`. Relative timers each use the
production `send_after(1 ms)` path, whose actor compilation computes deadlines;
native deadlines can differ slightly. Deterministic work explicitly advances
virtual time by 1 ms after each root commits. Native qualification uses real
timers, not immediate sends; it makes no elapsed-time or timer-speed claim.

The executable finite release matrix is `tests/cli.rs::finite_width_and_depth_matrix`:
each width mode uses `(N,R)=(0,1),(1,1),(16,1),(256,1),(16,3)`; native timers omit
`(256,1)` and virtual timers include it. Map uses `(0,1),(1,1),(16,1),(16,3)`;
cancellation uses `(0,1),(1,1),(4,1),(16,1),(16,3)`. This is 47 native and 49
deterministic fresh-process cases, not a Cartesian product. An additional tiny
matrix checks independently specified counters at R=3. Run the CLI tests with
`cargo test --release` in each of default, `deterministic`, `allocations`, and
`deterministic,allocations` configurations.

Native cases use a current-thread Tokio host with a 300-second deadline per
drive operation. Deterministic cases use the real production Store with
`rca-test::DeterministicRuntime` and a checked poll budget of `16 * reducer_calls
+ 1024`. Deterministic driver costs are not production Store costs. Both lanes
share the same actions, state, useful-work, validation and teardown code.
Default builds contain only the native host: no `rca-test` or core `diagnostics`
or `test-util`. Opting into `deterministic` adds `rca-test` and futures-executor,
enabling core `diagnostics` and `test-util` for **both** lane names in that
binary. Neither lane uses TestStore. Counter/effect modes have no warm-up actions.

`Host::run_ready_bounded` is adopted by the observation and Document workloads; existing
binary callers and their old drive contracts remain unchanged. It rejects
zero outer-poll caps on both hosts and delegates native work to the existing
cooperative deadline. Deterministic ready-only work polls one persistent public
Drive, continuing only on witnessed wakes, with separate scheduler-task and
outer-poll caps; unwoken Pending is rejected without advancing timers or waiting
for external signals. Every poll/destructor must return. Rejection drops the
client, not admitted tasks or physical owners: release/cancel external owners
before separately bounded cleanup. Its one wake Arc and atomic/refcount work
are harness overhead; isolated deterministic `Measurement(None)` controls
require zero retained blocks/bytes after all owners drop, without a TLS allowance.
Observation records identify `drive_contract="ready-bounded-v1"`. The adopted
body is unchanged from helper qualification; normal observation execution and
fresh-process retention are separately tested, including the distinct Document path.

`profile_useful_work` is a synchronous, non-inlined profiler boundary around
the actual host drive and all root/causal completions. Parsing, construction,
state validation, shutdown, final ownership validation and reporting lie
outside this boundary. One parked host task and two weak owner probes witness
host-task and reducer teardown; these are harness overhead, not useful work.
The witness's initial poll occurs in the useful-work drive.
Seven atomic counters share one harness-owned probe allocation. New modes keep
a bounded per-root identity buffer in Store state; validation clones it and
teardown releases both copies. Future/run/map captures clone the probe and the
reducer ownership witness. These counters, checks and buffers are visible
harness costs, held constant in paired product comparisons, not attributed to
RCA. Timer registration counts mean submitted one-shot programs: exact delivered
identities and final causal completion certify that all reached actor dispatch.

Build normally for Instruments/Callgrind with the system allocator. Feature
`allocations` independently adds DHAT 0.3.3 and JSON attribution parsing, using
the same actions/assertions. Normal release binaries contain neither dependency;
the CLI tests use JSON parsing only as a dev-dependency. Release and bench
profiles use optimisation level 3, fat LTO, one code-generation unit, no
incremental compilation, debug symbols and no stripping.

`whole_run_peak_bytes` is DHAT's maximum live byte count.
`whole_run_peak_blocks` counts blocks live at that byte-peak instant, not the
maximum live block count independently. It can be below a later phase snapshot's
live block count. Neither field is a per-phase peak.

## Exhaustive owned TestStore pairs

`teststore-owned P I` uses the real full-state exhaustive TestStore, one deep-owned
payload moving through each non-Clone `Start -> Response` pair, and one independently
constructed scalar RevisionCapture. P is exactly 0, 8, 4096 or 65536 bytes;
I is 1..=256. `teststore-idle P 1` is a separate cold no-action control, not
an exhaustive-action sample. Both names require `deterministic` before parsing
dimensions or constructing a measurement. Schema is
`rca-teststore-performance-profile-v1`; failure has a separate contract below.

For I pairs: commits I/I; reductions, full-state boundaries, expected updates and
task finishes 2I; matched receives I; full-state clones 4I+1 and equalities 2I;
declared captures 4I+2, clones/equalities 4I+1, and sets 2I. Actions created/dropped
are 2I, byte owners 6I+2, and declared owners 10I+3. Idle substitutes I=0 in these
ownership formulas. Every byte and final pointer are independently checked,
renderer calls remain zero, and all owners balance after teardown. P=0 retains
owned-value counts but implies no empty Vec heap allocation.

`profile_teststore_pairs` contains only useful pairs, their assertion/poller work
and causal finishes. Construction, borrowed validation, exhaustive finish/drop
and reporting are separate phases; no warm-up exists. The TestStore-local safe
wake-aware poller pins one operation, rejects unwoken Pending immediately, and
caps witnessed-wake Pending at 1024 client polls. It neither drives another runtime
nor advances time. The 4096 scheduler-task budget belongs to each internal Drive/
settle, not a whole operation; private scheduler polls are not invented. Blocking
polls/destructors require the fresh child's hard containment deadline, which is
never a completion oracle. Drive contract: `teststore-operation-ready-v1`.

Normal `deterministic` and `deterministic,allocations` use the same fixture and
checks, with core diagnostics/test-util enabled. T uses `Measurement::new(None)`;
final retained DHAT blocks/bytes must both be zero, with no futures-executor TLS
allowance. Probe/wake Arcs, atomic counts and Weak witnesses are explicit harness
costs. The six owned and three idle rows in `tests/teststore_cli.rs` form the finite
matrix, not a Cartesian expansion. Mismatch/rejection controls use isolated
owned children, retain typed/panic failures, and catch expected failing Drop only
for disposal; they never resume or credit success. These rows do not establish
projected/shared/private capture, snapshot, full API or performance conclusions.

### Structural TestStore mismatch

`teststore-mismatch P 1` requires deterministic and P=8/4096/65536. It sends
one external Response(1), correctly expects roots0/responses1/last1 and declared
(0,1), but flips the final expected byte. There is one commit, zero effect
responses and zero completed assertion boundaries or causal task finishes.
Semantic equality is authoritative even for identical rendered text. The opaque
multiline renderer executes twice, with proportional P-sized output; excerpt
limits do not bound renderer allocations or the complete panic payload. T's
successful renderer remains lazy (zero calls).

Construction is followed by caught failure-return while the panic, store,
three byte owners and one declared baseline remain live; the moved capture batch
drops its expected and actual revisions on the root mismatch. Validation checks every
actual byte, sticky incomplete finish and unchanged counts. Separately expected
Drop unwind disposes the failed store; no success work resumes. Final counts
are state clones3/equality1, declared captures3/clones2/equalities2/set1,
actions1 and byte/revision owners6/6, all dropped. Weak witnesses, errors,
panic payloads, probes and operation Wakers are released before teardown.
The child-local deterministic panic hook is restored before return.

Schema `rca-teststore-failure-profile-v1` reports
`outcome: "expected-state-mismatch"` and
`phase_semantics: { "work": "failure-return" }`. It embeds unchanged
`Record::json`: `allocations.phases.work` is construction-to-caught-failure,
not successful work. `Measurement::new(None)` requires final retained0/0;
only whole-run peak is available, never a per-phase maximum. Normal and DHAT
run the same three finite rows. This is qualification, not profiling or an
allocation improvement, timing, coverage or Composable comparison claim.

## Controlled Document traces

Feature `persistence` admits `document-coalesced-native` and, with the separately
opted-in `deterministic` feature, `document-coalesced-deterministic`. SIZE is
edits per burst B (1/2/16/64 with I=1..20, or B1000 with I=1 only); ITERATIONS
is sequential bursts I,
not the existing counter/effect modes' root denominator. Encoded payload P is
4096 bytes. Separately named `document-coalesced-8-native` / `-deterministic`
accept only B=2,I=1,P=8. Invalid dimensions/features fail before construction.
`document-coalesced-65536-native` / `-deterministic` likewise accept only B2/I1,
with P65536. Larger payload/burst/iteration combinations are not admitted.
Finite primary points are (2,1), (2,3), (2,20), (64,1), (64,3), (64,20).
The finite scale amendment adds P4096/B1/I3, B16/I3, B1000/I1 and P65536/B2/I1
in each lane, not their Cartesian product.

One real Store and paired production Document first complete a present tag-7
cold load outside `profile_document_work`. No warm-up action exists. Every
burst holds the first save, commits the remaining edits, completes first/latest
saves for B>=2, awaits every causal root and consumes one clean flush receipt. B1
completes only its first save: it is a single-edit baseline, not coalescing
evidence. B2 is the first/latest boundary; B16/B64/B1000 omit intermediate writes. Task
completion is never awaited before releasing its held save. The backend has
bounded(1) request/reply gates, one pending request and one moved latest byte
slot, without byte history. Every byte and tag is validated; scalar checksum
does not replace exact validation. All E=B*I deep-owned action payloads are
preconstructed, and their drops are still counted. Validation uses borrowed
scalar projection and verifies extracted-state parity without cloning values.

Cold roots/responses/reductions/completions are 1/1/2/1; load/decode are 1/1,
Payload clones are 1 and drops are 2. Work roots/completions are I(B+1),
responses 2I, reductions I(B+3), encodes/saves/writes 2I, receipts I,
Payload clones E+2I and drops 2E+2I. Write tags are each burst's first/latest,
their sum is B*I*I+I, saved bytes are 2IP, and receipt revisions are kB.
Final tag/revision/ack are E; failure, active save and barriers are absent.
Original Payloads E+2, clones E+2I+1 and drops 2E+2I+3 must leave live=0.
Snapshots drop before backend await; held-save gates independently check this.
For B1 only, responses/encodes/saves/writes are I, reductions 3I, work clones 2I
and drops 3I. Write tags sum to I(I+1)/2, saved bytes are IP; final originals I+2,
clones 2I+1 and drops 3I+3. Physical/delivery guards total I+1 (otherwise 2I+1).
Every case still verifies final tag/revision/ack, receipts and owner release.
Preconstruction owns E action buffers, an initial-state buffer and a raw cold-load
buffer, before transient copies: `(E+2)P` payload bytes, not measured RSS.
The B1000 addition holds 4,104,192 such bytes; P65536/B2 holds 262,144. Existing
P4096/B64/I20 remains the largest admitted input population at 5,251,072 bytes.

The `rca-document-performance-profile-v1` schema reports separate cold/work
counts, construction originals, entering work originals, exact physical/delivery
guard counts and separate join/extraction/owner/host facts. A controller admission
fence and physical join precede shutdown; shutdown is not flush or physical join.
Correctness controls execute typed failures/retries, receipt ownership, captured
barriers, close fencing, abort, shutdown abandonment and cancellation followed
by external completion/rejected delivery/join and a new Document load. Deliberate
byte/task/counter/receipt/physical/capture corruption rejects and releases owners.

Every drive uses `ready-bounded-v1`, checked scheduler S=64*(2+I*(B+3))+4096,
outer C=S+1, and native cooperative deadline 5 seconds. These bounds are per
call, not aggregate process bounds; neither preempts a nonreturning poll/drop.
The host witness is first driven during cold load. Native uses established
source-specific Tokio retention attribution; deterministic `Measurement(None)`
requires zero retained blocks/bytes. No Document/helper allowance is added.

Persistence-only binaries do not enable `file`, `rca-test`, core diagnostics or
test-util. Deterministic opt-in enables core diagnostics/test-util for both lane
names. Records identify `persistence-production` or
`persistence-deterministic-enabled`, plus allocator features; neither is labelled
minimal-production. Normal allocator and separately opted-in DHAT builds share
the same assertions. Backend/controller, codec copies, deep Payload probes,
atomic counters, projection and host driving are visible harness costs, not
Document costs. No native/deterministic performance equivalence is implied.

This is a representative owned-snapshot/coalescing/receipt slice, not complete
W09/W12 or S6 coverage. Unadmitted dimension combinations, multiple documents, observation,
BackendKey, SharedDocument/cohorts/conflicts, atomic RevisionBackend, FileBackend
and broader native host/physical I/O lanes require separately reviewed work.

### Ordinary joint editor and document save

With `persistence,deterministic`, run `joint-document-deterministic 2 1`.
Only those dimensions are admitted. Two keyed editor children use an injected
document client, `SharedKeyCache` and real keyed `SharedDocument` owners. Opening
both documents loads 7 and 20; a separate cache hit must neither load nor seed.
Changing only the first draft to 8 triggers one `on_change` delayed flush. An
equal edit and unrelated parent action trigger none. Virtual time advances by
10 ms; no wall-clock latency or timer-speed claim follows.

The held physical save separates local publication from acknowledgement: the
document and cached shared value become 8 while the backend remains 7, acknowledged
revision remains 0, and the causal edit task remains unfinished. Releasing the save
makes backend value 8 and acknowledged revision 1, then emits one application Note.
The sibling stays 20 throughout. Core distinct observation delivers only
`[7,20]` and `[8,20]`; polling again after acknowledgement and draining any actor
Take round-trip must remain pending, not deliver a duplicate or report closure.

Exact counts are two loads, one save/write, three dependency lookups, one cache
hit, zero hit seeds, one changed edit, one equal edit, one unrelated action,
one on-change reaction, one timer action and one save-success action. Editor
parent/child reductions are 9/8. Document reductions are six cold
(`KeyLoad`, `Loaded`, `Subscribe` per miss), two save actions (`KeySave`, `Saved`),
then two owner-close `Abort` actions: the final JSON counter is 10. Distinct
selection runs three times, independently from its two deliveries. All three
physical deliveries complete and drop; both backends close, no save remains
pending, backend weak owners release and the parked host witness drops.

The same editor reducer has a TestStore companion with root projection `()` and
separate owned private/shared captures. Shared publication is asserted before
physical completion, and private acknowledgement is asserted separately on
`Saved`. The companion is semantic proof, not part of the measured production
Store work. Normal and DHAT builds use the same workload and oracles; five
measurement phases retain no executor-cache allowance. Backend gates, probes,
observation checks and host driving remain visible harness cost. This single
ordinary-save chain does not claim cancellation, replacement, CAS conflict,
native I/O, contention or all possible component interactions.

### Keyed document replacement and cache rebind

With `persistence,deterministic`, run
`document-keyed-rebind-deterministic 2 1`. Dimensions are fixed: two schedules,
one replacement in each. Native mode and other dimensions are rejected.

Each schedule replaces a keyed document while its old backend load and producer
join are pending. Both release orders are exercised. Closing the retired owner
must remain pending after either first release. Explicit cache reset/rebind
keeps the old reader at 7, gives the fresh reader 42/revision 2, and lets the
unrelated sibling advance to 23/revision 3. Late old value 99 cannot replace the
fresh binding. Production Store, keyed composition, document ownership and cache
APIs perform the work; controlled gates supply ordering without sleeps.

One invocation performs exactly ten revisioned loads: four cold, six useful;
there are zero saves. Ten physical completions and delivery guards, two producer
join guards and two cancellation callbacks are accounted for. Final backend and
host owners, pending physical queues and allocated retention must all be zero.
Construction, cold work, useful work, validation and teardown are separate phases.
Causal assertions and polling barriers inside the useful-work boundary remain
explicit harness costs; this is not a raw backend throughput measurement.

Four fault controls run independently in each schedule: wrong fresh value,
unexpected sibling mutation, premature close and retained backend ownership.
Each must return its named error and release both gates during cleanup. Ordinary
and allocation-enabled focused tests qualify this fixed interaction, not every
document failure, W18 interaction, or native/deterministic equivalence.

## Direct shared-cache traces

`shared-race-cold CALLERS` and `shared-race-hit CALLERS` add native-thread
cache resolution at CALLERS=1, 2, 4 or 8. Cold forces one candidate per caller
before publication; hit makes 64 lookups per caller without provider work.
Returned cohorts remain live through serial validation, unlike the serial
traces below, so these cells are not equal-work speedup comparisons. They do
not measure concurrent reads, lock wait time or native instruction cost.
At 8 callers, cold retains 8 returned cohorts; hit retains 512 plus its cold base.
Both retain one winning candidate, then verify shared mutation and final release.

`shared-reader-hit`, `shared-cohort-hit`, `shared-reader-expired` and
`shared-cohort-expired` call the real public cache APIs without a Store, host,
executor or runtime lane. Each future is polled once with `Waker::noop()`;
Pending fails immediately. The normal fixture's provider uses `ready`, returns
Initial absence, subscribes without delivery and never saves. Every key call
uses one explicit probe Arc clone and the same address/codec identity.

One namespace and one separately measured cold binding precede useful work.
Let `I=ITERATIONS` (positive) and `H=SIZE*I`. Hit SIZE is exactly 1 or 64;
expired SIZE is exactly 1. A live base suppresses seed/load/subscribe throughout
H same-API hits, each immediately read and dropped. Useful checksum is `7H`.
Total successful lookups are `1+H`; seed/Initial-load/Initial-subscribe and
value/subscription creation/drop totals are each one. Reads total `H+3`:
cold, useful hits, validation and final captured-reader teardown. Capturing once
keeps the subscription alive after the base drops. Cohort mode creates `1+H`
independent writable cohorts; reader mode creates none. These are API-derived
cohort counts, not measurements of private allocations or Arc clones.

Expired cold work constructs, reads tag zero and releases every owner. Each
useful cycle j constructs tag j in the same namespace, reads once, drops and
checks exact drop counts, weak expiry and closed producer before the next miss.
Useful seed/load/subscribe/create/drop counts are I, checksum `I(I+1)/2` and
last observed tag I; totals include cold and are I+1. There is no reset or
surviving capture. Expired cold includes release; hit cold retains the base,
so these cold phase costs are not equivalent work.

Schema `rca-shared-cache-performance-profile-v1` records actual feature/allocator
identity and `execution="direct-ready"`; iterations/cycles are I, never Store
roots. Roots, responses, causal tasks, provider-returned values and domain clones
are zero. Move-only Value and Subscription each have exact Drop counters and
unique Arc witnesses. Fixed scalar atomics, latest weak witness/subscriber slots,
identity strings, ready polling, count/expiry checks and witness allocations are
visible harness overhead, not RCA-only costs. No event log grows per cycle.

The non-inlined useful boundaries are `profile_shared_cache_hits` and
`profile_shared_cache_expired`. Allocation phases are construction/cold/work/
validation/teardown, with six fixed snapshots. Counter phase deltas exclude cold
from useful work. System records have `allocations=null`; independent DHAT runs
require final retained blocks and bytes both zero, with no executor allowance.
All owners, weak routes, namespace and probes drop before the final snapshot;
JSON materialises only after profiling shutdown. The cold known-allocation
control preserves the existing warmup and five-snapshot controls separately.

`tests/shared_cache_cli.rs` runs 18 fresh-process success cases per graph:
both hit names at `(SIZE,I)=(1,1),(64,1),(1,3),(64,3),(1,100),(64,100)`;
both expired names at `(1,1),(1,3),(1,100)`. Default and allocations qualify
36 cases. Deterministic-enabled builds retain honest feature identity but do not
constitute another direct runtime comparison. All four feature graphs receive
strict all-target Clippy; the two direct release test graphs retain old/E tests.

Correctness-only controls cover live seed suppression, real retained-owner
faults, move-owned provider/subscription failures and retries, present initial
loads, independent-cohort rebinds, final-capture/namespace/reset lifetime, weak
producer rejection, require/load/load_reader distinctions, codec collision,
Pending cancellation and same-thread losing-candidate destructor re-entry. The
re-entry schedule runs in an isolated child with a five-second hard deadline.
It does not prove concurrent publication safety or general deadlock freedom.
Native 2/4-caller publication, overlapping reload arbitration, poison/update
panic, Store bridges, partial projections and all other service families remain
follow-ups, not coverage or performance claims from these direct traces.

```sh
cargo build --offline --locked --release --manifest-path tests/performance-profile/Cargo.toml
cargo run --offline --locked --release --manifest-path tests/performance-profile/Cargo.toml -- effects-native 16 100
cargo run --offline --locked --release --manifest-path tests/performance-profile/Cargo.toml --features allocations -- effects-native 16 100
cargo run --offline --locked --release --manifest-path tests/performance-profile/Cargo.toml --features deterministic -- effects-deterministic 16 100
cargo run --offline --locked --release --manifest-path tests/performance-profile/Cargo.toml --features deterministic,allocations -- effects-deterministic 16 100
```

The single JSON line has schema `rca-performance-profile-v2`, identity/inputs,
committed roots/responses, reducer calls, causal completions, final counter and
checksum, and separate extraction/drop/host-stop facts. Version 2 adds
`features` with the actual compile-time `deterministic` and `allocations`
booleans, plus `feature_profile`: `minimal-production` when `deterministic` is
off, otherwise `deterministic-enabled`. This is the dependency feature profile,
not Cargo's optimisation profile or the selected workload. In particular a
`store-native` record from a deterministic-enabled binary is **not** a minimal
production measurement. Compare records only with matching feature identity;
all existing fields retain their version-1 meanings. Additive `work_facts`
contains the seven exact counters above;
it does not redefine committed responses or reducer calls. `allocations` is null
for normal builds. Allocation builds include cumulative/live snapshots at
baseline, construction end, useful-work end, validation end and teardown end;
phase request deltas and live endpoints; and whole-session peak bytes plus
blocks live at that byte peak. JSON formatting happens after profiler shutdown.
An owned guard finalises DHAT into memory on success, early errors and unwinds,
never a heap file, and releases the session exactly once. Current-session live
allocation records are parsed and attributed after profiler shutdown, outside
all measured phases.

Cold executor caches remain visible where the workload uses them. Native
teardown retains Tokio's `CURRENT_PARKER`; deterministic work driven through
futures-executor can retain its `CURRENT_THREAD_NOTIFY`. Ready-only deterministic
keyed and keyed-scaling workloads do not enter that executor: they require zero
retained blocks and bytes, with attribution source `none`.
The pinned-version TLS caches live until thread
exit. Captured DHAT stacks identify one 56-byte and one 32-byte block,
respectively, on the qualified Apple Silicon host. The expected size is derived
from the pinned payload fields and platform type sizes, plus the Arc header;
each retained block must also have both the pinned TLS initializer and Arc
owner-allocation frames in that session's full stack. Unknown/missing frames,
malformed profile data or disagreement with live snapshot totals fail, even
when an unrelated retained block happens to have the same size. The record names and quantifies the cache
separately from released RCA/host owners. This layout check must be qualified
on another platform, not treated as proof of cross-platform size equality.
Repeated in-process test sessions may reuse the cache and retain zero newly
tracked blocks; published measurement runs use one fresh process per case.

The standalone lock was seeded from the baseline root lock then trimmed by
offline Cargo metadata. Every remaining registry name/version/source/checksum
tuple matches that root lock. Neither the root manifest nor its lock is edited.
Later slices add other composition/services/persistence/TestStore/macro workloads.
Interval, replacement, general cancellation races, backlog, invalid transport
and other structural-removal interactions remain unqualified gaps.

## Concrete keyed traces

`keyed-steady-{native,deterministic}` and
`keyed-retire-{native,deterministic}` use real move-only leaf state, `KeyedState`,
typed paths and `for_each` in the production Store. Here SIZE is exactly 2 or
256 resident children and ITERATIONS is a positive finite **cycle** count, not
the number of roots. Overflow is rejected before construction. The finite CLI
matrix covers both widths with 1 and 3 cycles in each enabled lane; it is not a
general scenario engine. Keyed native drive operations have a 10-second safety
deadline; deterministic drives use a checked budget `128 * (6 * cycles + 1) +
2048`. Neither limit is a timing denominator.

Steady explicitly commits and finishes one warm-up parent Noop, then checks the
unchanged full projection. This mounts topology outside useful work; a separate
cold-first-root control has no warm-up. Each useful cycle commits parent Noop,
target Ping and target EmitTwo. EmitTwo sends sequential ordinals 1 then 2;
missing, duplicate and swapped outputs fail. Each cycle has 3 roots, 2 responses,
5 parent and 4 leaf reductions, 3 completed roots and target value increment 3.

Retirement has no warm-up work. Each cycle starts target and sibling gated run
effects and awaits both actual future-entry handshakes. It reverses resident
order, then removes/reinserts the target under the same public key in one parent
action. The old gate is never released. Its actual sender reports cancellation;
old strong scoped send/read and callback report scope ended; its distinct reader
closes. Weak upgrade remains Some while an ended strong handle exists, but read
fails. After all strong handles/callbacks drop, upgrade is None while root and
sibling remain live. A fresh target Ping succeeds; the retained sibling sender
delivers output 1, its reader observes the increment, its gate is released and
all causal work finishes. No assertion requires retired root
`ActionTask::is_cancelled`. Each cycle has 5 roots, 1 response, 6 parent and 4
leaf reductions, 2 started tasks, 1 retired and 1 normally completed task. The
new target has incarnation=cycle and value=1; sibling value increments once.
All resident keys, order, incarnations and values are checked after every cycle.

The separate schema `rca-keyed-performance-profile-v1` preserves the old counter
schema and its meanings. It reports warm-up counts separately; useful roots,
responses, reductions and completions; task/capture/state counts; actual route,
callback, weak, reader and sender operations; and final ordered
`[id,incarnation,value]` projection. `projection_digest` is a 64-bit word-wise
FNV fold: seed `0xcbf29ce484222325`, XOR each row word then wrapping multiply by
`0x100000001b3`, in row/word order. This is a diagnostic checksum, not a
cryptographic identity. Keyed outcomes do not reuse counter `final_state`.

`profile_keyed_work` is the non-inlined synchronous actual-drive boundary.
Keyed allocation records have six snapshots (baseline, construction, warm-up,
work, validation, teardown) and five corresponding phase deltas; retirement's
warm-up allocation delta is zero. The old counter record retains five snapshots
and four phases. A known-allocation control checks allocation before, during and
after the warm-up boundary. Current-session cold executor stack attribution and
in-memory error/unwind cleanup are unchanged; no additional retention is allowed.
The final owned projection is fixed stack storage, not a cloned leaf state or
retained heap report buffer. Reporting allocates only after profiler shutdown.

Each state, reducer and future capture has a unique weak ownership witness and
exact drop counter. Future guards are created before async entry. Actual final
state is recovered and dropped, then host/tasks, authorities, probe references
and witness storage are dropped before the final allocation snapshot. These
diagnostic probes, handshakes, per-cycle reads and checks are harness costs and
must remain identical in paired measurements. The tiny shared host extraction
changes binary composition: earlier slice D and this binary are not directly
performance-comparable without freezing the same harness.

The direct async-channel dependency reuses its already locked production version;
there is no new registry version. Negative controls exercise real reducers/hosts
for stale authority, sibling cancellation, revived routes and incomplete finish,
plus wrong order, retained capture and skipped teardown. These two traces do not
qualify optional/case, stack/presentation, direct collection mutation, all keyed
APIs, general cancellation schedules or source coverage.

## Feature descendant retirement

`topology-retirement-{native,deterministic} DEPTH 1 1` accepts depths 5 and 20.
It removes the selected keyed entry's optional child while retaining the entry
itself, exercising non-empty descendant retirement rather than keyed-row removal.
Installation and acquisition happen in the cold phase. Useful work contains one
root action and its causal completion, with exactly 4 or 19 state, Feature and
cache owners dropped and no replacement construction.

Validation runs afterwards: the held leaf rejects before its selector executes,
the held parent remains usable, the keyed entry survives and the 1 MiB payload
is unchanged. Teardown checks that every owner is released and the host stops.
Controls reject skipped removal and retained state, Feature or cache owners.
The descendant declaration count describes the authored fixture, not a runtime
instrumentation counter. Whole-process instruction counts include all phases;
they do not isolate the cost of retirement itself.

## Stack retirement

`stack-retirement-{native,deterministic} SIZE ITERATIONS` uses the real stack
reducer, scoped clients, readers and parked child effects. SIZE is 0/1/16/256;
ITERATIONS is positive and checked for overflow. Each cycle updates children,
pops the upper half (including the midpoint), joins retired work and verifies
the retained prefix. Later cycles reuse public IDs while old clients and output
remain rejected. Final teardown releases the remaining children, captures,
readers, scoped handles and host.

The non-inlined `profile_stack_retirement_work` measures this complete trace,
including observation checks and replacement, not an isolated stack primitive.
Five deliberately broken trace/ownership controls must fail. Native drive has
a 60-second liveness bound; deterministic work retains its checked poll budget.
Neither bound is a performance denominator. Qualification covers these finite
traces, not all stack operations or cancellation schedules.

## Structural diagnostics

With `diagnostics`, `diagnostic-{observed,disabled} 2 ITERATIONS` runs a fixed
two-node graph with repeated and cyclic references; ITERATIONS is 1..=1024.
Each operation increments one value. Debug-observed mode captures before/after,
computes structural changes and writes redacted output. Independent hook,
equality, projection, leaf-lifetime and output counts are checked after work;
the final graph also checks edge labels, origins and reference kinds.

Release-observed mode elides diagnostic callbacks but retains caller-owned
callback construction and teardown. Explicit-disabled mode constructs neither
callback owner. Both still validate the operation result. Missing `diagnostics`
rejects either command. Normal and allocation builds share the workload;
allocation records admit no executor cache and require zero final retention.
This seed qualifies mode and ownership behaviour, not every diagnostic API or
a matched optimisation. The condition-coverage matrix uses debug-observed and
explicit-disabled modes; release-elision proof is separate.

## Store observation

`observe-plain`, `observe-distinct`, `observe-coalesced` (plain) and
`observe-distinct-coalesced` each have `-native`/`-deterministic` suffixes.
SIZE is 0/1/16/64 subscribers; ITERATIONS is positive, even, at most 1,000,000
committed roots. Each root increments move-only state once, with no effects or
responses. Selectors construct payload=revision/2 on every commit, including
equal distinct commits. Every returned value is checked and moved/dropped.

For N subscribers, R roots and H=R/2: selections/originals=N(R+1), equality=NR
only for distinct, clones=N(H+1) only for distinct. Drained deliveries are
N(R+1) plain or N(H+1) distinct; both coalesced modes deliver 2N. Per-subscriber
rows are `[deliveries,sum,last]`, with sums H² plain drained, H(H+1)/2 distinct
drained, H coalesced, and last=H. Final drops equal originals+clones. N=0 has
real root work without dummy registrations. Registration owner expiry is
checked independently of value drops and capture Drop counts.

Schema `rca-observation-performance-profile-v1` separates construction, cold
registration/initial drain, work, scalar validation and teardown (six allocation
snapshots, five deltas). Cold has no warm-up roots. Only
`profile_observation_work` is the synchronous non-inlined steady root-drive
boundary. Exact cold/work lifetime counts, real state extraction, closed next,
state/reducer/host/capture/value owner release are checked before reporting.
Weak control blocks, observations, all handles and probes drop before final
allocation measurement. Deterministic uses Measurement(None) and must retain
zero blocks/bytes; native only admits the existing qualified Tokio TLS stack.
Normal allocator and DHAT are separate builds; counters and driver costs,
one fixed cleanup Store handle, and one pinned receipt-completion box per root
are explicit harness overhead held fixed in any later paired comparison.

Every cold/work/validation/cleanup drive uses separately checked positive caps
S=128*(R*(N+1)+N+1)+2048 and C=2*S+16. S caps scheduler task polls, C caps outer
Drive polls (client polls at most 2C), not useful work. Native delegates the
10-second cooperative deadline; C is not its scheduling guarantee. On rejection,
release deliberate owners/cancel acquired work before separately bounded
cleanup. Terminal extraction is pinned outside rejected clients and resumed,
never re-admitted. Cleanup failure reports both errors and outstanding owners;
no success JSON is emitted. Polls/destructors must return; no timers, parking,
external producer threads, or timeout-as-cleanup claims.

Finite release/CLI qualification runs four modes at (N,R)=(1,2),(16,2),(1,64),
(16,64), plus plain (0,2): 17 per enabled lane in each of default,
deterministic, allocations, and combined builds. Only R=64 pairs are profiling
candidates. Tiny equality/cancelled-next, moved-waker, selector-reentry and
callback-detach controls, real broken traces/retained owners, and scalar
corruptions are correctness tests, not workload modes. Direct external
observation, waits/streams/scopes, cancellation/admission races, multithreaded
fan-out and full W06/R11 coverage remain excluded. Implementation establishes
neither performance gain nor comparison with Composable.

## Direct dependency services

`dependency-{cold,cached,reset,scope} SIZE ITERATIONS` uses explicit Live policy
and borrowed, non-Clone provider values. SIZE is 0, 1 or 16; ITERATIONS is
1..=1024 with SIZE*ITERATIONS at most 1024. For Q=SIZE*ITERATIONS, cold mode
constructs Q independent environments then resolves each once. Cached mode
warms SIZE environments then performs Q borrowed hits; reset mode warms them
then resets and resolves Q times. Scope mode creates Q child overrides (29),
checking the unchanged parent (11) after each child operation.

Independent read, parent/child checksum, factory and destruction counts must
match before reporting. Construction, optional warm-up, work, validation and
explicit teardown use the existing `Measurement(None)` contract. All environment,
provider and probe owners are released before the final snapshot; no executor
cache is allowed. Factory/drop probes add fixture overhead only at actual provider
lifetime boundaries, not extra Arc clones or atomics on each borrowed lookup.
`domain_clones=0` is a structural non-Clone guarantee, not an observed clone count.

The finite validation matrix uses empty, singleton and 16-environment cases at
1 or 64 iterations. This family measures direct services, not Store lexical
overlays, captured effect contexts, TestStore policy, contention, application
ID/random/clock clients or all dependency APIs. No performance improvement or
source-coverage completeness follows from adding the fixture.

## Direct snapshots

With `deterministic`, `snapshot-bytes P I` compares two independent P-byte
buffers under I unique names in one `SnapshotNamespace::new("perf")`.
P=0/8/4096/65536 and I=1..=256. The non-inlined `profile_snapshot_bytes`
contains exactly I compare-only calls; construction, full byte/name validation
and teardown are outside it. Missing references differ from empty bytes;
valid failures reserve names. No capture, redactor, writer, Store or executor
is involved. Schema: `rca-snapshot-bytes-profile-v1`.

`snapshot-ordered C I` admits C=0/1/16/64 and I=1..=16. Each batch starts C
same-type factories on the first direct poll, then completes owned eight-byte
little-endian indices in reverse order, polling once per completion with a
noop Waker. Finish validates input-order bytes and drops values immediately.
Per batch: outer/timeout polls=C+1, case polls=C(C+3)/2, factories/futures/
values created and dropped=C, finish=1. Endpoint wake attempts and observed
Waker callbacks are separate counters; case polls imply neither. C=0 finishes
empty on its first poll. Schema: `rca-ordered-capture-profile-v1`.

`profile_ordered_capture` deliberately includes per-batch endpoint, factory,
pending, value and result-vector allocations, timeout/case Boxes, probes and
manual driver work. It is a fixed caller-controlled schedule, not an executor;
no history accumulates across batches. Timeout-first ties, typed indexed errors
and unpolled/pending destruction are separate controls, never success rows.
Owned-future destruction does not cancel arbitrary caller-started external work.

Both S families use `Measurement::new(None)`, five allocation endpoints, no
warm-up and zero final retained blocks/bytes. Normal `deterministic` and DHAT
`deterministic,allocations` execute identical semantic bodies; rca-test admits
core `diagnostics,test-util`. Without deterministic, both names reject before
measurement. `direct-controlled-poll-v1` identifies this direct contract, not
minimal production Store or broader SnapshotStrategy/codec/writer coverage.
Finite qualification has five byte and five ordered rows, not a Cartesian
matrix. These fixtures establish no timing, allocation improvement, full API
coverage or Composable comparison.
