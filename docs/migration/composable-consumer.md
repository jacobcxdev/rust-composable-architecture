# Composable consumer migration evidence

This is a historical migration case study. Checkpoint revisions, archive hashes
and local log paths below identify retained development evidence. Those captures
and logs are not distributed in this repository, and their commands are not
public reproduction instructions. They do not qualify the current candidate.
See the [qualification summary](../qualification.md) for that boundary. Current
example source and tests remain available in this checkout.

Historical checkpoint: a modified leaf-only baseline passed its 24 existing tests.
The isolated RCA example has executable backend, polling and proxy traces,
including separate local-RAII and managed-cleanup variants. The
[managed keyed composition](#managed-ownership-in-keyed-server-composition) combines
backend, polling and independently owned proxy cleanup across child lifetimes.
Cross-feature cancellation uses an explicit inherited-ID boundary; see the
[composition contract](#proxyserver-composition-qualification).
[Local managed socket ownership evidence](native-managed-proxy-ownership.md)
is narrower than general native resource or
remote cleanup qualification. Complete application migration remains incomplete.
No full application is qualified.

The [captured-test coverage map](mcmd-test-coverage.md) accounts for all 24
baseline server tests at `1756b8e`. It separates retained outcomes, changed
policies and the separate managed consumer's asynchronous proxy-stop evidence.
The two stop outcomes now have controlled-client traces in the managed variant;
the separate original server/keyed example retains its local RAII contract.
No native remote release or whole-application migration is claimed.

## Baseline and scope

The preserved local MCMaitreD snapshot is the historical baseline, not the
changing original checkout or a capture supplied by this repository. Its archive
and member hashes identify dirty application bytes over commit
`553dafc4de964b8062adff54217111c7f5314377`. The extracted `server.rs`,
`routing.rs` and daemon entry point were rechecked against those hashes on
29 September 2026. The unchanged build previously failed with 13 routing
errors before application tests ran; that failure is not an RCA regression.

The snapshot contains a server leaf reducer with 24 test functions, three
injectable client interfaces with test mocks, configuration validation and a
keyed action/effect adapter. It does not contain a working daemon: its entry
point is `fn main() {}`. Floor/table configuration does not establish implemented
floor/table reducers or inactivity shutdown. Network clients, backend process
management and deployment are not demonstrated by mock-client tests.

MCMaitreD is one acceptance case, not RCA's domain model. Its custom routing,
task storage and hierarchy are evidence to assess, not architecture to copy.

## Observable traces in the captured source

Paths and line numbers below refer to archive member `mcmd-core/src/server.rs`.
These are source-derived expectations; the leaf-only checkpoint below executes
the existing tests, not every additional trace listed here. Each future
comparison must record all intermediate actions, state changes and client calls.

| Behaviour | Trace to qualify | Existing source anchor |
| --- | --- | --- |
| Start request | `RequestStart` changes target; `BackendStart` changes phase to `Starting` and calls backend; successful completion changes phase to `Running` and emits `StatusPollingStart` | Reducer at 374; tests at 595 and 623 |
| Start guards | A stopped target, non-stopped phase or error phase prevents a backend start call | Tests at 664, 684 and 707 |
| Start completes after stop request | Successful start completion with stopped target emits `BackendStop`, rather than starting polling | Test at 730 |
| Stop request | `RequestStop` changes target; `BackendStop` enters `Stopping` and calls backend; success resets phase, player count and failure count, then emits `StatusPollingStop` | Tests at 798 and 869 |
| Backend failure | Failed start/stop records backend error and enters error phase | Tests at 758 and 907 |
| Polling | Starting polling enables it and creates a leading tick; running-phase tick emits `StatusPoll`; client result updates count or records a failure; next tick occurs after five seconds | Tests at 944, 983, 1059 and 1097 |
| Stale tick | Mismatched polling generation leaves state unchanged and emits no poll | Test at 1034 |
| Proxy lifecycle | Start emits `ProxyStarted` before forwarding events; current-generation join while stopped requests backend start; stop disables proxy, invalidates generation and calls client | Driver at 516; tests at 1147 and 1276 |
| Proxy guards/errors | Already-enabled start and disabled stop do not call client; stale events are ignored; start failure disables proxy and records error; stop failure records error | Tests at 1123, 1194, 1224, 1255 and 1311 |
| Routing | Wrong server key returns before mutation; emitted actions carry server identity | Reducer at 366; `routed` at 169 |

Configuration validation is a separate behaviour surface in `config.rs`:
duplicate table/server identities, empty tables, zero inactivity duration,
zero ports and duplicate ports within a server are rejected. No configuration
test execution is claimed here.

## Differences requiring explicit treatment

- `ServerState` owns task handles. Its manual `Clone` resets those handles and
  `PartialEq` ignores them (`server.rs:72–115`). Preserve observable state and
  cancellation semantics, not this test-enabling representation. RCA's
  [TestStore contract](../runtime.md#teststore)
  supports independent projected snapshots without adding production Clone bounds.
- Polling generation guards apply to `StatusPollTick`, not `StatusPolled`.
  `poll_status` launches an unversioned future; `StatusPolled` updates state
  without checking phase or generation (`317–326`, `476–487`). Stopping the
  interval therefore does not, by itself, prove late query results harmless.
  Qualification needs an explicitly delayed query result across stop/restart;
  its desired disposition must not be inferred from the old happy-path tests.
- `ProxyStarted` and backend completion actions are also unversioned. The
  proxy driver carries a generation only on forwarded events. Test cancellation
  and late completions independently of event generation checks.
- Several source tests enable repeating polling and end without an explicit
  stop, including the start-success and interval tests. Their existence does
  not prove clean teardown. Migration must account for live effects rather
  than weaken RCA's exhaustive-test contract to make these tests pass.
- `KeyedEffects` is not a working parent composition baseline: its generic
  adapter does not compile against the locked upstream API. No sibling
  isolation or remove/reinsert trace is demonstrated by this snapshot.

## Leaf-only execution checkpoint

The unchanged snapshot was freshly extracted into
`/tmp/rca-mcmd-leaf-baseline.hETi6R` and reproduced the 13 compile errors with
Rust 1.98.0 using `cargo test --locked --offline --workspace`.
The separately retained local extraction patch then placed only the unused
`Route`/`KeyedEffects` adapter in a disabled module. Its source is retained with
the historical capture. This is a deliberate qualification exclusion, not a
repair or replacement of parent composition.

The locked upstream source exposes `Scheduler::schedule` with an iterator of
`Delay`, but re-exports `Delay` only as `pub(crate)` from a private module
(`composable-architecture/src/effects/mod.rs:13,97`). The captured external
adapter therefore cannot be repaired merely by adding the missing generic
bounds. No upstream dependency was patched or replaced for this checkpoint.

With that exclusion, the same command exited zero: all 24 existing leaf tests
passed, with zero ignored or failed tests. Four dead-code warnings remain for
floor/table configuration and their validators. The daemon and doctest targets
contain zero tests. A recursive comparison with the unchanged extraction found
only `mcmd-core/src/routing.rs` different: domain code, test assertions,
manifests and lockfiles remained byte-identical.

Patch SHA-256: `d5a4b007cdc9cf8c4a32e2d4c0b67a4a690a51b85f01a488dec1c404a8afbc71`.
Local passing log: `/tmp/rca-mcmd-leaf-test-20260929.log`, SHA-256
`7cf54cb977be686fe272cfcb84ca03045f4d6899a66d5c5341603e372a478032`.
The preceding failing log is `/tmp/rca-mcmd-leaf-red-20260929.log`.
The historical reproduction procedure required extracting the separately retained
archive into a fresh directory, applying the patch with `git apply`, and running
the pinned command above. It cannot be reproduced from this checkout alone;
offline mode also requires the locked dependencies already cached.
The original checkout and preserved archive were not modified.

## Distinct headless fixture

The existing `examples/composition-search` consumer supplies a second domain;
no new UI implementation is needed. Its reducer test drives replaced searches,
two keyed rows with equal local cancellation identities, nested state, optional
child removal and surviving work. It uses Store-owned dependency overrides and
explicitly finishes remaining work.

An audit found that its final query and row state were compared only between
handwritten and generated reducers. Temporarily changing completed-work
increment from one to zero still passed that test. Independent literal
expectations now require query `current` and exactly row `(1, "first", 1, 1)`.
With those assertions, the same mutation failed on work count zero versus one;
the production increment was then restored. This catches a shared reducer bug,
not merely a difference between two composition representations.

This fixture now compares an independent named snapshot after every action,
including query, ordered rows, nested favourite/work counts, detail identity and
timer count, and event history. Each mutation has a literal expected update.
The production state and actions remain move-only. This is projected coverage,
not a claim about arbitrary omitted state or shared aliases.

The TestStore migration also covers deterministic conformance, dependency
overrides, mapped effects, composition ownership, timer examples, benchmarks and
packaged downstream consumers. `send`/`receive` mean unchanged state; changed
steps use `send_with`/`receive_with`. An admitted interrupted assertion cannot be
recovered by another drive. Intentional unfinished-work tests now either complete
their obligations or catch the expected normal-Drop panic after cleanup.

This remains a reusable starting point, not complete consumer qualification.
Same-key reinsertion now has the additional trace below; full MCMaitreD migration
remains outstanding. The exact candidate and command results are retained in
historical local qualification records; no application parity follows from them.

### Same-key search reinsertion

Test candidate: signed `7e94d527e575b793c5dfcb1ae0a399baab7d3423`.
Both handwritten and generated reducers run the same independently asserted
trace. Real injected search results create row 1 at virtual time 10 ms. Its work
would complete at 25 ms. Removal immediately ends that work's root receipt.
A second real search result recreates key 1 at 20 ms, then starts fresh work
due at 35 ms. At the old 25 ms deadline the new row still has zero completions
and its receipt remains pending; at 35 ms it receives exactly one completion.
Every action compares the named projected snapshot with literal expected updates,
and `finish` rejects leftover actions or work. No production code changed.

Negative control: omit keyed reconciliation's scope departure for removed
entries. The test fails immediately because the old receipt remains live;
the original implementation was restored. The mutation fails in the handwritten
run before the generated run starts; its negative evidence is limited to that
path. Both forms pass the positive trace. This demonstrates detection of lost
teardown rather than merely comparing generated and handwritten implementations.
Strict all-target/all-feature example Clippy and workspace formatting pass.
At the exact candidate, the offline locked all-features workspace suite passed
467 top-level tests (452 unit/integration and 15 doctests), zero failures and one
existing ignored idle-CPU probe; nested filtered subprocess summaries are excluded.
Rust 1.98.0 and the existing isolated target were used.

Independent read-only review of `dcdf07a..7e94d52` found no Critical, Important
or Minor issues. It checked deadlines, gated action admission, literal projected
expectations and receipt completion. It did not run builds or validate the full
suite, which completed afterwards. The final suite result is author-verified.

SHA-256:

```text
337f9dab0da4489ed7968c4cdb087123716e7c4abe4b41341c8865dcfcb521d6  examples/composition-search/tests/app.rs
2f02a526af45fc81fe40a4fb5e7e6f0ebd9811bbce75613f230a481f61a33857  workspace.log
e580330abd4591fef5f3b209235a038da69be8cc27de641d0ee897a6633650bc  clippy.log
```

This proves cancellation of pending timer work and independent fresh lifetime
timing, not a queued-output admission race. It does not extend the projection's
coverage to arbitrary omitted fields or shared aliases. Local logs reside in
`/tmp/rca-search-reinsert.2vdhjx`.
The reviewer also declined native scheduler/cross-thread race claims, broader
roadmap completion and unrelated changes. Those remain outside this bounded
evidence; counting them as qualified would overstate portability and completion.
No deferred minors, dependency changes or production implementation changes.

## Finite owned product

The separate `tests/downstream-packaged` fixture now demonstrates a finite
synchronous host with an owned final product. Its `main` owns one current-thread
Tokio runtime and calls `block_on`. The Tokio adapter is constructed inside that
runtime. No thread-per-Store runtime or blocking Store API is required.

The production Store owns a non-Clone boxed `[1, 2, 3]` product. Its reducer
changes the bytes to `[2, 4, 6]`. The caller awaits the root `ActionTask::finish`,
then awaits `store.shutdown_into_state()`. Assertions check both the final bytes
and the original allocation address. A separate projected TestStore test checks
the reducer against literal expected bytes.

Extraction is an ordered terminal operation, not implicit job completion.
The first processed terminal command wins, even with other full owners alive.
Other extraction callers receive `StoreStopped`. Pending effects are cancelled,
not drained. The future retains its owner, but cancellation after admission
does not retract the command. Unclaimed output is dropped once.
The [runtime contract](../runtime.md#terminal-owned-state-extraction) defines
pre-admission cancellation, final-owner loss and receiver abandonment.

This qualifies only the tested Send-state ownership transfer in D03 and one
finite Tokio-host integration shape. Upstream local-state-to-Send-output
extraction remains different. D01, D02 and broader D04 host questions remain
open. This is not an MCMaitreD migration or a performance comparison.
The historical local extraction record separates source tests from
clean-candidate archive qualification; it is not supplied as a public receipt.

## Remaining executable evidence

1. Preserve the failed capture and distinguish the executable leaf-only
   extraction from a full baseline. Any further compatibility repair needs its
   own diff and verification; do not mix domain changes with baseline repairs.
2. Run equivalent leaf traces against RCA with controlled backend, query and
   proxy clients. Compare domain state and client invocations, not task-handle
   representation or incidental executor polling order.
3. Exercise at least two keyed children, independent cancellation, removal and
   reinsertion of the same key, and delayed results from the removed lifetime.
   These are additional composition requirements, not existing application
   behaviour proved by the captured leaf tests.
4. Extend qualification of the distinct headless composition-search fixture
   rather than duplicate it; successful migration must not define RCA solely
   around this server application.
5. Record negative controls for unexpected state changes, actions and unfinished
   work. Keep migration incomplete until those checks fail when violated.

No before/after performance result, daemon adoption or complete application parity
is claimed by this report.

## Isolated backend migration

The unpublished [`mcmd-server` example](../../examples/mcmd-server/README.md)
contains the first reusable migrated domain feature, not a replacement daemon.
Its backend reducer uses only current RCA public APIs: separate state/reducer,
borrowed actions, explicit effects, Store-owned dependencies and exhaustive tests.
The boundaries and deliberate domain-policy repair are described below.

The four state fields are server identity, target, phase and last backend error.
They are ordinary comparable data; no runtime tasks or custom Clone/PartialEq
exceptions are needed. The captured server's player count, query failures and
proxy/polling fields are not silently discarded: their features remain to migrate.
`Started` and `Stopped` delegates expose that future parent boundary but do not
implement it. Full-state assertions cover all four current backend fields, not
the omitted server features.

### Preserved and deliberately changed behaviour

| Behaviour | Backend migration disposition |
| --- | --- |
| Start/stop request, pending phase, exact server identity, success and failure | Executable equivalents with explicit client results and complete state assertions |
| Backend errors | Operational message retained in error state; no automatic retry. Dedicated start/stop wrapper types and Debug-formatted `Failed(...)` text replaced by one message-carrying error; operation remains explicit in the completion action |
| Start completes after stop request | Repaired: retain stopped target, await start completion, then stop exactly once; never emit a wanted-start delegate |
| Start requested while stop is pending | Repaired: retain running target, await stop, emit `Stopped`, then restart |
| Repeated stop/start during pending operation | Coalesced to latest target; no overlapping client calls |
| Unrelated completion kind | Ignored unless the corresponding phase is pending; not protection against forged same-kind completions |
| Task and dependency plumbing | No task handles in state, no ambient dependency lookup and no cancel-in-flight backend replacement |

The captured reducer's stop guard accepts `Starting` and `Stopping`
(`server.rs:294–314`). A stop can therefore complete before a pending start.
The later start-success handler emits another stop. Its `Stopped` guard now
suppresses the call (`387–419`). Conversely, the start guard rejects requests
during stopping. The stop-success handler does not resume them.

The old injected late-start test does not exercise those delayed external
operations. The migrated policy serialises them. Tests drive both directions
through pending futures controlled at the client boundary. This is application
policy work, not a newly added RCA runtime guarantee.

Thirteen tests cover stable successes, errors after target reversals, guards,
duplicate requests, both reversal directions and latest-target coalescing. They
also cover irrelevant completions and two distinct Store dependency environments.
Each send/receive asserts full state and each test explicitly finishes. Client
ledgers assert exact calls and identities. Unplanned calls fail rather than return
default success.

Test-first evidence: the inert reducer failed both basic request-state assertions.
After basic paths were implemented, six adversarial tests failed on missing error
state, incorrect delegate, absent restart or unguarded completion. Three separate
temporary mutations then proved detection of a missing phase update, duplicate
start admission and omitted `Started` delegate. All mutations were restored.
The local logs are under `/tmp/rca-mcmd-backend.4mtNYS`. They are local evidence,
not published artefacts.

### Qualification checkpoint

Tested implementation: signed `570125cf821d8530e789ec6a84748a1ef7e513a3`.
Host: macOS 27.0, build 26A428, arm64. The commands used absolute Rust 1.98.0
Cargo/Rustc/Rustdoc paths, cleared wrappers/flags and an isolated target directory.

| Check | Result |
| --- | --- |
| `cargo test --locked -p rca-example-mcmd-server` | 13 tests passed, none failed or ignored |
| `cargo-clippy clippy --locked -p rca-example-mcmd-server --all-targets -- -D warnings` | Exit 0, no warnings |
| `cargo-fmt fmt --all --check` | Exit 0 |
| `cargo test --locked --workspace --all-features` | 430 top-level tests passed, none failed, one existing idle-CPU probe ignored |
| Lockfile review | Only the new unpublished example entry was added; existing resolutions unchanged |

The workspace count includes 415 unit/integration tests and 15 doctests. It
excludes four repeated subprocess result lines. Existing macro compile fixtures
still emit their dead-code warnings. The generated allocation report was moved
to the local log directory. It is not comparative performance evidence.

SHA-256 evidence:

```text
b4aef3feeca37c1a1df6c336c9ec612d61412e3d2659955a7d7ba4f38ced5994  examples/mcmd-server/src/backend.rs
d03d7534f82f22c8f7aaabef9b47569146abb79d4e06c72690284e4dfa6ada5e  examples/mcmd-server/tests/backend.rs
868fe5a99c0cecee8050eab577fbc8df48f2a45c67901cc741c3a975f750de13  Cargo.lock
353f7dd9102a43a92035926d3383569e3039d30d7b6cd901e91be2de565e7d95  workspace-final.log
a8ddaf09215caa2ed37cc4437dbd75dcc971f5dea2dc1b28e3d3d8661ce27a37  clippy-final.log
3e1f04fe7e0f0ff0bccb72f41a3cd0be288d7db92cc5f826a6002368206852f1  green-final.log
```

External process cleanup, hung-client recovery, proxy resource acquisition,
polling shutdown and keyed removal/reinsertion remain open. Serial calls within
one live feature cannot prove that cancelling a future undoes external work.
Restart follows the `Stopped` delegate's commit. It does not await effects returned
by a parent handler. Parent cleanup completion needs its own explicit orchestration.
No original MCMaitreD source or archived baseline was changed.

Independent review covered the complete migration diff and relevant runtime
sequencing at `570125c`. It found one important documentation error: the original
prose incorrectly extended commit ordering to parent-effect completion. The
correction matches `EffectSender::send` and the actor's sequence grouping.
The review found no backend implementation defect or minor issue. This correction
changes no Rust source, so the exact-revision test evidence remains applicable.

## Polling migration

The unpublished example now also contains an isolated polling reducer. It uses
existing RCA merge, interval and cancellation primitives; no runtime API or
dependency was added. Backend integration and keyed parent composition are still
pending, so this is not a complete server migration.

Start emits an immediate tick and starts the captured five-second cadence. At most
one query is pending. Later ticks are skipped until that query completes, rather
than allowing overlapping requests and out-of-order results. A hung query stalls
polling until Stop; no timeout policy is implied. Stop cancels the timer and its
descendant query, and restart establishes a new runtime lifetime.

Success updates the count and clears failure state. Failure preserves the count,
records its message and saturates the counter. Stop preserves observations; the
captured backend-stop reset remains an explicit integration decision. The current
query error at this checkpoint retained text rather than the captured client's
error categories. The typed-query-error follow-up below repairs that loss.
There is no category-dependent policy or claim of full client API parity.

Eight full-state tests cover leading and periodic ticks, duplicate starts, skipped
ticks, errors and saturation, stop/restart, inactive actions, Store isolation, and
cancellation of both immediate and interval-derived pending queries. Controlled
response channels prove dropped futures; they do not prove real network cleanup.
Internal result actions are not authenticated against forged external inputs.

Test-first evidence includes the inert reducer's failing Start assertion and
three failing adversarial tests before guards/error handling were completed.
Three independent temporary mutations were detected: omitted cancellation left a
response receiver live, overlapping queries caused an unexpected client call,
and omitted count assignment failed the full-state assertion. All were restored.
Local logs are under `/tmp/rca-mcmd-polling.auRRWR`.

### Polling qualification checkpoint

Tested implementation: signed `7e73d0a123f6f7503aff637b39cb8a86e170c92a`.
The pinned Rust 1.98.0 environment and isolated target described above were reused.

| Check | Result |
| --- | --- |
| `cargo test --locked -p rca-example-mcmd-server` | 21 tests passed: 13 backend and 8 polling |
| Strict package Clippy, all targets | Exit 0, no warnings |
| Workspace formatting check | Exit 0 |
| `cargo test --locked --workspace --all-features` | 438 top-level tests passed, none failed, one existing idle-CPU probe ignored |

The workspace count includes 423 unit/integration tests and 15 doctests, excluding
four repeated subprocess result lines. Existing macro-fixture dead-code warnings
remain. The allocation report was moved to the local log directory; it is not
comparative performance evidence. Manifest and lockfile are unchanged.

SHA-256 evidence:

```text
c2a0213035f77352612de20259bd798efd87e14cdc35ef171670a06198b39b02  examples/mcmd-server/src/polling.rs
a522478da68319486018fe8c06637cc7580004e5020c2215db97b9e150fd401a  examples/mcmd-server/tests/polling.rs
87b0cb711fc501f6d64fb34641e140a2a9645fa37ea77354fa029f9d7cb4d100  workspace-review.log
43ecdc0bd41a6a7366429562f24d648aec965fe8bc19f4181471f17bda69dc64  clippy-review.log
e0a3aa1725dd24623ed08db726c6b457418f0b45486721241412fd9132d94147  green-review.log
```

Independent review found no critical or important issue. One minor coverage gap
was corrected: the cross-Store trace now receives another timer tick and result
after the first Store stops, rather than only completing the second pending query.
The reviewer checked runtime generation propagation and the amended test.

This qualification excludes real I/O cleanup/rollback, timeout/backoff policy,
typed-error parity, backend/proxy/daemon integration, same-Store keyed children,
forged internal actions or arbitrary restored state, domain reconciliation after
direct task cancellation, synchronous physical I/O termination before restart,
performance superiority, native-host coverage and full consumer parity. Those
limitations are not waived by the review or by passing deterministic tests.

## Backend and polling composition

`server::reducer()` now coordinates both features using two persistent required
RCA scopes. `server::State::new` assigns the same server identity to both children.
Backend actions reduce first, then polling Start/Stop reduces through its normal
scope in the same transaction. These controls are not extra Store commits.
Polling control effects precede returned backend effects. This avoids treating
backend delegate commit ordering as asynchronous parent-effect completion.

Polling is active only while the backend is Running and wanted Running. A stop
request cancels polling immediately, even before backend Reconcile. Confirmed
Stopping-to-Stopped clears count, failures and query error; failed stop retains
observations and leaves polling inactive. An unwanted completed start goes
straight towards stopping without a transient query. This deliberately changes
the captured application's later query cancellation while retaining confirmed-stop
observation reset. No library API, dependency or standalone child policy changed.

Six integration tests exercise wanted lifecycle with nonzero count/error reset,
pending query cancellation before backend completion, both reversal directions,
backend failures and duplicate delegates. Every action asserts full combined
state, client futures are controlled at the boundary, exact IDs/call order are
asserted, and each Store finishes exhaustively. The inert coordinator first failed
the RequestStart full-state assertion. The minimal coordinator then passed the
basic and adversarial traces; no additional failing-test claim is made for paths
already provided by that implementation. Independent negative controls proved
that missing cancellation leaves the pending receiver live and missing reset
fails the count assertion. Both mutations were restored.

This is still not a complete MCMaitreD migration. Proxy ownership, keyed parents,
real-client shutdown/reconciliation, arbitrary restored state and forged internal
actions remain unqualified. Child identities must remain stable and consistent.
Cooperative future cancellation does not imply synchronous physical I/O cleanup.
Logs are local under `/tmp/rca-mcmd-composition.BTtPhF`.

### Server composition qualification checkpoint

Tested implementation: signed `76e76f64fa64ce29a5cd8e14260f79c095aa4719`.
Pinned Rust 1.98.0, cleared wrappers/flags and the isolated target were reused.
Package tests passed 27/27, strict all-target package Clippy passed without
warnings, and workspace formatting passed. Full `cargo test --locked --workspace
--all-features` passed 444 top-level tests: 429 unit/integration and 15 doctests.
There were no failures and one existing ignored idle-CPU probe. Four repeated
subprocess summaries are excluded. Existing macro-fixture dead-code warnings
remain. The allocation report was moved to the local log directory; no comparative
performance claim follows. The captured source archive hash is unchanged.

```text
b0e4fd870c2aa3639ae4e0f3488c337ae7c7bceed5b6d177c684840d351cf6f3  examples/mcmd-server/src/server.rs
186de98a363dec8a4aaca0e86113ee5223d8019b5329d2fad14a8434e8f18379  examples/mcmd-server/tests/server.rs
e101f555f667059d05717289366176133aabc64dc56428a2c7b55b9c205f38cd  workspace-final.log
fe42ccf219dd92a73856a325615865c12ec346dbc4ed5c499592ad785aa77461  clippy-final.log
b8ff148663c05839ef8d7b0efc61826c7434730b99c93156aafadcd8c9f319b2  green-final.log
```

Independent review approved the composition slice with no critical or important
finding. Runtime inspection confirmed scope reuse, source-ordered cancellation
compilation and stale-generation rejection. One minor coverage improvement is
deferred: current tests consume Reconcile before checking receiver closure and
reverse Start only after entering Stopping. They do not directly exercise query
release before Reconcile or Stop-to-Start while the phase is still Running.
That interleaving is source-reviewed, not executable-qualified by these traces.

Review exclusions remain explicit: external forged-action validation; arbitrary
restoration/identity mutation; backend Error recovery/timeouts/ambiguous remote
outcomes; physical cleanup, rollback and direct task-cancellation reconciliation;
keyed/proxy ownership; daemon/configuration/deployment/native-host qualification;
and performance/full-consumer parity. None is treated as an achieved roadmap gate.

## Keyed server parent

The isolated `servers` module wraps the composed server reducer with existing RCA
`for_each`. Add constructs a stopped server with client identities matching its
string key. Duplicate Add preserves state and lifetime. Forget removes local
membership; absent Forget is idle. Child actions use `KeyedAction`, replacing the
captured non-compiling keyed-effects adapter without copying its machinery.

Three full-state tests qualify two sibling servers in one Store, equal local
cancellation IDs, the surviving sibling's query and next five-second timer tick,
duplicate preservation and equal-key reinsertion after pending query removal.
Client calls assert literal operation/identity sequences. The old query sender
is disconnected before replacement, so its result cannot update replacement
state. Every trace exhaustively finishes its local effects.

The inert parent first failed the Add state assertion. After minimal membership
and `for_each` wiring, all three traces passed. Negative controls independently
omitted removal and replaced duplicate entries; each failed the full-state
assertion. Both were restored. Logs: `/tmp/rca-mcmd-keyed.0znZXM`.

Forget is not remote shutdown: tests explicitly assert that no backend stop is
implied. The owner must coordinate external shutdown or retain independent
reconciliation authority before daemon adoption. This qualifies local lifetimes,
not cancellation-safe backend side effects, sockets, proxy resources or arbitrary
restored state. One child reducer value handles all keyed entries sequentially;
there is no independently instantiated mutable reducer object per server.

### Keyed qualification checkpoint

Tested implementation: signed `7484fa403b1ccf10c81d950205126400b3734d3a`.
Pinned Rust 1.98.0 and the isolated target were reused. Package tests passed 30/30;
strict all-target package Clippy and formatting passed. The full locked,
all-features workspace run passed 447 top-level tests: 432 unit/integration and
15 doctests, with no failures and one existing ignored idle-CPU probe. Four
repeated subprocess summaries are excluded. Existing macro-fixture dead-code
warnings remain. No manifest or lockfile changed. Generated allocation output
was moved to the local evidence directory, not used for comparative claims.

```text
37fa968f7cb2a89129fc9ddf934a8b8b14748e2f18b94deee93e94f95f391f1a  examples/mcmd-server/src/servers.rs
4f01ef23b93a5525394b21fbac19dbaf87a2a8630eee5fad32e3d87fef0419b8  examples/mcmd-server/tests/servers.rs
292f5bab76ec1d15467ad5b416ac4a06062721c4f84a94e4f962096a53db3406  workspace-final.log
f086f2cfc204125423e56b8f8b2140fa64a7f1b18a0336d592332992d11fdfbc  clippy-final.log
d5f1dd4f841bd6e4b177768b38655a41aa284be66253199a5dd315992669db3e  green-final.log
```

Independent review approved the local keyed-lifetime slice with no critical or
important findings. It confirmed parent-qualified structural scopes, subtree
invalidation and actor provenance rejection against current source and generic
runtime tests. One minor consumer-coverage opportunity is deferred: a result
already emitted before removal but queued until after reinsertion is not driven
by the new trace. That trace proves pending-future destruction, not the queued
result race. Existing core tests and source review remain separate evidence.

Forget plus a simultaneously live sibling is likewise not combined in one new
consumer trace; generic subtree tests, stop-based sibling isolation and separate
Forget coverage support the unchanged runtime contract. Missing-child actions
retain the documented programming-error panic. Forged actions, restored state,
identity edits, pending backend side effects and remote reattachment still need
external policy. No proxy, daemon, native-host, performance or full-parity gate
was closed by this review.

## Owned proxy session

The isolated `proxy` module owns acquisition and event delivery in one cancellable
effect. Its client transfers local resources from an acquisition future into an
owned stream. Dropping either must release those resources. State records only
configuration, phase and error. Ready commits before events; EOF explicitly drops
the stream before sending Ended. Start retries failures; duplicate active Start
does nothing. Stop cancels the lifetime.

Six full-state traces use a non-Clone lease with an exact acquisition/release
ledger. They cover pending and live cancellation, failure/retry, EOF/restart,
Ready-before-event ordering, duplicate suppression, irrelevant inactive messages
and separate Store clients. Literal Config assertions preserve all four input
fields. The pending-restart trace proves old response receiver destruction, not
an already-emitted old event queued across restart. The EOF trace observes release
after Ended; the stronger drop-before-send ordering is explicit in source.

The captured client had separate start and stop-by-ID operations. This new local
RAII contract deliberately narrows that interface: it cannot represent fallible
asynchronous remote shutdown without a separate cleanup/reconciliation owner.
Client Drop correctness is a semantic obligation, not enforced by Rust's type
system. Neither physical sockets nor synchronous destruction on every executor
are qualified. Proxy/backend/keyed composition and JoinObserved policy remain
pending. No real daemon or full old-client parity is claimed.

### Proxy qualification checkpoint

Tested implementation: signed `83c2bc48d3a9b108497fed02db4ccbde86a1def7`.
Pinned Rust 1.98.0, cleared wrappers/flags and the existing isolated target were
used. Package tests passed 36/36; strict all-target package Clippy and formatting
passed. The offline locked all-features workspace suite passed 453 top-level
tests: 438 unit/integration and 15 doctests, zero failures, one existing ignored
idle-CPU probe. Repeated subprocess summaries are excluded. Existing macro-fixture
dead-code warnings remain. Only two existing workspace dependencies were added
to the example; no locked dependency version or RCA public API changed.

The inert reducer failed the initial full-state Starting assertion. After the
owned effect was implemented, that trace passed. The five adversarial traces
passed without further product changes; one test was corrected not to call
TestStore finish before retry, because finish shuts down the Store. Negative
controls independently removed cancellation and the EOF phase transition: the
first failed the live receiver assertion, the second failed full-state equality.
Both were restored before qualification.

Logs are retained in `/tmp/rca-mcmd-proxy.vnajk4`. Generated allocation output was
moved there without making performance claims.

```text
8b20ec7973aee8f2b6b09a6edadb887b772c71ec7c897f4fb4e67d4bda7771fd  examples/mcmd-server/src/proxy.rs
94ade2881b1d0f9ca4d27372f21af05d04e6be4e743661ed53ee1df1758a4b2d  examples/mcmd-server/tests/proxy.rs
18baef003a7fbbe33326fb91b16df5352aa674bf6738c9d60c026fc99a8c9818  workspace-final.log
550e51f1134da442d9c3788e0cef179b7d024f8530584ad65cc45bf17ae7f42b  clippy.log
```

Independent review approved the standalone slice with no critical or important
findings. Current runtime source confirmed cancellation-generation propagation
and obsolete-output rejection before reduction. Queued stale outputs and failed
send cleanup were source-reviewed, not separately driven by these consumer tests.
One minor is deferred: the EOF trace does not isolate release before Ended commit;
moving Drop after the send might preserve the test. The implementation's explicit
ordering is correct; the qualification claim remains limited to the evidence above.

Real socket/process release, asynchronous remote cleanup, server/keyed composition,
backend wake-up, external configuration validation, forged internal actions and
cross-host instantaneous teardown remain outside this checkpoint. They must not
be inferred from the review or the green workspace suite.

## Proxy/server composition qualification

Candidate: signed `bcb43963f3e2269f29b90796855eeb4629b8d8de`.
The server now retains required backend, polling and proxy scopes. A real proxy
JoinObserved requests backend start only while the proxy is Listening and the
backend is Stopped. Its backend effect explicitly removes inherited proxy
cancellation IDs, while retaining root ActionTask and structural ownership.
That boundary preserves default causal cancellation elsewhere. Its separate
qualification record remains historical local evidence.

Five full-state controlled-client traces establish:

- Proxy Stop preserves pending backend start and running backend/query work.
- Repeated joins while Starting/Running do not duplicate starts; joins during
  Stopping/Error do not reverse stop or retry failed work.
- Backend Stop cancels polling but preserves the listening proxy.
- Acquisition failure, explicit retry and EOF leave stopped backend state intact
  without backend calls.
- Duplicate Add preserves live configuration; Forget releases only the selected
  session; a sibling still emits a real event and starts its backend; equal-key
  replacement uses fresh configuration and the old stream sender stays closed.
  Forget also cancels the surviving sibling's pending local backend future when
  that sibling is subsequently removed, without invoking remote stop.

The original inert routing and missing wake policy failed full-state assertions.
Initial integration then exposed the cancellation defect documented above. After
the prerequisite repair, all five traces pass. Negative controls removing the
Stopped-only guard or proxy routing independently fail full-state equality; both
were restored. Three additional traces required no further product changes.

With pinned Rust 1.98.0, package tests passed 41/41. Strict all-target/all-feature
package Clippy and workspace formatting passed. The offline locked all-features
workspace suite passed 466 top-level tests (451 unit/integration and 15 doctests),
zero failures and one existing ignored idle-CPU probe. Nested filtered subprocess
summaries are excluded. Logs: `/tmp/rca-mcmd-proxy-composition.8m8Yj8`.

```text
a588cd4a2982b5e3f04ebaf678b7aed69451a8b19ae33094a7b300156d55611d  examples/mcmd-server/src/server.rs
7f1cda63540d73e4c7e60f01aa671ae8d714a8bf623721065ba48e72777e6835  examples/mcmd-server/src/servers.rs
3485c6a084b745c73c5c7ae6be30969ff36cb7438cc21587dddd0bbc5dcd14a8  examples/mcmd-server/tests/server_proxy.rs
5141ef39ab84640d2293569a222a9d0dbfa0b710d60af9fc1d61b1820621e2f4  workspace-final.log
c9dd95eea983ba657ea34a820a198ab9b85f2ad120ec312912ddff63aa767054  clippy-final.log
```

Independent read-only review of `83a9dad..bcb4396` found no Critical, Important
or Minor issues. It checked all five planned concerns, the cancellation
prerequisite and integration, and independently verified the suite count/hash
and mutation failures. No reviewer build was run. Graph coverage omitted the
new boundary despite matching HEAD, so current source was authoritative.

Rulings retained, including the reviewer's declined scope:

- Required Config replaces ID-only construction/Add: no fabricated endpoints.
  Cost: unpublished example callers must adapt.
- The original no-core-change assumption was superseded by the separately
  qualified explicit cancellation boundary. Cost: one additional API/ownership
  concept; default inheritance and structured ownership remain tested.
- Real socket/process teardown and asynchronous remote cleanup remain open.
  Controlled clients prove local ownership only; treating them as physical
  cleanup evidence would misrepresent adoption safety.
- Daemon integration, configuration loading/validation and complete old-consumer
  parity remain open. Extending this slice's claim would hide missing application
  infrastructure and deliberate client-contract differences.
- Already-queued old proxy-event replacement races are not newly established
  consumer evidence. Closed senders prove local release; separate runtime tests
  prove stale admission. Cost: no end-to-end queued-race claim here.
- Native-host portability, comparative performance and release readiness remain
  open; this macOS deterministic suite cannot establish those broader contracts.
- Post-candidate receipt edits are author-checked documentation, outside the
  immutable reviewer range. Counts, hashes and limitations were checked against
  terminal outputs; they do not receive an independent prose-review claim.

No deferred minors. Equal-ID cancellation isolation combines keyed lifetime
consumer tests with existing explicit scoped-cancellation runtime tests. This
slice does not itself issue Proxy Stop against one of two listening keyed
sessions. The captured separate proxy stop-by-ID interface remains a deliberate
difference, not parity. No complete application or release claim follows.

### Keyed proxy Stop follow-up

The earlier two-listening-session gap is now covered by
`keyed_proxy_stop_preserves_live_sibling_events` in `tests/server_proxy.rs`
(relative to `examples/mcmd-server`). Based on `e783c98`, this test starts two
keyed sessions, stops only `s1`, and checks that only its local session lease is
released. It then feeds a real controlled stream event into surviving `s2`,
receives its keyed Join and backend Reconcile actions, and verifies the exact
backend start identity. Full state assertions preserve all unspecified fields.
Forgetting `s1` leaves sibling proxy/backend work alive; forgetting `s2` releases
both. Exhaustive finishing detects remaining actions and work.

Mutation control: replacing proxy Stop's `Effect::cancel(ProxyLifetime)` with
`Effect::none()` made the new test fail at `stopped_events.is_closed()`. The
mutation was restored, leaving no production-source change. The six-test
server/proxy suite passes, including the final test-only semicolon repairs.
The full workspace passed 561 tests (544 unit/integration and 17 doctests),
zero failed and one ignored idle-profile probe; four nested subprocess replays
are excluded. Strict workspace Clippy, formatting and diff checks passed.
Pinned Rust/Cargo/Rustdoc 1.98.0 ran on aarch64 macOS with explicit compiler
paths, matching dynamic-library path and cleared compiler flags/wrappers.

| Evidence | SHA-256 |
| --- | --- |
| `/tmp/rca-keyed-proxy-workspace.log` | `13aa16a052facfe112371fd4051104e66534c991c45284be7b9da174bbe07773` |
| `/tmp/rca-keyed-proxy-stop-mutation.log` | `9a57dd9b0298c6f8ee7db3e844767be4e53ad3b510d73f1e4187c31917c5e24d` |
| `/tmp/rca-keyed-proxy-stop-clean.log` | `a2a87ad3f26b4819be8f27c24fbcf7cb76338b95a9aa5c35c68afee5abd6b4f2` |
| `/tmp/rca-keyed-proxy-clippy-clean.log` | `5831ce06c15a4910703c822bd85d79ac076afc2e59d7de8cf856b44715c8bf1b` |

Independent read-only review approved this bounded trace with no severity
findings. Declined scope is retained: no reviewer Cargo execution, edits or
subagents; parent owns execution evidence. No production changes, real sockets,
remote cleanup, backend start completion or wider migration/readiness claim is
established. These remain separate concerns, not inferred from controlled-client
lease release. Receipt prose is author-checked. The generated allocation report
was moved to `/tmp/rca-keyed-proxy-dhat.json`.

## Typed query error follow-up

Starting from `9c36c13`, the isolated example repairs the text-only query error
loss recorded in the polling checkpoint. The captured `query_client.rs` exposes
`ConnectionFailed`, `Protocol(String)` and `Other(String)`, and `server.rs`
records their Debug spelling. Current `QueryError` now preserves those categories
and payloads through client results and action routing. Explicit Display arms
retain the captured observable diagnostic spelling, including quoted and escaped
payloads; state still holds `Option<String>`.

Exhaustive polling traces cover all three categories at ordinary and saturated
failure counts, retention of the last good player count, and successful recovery
clearing failures and error text. A composed server trace checks a protocol
payload containing quotes and a newline through the injected client, nested
action, full state and subsequent successful query.

Missing enum variants failed compilation before implementation. An actual
mutation flattening the protocol diagnostic to its raw payload failed the
composed full-state assertion. After restoration, all 43 example tests passed
with pinned Rust 1.98.0; strict all-target Clippy, formatting and diff checks
passed. Independent source review found no actionable issue. Local receipts:
`/tmp/rca-finite-stream.PqmMCW/query-{red,flattened-negative,final,clippy}.log`.

No new dependency, RCA core change, category-based retry/shutdown policy or live
client was added. Original MCMaitreD checkout remains untouched. Configuration
validation, real external-resource contracts and complete consumer migration
remain separate requirements; these controlled traces do not close them.

## Configuration validation follow-up

Starting from `33c505b`, the isolated example preserves the captured
`FloorConfig`, `TableConfig` and `ServerConfig` metadata in a pure configuration
module. It does not flatten table membership or discard inactivity settings.
Validation retains all seven error categories and their diagnostic context:
duplicate tables, empty tables, zero inactivity duration, duplicate servers
across tables, duplicate servers within a table, zero ports and repeated ports.

Traversal precedence matches the preserved source: table identity precedes that
table's validation; table shape precedes its servers; each server identity
precedes its ports; the entire table is validated before floor-wide server
identity checks. Ports scan Java before Bedrock. Empty floors and port lists
remain allowed, and different servers may reuse ports. No socket-binding
feasibility or hostname validation is implied.

Eight tests cover invalid input, competing failures, metadata preservation and
allowed boundaries. An actual mutation checking global server duplicates before
table contents failed the expected port-error assertion. After restoration all
51 example tests, strict all-target Clippy and formatting passed on pinned Rust
1.98.0. Independent review against the archive found no actionable issue.
Local logs: `/tmp/rca-finite-stream.PqmMCW/config-{red,precedence-negative,final,clippy}.log`.

This preserves validation behaviour, not the original typed-ID API: table and
server IDs remain separately named String fields, consistent with the existing
example. No parser, startup adapter, inactivity orchestration, dependency or RCA
core change was added. Existing direct proxy construction and keyed Add do not
automatically invoke this validator; complete application input wiring remains
open. Original MCMaitreD files remain untouched.

The cumulative query/configuration source also passed
`cargo test --offline --locked --workspace --all-features` with absolute pinned
Rust/Cargo/Rustdoc 1.98.0 paths and cleared compiler overrides: 583 passing test
executions, zero failures and one ignored probe. This count includes nested
subprocess executions and is not a distinct-test count. Log:
`/tmp/rca-config-workspace.log`. The generated allocation report was preserved
at `/tmp/rca-config-allocation.e64top/dhat-heap.json` rather than committed.

## Gated proxy event cancellation follow-up

After `5796dcc`, a composed consumer test supplies a real old-session
`JoinObserved` through the injected stream. The receive matcher checks that exact
action and deliberately rejects it, proving the action remains at the assertion
gate before reduction. Explicit cancellation of the proxy Start action's causal
task removes that output; the session is released, no event remains to receive,
and no backend call occurs. Stop/Start then establishes a fresh session whose
real join starts the backend exactly once. A controlled backend failure and proxy
Stop close the trace with exhaustive finishing.

This is not Stop-driven overtaking of an already-gated action. TestStore refuses
new sends while output awaits assertion, and the actor's assertion gate blocks
ordinary mailbox commands. Causal cancellation is an independently serviced
control boundary. A production-mailbox Stop/replacement race therefore remains
a separate consumer qualification obligation; this test does not substitute an
impossible test-gate ordering for it or weaken the exhaustive contract.

Omitting causal cancellation failed the new test. All 52 example tests, strict
all-target Clippy and formatting passed on pinned Rust 1.98.0. Local logs:
`/tmp/rca-finite-stream.PqmMCW/queued-{uncancelled-negative,final,clippy}.log`.
No production code, dependency or runtime contract changed.

## Queued proxy event invalidation follow-up

After `b072ecb`, `tests/proxy_mailbox.rs` exercises the ordinary Store mailbox,
without TestStore's assertion gate. A test-only wrapper blocks the actor while
the controller admits Stop, then supplies `JoinObserved` through the real old
session stream. A test runtime observes the return of the precise effect poll
that consumed that event. With the actor still blocked, a single failed Full
probe proves both mailbox slots are occupied: Stop and the old effect output.
The probe is not retried, so it cannot itself fill an earlier empty slot.

Releasing the actor lets Stop invalidate the queued output before reduction.
The wrapper records zero reduced events, the backend remains stopped and no
backend client call occurs. A replacement session's real join then reaches the
reducer and starts the backend exactly once. Controlled backend failure and
explicit proxy Stop finish the trace, with both stream receivers closed.

Thread-local markers attribute notices to the same wrapped task poll; a one-shot
initial Pending handshake ensures the stream has parked before the actor worker
is blocked. Every external gate and async operation is bounded. The actor gate
is released before assertions and also on early unwind. These are test-host
controls, not changes to application or core scheduling contracts.

This qualifies that particular Stop-before-old-output FIFO ordering. It does
not prove arbitrary interleavings, remote socket cleanup or general stream
equivalence. The example adds Tokio's multi-thread runtime only as a test
dependency; no new dependency version or core API is introduced.

Bypassing Stop failed immediately with one stale reducer entry instead of zero.
All 53 example tests, strict all-target Clippy and formatting passed on pinned
Rust 1.98.0; 30 additional fixture runs passed. Independent source review found
no actionable issue. Negative-control and repeated-run logs are
`/tmp/rca-finite-stream.PqmMCW/mailbox-{stop-negative,repeat}.log`; independent
parent checks are `/tmp/rca-mailbox-parent-{tests,clippy}.log`.

### Local socket ownership checkpoint

The [native proxy ownership receipt](native-proxy-ownership.md) adds real
loopback socket release evidence through the existing client seam. It covers
gated acquisition cancellation, normal stream completion, keyed removal and
replacement with sibling isolation, and ordinary Store shutdown followed by
owned-host join. A retained-handle negative control proves that a cleanup
acknowledgement alone is insufficient: the peer must independently observe EOF.
All 58 example tests and strict Clippy passed; independent review and parent
rerun of the five new socket tests passed. This is test-only local adapter
evidence, not a production proxy implementation, cancellation-safe connection
establishment, remote cleanup or a complete application migration.

### Typed proxy acquisition errors

Starting from `333b2fd`, acquisition now preserves the captured `StartFailed` and
`Other` categories and message payloads instead of flattening them into text.
Explicit Display arms retain debug-shaped diagnostics, including quoting and
escaping. Composed server traces check both categories, exact result actions,
full state, released acquisition resources, explicit retry and unchanged backend
calls. No category-dependent retry policy or RCA core API is added.

At this checkpoint, `StopFailed` remained an open migration gap with fallible asynchronous shutdown.
The local Drop-based client cannot report that outcome. This refinement does not
claim complete proxy parity or close either captured asynchronous-stop test.

On pinned Rust 1.98.0, all 58 example tests and strict all-target, all-feature
Clippy passed. The initial tests failed compilation against the missing variants.
A negative control restoring text-only Display failed the composed full-state
assertion. All 58 tests passed again after restoration; formatting and diff checks
passed. Logs are `/tmp/rca-proxy-errors-{red,tests,clippy,negative,restored}.log`.
This checkpoint has author review, not independent review clearance.

### Asynchronous cleanup ownership prerequisite

The [controlled host fixture](../../rca-tokio/tests/async_cleanup_host.rs) adds six
tests with nine traces. An application-owned Tokio task awaits the stop operation.
Its bounded command carries a session token and a reply channel. RCA owns only
the reply effect, not the stop operation.

Success and failure retain their exact outcomes after causal-task cancellation
and Store shutdown. The Store remains Stopping until successful completion.
Wrong-session, full-queue and closed-host requests cannot report successful release.
The fixture uses ordered `Store::read` calls instead of synthetic inspection actions.
The final report requires an outcome rather than an optional value.

The initial lost-outcome control failed all three caller-mode tests. A second
control reported Stopped at admission and failed the pending-state assertion.
The restored all-feature workspace suite passed 615 top-level tests and 17
doc-tests on aarch64 macOS with pinned Rust 1.98.0. The existing
`idle_actor_profile_window` test remained ignored. Counts exclude subprocess
repeats. Strict package Clippy, formatting, diff checks and workflow YAML parsing
passed. Existing native-host CI now includes the fixture, but no hosted run occurred.

Independent read-only review found no actionable issue before the required-outcome
refinement and CI inclusion. Those final changes received author review and the
checks above. This is not full-branch review clearance.

No public API, dependency or runtime behaviour changed. The operation is controlled,
not native. Recovery after failure, host panic, process exit, persistence,
acquisition races, replacement and continued multi-command ownership remain open.
The fixture alone does not close either captured asynchronous-stop migration test.

Source SHA-256: `5b629b2a5437d7b399567b4c766db24893e0343dab2ca4287df747b5be3b0067`.
Logs use `/tmp/rca-async-cleanup-` with suffixes `red.log`, `green.log`,
`premature.log`, `final-workspace.log` and `final-clippy.log`.
The final workspace log SHA-256 is
`ccdb18f02944b195c12aaefd48c48f64122d07ad11a291d4be3fac8700a8e3e6`.
The final Clippy log SHA-256 is
`0cb5c79e1fec806cbf77030edcd21790741100705a565ca102ec3772ebc66c50`.

### Managed external proxy integration

The separate `managed_proxy` consumer adds the captured fallible start/stop-by-ID
contract without changing the local RAII proxy or existing server/keyed
composition. One application-driven owner serialises a fixed server's operations;
RCA owns only event delivery and reply effects. Both command and per-request reply
queues have capacity one. Completed release, not admission, changes Stopping to
Stopped. EOF transfers cleanup outside the event cancellation identity while
retaining ordinary structural and Store ownership of the reply effect.

Start records a cleanup obligation before calling the client, because failure may
leave partial resources. A failed start or stop blocks replacement until explicit
cleanup succeeds. Exact StartFailed, StopFailed and Other payloads survive the
result action; state retains Debug-shaped diagnostics. Last-control closure
attempts cleanup once when no previous error is retained. The must-use Recovery
retains pending authority and supports explicit borrowed retries. No automatic
retry, timeout or backoff is added.

Successful stream transfer uses a bounded acknowledgement, not buffered send
success alone. Review exposed a race where cancelling an unread successful reply
missed prompt orphan cleanup. A deterministic test failed with no Stop call;
acknowledged hand-off fixes that window. The owner still processes Stop while
acknowledgement is pending, and retires the old acknowledgement before cleanup so
later abandonment cannot affect a replacement. A received event stream does not
own external cleanup; callers still request Stop or close all controls.

This narrows the two captured stop rows to controlled-client coverage. It does
not migrate the whole server, qualify a real proxy, or establish native release
on any host. The application must maintain one owner per external server ID,
drive it independently, join it and preserve pending recovery. Host panic/abort,
process exit, duplicate owners and discarded recovery remain outside the proof.
A client that never completes also prevents subsequent serial operations.

Qualification on aarch64 macOS with pinned Rust/Cargo/Rustdoc 1.98.0 passed
`cargo test --offline --locked --workspace --all-features`: 631 top-level native
tests and 17 doc-tests, excluding subprocess repeats. The existing
`idle_actor_profile_window` remained ignored. The consumer contributes 73 tests,
including ten owner tests and five exhaustive reducer tests (one covers two
acquisition-error categories). Strict all-target/all-feature package Clippy,
workspace formatting and diff checks passed. No timing measurement is claimed.

The initial owner and reducer scaffolds failed their behavioural tests. A
premature-success mutation failed at the pending-cleanup assertion and was
restored. The EOF trace caught inherited cancellation before its explicit
hand-off fix. Independent review found the buffered-reply cancellation race;
the fix and replacement regression received fresh read-only review with no
remaining actionable findings. Review excludes native correctness, whole-server
migration and crash recovery; those remain open, not waived acceptance gates.

| Local evidence | SHA-256 |
| --- | --- |
| `/tmp/rca-managed-workspace.log` | `7b176792b7816290706ed9ea062860368c76b113b1a826a24027aa42a902c0b9` |
| `/tmp/rca-managed-clippy.log` | `a1ddde70e38030c4dac8f35ef79aa83de67c9fd4dd68a3a5d28b4f6b779469ea` |
| `/tmp/rca-managed-owner-negative.log` | `859152baf10354b9200579f9fe0bbd0a6a72dd428916cca0890bb01b9657a8cb` |
| `/tmp/rca-managed-buffered-red.log` | `419f1a22319c3602d2078749f39835cbf9fbe182e1fbc50c7a89aec7230661ac` |

### Managed ownership in keyed server composition

The later [managed keyed consumer](../../examples/mcmd-server/tests/managed_servers.rs)
composes the managed proxy with backend and polling through persistent keyed
scopes. A fixed configured roster receives one independently driven owner per
ID. Duplicate IDs are rejected before owner construction. Control routing is
local to each composition, with no global owner registry or new core API.

Removal requires confirmed backend and proxy shutdown plus inactive polling;
failed proxy cleanup keeps membership intact until explicit retry succeeds.
Restoration reuses the original owner. The separate RAII example retains its
explicit local-only `Forget` contract. This extends controlled-client integration,
not native resource qualification or whole-daemon migration. Applications still
drive/join owners and preserve pending recovery after Store closure.
