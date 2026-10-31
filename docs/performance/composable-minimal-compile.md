# Minimal target-clean compile diagnostic

This page defines the current RCA/upstream compile diagnostic. Archived development
results do not qualify the current release. See [current qualification](../qualification.md)
for release gates and available evidence.

This opt-in diagnostic compares the current RCA and upstream `minimal-counter` applications,
not the workspace test suites. It adds no Rust workload and changes no existing
size, allocation or timing protocol. The v2 JSON protocol is frozen before capture.

Local captures require [compiler memory protection](../../CONTRIBUTING.md#build-memory-safety).
The current controller creates separate process groups and does not protect
their memory use. Do not run this command locally until each compiler process
has equivalent protection.

```sh
python3 scripts/composition_runner.py --mode minimal-application-compile \
  target/qualification-evidence/0.1.0/composition/minimal-compile-v2-<revision>
```

Run only from the committed, clean source on the pinned Darwin host, with no
competing builds. No complete comparative capture is recorded yet.

## Work and schedule

Six blocks each run both workloads against both engines: 24 measured commands,
six observations per engine and workload. Forward and reverse engine orders each
appear three times. Odd blocks run check before release. Even blocks reverse that
workload order. Both workloads use the same engine order within a block. The
[protocol](composable-minimal-compile-protocol-v2.json) and its exact hashes fix
this schedule; archived three-engine captures cannot satisfy it.

- Check: `cargo check --locked --offline --bin minimal-counter --target aarch64-apple-darwin -j 1`.
- Release: `cargo build --locked --offline --release --bin minimal-counter --target aarch64-apple-darwin -j 1`.

Each command also supplies its explicit manifest and a unique, previously absent
`CARGO_TARGET_DIR`. The runner fetches and authenticates private Cargo sources
before measurement. Fetching, metadata collection, hashing and host-idle checks
remain outside the timed command. The runner uses no incremental state or compiler wrapper.
The explicit dev profile fixes check settings. The release profile matches the
minimal-size programme, including unstripped output, no LTO and 16 codegen units.
This is **target-clean**, not OS-cache-cold. Filesystem caches and shared source
pages can remain warm. The diagnostic does not measure application-edit incremental checking.

Each release binary executes outside measurement and must print exactly `42\n`.
The runner records and checks its hash before and after execution. Check-only success
does not claim a runnable executable. After capture, the runner removes temporary
build directories and executables. Command, execution and hash receipts remain.

## Admission and retained evidence

The runner reuses the canonical host identity and active host monitor. Each
measured command requires sustained quiescence first. Active samples check thermal
state, power and competing processes. The controller's own work is not mistaken
for a competing process. A failed idle sample restarts the idle window.
This does not retry a measurement. Timeout, failed command, contaminated measurement,
changed input or incorrect output aborts the capture without retrying it.

The existing machine lock prevents another cooperating controller. It does not
replace process/thermal checks. Each command has a 600-second deadline and each
release execution a 30-second deadline. Failure retains incomplete evidence.

Retained evidence includes:

- The exact protocol, schedule, source revision and source hashes.
- Locked authenticated dependencies, compiler identities and the `/usr/bin/time` identity.
- The sanitised build environment, profiles and host telemetry.
- Explicit command, working-directory, target and counter-file bindings.
- Terminal status, raw `time -lp` output and parsed counters.
- Application execution receipts, analysis and a complete hash inventory.

Before publication, the runner reconstructs the full analysis from raw counters
and commands, independently of the in-memory measurement rows. Duplicate JSON keys,
missing counters, failed exits and changed receipt bindings are rejected.
The runner freezes each admitted document and preparation receipt before later
commands run. Final hashing cannot silently accept an earlier rewrite.

## Interpretation

Report each observation and its engine/workload median and range:

- Command elapsed wall seconds.
- Command plus reaped descendants' user and system CPU seconds, separately and summed.
- Maximum reported process RSS across the accounted command and reaped descendants.

The RSS counter is **not aggregate simultaneous compiler-tree memory**. The raw
peak-footprint counter is retained but is not substituted for RSS. See
[Darwin command resource accounting](darwin-command-resource-accounting.md).

Paired ratios divide RCA by upstream Composable in the same workload and block.
They are descriptive samples, not confidence intervals or acceptance thresholds.
Darwin `time -p` prints two decimal places. Quantisation is not measurement accuracy.
Zeros remain unchanged. If either paired value is zero, the ratio is `null`, with
no pseudocount or infinite speedup claim.

All reports and completion receipts retain `timing_qualified: false`,
`qualified_comparison: false` and `regression_pass: false`. Successful provenance
and a complete schedule do not establish categorical performance superiority,
non-regression, cross-host portability, or crate-level cost attribution. Six blocks
are a bounded diagnostic, not a statistically qualified regression gate.

A failed upstream baseline remains failed evidence. It does not block RCA's own
release checks, but no comparison may use a reference command that failed or did
not complete. No current comparative compile result is claimed here.
