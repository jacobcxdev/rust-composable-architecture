# Keyed storage allocation probe

This page records development history, not results for the current release
candidate. The supporting audit receipts and development commits remain in the
local archive. A fresh public clone does not include that archive.
[Current qualification](../../qualification.md) records the release gates and
available evidence.

Diagnostic evidence, not a release benchmark or a successor verdict.

## Inputs and reproduction

- Current implementation: `6def3d9ca31d6793b399418e6c9799cd941dc128`.
- Historical RCA 0.7 storage: exact `composable-architecture/src/keyed.rs` at
  `c6200234cebbbd9b0009cd5788736c78a47f32b0`, retained as `src/legacy.rs`.
  This is **not** the upstream Composable 0.6 baseline.
- Host: macOS 27.0, build `26A428`, `aarch64-apple-darwin`.
- Rust: 1.98.0, commit `88d9e12ae178fab0fb5cc050a94da85685d449ea`, LLVM 22.1.8.
- Profile: this standalone package's default release profile. No LTO/profile
  override, rustc wrapper or extra compiler flags. This is not the workspace's
  benchmark profile. `dhat` 0.3.3 and IndexMap 2.14.1 are locked here.

From this directory, select the matching Rust toolchain and run:

```sh
cargo run --offline --locked --release
```

The recorded run explicitly set `RUSTC`, `RUSTDOC`, toolchain-first `PATH` and
matching `DYLD_LIBRARY_PATH` to the installed Rust 1.98.0 toolchain. It cleared
`RUSTC_WRAPPER`, `RUSTC_WORKSPACE_WRAPPER`, `RUSTFLAGS`,
`CARGO_ENCODED_RUSTFLAGS` and `RUSTDOCFLAGS`. Its shared build cache was
`/tmp/rca-mcmd-backend.4mtNYS/target`. Reproduction need not use that cache.
The isolated lockfile does not inherit every root-workspace dependency version;
the storage dependency is explicitly matched. No Store/runtime is constructed.

The driver creates fresh collections of `usize` keys and values, inserting
ascending unique IDs from zero. RCA and HashMap reserve the requested entry
count; BTreeMap has no reservation API. It snapshots allocation statistics
before content validation, checks every key/value, then drops the collection
and requires live tracked bytes and blocks to return to baseline. Each case
starts a fresh profiler and asserts zero initial tracked blocks, bytes and peak.
Printing occurs after profiling ends. `black_box` and live content checks
limit optimisation, without promising universal immunity to allocation elision.

The first temporary build needed explicit legacy wrapper generic parameters;
that failed compile produced no measurement. Initial isolated resolution used
IndexMap 2.14.2; the retained lockfile was corrected to 2.14.1 and the probe
rerun. All 15 corrected rows exactly match `output.csv`; the earlier run is not
used as the candidate-version receipt. Strict all-target Clippy passed for the
final standalone probe. The copied historical source is deliberately unchanged.

## Results and limits

| Entries | RCA retained requested bytes | Legacy HashMap | Legacy BTreeMap |
| ---: | ---: | ---: | ---: |
| 0 | 24 | 0 | 0 |
| 1 | 100 | 76 | 192 |
| 10 | 496 | 280 | 192 |
| 1,000 | 50,464 | 34,824 | 34,368 |
| 100,000 | 4,379,680 | 2,228,232 | 3,428,352 |

At 100,000 entries RCA uses about 1.97 times the retained requested heap bytes
of the legacy HashMap wrapper, and about 1.28 times BTreeMap. Current empty
collections allocate a 24-byte lineage; both legacy empty maps allocate nothing.
Inline object sizes are separately 88, 48 and 24 bytes on this target.

These are real costs, not an improvement claim. Current RCA carries private
entry revisions and collection lineage and preserves insertion order. Legacy
HashMap has neither that order nor these lifecycle guarantees; legacy BTreeMap
orders by key, not insertion. The probe does not add equivalent lifecycle
tracking to the legacy controls, so it cannot attribute the entire difference
to inefficient implementation of identical semantics.

Counts measure allocator requests; bytes are requested sizes, not usable size,
fragmentation, RSS or whole-process memory. Inline size is not total stack use.
Reserved entry counts do not imply identical actual capacities. BTreeMap's
ascending insertion affects node occupancy. All observed construction bytes
remain live at the snapshot; drops are separately checked for release.

No throughput, latency, actor mounting, clone/churn/removal, unreserved growth,
other key/value types or host architectures are qualified. CPU load does not
turn these deterministic allocation counts into timing evidence. The broader
D10 representation and performance disposition remains open: a semantics-matched
alternative and actual consumer capacity needs are still needed before choosing
storage polymorphism or accepting the whole cost profile.

## Evidence integrity and review

The checksums below describe the original recorded revision. The live lockfile
later moved its local RCA dependency to `0.1.0` with the
historical version transition. Use the
recorded Git revision when reproducing the original lockfile checksum.

| Retained file | SHA-256 |
| --- | --- |
| `src/main.rs` | `b5f6205eb930effa95c76c5fb54813ecacf22a5650c3efebccc2c482d5424967` |
| `src/legacy.rs` | `7bed16e53a9b78c487df5f2152633e5815564988e0cd86bbbebb07b4f664d26e` |
| `Cargo.lock` | `4b30b0fe6bc6c8ea247d7a2a0edbfe59fe4bbc7a97fb9c8e733517b3921fbcc2` |
| `output.csv` | `40f469f6e4121ef02e907b84c2c8cb14a04e1ec483acb6a0a4a1ac29185c6564` |

Local final build/run logs: `/tmp/rca-keyed-storage-pinned.log` and
`/tmp/rca-keyed-storage-clippy-pinned.log`; fresh output:
`/tmp/rca-keyed-storage-pinned.csv`. No production source changed.

A fresh read-only reviewer approved the narrow measurement, verified exact
legacy source and identified peak-baseline and toolchain-provenance caveats.
Both are addressed above and by zero-baseline assertions. The parent corrected
the independently resolved storage version and executed final checks; the
reviewer did not claim to run them. All review exclusions—timing, actor costs,
other operations/payloads/platforms, overall compatibility and Composable 0.6
parity—are accepted limits, not silently satisfied requirements.

## Ordered storage control

After `34a3191`, the same driver adds the unchanged historical wrapper around
`IndexMap<usize, usize>`, pinned to the same 2.14.1 version as current RCA.
This is a valid storage instantiation of that wrapper, not a claim that
historical applications used it. The pinned source supplies no `KeyedMap`
implementation for IndexMap; the probe uses dereferenced storage methods and
does not establish derived reducer routing support. `ordered-control.csv` retains a separate receipt; the
original CSVs and copied historical source are unchanged.

Both ordered collections verify ascending iteration after construction. An
unprofiled sanity trace inserts `[7, 2, 9]`, removes 2 without swapping, and
reinserts it; both must expose `[7, 9, 2]`. This distinguishes insertion order
from sorted-key order. The measured construction and reservation workloads are
otherwise unchanged. All 15 previous lazy-lineage rows reproduced exactly.

| Entries | RCA requested retained bytes | Historical wrapper with IndexMap |
| ---: | ---: | ---: |
| 0 | 0 | 0 |
| 1 | 100 | 68 |
| 10 | 496 | 392 |
| 1,000 | 50,464 | 42,440 |
| 100,000 | 4,379,680 | 3,579,656 |

At 100,000 entries the ordered control makes two allocations, versus RCA's
three; inline sizes are 72 and 88 bytes. On this target, the 800,024-byte
difference matches 100,000 eight-byte entry revisions plus the 24-byte shared
lineage allocation. Ordered storage itself costs 1,351,424 more requested bytes
than the historical HashMap control in this reserved integer workload.

This narrows attribution; it does not waive the costs. The ordered control still
lacks RCA's lifetime/revision guarantees, duplicate rejection and stale-output
protection. It is not a full semantics-matched successor comparison. Replacing
IndexMap or discarding metadata is not justified by these counts alone; custom
storage would also need panic-safe indexing and rehashing qualification.

Pinned Rust 1.98.0 release execution passed twice, including an independent
parent run whose output exactly matched the retained CSV. Strict all-target
Clippy and formatting passed. Parent logs:
`/tmp/rca-ordered-storage-parent.{csv,log}`; implementation logs:
`/tmp/rca-keyed-storage-ordered-{run,locked,clippy-pinned}.log`.
The standalone lockfile adds only the probe's direct dependency edge to the
already-locked IndexMap package. No RCA product source or dependency changed.
Independent source review found no actionable issue and identified the routing
boundary above. CSV SHA-256:
`ac7505a4f132500b66c7e0d33b2374d8f1126ea8eb17098f553f3e8690617c16`.
