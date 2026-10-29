# Minimal native application size qualification

This page records development history, not results for the current release
candidate. The supporting audit receipts and development commits remain in the
local archive. A fresh public clone does not include that archive.
[Current qualification](../qualification.md) records the release gates and
available evidence.

This closes one bounded part of the roadmap's comparative cost matrix:
the release binary size of a minimal synchronous counter application. It
does not establish compile-time, runtime, memory or general application parity.

## Workload and boundaries

Use the three existing pinned counter packages' `minimal-counter` binaries.
Each applies `[3, 1, 7, 2, 9, 4, 16]` and requires the literal final value 42.
Output must be exactly `42\n`, after state extraction and owned-host teardown.
The native applications import no benchmark helpers. Default features exclude
the optional DHAT dependency; test-only reducer-release witnesses are absent.
Legacy application sources are identical. RCA includes the required owned
Tokio runtime and committed-send integration, rather than excluding that glue.

The final sum does not prove ordering. Existing gated tests establish separate
native API contracts; they are not direct tests of these application binaries.
Seven no-effect operations are not representative of every headless workload.

## Frozen capture contract

Extend the existing composition controller and evaluator, preserving prior
protocols and their hashes. Before collecting results, freeze a size-only
protocol with the existing Rust 1.98.0 toolchain, `aarch64-apple-darwin` target,
explicit release profile, one Cargo job and disabled incremental compilation.

Fetch and authenticate locked dependencies before building. Build two separate
fresh targets per engine, selecting only `--bin minimal-counter`. Record both
observations; do not require byte equality or infer statistical confidence.
Retain source, tool, dependency, command, process and executable identities.

Execute each original binary under a deadline. Copy it to a dedicated output,
strip the copy with the pinned Rust `rust-objcopy --strip-all`, then execute
that copy under the same deadline. Both executions must succeed with exact
output. Record pre/post byte counts and hashes, and verify ARM64 Mach-O
executable identity. A failed stripping command invalidates evidence even
when Cargo succeeded. No fallback tool or silently retained unstripped size.

Revalidate frozen inputs before publication. Preserve failed captures and
never retry a failed measurement inside the same capture. Admission must
reject missing arms, changed inputs, wrong target/features/profile, failed
builds/stripping/execution, and incorrect application output.
Freeze build receipts before executing applications and each phase's artefacts
and transcripts before starting the next phase. A changed earlier phase must not be
accepted merely because final inventory hashing sees its changed bytes.

Busy-host execution is permitted for this size-only result. Do not convert
wall-clock observations into qualified compile-time evidence. Clean-check
and release-build timings need their own predeclared host controls, balanced
orders, A/A sensitivity programme and retained CPU/RSS evidence.

## Status

Application fixtures are implemented and verified at `8313d3f`. The controller
and evaluator now implement `--mode minimal-application-size`; published
evidence can be replayed with `load_minimal_size_publication` without changing
original execution receipts. The frozen protocol is
`composable-minimal-size-protocol-v1.json`.

All 212 Python tests pass, including 24 focused tests. Parent independently
reran those 24 tests. Negative controls cover missing/duplicate arms, wrong
output including CRLF, failed stripping, wrong metadata/features/profile,
reused targets, intra-phase and earlier-arm tampering, changed build receipts,
missing or inconsistent provenance, malformed observations, and publication
replay. Independent review found no remaining issues after the demonstrated
failures were repaired. Full-suite receipt:
`/tmp/rca-minimal-size-repaired-python.log`. Initial missing-feature failures are retained
in `/tmp/rca-minimal-size-{red,runner-red}.log`.

Published replay verifies retained consistency: commands, pins, configuration,
tool identities, build receipts and retained executable hashes agree. It does
not independently authenticate external source, tool or dependency origins;
that requires a separate origin audit. Neither replay nor capture qualifies
timing or general comparative performance.

## Frozen native capture

For the refreshed candidate and the explicit footprint decision, see
[1 October refresh and disposition](#1-october-refresh-and-disposition).
The following original capture remains historical evidence.

Signed candidate `f4176ffd6c8124c4ed3716539fd353910a0c0d62` completed capture
at `2026-09-30T17:48:13Z`. Retained evidence:
`target/qualification-evidence/0.1.0/composition/minimal-size-v1-f4176ff`.
All six fresh release builds, six stripping commands and twelve application
executions succeeded without retries. Every execution returned exactly `42\n`.
Parent replay verified the 139-file inventory and recomputed all six rows.

Independent audit matched all 819 captured source hashes to the signed
candidate's Git objects. All 2,211 dependency hashes matched cached bytes:
140 Git files matched the pinned commit objects, and 34 crate archives matched
fixture lockfile checksums, with their source members checked against captured
hashes. All four tool hashes and the LLVM
library matched the installed pinned files. This independently checks local
Git objects, locked archives and retained evidence, not remote fetch history
or toolchain distribution signatures.

| Engine | Original bytes | Stripped bytes |
| --- | ---: | ---: |
| Original Composable | 592,992 | 414,320 |
| Historical RCA v0.7 | 593,616 | 414,320 |
| Candidate RCA | 1,085,456 | 757,296 |

Both independent builds had identical executable hashes within each engine,
before and after stripping. This is observed reproducibility on this host,
not a portable reproducible-build guarantee. Candidate stripped size is
342,976 bytes larger than either baseline: a ratio of 1.8278, or 82.8% larger.
That disadvantage remains open; it is not waived by stronger RCA semantics.
Runtime topology and linked dependencies differ, so these whole-application
sizes do not isolate architecture overhead or establish general superiority.

- Protocol SHA-256: `ea93c62049605a7414d3201522b41d176fe228cde05674230bf0bce542c8fff7`.
- `complete.json`: `f50954521b501307eb37ac91e7b3b9c02feadf9a1031fc310d04d3b000735d03`.
- `analysis.json`: `37801ce1242eea5581f26172a9a76166c43e76b8cdd4f0e9a1677f6b28025797`.
- Controller log: `/tmp/rca-minimal-size-native-f4176ff.log`.

## Retained-binary inspection

Read-only inspection attributes the stripped file gap to 327,680 additional
`__TEXT` segment file bytes and 15,296 additional `__LINKEDIT` bytes. Data
segment file sizes match. Actual `__text` code is 495,216 bytes for RCA versus
253,704 upstream; exception/unwind section payload adds another 80,856 bytes
over upstream. Constants, strings and alignment also differ. Segment padding
means section deltas cannot simply be added to explain file-size deltas.
`__PAGEZERO` reserves virtual addresses but contributes no file bytes.

Symbol-address spans identify both RCA core and Tokio machinery. These are
heuristics, not exact function sizes or removable savings: inlining, generic
instantiation, aliases and alignment affect attribution. Mach-O symbol sizes
printed as zero were not treated as measured zero-cost functions. This rules
out an explanation based only on removable symbol names; it does not identify
a safe product optimisation or justify dropping lifecycle guarantees.

Inspection used Apple LLVM 21.0.0 `llvm-size` and `llvm-nm` from
`/Applications/Xcode-27.2.0-Beta.2.app/Contents/Developer/Toolchains/XcodeDefault.xctoolchain/usr/bin`,
plus `/usr/bin/otool`. These inspect the retained Rust LLVM 22-built artefacts;
they did not rebuild or modify them. Commands were `llvm-size -m -l <binary>`,
`otool -l <binary>` and `llvm-nm --numeric-sort --demangle --defined-only <original>`.
Use Mach-O load-command `filesize`, not `llvm-size` segment virtual size.

Raw outputs and the read-only `inspect.py` recipe remain in
`/tmp/rca-size-inspection.2Oxb09`.
`attribution.txt` SHA-256:
`be01528fd11afc99d7b793cbd498491505df257e3cc95c0a176d4ff53c2ae16b`.
`inspect.py` SHA-256:
`5048a6119b10a19bb2d98e481c18269e696fa25fff5b06fbd66cf1678d95206c`.

Compile time, exact optimisation attribution, the broader composition size
matrix and representative downstream application costs remain separate
obligations.

## Link-time optimisation sensitivity

A separate diagnostic at clean signed `709e256746e85d78aad808b33eea22297198b87a`
compared `lto=false` with `lto=fat` across the three unchanged counter packages.
Only LTO changed between profiles. Both used 16 codegen units, optimisation
level 3, unwind panics, no debug information and the same pinned tools.
No product source, package manifest or default build profile changed.

| Engine | No LTO, stripped bytes | Fat LTO, stripped bytes |
| --- | ---: | ---: |
| Original Composable | 414,320 | 353,136 |
| Historical RCA v0.7 | 414,320 | 353,136 |
| Candidate RCA | 757,296 | 601,696 |

LTO reduced the candidate by 155,600 bytes, but reduced both baselines too.
The candidate remained 248,560 bytes, or 70.4%, larger with fat LTO.
Default-profile stripped sizes matched the earlier capture, despite later
storage repairs. Those repairs do not close this application-size gap.

The fat-LTO candidate still contains the actor's effect compiler and effect-task
poll implementations. Symbol presence establishes retained code, not its exact
size or a safe opportunity for removal. This result rejects LTO alone as a
solution. It does not establish that reducer erasure causes the entire gap.
Future changes need source-level attribution and preserved lifecycle behaviour.

All six fresh builds, six stripping commands and twelve bounded executions
passed. Every execution returned exactly `42\n` with empty standard error.
The retained checker checks ARM64 Mach-O executable headers, byte counts,
exit receipts, output and unchanged source/HEAD receipts. Tool and executable
hashes also passed separate checks. No full test-suite result follows.

Evidence remains in `/tmp/rca-size-lto.GsEWsW`, including the recipe, six
build logs, twelve executables, outputs, source hashes and inspection outputs.
The complete non-target inventory passed its hash check:

| Artefact | SHA-256 |
| --- | --- |
| `freeze-hashes.txt` | `f8833fb8bd9210f6dbfeb54114dcc875eaf6118574579b5e492c3f62a4e5ae13` |
| `run.sh` | `6d7a3929c276eda8c2634f54f65d544263e748ba1eecb162c938d721a5abda1d` |
| `check.py` | `ca5e6b94bbdaa7036ddf23c44c3f17d7a0325aef6732a6771da22c655edaa364` |
| `analysis.json` | `8f5392acee5d7ca3d56c9531232146726ee5d21473ee38cab4140ade606e7b03` |

An earlier launch in `/tmp/rca-size-lto.p3VtYC` failed before compilation:
the configured `kache` wrapper was unavailable on the restricted tool path.
The successful fresh run explicitly disabled compiler wrappers. The failed
directory remains separate and contains no accepted measurements.

This single-build-per-arm diagnostic lacks the frozen capture's repeated-build
and dependency-origin audit. It is not a new qualified capture, compile-time
measurement, runtime comparison or release gate. The original adverse result
and remaining performance obligations stay open.

## Rejected task-registration extraction

A probe based on `3e03b9e7108222527275353df7466c662b4dcf41` moved task-node
insertion from generic `TaskRegistry::register` into a non-generic helper.
The hypothesis was that shared bookkeeping would reduce repeated machine code.
It retained the existing `Abortable`/`Map` wrapper and all public APIs.

The same two candidate profiles produced stripped sizes of 757,328 and
601,696 bytes. These are respectively 32 bytes larger and unchanged against
the previous baseline. Both fresh builds, stripping commands and four
bounded application executions passed. Source hashes remained unchanged
during the probe. The probe supplies no timing result.

Ten registry tests passed before and after extraction. The candidate also
passed nineteen effect tests and the seven-allocation dispatch gate.
Logs remain at `/tmp/rca-task-wrapper-{before,after}.log` and
`/tmp/rca-register-effects.log`. These focused checks do not establish full
behavioural equivalence. Independent review could not run because agent
routing returned HTTP 503.

The extraction was rejected: it provided no measured application-size benefit.
The product file was restored byte-for-byte to the committed baseline.
The earlier historical wrapper assessment
also rules out a direct async-wrapper replacement as an allocation improvement.
Neither candidate is a reason to weaken cancellation or destruction ordering.

The patch, two profiles and executable receipts remain in
`/tmp/rca-register-size.Jk35hf`. The non-target inventory passed its hash check.
Inventory SHA-256: `eeb7e38c7f4af5586e0215fab9d3e336d0c5bf1cae6b48e1770af4f6c9e2a660`.
Patch SHA-256: `8ae8a991c0d2b28a9380e44455b868046dfb3e6f5d245de614d8dfaa3fdf9199`.

## Component attribution follow-up

Read-only inspection of the retained `709e256746e85d78aad808b33eea22297198b87a`
LTO diagnostic binaries narrows the next optimisation target. All twelve
retained executable hashes still match their original receipt. These are not
fresh binaries for the later paired-harvest candidate.

The inspector reads the Mach-O `__TEXT,__text` bounds and groups defined text
symbols by address. Aliases share one span; each span ends at the next distinct
address or the section boundary. It rejects an unaccounted leading region and
checks that attributed spans sum to the section size. Names drive classification,
not source-level ownership: a generic library function containing an RCA type
is counted as RCA-containing. Drop glue takes precedence over hash tables,
then mixed Tokio/RCA, then actor-containing and other RCA names.
Crate-name matching uses word boundaries: `rca_tokio::` alone does not count
as a `tokio::` reference.

| Heuristic symbol-address span category | No LTO | Fat LTO |
| --- | ---: | ---: |
| RCA-containing drop glue | 52,612 | 35,952 |
| RCA-containing hash-table specialisations | 24,760 | 17,376 |
| Mixed RCA and Tokio | 8,304 | 11,244 |
| Actor-containing remainder | 37,816 | 55,744 |
| Other RCA-containing symbols | 44,560 | 28,584 |
| Tokio without RCA names | 60,908 | 39,904 |
| Other | 265,364 | 244,236 |
| Total `__text` bytes | 494,324 | 433,040 |

The non-LTO binary has multiple distinct address spans with the same demangled
`Command<u64, u8>` drop-glue name, including four 1,440-byte spans among the
twenty largest RCA-containing spans. Equal names and lengths do not prove
byte-identical implementations or that linker folding would be valid.
Inlining also changes category attribution: the larger actor category with
fat LTO is not evidence that actor source became intrinsically more costly.

This evidence changes the investigation priority from another small generic
registration extraction to generated destruction and repeated container code.
It does not justify deleting drop paths, weakening unwinding, replacing typed
IDs with untyped storage, or adding allocations merely to hide drop glue behind
indirection. A candidate must show a measured whole-binary improvement and
preserve cancellation, destruction order, panic handling and allocation costs.
No safe product change is established by this attribution alone.

The Ruby inspector and JSON-lines report remain in
`/tmp/rca-size-components.7qqZWd`. The inspector uses the same Apple LLVM
`llvm-nm` identified above and verifies that each binary remains byte-identical
through inspection. Padding, aliases, inlining and unnamed ranges remain limits
of address-span attribution. These figures are neither exact function sizes nor
removable savings. No compiler or runtime benchmark ran for this documentation
follow-up; the earlier adverse whole-application result remains open.

| Artefact | SHA-256 |
| --- | --- |
| `inspect.rb` | `4b2509e22c0da5fde923e2a567b27e9082181842db613af52471ede563a4ca81` |
| `report.jsonl` | `86def2f7873094b7e0b6e4cf6c4ab220793a3061d2a83b453bd6a3009280c20b` |

## Typed actor feasibility probe

A throwaway experiment starts from two archives of signed
`b24ae3e4951361257c3860b2bdbc98f458021154`. The candidate changes the internal
actor from a boxed erased reducer to an owned generic reducer. Public Store
types and effect interpretation remain unchanged. The existing internal
`ErasedReducer` blanket implementation is called statically; the builder passes
its concrete reducer directly. Actor unit-test constructors are adjusted to the
same representation. Only the actor and builder source files differ between
the archives. The managed worktree's product source was never changed.

The question is whether specialising the actor for a counter that always returns
`Effect::none()` lets optimisation discard substantial effect machinery.
The two source variants use identical minimal-counter sources and lockfiles.
Four release builds use Rust 1.98.0, optimisation level 3, sixteen codegen units,
unwinding, no debug information, no incremental compilation, and either no LTO
or fat LTO. One shared fresh target cache is reused sequentially; logs confirm
that changed local packages are rebuilt. This is not compile-time evidence.
The isolated source directory paths differ and were not remapped; the tiny
file-size delta is not uniquely attributable to reducer representation.

| Profile | Erased actor stripped bytes | Typed actor stripped bytes | Difference |
| --- | ---: | ---: | ---: |
| No LTO | 757,280 | 757,280 | 0 |
| Fat LTO | 601,712 | 601,696 | -16 |

All four builds and eight original/stripped executable runs passed. Each run
returned exactly `42\n`, with empty standard error. All eight retained executable
hashes passed their receipt check. The binary inspector reports `__text` sizes
of 493,612 versus 493,216 bytes without LTO and 435,336 versus 430,308 with fat
LTO. The code-section decreases are real for these builds but do not translate
to comparable file-size savings: section layout, padding and metadata also matter.

The candidate's four all-feature actor unit tests passed: compile-panic cleanup,
intermediate-spawn panic cleanup, stale-scope rejection and runtime abandonment
of queued successor work. This is focused evidence only. No workspace suite,
Miri, throughput or allocation comparison was run for this rejected prototype.

Reject this candidate as the binary-size repair. It does not materially reduce
the measured application file and would introduce a wider implementation
trade-off: actor specialisation per reducer type and inline reducer storage.
This does not rule out throughput benefits or other workloads; those were not
measured. Multi-reducer code growth, large reducer layout, allocations and
complete lifecycle equivalence would need separate qualification before adoption.
The experiment does not justify replacing the current erased actor.

Sources, recipe, build logs, binaries and component report remain under
`/tmp/rca-typed-actor.Xu7GUZ`. Source archives are retained so the experiment is
not dependent on later worktree edits. There is one build per arm/profile, not
a new repeated-build qualification capture or a release result.

| Artefact | SHA-256 |
| --- | --- |
| `run.sh` | `fbd7c0d1b984c6be40d98266fd0a894fd01cea6722fc5150fe31821273e1393a` |
| `run.log` | `be0df4c05e5054a00bdab6a196d0211114f50520cb3aa8711f2b471835ddcc40` |
| `actor-tests.log` | `d925a2866d25d7006a386ec95d7c3ec10529c747f52b2d0acb9e4a72e7cdd237` |
| `components.jsonl` | `d1222ba32395902b6de5c888928a08624baee3bbe40960add7e345b5773fede1` |
| Candidate `actor.rs` | `e3305b32005c94eec15c6a8124ec773df427325f248e1b3d2dcbc851566c87c4` |
| Candidate `builder.rs` | `56186df1d5b437ee5fc9163a878b155f1b6680f6153b5253ac659a98983a6026` |

## 1 October refresh and disposition

Clean signed `3420f4d9e44d1b7c46e315038d4e6dd5c3937596` completed the unchanged
size-only protocol at `2026-10-01T13:43:46Z`. All six fresh builds, six stripping
commands and twelve application executions succeeded without retries. Each
application returned exactly `42\n`. Both builds had identical executable hashes
within each engine. Parent replay verified the 139-file inventory, retained
provenance consistency and all six observations, recomputing the analysis.

| Engine | Original bytes | Stripped bytes |
| --- | ---: | ---: |
| Original Composable | 592,992 | 414,320 |
| Historical RCA v0.7 | 593,616 | 414,320 |
| Candidate RCA | 1,106,128 | 774,176 |

Current RCA is 359,856 bytes (86.9%) larger than both legacy applications. It is
also 16,880 bytes (2.23%) larger than the original RCA capture above. This refresh
does not attribute that intervening growth to any single change. Section sizes,
padding and runtime linkage make extrapolation from one isolated optimisation
unsafe. Busy-host execution is permitted for this size protocol; its durations
are not compile-time or runtime-performance evidence.

### Product decision

Accept this measured executable-footprint disadvantage for the current
pre-stable headless product. RCA is not a minimum-size reducer kernel. Its
selected architecture retains bounded asynchronous ingress, commit replies,
causal task ownership, lifecycle cancellation, observation and a runtime-neutral
boundary with an actual Tokio adapter. Those are requirements of the product,
even when a seven-action counter does not exercise them. This explains the
selection, not every byte of the gap; the attribution above remains heuristic.

The absolute additional on-disk footprint is approximately 351 KiB for this
application. It is material and disclosed, but does not justify dropping those
contracts or adding a second reduced-guarantee Store. Prior registration
extraction and typed-actor probes did not produce a material whole-file saving;
fat LTO reduced both sides without closing the gap. No selected consumer has
supplied a binary-size budget that this result violates. That is not proof that
every headless consumer can afford it: minimum-footprint executables are a real
case where Composable or a smaller architecture may remain preferable.

This decision closes the measured minimal-application file-size disadvantage,
not a requirement to be smaller than Composable. It does not accept unmeasured
compile latency, startup cost, resident memory or growth across many reducer
types. Reopen for a concrete executable-size constraint, significant new growth,
or a safe representation change with measured whole-application savings and
preserved runtime contracts. No universal superiority or optimal-code-size claim
follows; other comparative gates remain separate.

Capture command:

```sh
PYTHONDONTWRITEBYTECODE=1 python3 scripts/composition_runner.py \
  --mode minimal-application-size \
  target/qualification-evidence/0.1.0/composition/minimal-size-v1-3420f4d
```

Evidence remains at that output path. Controller log:
`/tmp/rca-minimal-size-native-3420f4d.log` (empty on success).

| Artefact | SHA-256 |
| --- | --- |
| `complete.json` | `cd6606b14bc88c760c3eabdb76d9b7a4df5cd7c914df54b21ac3b5de212d2035` |
| `analysis.json` | `6086326cbcad44258fe62747eed8b00274fa2dba801e0e981e240765cbef23f5` |
| Candidate original binary, both builds | `d89c20a48dc89f07a66f824a74b00cbd8188ca06f07769619359ffa19ab9a4cd` |
| Candidate stripped binary, both builds | `628820de2e302f03ba2ded4f0c7f1c2967795141d3503f5d2c561b7bb1ab2d96` |

The protocol hash is unchanged. Capture completion proves the recorded size
experiment, not a release or a general performance comparison. No product code,
dependency, remote host, public registry, tag or branch publication changed.

Independent review also checked the complete retained inventory and analysis,
matched all 961 captured source hashes to Git blobs at the candidate revision,
and matched all four tool hashes to installed bytes. The controller authenticated
dependencies while its private cache existed; that cache was removed normally.
The reviewer checked retained dependency-origin/hash consistency but did not
independently reauthenticate the 2,211 dependency-file hashes against registry
archives or Git objects. Do not label this refresh a second independent
dependency-origin attestation.
