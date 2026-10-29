# Mounted keyed allocation diagnostic

This page records development history, not results for the current release
candidate. The supporting audit receipts and development commits remain in the
local archive. A fresh public clone does not include that archive.
[Current qualification](../qualification.md) records the release gates and
available evidence.

The subsequent historical owned-identity metadata repair
removes 32,768 retained bytes at mounted/cleared boundaries with 1,000 children,
without changing ownership or capacity policy. The baseline observations below
remain unchanged.

The later historical mailbox provenance repair
removes another 6,168 retained bytes from Store construction through clearing
at each measured cardinality, without changing mailbox capacity or admission.

The subsequent historical required mount-record repair
removes another 16,384 retained bytes at mounted/cleared boundaries with
1,000 children, without changing teardown or capacity policy.

This executes the [mounted fixture](../../tests/composable-keyed-policy/README.md#mounted-allocation-diagnostics)
at `68114f4`. It is a release-profile diagnostic, not a qualified comparative
capture. Original Composable has no matching native keyed API and is excluded;
the reference is historical RCA v0.7 at its existing locked revision.

The two binaries were built with pinned Rust/Cargo 1.98.0, `--locked --release`,
one Cargo job, incremental compilation disabled, and separate fresh target
directories. Unrelated new socket tests were present during building but are
not part of either measured package or its dependencies. The subsequent
`48a6fd9` commit adds only those socket tests and migration documentation.
No product or measured fixture change occurred during these runs.

Each engine ran cardinalities 0, 2, 10 and 1,000 in a separate process, repeated
three times, for 24 successful processes. Every process had a 15-second external
deadline. Historical then candidate order was fixed, not randomised or balanced.
There is no timing, uncertainty, repeatability qualification or host-admission
claim. Both adapters passed exact output/state/release checks before printing.

## Observed retained requested bytes

These boundary values were identical across the three observed runs:

| Engine | Children | Populated, unmounted | Mounted | Cleared | All domain state released |
| --- | ---: | ---: | ---: | ---: | ---: |
| Historical RCA | 0 | 96 | 1,307 | 1,403 | 0 |
| Candidate RCA | 0 | 96 | 43,568 | 43,568 | 56 |
| Historical RCA | 2 | 300 | 1,511 | 1,703 | 0 |
| Candidate RCA | 2 | 292 | 44,140 | 44,444 | 56 |
| Historical RCA | 10 | 888 | 2,099 | 2,291 | 0 |
| Candidate RCA | 10 | 912 | 47,404 | 47,056 | 56 |
| Historical RCA | 1,000 | 100,456 | 101,667 | 101,859 | 0 |
| Candidate RCA | 1,000 | 82,560 | 552,284 | 461,952 | 56 |

The current collection alone is smaller at 1,000 entries for this scalar child
layout, but the mounted application retains substantially more. Collection-only
results therefore do not explain the complete keyed lifecycle cost. The empty
control also exposes a large fixed runtime/Store difference. Subtracting it can
guide attribution, but does not prove additive allocator capacities.

After extraction and host teardown, the 1,000-child case retains 82,616 current
bytes versus 100,456 historical bytes. Dropping the returned empty domain state
leaves 56 versus zero. Cleared capacity is not automatically a leak, and the
56-byte residual is not classified here. Domain child/root destruction is
independently asserted; arbitrary runtime/global allocations are not required
to reach zero by this fixture.

Cumulative requested bytes at final release were 1,018,976 for the current
1,000-child case versus 104,734–104,798 historically. The historical 64-byte
variation is retained, not silently normalised. The raw file includes allocation
counts, peaks and every intermediate boundary; no allocation cost is inferred
from retained bytes alone.
DHAT `max_blocks` is the number of blocks at peak bytes, not an independent
peak block count. Consequently, current blocks may exceed `max_blocks`.

Action completion is not heap quiescence. Historical worker bookkeeping may
overlap snapshots, and RCA's causal finish does not promise every bookkeeping
allocation has been freed. The engines also provide different lifecycle and
stale-output guarantees. These observations identify costs to investigate; they
neither waive them nor establish equivalent-work performance qualification.

Source inspection finds retained storage in the domain IndexMap, the reducer's
active-entry IndexMap, and the Store scope registry's records and outer indexes.
Clear drops entry identities and records but does not shrink every container.
At 1,000 children, mounting-to-clearing releases 90,332 current bytes, and
clearing-to-extraction releases another 379,336. This identifies an infrastructure
retention trade-off, not proof that automatic shrinking is preferable to reuse.
Separate endpoint allocation-stack profiles are needed before changing storage
or classifying the 56-byte residual.

## Retained evidence

Directory: `/tmp/rca-mounted-release.4JaABd`.

- `candidate-build.log` and `historical-build.log`: successful release builds.
- `observations.json`: commands, exit status, stderr and all 24 decoded ledgers.
- `candidate-target/release/mounted-allocations` and
  `historical-target/release/mounted-allocations`: executed binaries.

SHA-256 identities:

- Observations: `5d5d91e59afa3b7523b1a8c86e4868ac8a6d1af9dc7e80d3cc7e02e71296f8f0`.
- Candidate binary: `33190df53bce79ea2447a5ad3e505c930be1d4d8f2051d67993bf80eb753ea90`.
- Historical binary: `aba2bd7c114d1542056baff364e47be21dd89bf71f1f1aa80628a1c1a25d0ef7`.

This ad hoc diagnostic does not retain the full authenticated source/tool/cache
inventory used by the frozen composition capture controller. Exact allocation-site
attribution and a qualified capture remain separate work.

Independent audit validated all 24 ledger schemas, commands, successful exits,
traces, domain release counters and ten snapshots, plus the retained binary
identities and release-build logs. It verified measured source is unchanged
between the two named commits. Build logs alone do not authenticate tool
invocation or dependency origin; no stronger provenance claim follows.

## Endpoint allocation-stack attribution

An isolated copy at `/tmp/rca-mounted-stacks.IEcoGS` uses ordinary DHAT with
full allocation stacks and release line information (`debug=1`). It leaves
product and quantitative fixture sources unchanged. Each process stops profiling
at one endpoint while its owners remain alive, then completes normal operations,
extraction, runtime/state destruction and domain-release checks. No `exit` or
`forget` shortcut is used. Later stdout snapshots are explicitly placeholders,
not quantitative evidence; only the chosen profile endpoint is admitted.

All four 1,000-child profile endpoints reproduce the earlier retained-byte
totals: mounted 552,284; cleared 461,952; extracted 82,616; released 56.
The allocation stacks identify:

| Retained site | Mounted bytes | Cleared bytes |
| --- | ---: | ---: |
| Scope registry records backing table | 231,432 | 231,432 |
| Reducer active-entry dense buckets and hash index | 104,456 | 104,456 |
| Domain collection, lineage and fixture trace | 82,560 | 82,560 |
| Child mount fingerprint table | 34,824 | 0 |
| Owned entry-stamp identities | 32,000 | 0 |
| Active ID allocations | 24,000 | 0 |
| Mailbox slots | 30,720 | 30,720 |
| Tokio timer wheel | 6,240 | 6,240 |
| Other retained sites | 6,052 | 6,544 |

Thus 335,888 cleared-state bytes belong to reusable scope-record and active-entry
buffers. Live entry identities and IDs are released. These figures separate
capacity retention from surviving child ownership; they do not decide whether
automatic shrinking is preferable to refill reuse.

The sole final 56-byte allocation is `Arc<tokio::runtime::park::Inner>` created
by Tokio 1.53.1's `CURRENT_PARKER` thread-local during runtime shutdown. A
zero-child released profile has the same allocation site and size. This resolves
the attribution left open above: it is not surviving keyed/domain/Store state.
Thread-exit deallocation was not measured, so no broader leak claim follows.

Independent review checked the copied source, driver patch, recorded hashes,
profile totals, allocation sites and successful cleanup markers. The first five
completed runs lacked an external deadline; they remain preserved. A separate
`bounded/` follow-up reran all five endpoints with the same binary under
`timeout --kill-after=5s 30s`; every exit was zero and totals matched.
`run-bounded.sh` records that correction without overwriting earlier profiles.

Retained files include both recipes, source patch, build/version receipts,
profiles and full allocation-stack extraction. Original review inventory
`freeze-hashes.txt` SHA-256 is
`236e198c4916aaab1c68592b1d365135929663349054bad8530cc1f0a3b410a4`;
the additive follow-up inventory `followup-hashes.txt` SHA-256 is
`503c62e619d1c43fb6a0949f33e48545329bf6cfefc9601e0109a274fac4b1b4`.
The latter records the amended diagnostic README and bounded-run inventory.
Tool evidence records explicit paths and compiler version, not binary hashes
or distribution authentication. Profile agreement strengthens attribution,
not timing, scheduler-quiescence or qualified comparative claims.
