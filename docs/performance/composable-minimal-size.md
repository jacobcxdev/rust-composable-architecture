# Minimal native application size qualification

This page defines the current RCA/upstream size diagnostic. Archived development
results do not qualify the current release. See [current qualification](../qualification.md)
for release gates and available evidence.

This diagnostic measures the release binary size of a minimal synchronous counter
application. It does not establish compile-time, runtime, memory or general
application parity.

## Workload and boundaries

Use the current RCA and pinned upstream counter packages' `minimal-counter` binaries.
Each applies `[3, 1, 7, 2, 9, 4, 16]` and requires the literal final value 42.
Output must be exactly `42\n`, after state extraction and owned-host teardown.
The native applications import no benchmark helpers. Default features exclude
the optional DHAT dependency; test-only reducer-release witnesses are absent.
RCA includes the required owned Tokio runtime and committed-send integration,
rather than excluding that glue.

The final sum does not prove ordering. Existing gated tests establish separate
native API contracts; they are not direct tests of these application binaries.
Seven no-effect operations are not representative of every headless workload.

## Frozen capture contract

The [size protocol](composable-minimal-size-protocol-v1.json) fixes Rust 1.98.0,
the `aarch64-apple-darwin` target, an explicit release profile, one Cargo job and
disabled incremental compilation. Its current byte and document hashes are
checked by the evaluator; archived three-engine captures are not valid inputs.

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

## Running and interpreting the diagnostic

```sh
python3 scripts/composition_runner.py --mode minimal-application-size \
  target/qualification-evidence/0.1.0/composition/minimal-size-<revision>
```

The schedule contains four builds: two for upstream Composable and two for RCA.
Each original and stripped executable must pass its output check. The upstream
revision is pinned in the protocol and fixture lock. Do not patch a failed
reference or retry a failed capture to produce a favourable result.

Published evidence can be replayed with `load_minimal_size_publication`.
Replay checks retained commands, pins, configuration, tool identities, build
receipts and executable hashes. It does not independently authenticate external
origins or qualify timing and general comparative performance.

A failed upstream baseline remains failed evidence. It does not prevent RCA's
own release checks from running, but it prohibits size comparisons that require
the missing reference result. No current comparative size result is claimed here.
