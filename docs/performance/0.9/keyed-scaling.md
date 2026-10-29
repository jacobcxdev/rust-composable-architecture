# Keyed scaling characterisation

This is local characterisation, not canonical release qualification. It fixes
the pre-optimisation workload, identifies the exact source and host, and records
baseline, registry-only, and complete keyed-scaling attribution. It does not
establish a canonical release decision.

## Scenarios

- Initial mount constructs the keyed fixture before the sample, then times its
  first reconciliation. The consuming closure returns the fixture, so teardown
  occurs after `sample_end`.
- Matching route warms the fixture before timing an action for its middle keyed
  child.
- Unrelated reconciliation warms the fixture before timing a parent-only action.
- One-removal cancellation creates a pending middle-child task before timing
  that child’s removal.
- Bulk removal warms the fixture before timing a transaction that removes the
  first 1,000 active task-free entries.

The initial-mount row requests five samples with `sample_size=1`; its first
100,000-entry sample exceeded Divan's 0.5-second bound, so Divan reported one
sample. The existing lifecycle rows retain their established bounded policies.
Every consuming closure returns its input, keeping destruction after
`sample_end`.

Baseline command, source hash, environment, timer precision, and raw
measurements are recorded in
[keyed-scaling-baseline.txt](keyed-scaling-baseline.txt). Complete-candidate
Divan output is in
[keyed-scaling-candidate.txt](keyed-scaling-candidate.txt); allocation, clone,
and paired memory evidence is in
[keyed-scaling-allocation.txt](keyed-scaling-allocation.txt).

## Three-cohort attribution

Task 4 compared exact Task 1 workload commit
`4a86e5eff3c81b97a8450f4f10f6a13e784cb05a` with repaired registry head
`d70766c11c195a60650ab9d4d5e3636377f5fc06`. The benchmark source hash was
identical at both revisions. The repaired head contains the scope index,
ordered iterative scope tree, and compact scope tokens. Collection structural
revisions do not yet exist, so unchanged keyed scans remain attributable to the
pre-Task-6 path. Complete raw cohort, paired process method, DHAT diagnostic,
arithmetic, and limitations are in
[keyed-scaling-registry.txt](keyed-scaling-registry.txt). Complete candidate
`87024c1` adds private collection revisions and unchanged-stamp caching without
changing benchmark source identity.

| Scenario / median | Task 1 baseline | Repaired registry | Complete candidate | Complexity / observation |
| --- | ---: | ---: | ---: | --- |
| Initial mount, 100,000 entries | 3.125 s | 35.49 ms | 45.19 ms | Average O(n); complete candidate is 69.15x below baseline. |
| Matching route, 100,000 entries | 3.209 ms | 2.990 ms | 12.72 µs | O(1) unchanged structural reconciliation; 252.3x below baseline. |
| Unrelated reconciliation, 100,000 entries | 3.158 ms | 3.015 ms | 8.562 µs | O(1) unchanged structural reconciliation; 368.8x below baseline. |
| One-removal cancellation, 100,000 entries | 4.138 ms | 3.868 ms | 5.740 ms | Structural change forces O(n) keyed refresh plus average O(1) registry removal. |
| Remove 1,000 active entries, 100,000 total | 163.5 ms | 4.379 ms | 5.868 ms | Average O(n + m) for n-entry refresh and m ordered registry removals; 27.86x below baseline. |
| Warmed matching/unrelated allocation delta | 0 blocks / 0 bytes | 0 blocks / 0 bytes | 0 blocks / 0 bytes | Six exercised deltas retain zero-allocation gate. |
| Direct-process RSS, six-run median | 64,684,032 B | 48,422,912 B | 48,898,048 B | Candidate paired result is -24.404762% versus baseline and +0.981221% versus registry-only median. |
| Private 100,000-identity registry DHAT peak | 26,779,612 B | 24,707,472 B | 24,707,472 B | Registry source is byte-identical; -7.737752% versus baseline. This is registry-only, not whole-candidate heap. |

Initial mounting N distinct scopes is average O(n): each indexed mount is
average O(1) plus its collision chain. Leaving M registry nodes is average
O(M), with average O(1) ordered unlink per node. Collision lookup is O(c) for
collision-chain length `c`; concrete identity equality remains reuse authority.
Each keyed collection owns a private lineage and monotonic structural revision.
An unchanged `ForEach` mount compares its last reconciled stamp in O(1) after
validating the Store-local structural token. Any lineage, revision, or mount
change invalidates the certificate before a full scan can mutate state or
unwind. Structural changes therefore retain the authoritative O(n) refresh;
unchanged matching and unrelated paths avoid entry scans. Revision increments
are O(1), warmed stamp checks allocate nothing, and public collection semantics
remain unchanged.

Commit `b84dd0d` remains valid historical evidence for its exact command: one
`/usr/bin/time` wrapper around `cargo bench` observed 69,828,608 B versus
88,473,600 B (+26.701079%) and therefore recorded a block. Repeated direct
benchmark-process measurements supersede that conclusion because they remove
Cargo-wrapper and process-launch effects; Divan remains inside each measured
process. Repaired registry and complete-candidate six-run medians remain about
24% below their paired baselines. Complete-candidate peak footprint is
47,481,228 B versus 63,234,444 B baseline (-24.912397%). These corrected results
remain one-host local evidence, not canonical release qualification.
